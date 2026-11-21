//! Model discovery and serving-connection eligibility for SystemOne decisions.
use super::{InferenceRouter, ops, select};
use crate::context::InvocationContext;
use crate::ports::router::ConnectionStatus;
use crate::ports::storage::{ReadSnapshot, StorageError, StoredDecisionModel};
use crate::protocol::records::DecisionCatalogModel;
use crate::protocol::results::OperationResult;
use crate::protocol::support::{BoundedList, BoundedText};
use crate::services::cursor::{self, CursorBinding};
use crate::services::kernel::{Failure, counter, rev};
use std::collections::BTreeMap;

pub(super) const PROFILE: &str = "SystemOne";

pub(super) fn availability(
    model: &StoredDecisionModel,
    connections: &[ConnectionStatus],
    cooldowns: &BTreeMap<String, i64>,
) -> Result<(), select::Block> {
    if !model.enabled {
        return Err(select::Block::Disabled);
    }
    select::serving_connection(
        PROFILE,
        Some(&model.connection_id),
        &model.upstream_model_id,
        connections,
        None,
    )?;
    if let Some(until) = cooldowns.get(model.id.as_str()) {
        return Err(select::Block::Cooling(*until));
    }
    Ok(())
}

fn catalog_record(
    model: StoredDecisionModel,
    blocked: Option<select::Block>,
    schema_revision: u32,
) -> Result<DecisionCatalogModel, StorageError> {
    let current = schema_revision >= 4;
    let malformed = |_| StorageError::InvalidRecord("stored decision model schema".into());
    let retry_at_ms = match blocked {
        Some(select::Block::Cooling(until)) if current => Some(counter(until)),
        _ => None,
    };
    Ok(DecisionCatalogModel {
        id: model.id,
        revision: rev(model.revision),
        display_name: BoundedText(model.display_name),
        connection_id: model.connection_id,
        upstream_model_id: BoundedText(model.upstream_model_id),
        question_types: BoundedList(
            model
                .question_types
                .iter()
                .map(|name| super::parse_wire(name))
                .collect::<Result<Vec<_>, _>>()?,
        ),
        accepts_images: model.accepts_images,
        accepts_videos: model.accepts_videos,
        enabled: model.enabled,
        extension_question_schemas: serde_json::from_str(&model.extension_schemas)
            .map_err(malformed)?,
        result_schema: serde_json::from_str(&model.result_schema).map_err(malformed)?,
        decision_policy: model
            .decision_policy
            .as_deref()
            .filter(|_| current)
            .map(serde_json::from_str)
            .transpose()
            .map_err(malformed)?,
        available: current.then_some(blocked.is_none()),
        availability_reason: blocked
            .filter(|_| current)
            .map(|block| super::wire_label(block.code())),
        retry_at_ms,
    })
}

impl InferenceRouter {
    pub(super) fn decision_models(
        &self,
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        input: &ops::DecisionModelsInput,
        schema_revision: u32,
    ) -> Result<OperationResult, Failure> {
        let read = snapshot.router();
        let connections = self.connections.connections(read)?;
        let cooldowns = select::active_cooldowns(&read.cooldowns()?, self.clock.now_ms().get());
        let binding = CursorBinding::new(
            ctx,
            "router.decision_models",
            &[
                if input.include_disabled {
                    "inspect"
                } else {
                    "available"
                },
                &schema_revision.to_string(),
            ],
            snapshot.stamp().committed_sequence,
        );
        let rows = cursor::page(
            &binding,
            &input.page,
            |after, limit| {
                cursor::scan(
                    after.map(str::to_owned),
                    limit,
                    |after, limit| read.decision_models(after.as_deref(), limit),
                    |model| Some(model.id.as_str().to_owned()),
                    |model| {
                        Ok(input.include_disabled
                            || availability(model, &connections, &cooldowns).is_ok())
                    },
                )
            },
            |model| model.id.as_str().to_owned(),
        )?;
        Ok(OperationResult::RouterDecisionModels {
            value: rows.try_into_page(|model| {
                let blocked = availability(&model, &connections, &cooldowns).err();
                catalog_record(model, blocked, schema_revision)
            })?,
        })
    }
}
