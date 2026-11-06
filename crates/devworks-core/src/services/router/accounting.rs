//! Attempt admission reuses the router's current pool/capability/connection assessment. A
//! retry may select another eligible route without rewriting its logical request's first route.
use super::*;
use crate::services::runs::ControlService;
use crate::services::usage::accounting::{Admission, AttemptRequest};

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct PinFence {
    pub id: Id,
    pub revision: i64,
}
#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub enum AttemptSelection {
    Conversation {
        intent: RouteIntent,
        effort: EffortName,
        required_capabilities: Vec<String>,
        pin: Option<PinFence>,
    },
    Decision {
        question_type: String,
        images: bool,
        videos: bool,
    },
}

impl InferenceRouter {
    pub(crate) fn attempt_route_current(
        &self,
        tx: &mut dyn WriteTransaction,
        session: Option<&Id>,
        selection: &AttemptSelection,
        query: &crate::ports::usage::PriceQuery,
    ) -> Result<Option<String>, StorageError> {
        let connections = self.connections.connections(tx.router())?;
        match selection {
            AttemptSelection::Conversation {
                intent,
                effort,
                required_capabilities,
                pin,
            } => {
                let need = Need::parse(
                    &wire(effort),
                    required_capabilities.iter().map(String::as_str),
                )
                .map_err(StorageError::InvalidRecord)?;
                let members = tier_members(tx.configuration())?;
                let now = tx.now_ms();
                let facts = facts_with(members, tx.router(), now)?;
                let current_pin = session
                    .map(|id| tx.router().session_pin(id))
                    .transpose()?
                    .flatten();
                if let Some(expected) = pin
                    && !current_pin.as_ref().is_some_and(|current| {
                        current.pin_id == expected.id
                            && current.revision == expected.revision
                            && current.model_id == query.catalog_model_id
                            && current.connection_id == query.connection_id
                            && current.intent == intent_key(intent)
                    })
                {
                    return Ok(Some(
                        "session route pin changed before inference admission".into(),
                    ));
                }
                let pinned = current_pin
                    .as_ref()
                    .filter(|current| pin.as_ref().is_some_and(|p| p.id == current.pin_id))
                    .and_then(|pin| facts.assess_pin(intent, pin, &connections, &need));
                let pool = facts.assess_pool(
                    intent,
                    &connections,
                    &need,
                    Some((&query.catalog_model_id, &query.connection_id)),
                );
                let chosen = pinned
                    .as_ref()
                    .filter(|assessment| {
                        assessment.model_id == query.catalog_model_id
                            && assessment
                                .connection
                                .as_ref()
                                .is_some_and(|connection| *connection == query.connection_id)
                    })
                    .or_else(|| {
                        pool.iter().find(|assessment| {
                            assessment.model_id == query.catalog_model_id
                                && assessment
                                    .connection
                                    .as_ref()
                                    .is_some_and(|connection| *connection == query.connection_id)
                        })
                    });
                let Some(chosen) = chosen.filter(|assessment| assessment.eligible()) else {
                    return Ok(Some(format!(
                        "selected inference route is not currently eligible: {}",
                        select::block_codes(&pool).join(", ")
                    )));
                };
                let model = chosen.model.as_ref().expect("eligible model");
                if model.endpoint_profile != query.endpoint_profile {
                    return Ok(Some("selected model endpoint profile changed".into()));
                }
                // An upstream auto route is unresolved until the provider responds. It cannot
                // borrow a concrete model's tariff merely because both share a connection.
                if model.upstream_model_id == "openrouter/auto"
                    || model.upstream_model_id == "openrouter/auto:beta"
                {
                    if query.provider_model_id.is_some() {
                        return Ok(Some(
                            "upstream auto model is not resolved at admission".into(),
                        ));
                    }
                } else if query.provider_model_id.as_ref() != Some(&model.upstream_model_id) {
                    return Ok(Some(
                        "concrete upstream identity changed before admission".into(),
                    ));
                }
            }
            AttemptSelection::Decision {
                question_type,
                images,
                videos,
            } => {
                let Some(model) = tx.router().decision_model(&query.catalog_model_id)? else {
                    return Ok(Some("decision model is unavailable".into()));
                };
                let connection = connections
                    .iter()
                    .find(|connection| connection.id == model.connection_id);
                let now = tx.now_ms();
                let cooling = tx
                    .router()
                    .cooldowns()?
                    .iter()
                    .any(|cooldown| cooldown.model_id == model.id && cooldown.until_ms > now);
                let eligible = !cooling
                    && model.enabled
                    && model.connection_id == query.connection_id
                    && query.endpoint_profile == "SystemOne"
                    && query.provider_model_id.as_ref() == Some(&model.upstream_model_id)
                    && model.question_types.contains(question_type)
                    && (!images || model.accepts_images)
                    && (!videos || model.accepts_videos)
                    && connection.is_some_and(|connection| {
                        connection.profile == "SystemOne"
                            && connection.state == crate::ports::router::ConnectionState::Connected
                            && connection.permits(&model.upstream_model_id)
                            && connection.discovered.as_ref().is_none_or(|ids| {
                                select::lists(&connection.profile, ids, &model.upstream_model_id)
                            })
                    });
                if !eligible {
                    return Ok(Some("decision model does not meet the current question, media or connection constraints".into()));
                }
            }
        }
        Ok(None)
    }

    /// Host-only native accounting seam. It never calls a provider or consumes another routing
    /// cursor; the selected route is rechecked and the journal creates the actual attempt ID.
    pub fn admit_attempt(
        &self,
        tx: &mut dyn WriteTransaction,
        control: &ControlService,
        ctx: &InvocationContext,
        request: &AttemptRequest,
    ) -> Result<Admission, StorageError> {
        let route = tx
            .router()
            .route_request(&request.request_id)?
            .ok_or_else(|| {
                StorageError::InvalidRecord("missing logical inference request".into())
            })?;
        if route.purpose == "TierClassification"
            && matches!(&request.selection,AttemptSelection::Conversation{intent,..} if !matches!(intent,RouteIntent::Tier{tier:TierName::Low}))
        {
            return Ok(Admission::SourceChanged(
                "the conversational classifier must target Low directly".into(),
            ));
        }
        crate::services::usage::accounting::admit_attempt(tx, control, self, ctx, request)
    }
    pub fn mark_attempt_dispatched(
        &self,
        tx: &mut dyn WriteTransaction,
        control: &ControlService,
        id: &Id,
    ) -> Result<Admission, StorageError> {
        let attempt = tx
            .usage()
            .inference_attempt(id)?
            .ok_or_else(|| StorageError::InvalidRecord("unknown accounted attempt".into()))?;
        if attempt.state == "Admitted" {
            let snapshot = crate::services::usage::accounting::snapshot(&attempt)?;
            if let Some(reason) = self.attempt_route_current(
                tx,
                attempt.session_id.as_ref(),
                &snapshot.selection,
                &snapshot.query,
            )? {
                return Ok(Admission::SourceChanged(reason));
            }
        }
        crate::services::usage::accounting::mark_dispatched(tx, control, id)
    }
}
