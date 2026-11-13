//! Attempt admission reuses the router's current pool/capability/connection assessment. A
//! retry may select another eligible route without rewriting its logical request's first route.
use super::*;
use crate::services::runs::ControlService;
use crate::services::usage::accounting::{Admission, AttemptRequest};

/// Actual native entry point captured by its trusted host owner, independent of purpose/role.
#[derive(Clone, Copy, Debug, Serialize, serde::Deserialize)]
pub enum ReportCategory {
    Assistant,
    CodeReview,
    RepositoryAnalysis,
    Mcp,
    Other,
    Unknown,
}
impl ReportCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Assistant => "Assistant",
            Self::CodeReview => "CodeReview",
            Self::RepositoryAnalysis => "RepositoryAnalysis",
            Self::Mcp => "Mcp",
            Self::Other => "Other",
            Self::Unknown => "Unknown",
        }
    }
}

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct PinFence {
    pub id: Id,
    pub revision: i64,
}
#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub enum AttemptSelection {
    /// Trusted native normalization retained alongside the existing exact route eligibility.
    NativeGeneration {
        route: Box<AttemptSelection>,
        requested_effort: Option<String>,
        effective_effort: Option<String>,
    },
    Conversation {
        intent: RouteIntent,
        effort: EffortName,
        required_capabilities: Vec<String>,
        pin: Option<PinFence>,
    },
    /// Exact discovered embedding model and explicitly qualified native host space; no pin.
    Embedding {
        embedding_space_id: Id,
        dimensions: u32,
        upstream_model_id: String,
        connection_revision: i64,
        registration_digest: String,
    },
    Decision {
        question_type: String,
        images: bool,
        videos: bool,
    },
}

impl AttemptSelection {
    fn route(&self) -> &Self {
        match self {
            Self::NativeGeneration { route, .. } => route,
            _ => self,
        }
    }
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
        if let AttemptSelection::NativeGeneration {
            route,
            effective_effort,
            ..
        } = selection
        {
            let mut selected = route.as_ref().clone();
            let AttemptSelection::Conversation { effort, .. } = &mut selected else {
                return Err(StorageError::ScopeDenied);
            };
            if let Some(level) = effective_effort.as_deref() {
                *effort = match level {
                    "none" => EffortName::Default,
                    "minimal" => EffortName::Minimal,
                    "low" => EffortName::Low,
                    "medium" => EffortName::Medium,
                    "high" => EffortName::High,
                    "xhigh" => EffortName::XHigh,
                    "max" => EffortName::Max,
                    _ => return Err(StorageError::ScopeDenied),
                };
            }
            return self.attempt_route_current(tx, session, &selected, query);
        }
        match selection {
            AttemptSelection::NativeGeneration { .. } => unreachable!("handled above"),
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
                if model.endpoint_profile == select::OPEN_ROUTER_PROFILE
                    && crate::ports::router::is_open_router_auto(&model.upstream_model_id)
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
            AttemptSelection::Embedding {
                dimensions,
                upstream_model_id,
                connection_revision,
                registration_digest,
                ..
            } => {
                let connection = tx.router().connection(&query.connection_id)?;
                let eligible = connection.as_ref().is_some_and(|c| {
                    c.enabled
                        && c.lifecycle == "Connected"
                        && c.profile == "Embedding"
                        && c.revision == *connection_revision
                        && *dimensions > 0
                        && *dimensions <= 65536
                        && c.discovered
                            .as_ref()
                            .is_some_and(|models| models.iter().any(|m| m.id == *upstream_model_id))
                        && query.catalog_model_id
                            == super::connections::discovered_model_id(
                                "Embedding",
                                &c.id,
                                upstream_model_id,
                            )
                }) && query.endpoint_profile == "Embedding"
                    && query.provider_model_id.as_ref() == Some(upstream_model_id)
                    && crate::fingerprint::is_sha256_hex(registration_digest);
                if !eligible
                    || tx
                        .router()
                        .cooldowns()?
                        .iter()
                        .any(|c| c.model_id == query.catalog_model_id && c.until_ms > tx.now_ms())
                {
                    return Ok(Some(
                        "embedding model, connection or qualified native space changed".into(),
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
        self.admit_attempt_with_process(tx, control, ctx, request, None)
    }
    pub(crate) fn admit_attempt_with_process(
        &self,
        tx: &mut dyn WriteTransaction,
        control: &ControlService,
        ctx: &InvocationContext,
        request: &AttemptRequest,
        host_process: Option<&crate::services::usage::accounting::HostProcessIdentity>,
    ) -> Result<Admission, StorageError> {
        let route = tx
            .router()
            .route_request(&request.request_id)?
            .ok_or_else(|| {
                StorageError::InvalidRecord("missing logical inference request".into())
            })?;
        if (route.purpose == "Embedding")
            != matches!(
                request.selection.route(),
                AttemptSelection::Embedding { .. }
            )
        {
            return Ok(Admission::SourceChanged(
                "embedding requires its qualified native route".into(),
            ));
        }
        if route.purpose == "TierClassification"
            && matches!(request.selection.route(),AttemptSelection::Conversation{intent,..} if !matches!(intent,RouteIntent::Tier{tier:TierName::Low}))
        {
            return Ok(Admission::SourceChanged(
                "the conversational classifier must target Low directly".into(),
            ));
        }
        crate::services::usage::accounting::admit_attempt(
            tx,
            control,
            self,
            ctx,
            request,
            host_process,
        )
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
