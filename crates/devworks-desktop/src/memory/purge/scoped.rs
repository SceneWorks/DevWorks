//! Partition the persisted, explicitly scoped editor producers. Original work still in
//! flight is returned as a barrier, never restarted here under a different request key.
use super::*;
use crate::conventions_ops::{SearchContext, SearchRecovery, Submission};
use crate::conventions_screen::Scope;
use crate::conventions_transfer::Pipeline;
use crate::shell::work::PlanForm;
use devworks_core::protocol::records::OperationRef;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::BTreeMap;

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Conventions {
    pipelines: BTreeMap<String, Pipeline>,
    submissions: BTreeMap<String, Submission>,
    operations: Vec<Operations>,
    searches: Vec<Search>,
    scope: Scope,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Operations {
    scope: Scope,
    ids: Vec<String>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Search {
    scope: Scope,
    recovery: Option<SearchRecovery>,
    last: Option<SearchRecovery>,
    preparation: Option<(String, OperationRef)>,
    earlier: Vec<SearchContext>,
}
pub(super) fn checked<T: DeserializeOwned + Serialize>(
    value: &Value,
) -> Result<T, MemoryPurgeFault> {
    let typed: T =
        serde_json::from_value(value.clone()).map_err(|_| MemoryPurgeFault::UnknownShape)?;
    let encoded = serde_json::to_value(&typed).map_err(|_| MemoryPurgeFault::UnknownShape)?;
    if !closed_keys(value, &encoded) {
        return Err(MemoryPurgeFault::UnknownShape);
    }
    Ok(typed)
}

pub struct ScopedProjection {
    pub sessions: SessionProjection,
    /// Original convention recovery frames and active operation IDs must settle before commit.
    pub convention_frames: Vec<Vec<u8>>,
    pub convention_operations: Vec<String>,
    /// A pipeline can be preparing bytes before its original command frame is available.
    pub active_convention_pipelines: Vec<String>,
    pub active_plan_saves: Vec<String>,
    pub active_database_batch: bool,
}

pub fn project_scoped_editors(
    memory: &Memory,
    scope: &ProjectPurgeClientScope,
) -> Result<ScopedProjection, MemoryPurgeFault> {
    let sessions = project_sessions(memory, scope)?;
    let owned = Scope::Project(scope.project_id.0.as_str().into());
    let mut result = ScopedProjection {
        convention_frames: vec![],
        convention_operations: vec![],
        active_convention_pipelines: vec![],
        active_plan_saves: vec![],
        active_database_batch: false,
        sessions,
    };
    for command in &memory.pending.commands {
        if super::frames::affinity(
            &command.operation,
            devworks_core::registry::Mode::Command,
            &command.frame,
            scope,
        )? == super::frames::Affinity::Owned
        {
            if !result.sessions.unsettled_commands.contains(command) {
                result.sessions.unsettled_commands.push(command.clone());
            }
            result
                .sessions
                .after
                .pending
                .commands
                .retain(|c| c != command);
        }
    }
    if let Some(import) = &memory.pending.knowledge_import
        && super::frames::affinity(
            "artifacts.save",
            devworks_core::registry::Mode::Command,
            &import.frame,
            scope,
        )? == super::frames::Affinity::Owned
    {
        result
            .sessions
            .unsettled_attachments
            .push(import.frame.clone());
        result.sessions.after.pending.knowledge_import = None;
    }
    if !memory.pending.conventions.is_null() {
        let mut conventions: Conventions = checked(&memory.pending.conventions)?;
        for (key, pipeline) in &conventions.pipelines {
            if pipeline.scope != pipeline.submission.scope {
                return Err(MemoryPurgeFault::UnknownShape);
            }
            let removing = pipeline.scope == owned;
            if removing {
                result.active_convention_pipelines.push(key.clone());
            }
            for file in &pipeline.files {
                if removing && file.owned {
                    result.sessions.owned_paths.insert(file.path.clone());
                } else if !removing {
                    result.sessions.surviving_paths.insert(file.path.clone());
                }
            }
        }
        conventions.pipelines.retain(|_, p| p.scope != owned);
        // Submissions without a frame still denote a producer that must stop or finish.
        for (key, submission) in &conventions.submissions {
            if submission.scope == owned {
                result.active_convention_pipelines.push(key.clone());
            }
        }
        conventions.submissions.retain(|_, p| p.scope != owned);
        for operation in &conventions.operations {
            if operation.scope == owned {
                result.convention_operations.extend(operation.ids.clone());
            }
        }
        conventions.operations.retain(|o| o.scope != owned);
        for search in &conventions.searches {
            if search.scope != owned {
                continue;
            }
            if let Some(request) = &search.recovery {
                result.convention_frames.push(request.frame.clone());
            }
            for earlier in &search.earlier {
                result.convention_frames.push(earlier.request.frame.clone());
                if let Some((_, operation)) = &earlier.preparation {
                    result
                        .convention_operations
                        .push(operation.operation_id.0.as_str().into());
                }
            }
            if let Some((_, operation)) = &search.preparation {
                result
                    .convention_operations
                    .push(operation.operation_id.0.as_str().into());
            }
        }
        conventions.searches.retain(|s| s.scope != owned);
        if conventions.scope == owned {
            conventions.scope = Scope::Workspace;
        }
        result.sessions.after.pending.conventions =
            serde_json::to_value(conventions).map_err(|_| MemoryPurgeFault::UnknownShape)?;
    }
    let mut surviving = Vec::new();
    for value in &memory.pending.plans {
        let plan: PlanForm = checked(value)?;
        if plan.snapshot.project_id == scope.project_id {
            result
                .active_plan_saves
                .push(plan.snapshot.id.as_str().into());
            if let Some(reference) = plan.content {
                result.sessions.removed_references.push(reference);
            }
            for drop in plan.drops.values() {
                result
                    .sessions
                    .removed_references
                    .extend(drop.artifact.clone());
            }
        } else {
            surviving.push(value.clone());
            result.sessions.surviving_references.extend(plan.content);
            for drop in plan.drops.values() {
                result
                    .sessions
                    .surviving_references
                    .extend(drop.artifact.clone());
            }
        }
    }
    result.sessions.after.pending.plans = surviving;
    result.active_database_batch = super::database::partition(&mut result.sessions.after, scope)?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn scoped_transfers_preserve_other_owners_and_return_original_producers() {
        let scope = super::super::tests::scope();
        let mut memory = Memory::default();
        let pipeline = |owner: Value, path: &str| {
            json!({
                "scope":owner,"operation":"conventions.create","body":{},"at":0,
                "files":[{"path":path,"name":"source","media_type":"text/plain",
                    "pointer":"/content","owned":true}],
                "submission":{"scope":owner,"operation":"conventions.create","target":null,
                    "draft":null,"reader_generation":0,"resource":null}
            })
        };
        memory.pending.conventions = json!({
            "scope":{"Project":"p"},
            "pipelines":{"owned":pipeline(json!({"Project":"p"}),"owned.txt"),
                "other":pipeline(json!({"Project":"other"}),"other.txt"),
                "home":pipeline(json!("Workspace"),"home.txt")},
            "submissions":{},"operations":[],"searches":[]
        });
        let partition = project_scoped_editors(&memory, &scope).unwrap();
        assert_eq!(partition.active_convention_pipelines, ["owned"]);
        assert_eq!(
            partition.sessions.owned_paths,
            [PathBuf::from("owned.txt")].into()
        );
        assert_eq!(
            partition.sessions.surviving_paths,
            [PathBuf::from("home.txt"), PathBuf::from("other.txt")].into()
        );
        let after = &partition.sessions.after.pending.conventions;
        assert!(after["pipelines"].get("owned").is_none());
        assert_eq!(
            after["pipelines"]["other"],
            memory.pending.conventions["pipelines"]["other"]
        );
        assert_eq!(after["scope"], "Workspace");
        assert!(
            memory.pending.conventions["pipelines"]
                .get("owned")
                .is_some()
        );
    }

    #[test]
    fn unknown_scoped_editor_state_refuses_destructive_projection() {
        let mut memory = Memory::default();
        memory.pending.conventions = json!({"futureDraft":"private"});
        assert!(matches!(
            project_scoped_editors(&memory, &super::super::tests::scope()),
            Err(MemoryPurgeFault::UnknownShape)
        ));
    }
}
