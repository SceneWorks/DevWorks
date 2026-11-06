//! Crash-safe private source staging, shared by committed and worktree unit publishers.
use super::{IndexEnv, worker::Stop};
use crate::ports::storage::{StoredUnit, WriteTransaction};
use crate::protocol::Id;
use crate::services::artifacts::StagedFile;
use serde_json::json;
use std::sync::Arc;

pub(crate) struct Sources {
    env: Arc<IndexEnv>,
    repository: Id,
    staged: Vec<(String, StagedFile)>,
    committed: bool,
}

impl Sources {
    pub fn new(env: Arc<IndexEnv>, repository: Id) -> Self {
        Self {
            env,
            repository,
            staged: Vec::new(),
            committed: false,
        }
    }

    pub fn stage(&mut self, unit: &mut StoredUnit, content: Option<&[u8]>) -> Result<(), Stop> {
        let Some(content) = content else {
            unit.payload["sourceUnavailable"] = json!("source_bytes_not_captured");
            return Ok(());
        };
        let Some(artifacts) = &self.env.artifacts else {
            unit.payload["sourceUnavailable"] = json!("source_storage_unavailable");
            return Ok(());
        };
        let staged = artifacts
            .stage_host_bytes(
                &*self.env.store,
                content,
                json!({
                    "kind": "IndexSource", "repositoryId": self.repository,
                    "unitKey": unit.unit_key, "sourceDigest": unit.content_digest,
                }),
            )
            .map_err(Stop::storage)?
            .map_err(|e| Stop::transient("source_publication", e))?;
        unit.payload["sourceCapture"] = json!({
            "id": staged.operation_id.0, "sha256": staged.digest, "byteCount": staged.byte_count,
            "textEncoding": if std::str::from_utf8(content).is_ok() { "Utf8" } else { "Binary" },
            "lineCount": if content.is_empty() { 0 } else {
                content.iter().filter(|b| **b == b'\n').count() + usize::from(!content.ends_with(b"\n"))
            },
        });
        self.staged.push((unit.unit_key.clone(), staged));
        Ok(())
    }

    /// Clone only the small staging metadata into the write closure. This owner remains
    /// outside it so every early error and cancellation settles all prior staging operations.
    pub fn records(&self) -> Vec<(String, StagedFile)> {
        self.staged.clone()
    }

    pub fn commit(
        env: &IndexEnv,
        repository: &Id,
        records: &[(String, StagedFile)],
        tx: &mut dyn WriteTransaction,
        unit: &StoredUnit,
    ) -> Result<(), crate::ports::storage::StorageError> {
        if let Some((_, staged)) = records.iter().find(|(key, _)| key == &unit.unit_key)
            && let Some(artifacts) = &env.artifacts
        {
            artifacts.commit_index_source(
                tx,
                staged,
                repository,
                &unit.unit_key,
                &unit.content_digest,
            )?;
        }
        Ok(())
    }

    pub fn committed(&mut self) {
        self.committed = true;
    }
}

impl Drop for Sources {
    fn drop(&mut self) {
        if let Some(artifacts) = &self.env.artifacts {
            for (_, staged) in &self.staged {
                artifacts.settle_host_bytes(&*self.env.store, staged, self.committed);
            }
        }
    }
}
