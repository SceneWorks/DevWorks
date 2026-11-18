//! `router.configure` decisions for catalog models, tiers and decision models.
use super::*;

/// `Model`: a catalog model edit (create, or compare-and-set update).
pub(super) fn configure_model(
    tx: &mut dyn WriteTransaction,
    actor: &ActorId,
    model: &RouteModel,
    expected: Option<i64>,
) -> Result<DomainDecision, Failure> {
    if latest::alias_family(model.id.as_str()).is_some() {
        return Err(invalid("reserved_latest_alias"));
    }
    let profile_name = wire(&model.endpoint_profile);
    if profile_name == "SystemOne" {
        return Err(invalid("endpoint_profile"));
    }
    let efforts: Vec<String> = model.efforts.0.iter().map(wire).collect();
    if efforts.iter().any(|e| e == select::DEFAULT_EFFORT) {
        return Err(invalid("efforts"));
    }
    let unique: BTreeSet<&String> = efforts.iter().collect();
    if unique.len() != efforts.len() {
        return Err(invalid("efforts"));
    }
    if tx.router().decision_model(&model.id)?.is_some() {
        return Err(refuse(
            DomainErrorCode::Conflict,
            "decision_model",
            Some(&model.id),
        ));
    }
    if let Some(binding) = &model.connection_id {
        match tx.router().connection(binding)? {
            None => return Err(refuse(DomainErrorCode::NotFound, CONNECTION, Some(binding))),
            Some(c) if c.profile != profile_name => {
                return Err(refuse(
                    DomainErrorCode::InvalidArgument,
                    "connection_profile",
                    Some(binding),
                ));
            }
            Some(_) => {}
        }
    }
    let context = match &model.context_tokens {
        None => None,
        Some(n) => Some(n.0.parse::<i64>().map_err(|_| invalid("context_tokens"))?),
    };
    // Tier membership has one home, the routing settings (`Tier`): it cannot change here.
    let members = tier_members(tx.configuration())?;
    let current_tiers: Vec<String> = TIERS
        .iter()
        .filter(|(t, _)| members.get(t).is_some_and(|ids| ids.contains(&model.id)))
        .map(|(t, _)| (*t).to_owned())
        .collect();
    let mut asked: Vec<String> = model.tier_memberships.0.iter().map(wire).collect();
    asked.sort_by_key(|t| TIERS.iter().position(|(n, _)| n == t));
    asked.dedup();
    if asked != current_tiers {
        // Catalog annotations are projections: revision 3 shows virtual family slots, while
        // older clients see current concrete targets. Accept their current read-only echo,
        // never a membership change (only the Tier action writes the routing setting).
        let models = all_models(tx.router())?
            .into_iter()
            .map(|m| (m.id.as_str().to_owned(), m))
            .collect();
        let connections: Vec<_> = all_connections(tx.router())?
            .iter()
            .map(status_filter)
            .collect();
        let projected = [1, 3].into_iter().any(|revision| {
            let members = catalog_tier_members(&members, &models, &connections, revision);
            let tiers: Vec<String> = TIERS
                .iter()
                .filter(|(tier, _)| members.get(tier).is_some_and(|ids| ids.contains(&model.id)))
                .map(|(tier, _)| (*tier).to_owned())
                .collect();
            asked == tiers
        });
        if !projected {
            return Err(invalid("tier_memberships"));
        }
    }
    let current = tx.router().route_model(&model.id)?;
    let now = tx.now_ms();
    let position = match (&current, expected) {
        (Some(_), None) => {
            return Err(refuse(
                DomainErrorCode::Conflict,
                MODEL_AGGREGATE,
                Some(&model.id),
            ));
        }
        (None, Some(_)) => {
            return Err(refuse(
                DomainErrorCode::NotFound,
                MODEL_AGGREGATE,
                Some(&model.id),
            ));
        }
        (Some(c), Some(e)) => {
            if c.revision != e {
                return Err(Failure::Storage(StorageError::conflict(
                    RevisionDimension::Configuration,
                    MODEL_AGGREGATE,
                    model.id.clone(),
                    rev(e),
                    rev(c.revision),
                )));
            }
            c.position
        }
        (None, None) => all_models(tx.router())?
            .iter()
            .map(|m| m.position)
            .max()
            .unwrap_or(0)
            .saturating_add(10),
    };
    let next = StoredRouteModel {
        id: model.id.clone(),
        display_name: model.display_name.0.clone(),
        endpoint_profile: profile_name,
        upstream_model_id: model.upstream_model_id.0.clone(),
        enabled: model.enabled,
        efforts,
        image_input: model.image_input,
        tools: model.tools,
        context_tokens: context,
        connection_id: model.connection_id.clone(),
        position,
        revision: current.as_ref().map_or(1, |c| c.revision),
        updated_at_ms: now,
    };
    if let Some(c) = &current {
        let mut same = next.clone();
        same.updated_at_ms = c.updated_at_ms;
        if &same == c {
            return Ok(unchanged(&c.id, c.revision));
        }
    }
    let saved = tx.router().put_route_model(&next, expected)?;
    // A configuration change ends the model's cooldown (its cause may be gone).
    if current.as_ref().is_some_and(|c| {
        c.endpoint_profile != saved.endpoint_profile
            || c.upstream_model_id != saved.upstream_model_id
            || c.connection_id != saved.connection_id
    }) {
        tx.router().clear_cooldown(&saved.id)?;
    }
    event(
        tx,
        actor,
        (MODEL_AGGREGATE, &saved.id, saved.revision),
        "RouteModelConfigured",
        json!({
            "modelId": saved.id.as_str(),
            "endpointProfile": saved.endpoint_profile,
            "upstreamModelId": saved.upstream_model_id,
            "enabled": saved.enabled,
            "efforts": saved.efforts,
            "imageInput": saved.image_input,
            "tools": saved.tools,
            "contextTokens": saved.context_tokens,
            "connectionId": saved.connection_id.as_ref().map(Id::as_str),
        }),
    )?;
    Ok(applied(&saved.id, saved.revision))
}

/// `Tier`: the members of one tier, saved as its routing setting (the workspace settings revision
/// is the expected revision).
pub(super) fn configure_tier(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    tier: &TierName,
    expected: i64,
    members: &[Id],
) -> Result<DomainDecision, Failure> {
    let name = wire(tier);
    let (_, key) = TIERS
        .iter()
        .find(|(t, _)| *t == name)
        .ok_or_else(|| invalid("tier"))?;
    let mut seen = BTreeSet::new();
    for id in members {
        if !seen.insert(id.as_str()) {
            return Err(invalid("member_ids"));
        }
        if latest::alias_family(id.as_str()).is_none() && tx.router().route_model(id)?.is_none() {
            // Decision models are never tier members; unknown models cannot be pooled.
            return Err(refuse(DomainErrorCode::NotFound, MODEL_AGGREGATE, Some(id)));
        }
    }
    let ids: Vec<&str> = members.iter().map(Id::as_str).collect();
    let tier_id = Id::new(name.clone()).map_err(|_| invalid("tier"))?;
    match crate::settings::service::save_workspace_value(tx, ctx, key, json!(ids), expected)? {
        Ok(Some(revision)) => Ok(applied(&tier_id, revision)),
        Ok(None) => Ok(unchanged(&tier_id, expected)),
        Err((code, kind)) => Err(refuse(code, kind, Some(&tier_id))),
    }
}

/// `DecisionModel`: a SystemOne decision model, its own model ID (never a tier member).
pub(super) fn configure_decision_model(
    tx: &mut dyn WriteTransaction,
    actor: &ActorId,
    model: &DecisionModel,
    expected: Option<i64>,
) -> Result<DomainDecision, Failure> {
    if tx.router().route_model(&model.id)?.is_some() {
        return Err(refuse(
            DomainErrorCode::Conflict,
            MODEL_AGGREGATE,
            Some(&model.id),
        ));
    }
    match tx.router().connection(&model.connection_id)? {
        None => {
            return Err(refuse(
                DomainErrorCode::NotFound,
                CONNECTION,
                Some(&model.connection_id),
            ));
        }
        Some(c) if c.profile != "SystemOne" => {
            return Err(refuse(
                DomainErrorCode::InvalidArgument,
                "connection_profile",
                Some(&model.connection_id),
            ));
        }
        Some(_) => {}
    }
    let types: Vec<String> = model.question_types.0.iter().map(wire).collect();
    let unique: BTreeSet<&String> = types.iter().collect();
    if types.is_empty() || unique.len() != types.len() {
        return Err(invalid("question_types"));
    }
    let current = tx.router().decision_model(&model.id)?;
    match (&current, expected) {
        (Some(_), None) => {
            return Err(refuse(
                DomainErrorCode::Conflict,
                DECISION_AGGREGATE,
                Some(&model.id),
            ));
        }
        (None, Some(_)) => {
            return Err(refuse(
                DomainErrorCode::NotFound,
                DECISION_AGGREGATE,
                Some(&model.id),
            ));
        }
        (Some(c), Some(e)) if c.revision != e => {
            return Err(Failure::Storage(StorageError::conflict(
                RevisionDimension::Configuration,
                DECISION_AGGREGATE,
                model.id.clone(),
                rev(e),
                rev(c.revision),
            )));
        }
        _ => {}
    }
    let next = StoredDecisionModel {
        id: model.id.clone(),
        display_name: model.display_name.0.clone(),
        connection_id: model.connection_id.clone(),
        upstream_model_id: model.upstream_model_id.0.clone(),
        question_types: types,
        accepts_images: model.accepts_images,
        accepts_videos: model.accepts_videos,
        enabled: model.enabled,
        extension_schemas: serde_json::to_string(&model.extension_question_schemas)
            .map_err(|e| StorageError::InvalidRecord(e.to_string()))?,
        result_schema: serde_json::to_string(&model.result_schema)
            .map_err(|e| StorageError::InvalidRecord(e.to_string()))?,
        revision: current.as_ref().map_or(1, |c| c.revision),
        updated_at_ms: tx.now_ms(),
    };
    if let Some(c) = &current {
        let mut same = next.clone();
        same.updated_at_ms = c.updated_at_ms;
        if &same == c {
            return Ok(unchanged(&c.id, c.revision));
        }
    }
    let saved = tx.router().put_decision_model(&next, expected)?;
    event(
        tx,
        actor,
        (DECISION_AGGREGATE, &saved.id, saved.revision),
        "DecisionModelConfigured",
        json!({
            "modelId": saved.id.as_str(),
            "connectionId": saved.connection_id.as_str(),
            "upstreamModelId": saved.upstream_model_id,
            "questionTypes": saved.question_types,
            "acceptsImages": saved.accepts_images,
            "acceptsVideos": saved.accepts_videos,
            "enabled": saved.enabled,
        }),
    )?;
    Ok(applied(&saved.id, saved.revision))
}
