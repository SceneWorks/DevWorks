//! Boot and durable catch-up use ordinary host ingress and the shared command gateway.
use super::{HelpService, JOB_KIND, current_generation_args};
use crate::context::{HostIngress, InvocationContext, PrincipalKind};
use crate::ports::IdKind;
use crate::ports::storage::*;
use crate::protocol::{Counter, Revision};
use serde_json::json;

impl HelpService {
    /// One source-change attempt. Failed attempts remain observable; an explicit new request
    /// can retry them. Unknown operations are recovered through their existing queue identity.
    pub fn maintenance_request(
        &self,
    ) -> Result<Option<(InvocationContext, Vec<u8>)>, StorageError> {
        if self.is_stopped() {
            return Ok(None);
        }
        read_snapshot(&*self.store, &ReadOptions::default(), |read| {
            let head = read.help().help_head()?;
            let args = current_generation_args(read)?;
            let input_digest = self.source_digest(args.registry_revision.get());
            if let Some(id) = head.generation_operation_id.as_ref()
                && let Some(operation) = read.operations().operation(id)?
                && (!is_terminal(operation.state)
                    || operation.payload["inputDigest"].as_str() == Some(&input_digest))
            {
                return Ok(None);
            }
            let info = self.store.info();
            let policy = read
                .configuration()
                .current(&ConfigScope::Workspace, crate::settings::model::NAMESPACE)?
                .map_or(1, |row| row.revision);
            let invocation = self.ids.next_id(IdKind::Command);
            let context = HostIngress::new(&*self.store).context(
                info.host_actor,
                PrincipalKind::Host,
                invocation.clone(),
                crate::protocol::Id::new("help-maintenance").expect("owned namespace"),
                Counter::new(info.restore_epoch)
                    .map_err(|_| StorageError::InvalidRecord("invalid restore epoch".into()))?,
                Revision::new(policy)
                    .map_err(|_| StorageError::InvalidRecord("invalid policy revision".into()))?,
            );
            let frame = json!({"protocol":crate::protocol::CONTRACT,"mode":"Command","schemaRevision":1,
                "requestKey":invocation,"body":{"kind":"help.generate_capability_docs",
                    "registryRevision":args.registry_revision,"expectedHelpRevision":args.expected_help_revision}});
            Ok(Some((context, frame.to_string().into_bytes())))
        })
    }

    /// Bounded abandoned-draft cleanup never visits published generations. Only artifacts
    /// produced by this exact draft's trusted host publication may be retired.
    pub fn cleanup(&self) -> Result<usize, StorageError> {
        let service = self.clone();
        run_write(&*self.store, WriteOptions::default(), move |tx| {
            if service.is_stopped() {
                return Ok(0);
            }
            let drafts = tx.help().help_drafts(None, 32)?;
            let mut removed = 0;
            for draft in drafts {
                let Some(operation) = tx.operations().operation(&draft.operation_id)? else {
                    continue;
                };
                if !is_terminal(operation.state) || operation.payload["kind"] != JOB_KIND {
                    continue;
                }
                let rows = tx
                    .help()
                    .staged_help_topics(&draft.generation_id, None, 32)?;
                for row in &rows {
                    if let Some(reference) = row.content_ref.as_ref() {
                        let reference =
                            serde_json::from_value(reference.clone()).map_err(|_| {
                                StorageError::InvalidRecord("invalid draft Help artifact".into())
                            })?;
                        service.artifacts.retire_help_draft(
                            tx,
                            &reference,
                            &draft.generation_id,
                            &draft.operation_id,
                        )?;
                    }
                }
                tx.help()
                    .discard_help_draft(&draft.generation_id, &draft.operation_id, 32)?;
                removed += rows.len();
            }
            Ok(removed)
        })
    }
}
