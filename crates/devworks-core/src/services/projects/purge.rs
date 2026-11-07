//! Native preparation and exact confirmation fences. A permit is opaque, cannot be
//! deserialized, and is minted only after the ingress-bound Human decision and fresh owners.
use super::*;
use crate::ports::project_purge::{
    ProjectPurgeClient, ProjectPurgeFiles, PurgeClientSnapshot, PurgeFileFault, PurgeFileSnapshot,
};
use crate::ports::storage::{
    ProjectPurgeInventory, ProjectPurgeRowAction, ProjectPurgeRowReason, StoredProjectPurge,
    project_purge_digest,
};
use crate::protocol::ActorId;
use crate::protocol::records::{
    ProjectPurgeEntry, ProjectPurgeManifest, PurgeDisposition, PurgeEntryKind, PurgePhase,
};

pub struct PurgeCoordinator {
    ids: Arc<dyn IdSource>,
    files: Arc<dyn ProjectPurgeFiles>,
    client: Arc<dyn ProjectPurgeClient>,
}

/// Executes only effects already committed by the logical owner. Restarts claim those
/// original bodies; this worker cannot prepare a manifest or mint a Human permit.
pub struct PurgeWorker {
    store: Arc<dyn crate::ports::storage::ApplicationStore>,
    ids: Arc<dyn IdSource>,
    files: Arc<dyn ProjectPurgeFiles>,
    client: Arc<dyn ProjectPurgeClient>,
}
impl PurgeWorker {
    pub fn new(
        store: Arc<dyn crate::ports::storage::ApplicationStore>,
        ids: Arc<dyn IdSource>,
        files: Arc<dyn ProjectPurgeFiles>,
        client: Arc<dyn ProjectPurgeClient>,
    ) -> Self {
        Self {
            store,
            ids,
            files,
            client,
        }
    }
    pub fn step(&self, stopped: &mut dyn FnMut() -> bool) -> Result<usize, StorageError> {
        use crate::ports::storage::{ReadOptions, WriteOptions, read_snapshot, run_write};
        let mut pending = read_snapshot(&*self.store, &ReadOptions::default(), |read| {
            read.projects().purge_pending_effects(32)
        })?;
        pending.sort_by_key(|e| match &e.intent {
            crate::ports::project_purge::PurgeEffectIntent::File { witness } => (
                if witness.directory { 1 } else { 0 },
                std::cmp::Reverse(witness.relative_entry.matches('/').count()),
            ),
            crate::ports::project_purge::PurgeEffectIntent::Client { .. } => {
                (2, std::cmp::Reverse(0))
            }
        });
        let mut settled = 0;
        for effect in pending {
            if stopped() {
                break;
            }
            let owner = self.ids.next_id(IdKind::Operation);
            let (manifest, id, claim) =
                (effect.manifest_id.clone(), effect.id.clone(), owner.clone());
            let admitted = run_write(&*self.store, WriteOptions::default(), move |tx| {
                let until = tx
                    .now_ms()
                    .checked_add(30_000)
                    .ok_or_else(|| StorageError::InvalidRecord("purge lease overflow".into()))?;
                tx.projects()
                    .claim_purge_effect(&manifest, &id, &claim, until)
            })?;
            let Some(admitted) = admitted else {
                continue;
            };
            let outcome = if stopped() {
                Err(PurgeFileFault::Cancelled)
            } else {
                match &admitted.intent {
                    crate::ports::project_purge::PurgeEffectIntent::File { witness } => {
                        self.files.remove_reviewed(witness, stopped)
                    }
                    crate::ports::project_purge::PurgeEffectIntent::Client { scope, digest } => {
                        self.client.remove_reviewed(scope, digest)
                    }
                }
            };
            let manifest = admitted.manifest_id;
            let id = admitted.id;
            run_write(&*self.store, WriteOptions::default(), move |tx| {
                tx.projects()
                    .settle_purge_effect(&manifest, &id, &owner, outcome)
            })?;
            settled += 1;
        }
        Ok(settled)
    }
}

/// Its fields and constructor are private to this confirmation owner. A generic authority
/// grant or a wire input can never construct this storage capability.
pub struct PurgePermit {
    reviewed: StoredProjectPurge,
    files: PurgeFileSnapshot,
    client: PurgeClientSnapshot,
    scope: crate::ports::storage::ProjectPurgeClientScope,
    confirmed_by: ActorId,
}
impl PurgePermit {
    pub fn reviewed(&self) -> &StoredProjectPurge {
        &self.reviewed
    }
    pub fn files(&self) -> &PurgeFileSnapshot {
        &self.files
    }
    pub fn client(&self) -> &PurgeClientSnapshot {
        &self.client
    }
    pub fn client_scope(&self) -> &crate::ports::storage::ProjectPurgeClientScope {
        &self.scope
    }
    pub fn confirmed_by(&self) -> &ActorId {
        &self.confirmed_by
    }
}

fn invalid() -> Failure {
    refuse(DomainErrorCode::Conflict, "purge_witness_changed", None)
}
fn file_fault(fault: PurgeFileFault) -> Failure {
    let reason = match fault {
        PurgeFileFault::InUse => "purge_owner_unsettled",
        PurgeFileFault::Changed => "purge_witness_changed",
        PurgeFileFault::Unattributed => "purge_owner_unattributed",
        PurgeFileFault::Unsupported => "purge_owner_unsupported",
        PurgeFileFault::Cancelled => "purge_cancelled",
        PurgeFileFault::Io => "purge_file_unavailable",
    };
    refuse(DomainErrorCode::Conflict, reason, None)
}
fn hash(value: &impl serde::Serialize) -> String {
    crate::fingerprint::sha256_hex(&serde_json::to_vec(value).expect("native purge values"))
}
fn checked_count(n: u64) -> Result<crate::protocol::Counter, Failure> {
    i64::try_from(n).map(counter).map_err(|_| invalid())
}

struct Snapshot {
    inventory: ProjectPurgeInventory,
    removing: PurgeFileSnapshot,
    kept: PurgeFileSnapshot,
    client: PurgeClientSnapshot,
}
impl PurgeCoordinator {
    pub fn new(
        ids: Arc<dyn IdSource>,
        files: Arc<dyn ProjectPurgeFiles>,
        client: Arc<dyn ProjectPurgeClient>,
    ) -> Self {
        Self { ids, files, client }
    }
    fn snapshot(
        &self,
        tx: &mut dyn WriteTransaction,
        project: &ProjectId,
    ) -> Result<Snapshot, Failure> {
        let mut inventory = tx
            .projects()
            .purge_inventory(project)?
            .ok_or_else(|| not_found("project", &project.0))?;
        if !inventory.planning.is_resolved()
            || !inventory.unsettled_operations.is_empty()
            || !ProjectService::activity(tx, project)?.is_empty()
        {
            return Err(file_fault(PurgeFileFault::InUse));
        }
        let client = self.client.inspect(&inventory.client).map_err(file_fault)?;
        if client.pending {
            return Err(file_fault(PurgeFileFault::InUse));
        }
        if !client.surviving_artifacts.is_empty() {
            inventory = tx
                .projects()
                .purge_inventory_with_client_keepers(project, &client.surviving_artifacts)?
                .ok_or_else(|| not_found("project", &project.0))?;
            let checked = self.client.inspect(&inventory.client).map_err(file_fault)?;
            if checked.digest != client.digest || checked.pending {
                return Err(file_fault(PurgeFileFault::Changed));
            }
        }
        let removing = self
            .files
            .inventory(&inventory.removing_files, &mut || false)
            .map_err(file_fault)?;
        let kept = self
            .files
            .inventory(&inventory.kept_files, &mut || false)
            .map_err(file_fault)?;
        Ok(Snapshot {
            inventory,
            removing,
            kept,
            client,
        })
    }
    fn eligible(
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        project: &ProjectId,
        expected: i64,
    ) -> Result<StoredProject, Failure> {
        if ctx.store_id() != &tx.store().store_id
            || ctx.restore_epoch().get() != tx.store().restore_epoch
        {
            return Err(refuse(
                DomainErrorCode::PermissionDenied,
                "purge_store_context_changed",
                None,
            ));
        }
        let before = visible_project(tx.projects(), ctx, project)?;
        if tx.projects().purge_fenced(project)? {
            return Err(conflict("project_purge_already_committed", &project.0));
        }
        revision_conflict(&before, expected)?;
        if before.lifecycle != DELETED || before.archive_stop_pending {
            return Err(conflict(
                "purge_requires_deleted_quiescent_project",
                &project.0,
            ));
        }
        Ok(before)
    }
    pub fn prepare(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        input: &pops::PreparePurgeInput,
    ) -> Result<ProjectPurgeManifest, Failure> {
        let project = Self::eligible(tx, ctx, &input.project_id, input.expected_revision.get())?;
        let snapshot = self.snapshot(tx, &project.id)?;
        let stored = build(
            self.ids.next_id(IdKind::Operation),
            tx.now_ms(),
            ctx.principal(),
            tx.store(),
            &project,
            &snapshot,
        )?;
        tx.projects().insert_purge_manifest(&stored)?;
        Ok(stored.manifest)
    }
    pub fn authorize(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        input: &pops::PurgeInput,
    ) -> Result<PurgePermit, Failure> {
        if !ctx.project_purge_matches(input) {
            return Err(refuse(
                DomainErrorCode::PermissionDenied,
                "human_purge_confirmation_required",
                None,
            ));
        }
        let project = Self::eligible(
            tx,
            ctx,
            &input.project_id,
            input.expected_project_revision.get(),
        )?;
        let stored = tx
            .projects()
            .purge_manifest(&project.id, &input.manifest_id)?
            .ok_or_else(|| not_found("purge_manifest", &input.manifest_id))?;
        if !matches!(stored.manifest.phase, PurgePhase::Prepared)
            || stored.manifest.revision != input.expected_revision
            || stored.manifest.digest != input.expected_digest
        {
            return Err(invalid());
        }
        let snapshot = self.snapshot(tx, &project.id)?;
        let fresh = build(
            stored.manifest.id.clone(),
            stored.manifest.created_at_ms.get(),
            &stored.actor_id,
            tx.store(),
            &project,
            &snapshot,
        )?;
        if fresh.manifest.digest != stored.manifest.digest
            || project_purge_digest(&stored) != stored.manifest.digest.as_str()
        {
            return Err(invalid());
        }
        Ok(PurgePermit {
            reviewed: stored,
            files: snapshot.removing,
            client: snapshot.client,
            scope: snapshot.inventory.client,
            confirmed_by: ctx.principal().clone(),
        })
    }

    pub fn manifest_page(
        read: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        input: &pops::PurgeManifestInput,
    ) -> Result<crate::protocol::records::ProjectPurgeManifestPage, Failure> {
        let project = visible_project(read.projects(), ctx, &input.project_id)?;
        let stored = read
            .projects()
            .purge_manifest(&project.id, &input.manifest_id)?
            .ok_or_else(|| not_found("purge_manifest", &input.manifest_id))?;
        if stored.manifest.revision != input.expected_revision {
            return Err(invalid());
        }
        page(stored, ctx, &input.page)
    }
    pub fn status(
        read: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        input: &pops::PurgeStatusInput,
    ) -> Result<ProjectPurgeManifest, Failure> {
        visible_project(read.projects(), ctx, &input.project_id)?;
        read.projects()
            .purge_manifest(&input.project_id, &input.manifest_id)?
            .map(|s| s.manifest)
            .ok_or_else(|| not_found("purge_manifest", &input.manifest_id).into())
    }
}

fn page(
    stored: StoredProjectPurge,
    ctx: &InvocationContext,
    request: &crate::protocol::records::PageRequest,
) -> Result<crate::protocol::records::ProjectPurgeManifestPage, Failure> {
    let binding = CursorBinding::new(
        ctx,
        "projects.purge_manifest",
        &[
            stored.manifest.project_id.0.as_str(),
            stored.manifest.id.as_str(),
            &stored.manifest.revision.get().to_string(),
            stored.manifest.digest.as_str(),
            &stored.policy_digest,
        ],
        stored.scope_revision,
    );
    let offset = request
        .cursor
        .as_ref()
        .map(|c| {
            binding
                .decode(&c.0)
                .and_then(|s| s.parse::<usize>().map_err(|_| StorageError::CursorExpired))
        })
        .transpose()?
        .unwrap_or(0);
    if offset > stored.entries.len() {
        return Err(StorageError::CursorExpired.into());
    }
    let end = offset
        .saturating_add(request.limit.get() as usize)
        .min(stored.entries.len());
    let has_more = end < stored.entries.len();
    let entries = crate::protocol::records::ProjectPurgeEntryPage {
        items: BoundedList(stored.entries[offset..end].to_vec()),
        has_more,
        next_cursor: has_more.then(|| BoundedText(binding.encode(&end.to_string()))),
    };
    Ok(crate::protocol::records::ProjectPurgeManifestPage {
        manifest: stored.manifest,
        entries,
    })
}

fn build(
    id: Id,
    now: i64,
    actor: &ActorId,
    store: &crate::ports::storage::StoreInfo,
    project: &StoredProject,
    snapshot: &Snapshot,
) -> Result<StoredProjectPurge, Failure> {
    let mut groups = std::collections::BTreeMap::<(String, String, String), u64>::new();
    let mut removing_records = 0u64;
    let mut kept_records = 0u64;
    for row in snapshot
        .inventory
        .planning
        .rows
        .iter()
        .filter(|r| r.reason != ProjectPurgeRowReason::PurgeControlAudit)
    {
        let action = serde_json::to_string(&row.action).expect("row action");
        let reason = serde_json::to_string(&row.reason).expect("row reason");
        *groups
            .entry((row.table.clone(), action, reason))
            .or_default() += 1;
        if row.action == ProjectPurgeRowAction::Keep {
            kept_records += 1;
        } else {
            removing_records += 1;
        }
    }
    let mut entries = Vec::new();
    for ((table, action, reason), count) in groups {
        let disposition = match action.as_str() {
            "\"Keep\"" => PurgeDisposition::Keep,
            "\"Scrub\"" => PurgeDisposition::Scrub,
            _ => PurgeDisposition::Remove,
        };
        entries.push(ProjectPurgeEntry {
            id: Digest::try_from(hash(&(table.as_str(), &action, &reason)))
                .map_err(|_| invalid())?,
            kind: PurgeEntryKind::RecordClass,
            record_class: label(&table, "record"),
            disposition,
            record_count: checked_count(count)?,
            byte_count: counter(0),
            reason: label(reason.trim_matches('"'), "owned"),
            relative_path: None,
        });
    }
    let mut totals = [0u64; 2];
    for (index, files) in [&snapshot.removing, &snapshot.kept].into_iter().enumerate() {
        if files.selection.project_id != project.id
            || files.selection.store_id != store.store_id
            || files.selection.restore_epoch != store.restore_epoch
        {
            return Err(invalid());
        }
        let owners = files
            .selection
            .owners
            .iter()
            .map(hash)
            .collect::<BTreeSet<_>>();
        let mut seen = BTreeSet::new();
        for file in &files.files {
            if file.project_id != project.id
                || file.store_id != store.store_id
                || file.restore_epoch != store.restore_epoch
                || !owners.contains(&hash(&file.owner))
                || !seen.insert(hash(file))
            {
                return Err(invalid());
            }
            totals[index] = totals[index]
                .checked_add(file.byte_count)
                .ok_or_else(invalid)?;
            let value = serde_json::to_value(&file.owner).expect("native file owner");
            let class = value["kind"].as_str().ok_or_else(invalid)?;
            entries.push(ProjectPurgeEntry {
                id: Digest::try_from(hash(&(index, file))).map_err(|_| invalid())?,
                kind: PurgeEntryKind::ManagedFile,
                record_class: label(class, "managed_file"),
                disposition: if index == 0 {
                    PurgeDisposition::Remove
                } else {
                    PurgeDisposition::Keep
                },
                record_count: counter(1),
                byte_count: checked_count(file.byte_count)?,
                reason: label(
                    if index == 0 {
                        "OwnedManagedBytes"
                    } else {
                        "SurvivingByteKeeper"
                    },
                    "owned",
                ),
                relative_path: None,
            });
        }
    }
    if snapshot.client.removing_drafts > 0 {
        removing_records = removing_records
            .checked_add(snapshot.client.removing_drafts)
            .ok_or_else(invalid)?;
        entries.push(ProjectPurgeEntry {
            id: Digest::try_from(hash(&("client_drafts", &snapshot.client.digest)))
                .map_err(|_| invalid())?,
            kind: PurgeEntryKind::RecordClass,
            record_class: label("client_drafts", "client"),
            disposition: PurgeDisposition::Remove,
            record_count: checked_count(snapshot.client.removing_drafts)?,
            byte_count: counter(0),
            reason: label("OwnedClientDrafts", "owned"),
            relative_path: None,
        });
    }
    entries.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
    let witness_digest = hash(&(
        &snapshot.removing.digest,
        &snapshot.kept.digest,
        &snapshot.client.digest,
    ));
    let mut stored = StoredProjectPurge {
        manifest: ProjectPurgeManifest {
            id,
            project_id: project.id.clone(),
            revision: rev(1),
            project_revision: rev(project.revision),
            digest: Digest::try_from("0".repeat(64)).expect("digest"),
            phase: PurgePhase::Prepared,
            record_count: checked_count(removing_records)?,
            managed_byte_count: checked_count(totals[0])?,
            kept_record_count: checked_count(kept_records)?,
            kept_byte_count: checked_count(totals[1])?,
            pending_effects: counter(0),
            created_at_ms: counter(now),
            operation_id: None,
            error_code: None,
        },
        actor_id: actor.clone(),
        store_id: store.store_id.clone(),
        restore_epoch: store.restore_epoch,
        scope_revision: project.scope_revision,
        policy_digest: snapshot.inventory.planning.digest.clone(),
        witness_digest,
        entries,
        rows: snapshot
            .inventory
            .planning
            .rows
            .iter()
            .filter(|r| r.reason != ProjectPurgeRowReason::PurgeControlAudit)
            .cloned()
            .collect(),
    };
    stored.manifest.digest =
        Digest::try_from(project_purge_digest(&stored)).map_err(|_| invalid())?;
    Ok(stored)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::project_purge::{PurgeFileOwner, PurgeFileSelection, PurgeFileWitness};
    use crate::ports::storage::{
        AccessMode, ProjectPurgeCandidates, ProjectPurgeClientScope, ProjectPurgePlanning,
        ProjectPurgeRowWitness,
    };
    use crate::protocol::{StoreId, support::Check};
    fn id(s: &str) -> Id {
        Id::new(s).unwrap()
    }
    fn fixture() -> (crate::ports::storage::StoreInfo, StoredProject, Snapshot) {
        let store = crate::ports::storage::StoreInfo {
            store_id: StoreId(id("store")),
            restore_epoch: 0,
            boot_id: None,
            host_actor: ActorId(id("host")),
            access: AccessMode::ReadWrite,
        };
        let project = StoredProject {
            id: ProjectId(id("project")),
            name: "private title".into(),
            description: "private description".into(),
            lifecycle: DELETED.into(),
            revision: 7,
            scope_revision: 4,
            created_at_ms: 1,
            updated_at_ms: 3,
            archived_at_ms: Some(2),
            deleted_at_ms: Some(3),
            archive_epoch: 1,
            archive_stop_pending: false,
        };
        let selection = PurgeFileSelection {
            project_id: project.id.clone(),
            store_id: store.store_id.clone(),
            restore_epoch: 0,
            owners: vec![],
        };
        let files = PurgeFileSnapshot {
            selection: selection.clone(),
            files: vec![],
            digest: "a".repeat(64),
        };
        let snapshot = Snapshot {
            inventory: ProjectPurgeInventory {
                planning: ProjectPurgePlanning {
                    snapshot: ProjectPurgeCandidates {
                        project_id: project.id.clone(),
                        policy_digest: "b".repeat(64),
                        digest: "c".repeat(64),
                        rows: vec![],
                        references: vec![],
                    },
                    digest: "d".repeat(64),
                    rows: vec![],
                    unresolved: vec![],
                },
                removing_files: selection.clone(),
                kept_files: selection,
                client: ProjectPurgeClientScope {
                    project_id: project.id.clone(),
                    session_ids: vec![],
                    conversation_ids: vec![],
                    work_ids: vec![],
                    operation_ids: vec![],
                    deleted_artifact_ids: vec![],
                },
                unsettled_operations: vec![],
            },
            removing: files.clone(),
            kept: files,
            client: PurgeClientSnapshot {
                digest: "e".repeat(64),
                surviving_artifacts: vec![],
                pending: false,
                removing_drafts: 2,
            },
        };
        (store, project, snapshot)
    }
    #[test]
    fn review_counts_bind_scoped_rows_files_and_client_but_exclude_control_publication() {
        let (store, project, mut snapshot) = fixture();
        snapshot
            .inventory
            .planning
            .rows
            .push(ProjectPurgeRowWitness {
                table: "sessions".into(),
                key_digest: "1".repeat(64),
                row_digest: "2".repeat(64),
                action: ProjectPurgeRowAction::Delete,
                reason: ProjectPurgeRowReason::OwnedProjectRecords,
            });
        let owner = PurgeFileOwner::ClientStaging {
            file_id: id("file"),
        };
        snapshot.removing.selection.owners.push(owner.clone());
        snapshot.removing.files.push(PurgeFileWitness {
            project_id: project.id.clone(),
            store_id: store.store_id.clone(),
            restore_epoch: 0,
            owner,
            root: crate::authority::destructive::FileIdentity { volume: 1, file: 2 },
            relative_entry: "opaque.txt".into(),
            identity: crate::authority::destructive::FileIdentity { volume: 1, file: 3 },
            byte_count: 11,
            content_digest: "f".repeat(64),
            directory: false,
            link: false,
        });
        let prepared = build(
            id("manifest"),
            5,
            &ActorId(id("agent")),
            &store,
            &project,
            &snapshot,
        )
        .unwrap();
        prepared.manifest.check().unwrap();
        for entry in &prepared.entries {
            entry.check().unwrap();
        }
        assert_eq!(prepared.manifest.record_count.get(), 3);
        assert_eq!(prepared.manifest.managed_byte_count.get(), 11);
        let encoded = serde_json::to_string(&prepared.manifest).unwrap();
        assert!(!encoded.contains("private"));
        snapshot
            .inventory
            .planning
            .rows
            .push(ProjectPurgeRowWitness {
                table: "events".into(),
                key_digest: "3".repeat(64),
                row_digest: "4".repeat(64),
                action: ProjectPurgeRowAction::Keep,
                reason: ProjectPurgeRowReason::PurgeControlAudit,
            });
        let after_control = build(
            id("manifest"),
            5,
            &ActorId(id("agent")),
            &store,
            &project,
            &snapshot,
        )
        .unwrap();
        assert_eq!(after_control.manifest.digest, prepared.manifest.digest);
        snapshot.client.digest = "9".repeat(64);
        let after_client = build(
            id("manifest"),
            5,
            &ActorId(id("agent")),
            &store,
            &project,
            &snapshot,
        )
        .unwrap();
        assert_ne!(after_client.manifest.digest, prepared.manifest.digest);
        snapshot.removing.files[0].store_id = StoreId(id("other-store"));
        assert!(
            build(
                id("manifest"),
                5,
                &ActorId(id("agent")),
                &store,
                &project,
                &snapshot
            )
            .is_err()
        );
    }
}
