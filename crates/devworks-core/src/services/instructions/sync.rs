//! Host observation of instruction sources: skill directories (shared and harness) and the
//! effective role settings. Each observed change is captured as host artifacts and recorded as
//! a new approved, active revision in one unit of work. Repeated syncs are idempotent; a
//! source that cannot be read is reported and leaves its items unchanged.
use super::scan::{self, ScannedFile, ScannedSkill};
use super::{InstructionService, event, role_item_id};
use crate::ports::IdKind;
use crate::ports::indexing::SourceFault;
use crate::ports::skill_files::SkillFile;
use crate::ports::storage::{
    ConfigScope, InstructionFilter, InstructionKind, InstructionOrigin, InstructionRevisionState,
    MAX_PAGE, ReadOptions, StorageError, StoredInstructionItem, StoredInstructionRevision,
    WriteOptions, read_snapshot, run_write,
};
use crate::protocol::Id;
use crate::services::artifacts::{ArtifactService, StagedFile, version_ref};
use crate::settings::catalog::AgentRole;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

/// What one sync did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SyncReport {
    /// New revisions recorded from observed sources.
    pub observed: usize,
    /// Harness skill directories written from their active revision.
    pub materialized: usize,
    /// Skill directories that could not be captured, with the reason.
    pub skipped: Vec<(String, String)>,
    /// Sources that could not be read at all.
    pub failures: Vec<String>,
}

/// One file to capture: its path within the skill and its bytes.
pub(crate) struct Capture {
    pub instruction: ScannedFile,
    pub resources: Vec<ScannedFile>,
    pub digest: String,
}
impl From<ScannedSkill> for Capture {
    fn from(s: ScannedSkill) -> Self {
        Self {
            instruction: s.instruction,
            resources: s.resources,
            digest: s.digest,
        }
    }
}
impl Capture {
    pub(crate) fn text(text: &str) -> Self {
        let instruction = ScannedFile::new(scan::SKILL_FILE.into(), text.as_bytes().to_vec());
        let digest = scan::revision_digest(&instruction.sha256, &[]);
        Self {
            instruction,
            resources: Vec::new(),
            digest,
        }
    }
}

/// Staged bytes of one capture, settled after its recording transaction.
pub(crate) struct StagedCapture {
    pub instruction: StagedFile,
    pub instruction_media: &'static str,
    pub resources: Vec<(ScannedFile, StagedFile)>,
    pub digest: String,
}

pub(crate) fn media_type(path: &str) -> &'static str {
    if path.ends_with(".md") {
        "text/markdown"
    } else if path.ends_with(".json") {
        "application/json"
    } else if path.ends_with(".txt") || path.ends_with(".sh") || path.ends_with(".py") {
        "text/plain"
    } else {
        "application/octet-stream"
    }
}

impl InstructionService {
    /// Stages every file of a capture (outside any transaction).
    pub(crate) fn stage_capture(
        &self,
        capture: &Capture,
        source: Value,
    ) -> Result<StagedCapture, StorageError> {
        let mut staged = Vec::new();
        let stage = |file: &ScannedFile| -> Result<StagedFile, StorageError> {
            self.artifacts
                .stage_host_bytes(
                    &*self.store,
                    &file.bytes,
                    json!({"kind": "Instruction", "path": file.path, "source": source}),
                )?
                .map_err(|cause| StorageError::InvalidRecord(format!("skill capture: {cause}")))
        };
        let instruction = stage(&capture.instruction)?;
        for resource in &capture.resources {
            match stage(resource) {
                Ok(file) => staged.push((resource.clone(), file)),
                Err(e) => {
                    self.artifacts
                        .settle_host_bytes(&*self.store, &instruction, false);
                    for (_, s) in &staged {
                        self.artifacts.settle_host_bytes(&*self.store, s, false);
                    }
                    return Err(e);
                }
            }
        }
        Ok(StagedCapture {
            instruction,
            instruction_media: "text/markdown",
            resources: staged,
            digest: capture.digest.clone(),
        })
    }
    pub(crate) fn settle_capture(&self, staged: &StagedCapture, committed: bool) {
        self.artifacts
            .settle_host_bytes(&*self.store, &staged.instruction, committed);
        for (_, s) in &staged.resources {
            self.artifacts.settle_host_bytes(&*self.store, s, committed);
        }
    }

    /// Observes every source once. Called by the host at boot and before adoption.
    pub fn sync(&self) -> Result<SyncReport, StorageError> {
        self.sync_kind(None)
    }

    /// Observes the sources of one family (`None`: both).
    pub(crate) fn sync_kind(
        &self,
        kind: Option<InstructionKind>,
    ) -> Result<SyncReport, StorageError> {
        let _guard = self
            .sync_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut report = SyncReport::default();
        if kind != Some(InstructionKind::Role) {
            let harness = self
                .files
                .root()
                .ok()
                .or_else(|| Some(self.roots.harness.clone()));
            for (origin, root) in [
                (InstructionOrigin::Shared, self.roots.shared.clone()),
                (InstructionOrigin::Harness, harness),
            ] {
                let Some(root) = root else { continue };
                if let Err(e) = self.sync_root(origin, &root, &mut report) {
                    report
                        .failures
                        .push(format!("{} skills: {e}", origin.as_str().to_lowercase()));
                }
            }
        }
        if kind != Some(InstructionKind::Skill)
            && let Err(e) = self.sync_roles(&mut report)
        {
            report.failures.push(format!("roles: {e}"));
        }
        Ok(report)
    }

    fn items(
        &self,
        filter: &InstructionFilter,
    ) -> Result<Vec<StoredInstructionItem>, StorageError> {
        read_snapshot(&*self.store, &ReadOptions::default(), |read| {
            let mut all = Vec::new();
            let mut after: Option<Id> = None;
            loop {
                let page =
                    read.instructions()
                        .instruction_items(filter, after.as_ref(), MAX_PAGE)?;
                let done = page.len() < MAX_PAGE as usize;
                after = page.last().map(|i| i.id.clone());
                all.extend(page);
                if done {
                    return Ok(all);
                }
            }
        })
    }

    fn sync_root(
        &self,
        origin: InstructionOrigin,
        root: &Path,
        report: &mut SyncReport,
    ) -> Result<(), StorageError> {
        let existing: BTreeMap<String, StoredInstructionItem> = self
            .items(&InstructionFilter {
                kind: Some(InstructionKind::Skill),
                scopes: vec![ConfigScope::Workspace],
                name: None,
            })?
            .into_iter()
            .filter(|i| i.origin == origin)
            .map(|i| (i.name.clone(), i))
            .collect();
        let canonical = match self.source.canonical_dir(root) {
            Ok(path) => Some(path),
            Err(SourceFault::Missing) => None,
            Err(e) => return Err(StorageError::InvalidRecord(e.to_string())),
        };
        let (skills, skipped) = match &canonical {
            Some(path) => scan::scan_root(&*self.source, path)
                .map_err(|e| StorageError::InvalidRecord(e.to_string()))?,
            None => (Vec::new(), Vec::new()),
        };
        let mut seen = std::collections::BTreeSet::new();
        for s in skipped {
            // The directory exists but could not be captured now: keep its last observed state
            // rather than treating it as deleted.
            seen.insert(s.name.clone());
            report.skipped.push((
                s.name,
                format!("{}: {}", origin.as_str().to_lowercase(), s.reason),
            ));
        }
        for skill in skills {
            seen.insert(skill.name.clone());
            let item = existing.get(&skill.name);
            if let Some(item) = item {
                if item.observed_digest.as_deref() == Some(skill.digest.as_str()) && item.present {
                    continue;
                }
                // Our own last write of a harness skill whose active revision changed since:
                // the pending materialization runs below, not a user edit.
                if origin == InstructionOrigin::Harness
                    && item.materialized_digest.as_deref() == Some(skill.digest.as_str())
                {
                    self.mark(item, Some(skill.digest.clone()), true)?;
                    continue;
                }
                if self.active_digest(item)?.as_deref() == Some(skill.digest.as_str()) {
                    self.mark(item, Some(skill.digest.clone()), true)?;
                    continue;
                }
            }
            let path = canonical.as_ref().expect("scanned root");
            let name = skill.name.clone();
            match self.observe_skill(origin, path, item, skill) {
                Ok(()) => report.observed += 1,
                // Named by origin like a scan failure: a capture that could not be recorded
                // (an artifact write failed) leaves the directory changed but unrecorded, so a
                // replacement of that root is refused until it is captured.
                Err(e) => report
                    .failures
                    .push(format!("{}: {name}: {e}", origin.as_str().to_lowercase())),
            }
        }
        for item in existing.values() {
            if !seen.contains(&item.name) && item.present {
                self.mark(item, item.observed_digest.clone(), false)?;
            }
        }
        if origin == InstructionOrigin::Harness {
            for item in self.items(&InstructionFilter {
                kind: Some(InstructionKind::Skill),
                scopes: vec![ConfigScope::Workspace],
                name: None,
            })? {
                if item.origin != InstructionOrigin::Harness {
                    continue;
                }
                let active = self.active_digest(&item)?;
                // Repair only an interrupted write of our own: the directory still holds what we
                // last wrote (or nothing). A user edit was observed above instead.
                let on_disk_ours = item.observed_digest == item.materialized_digest
                    || (!item.present && item.materialized_digest.is_none());
                if active.is_some() && active != item.materialized_digest && on_disk_ours {
                    match self.materialize_locked(&item.id) {
                        Ok(true) => report.materialized += 1,
                        Ok(false) => {}
                        Err(e) => report.failures.push(e.to_string()),
                    }
                }
            }
        }
        Ok(())
    }

    fn active_digest(&self, item: &StoredInstructionItem) -> Result<Option<String>, StorageError> {
        let Some(active) = item.active_revision else {
            return Ok(None);
        };
        let id = item.id.clone();
        read_snapshot(&*self.store, &ReadOptions::default(), |read| {
            Ok(read
                .instructions()
                .instruction_revision(&id, active)?
                .map(|r| r.content_digest))
        })
    }

    /// Records presence and the observed digest without a new revision.
    fn mark(
        &self,
        item: &StoredInstructionItem,
        observed: Option<String>,
        present: bool,
    ) -> Result<(), StorageError> {
        let mut next = item.clone();
        next.observed_digest = observed;
        next.present = present;
        if next == *item {
            return Ok(());
        }
        next.version += 1;
        run_write(&*self.store, WriteOptions::default(), move |tx| {
            next.updated_at_ms = tx.now_ms();
            match tx.instructions().update_instruction_item(&next) {
                // A concurrent writer changed the item; the next sync observes it again.
                Err(StorageError::RevisionConflict(_)) => Ok(()),
                other => other,
            }
        })
    }

    fn observe_skill(
        &self,
        origin: InstructionOrigin,
        root: &Path,
        item: Option<&StoredInstructionItem>,
        skill: ScannedSkill,
    ) -> Result<(), StorageError> {
        let name = skill.name.clone();
        let source_path = root.join(&name).to_string_lossy().into_owned();
        let source =
            json!({"kind": "SkillDirectory", "origin": origin.as_str(), "path": source_path});
        let capture = Capture::from(skill);
        let staged = self.stage_capture(&capture, source.clone())?;
        let artifacts = self.artifacts.clone();
        let base = item.cloned();
        let root_path = root.to_string_lossy().into_owned();
        let staged = Arc::new(staged);
        let staged_in = staged.clone();
        let result = run_write(&*self.store, WriteOptions::default(), move |tx| {
            let now = tx.now_ms();
            let mut item = match &base {
                Some(item) => {
                    let current = tx
                        .instructions()
                        .instruction_item(&item.id)?
                        .ok_or(StorageError::ScopeDenied)?;
                    if current.version != item.version {
                        // Changed concurrently: the next sync observes again.
                        return Ok(false);
                    }
                    current
                }
                None => {
                    let item = StoredInstructionItem {
                        id: tx.new_id(IdKind::Operation),
                        kind: InstructionKind::Skill,
                        name: name.clone(),
                        scope: ConfigScope::Workspace,
                        origin,
                        root_path: Some(root_path.clone()),
                        source_path: Some(source_path.clone()),
                        latest_revision: 0,
                        active_revision: None,
                        enabled: true,
                        materialized_digest: None,
                        observed_digest: None,
                        present: true,
                        version: 1,
                        created_at_ms: now,
                        updated_at_ms: now,
                        confined_project: None,
                    };
                    tx.instructions().insert_instruction_item(&item)?;
                    item
                }
            };
            let host = tx.store().host_actor.clone();
            let revision = record_capture(
                tx,
                &artifacts,
                &item,
                &staged_in,
                RecordedRevision {
                    state: InstructionRevisionState::Approved,
                    author: host.clone(),
                    origin: json!({"kind": "Observed", "source": source}),
                    applicability: Vec::new(),
                    title: format!("Skill {}", item.name),
                    carried_resources: None,
                    confined: None,
                },
            )?;
            item.latest_revision = revision.revision;
            item.active_revision = Some(revision.revision);
            // Once something is in effect the item is no longer one project's draft.
            item.confined_project = None;
            item.observed_digest = Some(revision.content_digest.clone());
            if origin == InstructionOrigin::Harness {
                item.materialized_digest = Some(revision.content_digest.clone());
            }
            item.present = true;
            item.root_path = Some(root_path);
            item.source_path = Some(source_path);
            item.version += 1;
            item.updated_at_ms = now;
            tx.instructions().update_instruction_item(&item)?;
            event(
                tx,
                &host,
                &item,
                "InstructionRevisionObserved",
                json!({"revision": revision.revision, "contentDigest": revision.content_digest, "source": source}),
            )?;
            Ok(true)
        });
        self.settle_capture(&staged, matches!(result, Ok(true)));
        result.map(|_| ())
    }

    fn sync_roles(&self, report: &mut SyncReport) -> Result<(), StorageError> {
        let existing: BTreeMap<String, StoredInstructionItem> = self
            .items(&InstructionFilter {
                kind: Some(InstructionKind::Role),
                scopes: Vec::new(),
                name: None,
            })?
            .into_iter()
            .map(|i| (i.id.as_str().to_owned(), i))
            .collect();
        let mut targets: Vec<(AgentRole, ConfigScope)> = AgentRole::ALL
            .into_iter()
            .map(|r| (r, ConfigScope::Workspace))
            .collect();
        for item in existing.values() {
            if let ConfigScope::Project(_) = item.scope
                && let Some(role) = AgentRole::parse(&item.name)
            {
                targets.push((role, item.scope.clone()));
            }
        }
        // Project role overrides saved through settings before any instruction item existed.
        for (role, scope) in self.configured_project_roles()? {
            if !targets.iter().any(|(r, s)| *r == role && *s == scope) {
                targets.push((role, scope));
            }
        }
        for (role, scope) in targets {
            let id = role_item_id(role, &scope);
            let item = existing.get(id.as_str());
            let text = read_snapshot(&*self.store, &ReadOptions::default(), |read| {
                crate::settings::service::role_instructions(
                    read.configuration(),
                    role,
                    scope.project_id(),
                )
                .map_err(|f| match f {
                    crate::settings::service::Failure::Storage(e) => e,
                    crate::settings::service::Failure::Refused(_) => {
                        StorageError::InvalidRecord("role instructions".into())
                    }
                })
                .map(|e| (e.text, e.revisions))
            })?;
            let capture = Capture::text(&text.0);
            if let Some(item) = item
                && self.active_digest(item)?.as_deref() == Some(capture.digest.as_str())
            {
                continue;
            }
            match self.observe_role(role, &scope, item, capture, &text.1) {
                Ok(true) => report.observed += 1,
                Ok(false) => {}
                Err(e) => report.failures.push(e.to_string()),
            }
        }
        Ok(())
    }

    /// Every (role, project) whose project settings layer sets the role's instructions.
    fn configured_project_roles(&self) -> Result<Vec<(AgentRole, ConfigScope)>, StorageError> {
        read_snapshot(&*self.store, &ReadOptions::default(), |read| {
            let mut out = Vec::new();
            let filter = crate::ports::storage::ProjectFilter {
                only: None,
                lifecycles: vec!["Active".into(), "Archived".into()],
            };
            let mut after = None;
            loop {
                let page = read
                    .projects()
                    .projects(&filter, after.as_ref(), MAX_PAGE)?;
                for project in &page {
                    let scope = ConfigScope::Project(project.id.clone());
                    let layer =
                        crate::settings::model::current_layer(read.configuration(), &scope)?;
                    for role in AgentRole::ALL {
                        if layer.values.contains_key(role.instructions_key()) {
                            out.push((role, scope.clone()));
                        }
                    }
                }
                if page.len() < MAX_PAGE as usize {
                    return Ok(out);
                }
                after = page.last().map(|p| crate::ports::storage::ProjectPosition {
                    created_at_ms: p.created_at_ms,
                    id: p.id.clone(),
                });
            }
        })
    }

    fn observe_role(
        &self,
        role: AgentRole,
        scope: &ConfigScope,
        item: Option<&StoredInstructionItem>,
        capture: Capture,
        settings: &crate::settings::model::AdoptedRevisions,
    ) -> Result<bool, StorageError> {
        let source = json!({
            "kind": "RoleSetting",
            "key": role.instructions_key(),
            "scope": scope.key(),
            "workspaceRevision": settings.workspace,
            "projectRevision": settings.project,
        });
        // Only text equal to the catalog default is recorded as the default revision; an earlier
        // customization observed first is recorded as what it is.
        let is_default = crate::settings::catalog::descriptor(role.instructions_key())
            .map(|d| (d.default)())
            .and_then(|v| {
                v.as_str()
                    .map(|t| crate::fingerprint::sha256_hex(t.as_bytes()))
            })
            == Some(capture.instruction.sha256.clone());
        let staged = Arc::new(self.stage_capture(&capture, source.clone())?);
        let staged_in = staged.clone();
        let artifacts = self.artifacts.clone();
        let base = item.cloned();
        let scope = scope.clone();
        let result = run_write(&*self.store, WriteOptions::default(), move |tx| {
            let now = tx.now_ms();
            let (mut item, first) = match &base {
                Some(item) => {
                    let current = tx
                        .instructions()
                        .instruction_item(&item.id)?
                        .ok_or(StorageError::ScopeDenied)?;
                    if current.version != item.version {
                        return Ok(false);
                    }
                    (current, false)
                }
                None => {
                    let item = new_role_item(role, &scope, now);
                    if tx.instructions().instruction_item(&item.id)?.is_some() {
                        return Ok(false);
                    }
                    tx.instructions().insert_instruction_item(&item)?;
                    (item, true)
                }
            };
            let host = tx.store().host_actor.clone();
            let revision = record_capture(
                tx,
                &artifacts,
                &item,
                &staged_in,
                RecordedRevision {
                    state: InstructionRevisionState::Approved,
                    author: host.clone(),
                    origin: json!({"kind": if first && is_default { "Default" } else { "SettingsObserved" }, "source": source}),
                    applicability: vec![role.as_str().to_owned()],
                    title: format!("Role {}", role.as_str()),
                    carried_resources: None,
                    confined: None,
                },
            )?;
            item.latest_revision = revision.revision;
            item.active_revision = Some(revision.revision);
            // Once something is in effect the item is no longer one project's draft.
            item.confined_project = None;
            item.version += 1;
            item.updated_at_ms = now;
            tx.instructions().update_instruction_item(&item)?;
            event(
                tx,
                &host,
                &item,
                "InstructionRevisionObserved",
                json!({"revision": revision.revision, "contentDigest": revision.content_digest, "source": source}),
            )?;
            Ok(true)
        });
        self.settle_capture(&staged, matches!(result, Ok(true)));
        result
    }

    /// Writes the active revision of a harness skill into its directory, then records it.
    /// Serialized with source observation and every other materialization (the sync lock), and
    /// always reconciled against the item's current active revision, so a stale write can never
    /// be observed as a user edit. `Ok(false)` when there is nothing to write.
    pub(crate) fn materialize(&self, item: &StoredInstructionItem) -> Result<bool, StorageError> {
        let _guard = self
            .sync_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.materialize_locked(&item.id)
    }

    fn materialize_locked(&self, id: &Id) -> Result<bool, StorageError> {
        let mut wrote = false;
        // A concurrent activation can commit between the read and the record; write again for
        // the newer active revision (bounded: each round observes a newer committed state).
        for _ in 0..8 {
            let id = id.clone();
            let current = read_snapshot(&*self.store, &ReadOptions::default(), |read| {
                let Some(item) = read.instructions().instruction_item(&id)? else {
                    return Ok(None);
                };
                let revision = match item.active_revision {
                    Some(active) => read.instructions().instruction_revision(&id, active)?,
                    None => None,
                };
                Ok(Some((item, revision)))
            })?;
            let Some((item, Some(revision))) = current else {
                return Ok(wrote);
            };
            if item.origin != InstructionOrigin::Harness {
                return Ok(wrote);
            }
            // Matching digests prove nothing when the directory is gone: it is written again.
            if item.present
                && item.materialized_digest.as_deref() == Some(revision.content_digest.as_str())
                && item.observed_digest == item.materialized_digest
            {
                return Ok(wrote);
            }
            let files = self.revision_files(&revision)?;
            self.files.materialize(&item.name, &files).map_err(|e| {
                StorageError::unavailable(crate::ports::storage::Unavailable::Io, e.to_string())
            })?;
            wrote = true;
            let mut next = item.clone();
            next.materialized_digest = Some(revision.content_digest.clone());
            next.observed_digest = Some(revision.content_digest);
            next.present = true;
            next.version += 1;
            let recorded = run_write(&*self.store, WriteOptions::default(), move |tx| {
                let current = tx
                    .instructions()
                    .instruction_item(&next.id)?
                    .ok_or(StorageError::ScopeDenied)?;
                if current.version != next.version - 1 {
                    return Ok(false);
                }
                next.updated_at_ms = tx.now_ms();
                tx.instructions().update_instruction_item(&next)?;
                Ok(true)
            })?;
            if recorded {
                return Ok(true);
            }
        }
        Err(StorageError::Busy)
    }

    /// Every file of a revision, read back from its managed artifacts.
    pub(crate) fn revision_files(
        &self,
        revision: &StoredInstructionRevision,
    ) -> Result<Vec<SkillFile>, StorageError> {
        let mut files = vec![SkillFile {
            path: scan::SKILL_FILE.into(),
            bytes: self.artifact_bytes(&revision.content)?,
            executable: false,
        }];
        for resource in revision.resources.as_array().into_iter().flatten() {
            files.push(SkillFile {
                path: resource["path"]
                    .as_str()
                    .ok_or_else(|| StorageError::InvalidRecord("resource path".into()))?
                    .to_owned(),
                bytes: self.artifact_bytes(&resource["artifact"])?,
                executable: resource["executable"].as_bool().unwrap_or(false),
            });
        }
        Ok(files)
    }

    /// The exact bytes of a recorded `ArtifactRef`.
    pub(crate) fn artifact_bytes(&self, reference: &Value) -> Result<Vec<u8>, StorageError> {
        let version_id = reference["versionId"]
            .as_str()
            .and_then(|v| Id::new(v).ok())
            .ok_or_else(|| StorageError::InvalidRecord("instruction artifact".into()))?;
        let version = read_snapshot(&*self.store, &ReadOptions::default(), |read| {
            read.artifacts().version(&version_id)
        })?
        .filter(|v| v.live() && reference["sha256"].as_str() == Some(v.blob_digest.as_str()))
        .ok_or(StorageError::ViewUnavailable)?;
        let count = u64::try_from(version.byte_count)
            .map_err(|_| StorageError::InvalidRecord("artifact size".into()))?;
        self.artifacts
            .files
            .read_range(
                &version.blob_digest,
                &version.blob_generation,
                count,
                0,
                usize::try_from(count).unwrap_or(usize::MAX),
            )
            .map_err(|e| {
                StorageError::unavailable(crate::ports::storage::Unavailable::Io, format!("{e:?}"))
            })
    }
}

pub(crate) fn new_role_item(
    role: AgentRole,
    scope: &ConfigScope,
    now: i64,
) -> StoredInstructionItem {
    StoredInstructionItem {
        id: role_item_id(role, scope),
        kind: InstructionKind::Role,
        name: role.as_str().to_owned(),
        scope: scope.clone(),
        origin: InstructionOrigin::Role,
        root_path: None,
        source_path: None,
        latest_revision: 0,
        active_revision: None,
        enabled: true,
        materialized_digest: None,
        observed_digest: None,
        present: true,
        version: 1,
        created_at_ms: now,
        updated_at_ms: now,
        confined_project: None,
    }
}

pub(crate) struct RecordedRevision {
    pub state: InstructionRevisionState,
    pub author: crate::protocol::ActorId,
    pub origin: Value,
    pub applicability: Vec<String>,
    pub title: String,
    /// Already-recorded resource artifacts carried over unchanged (a draft of the text only).
    pub carried_resources: Option<Value>,
    /// A project-confined agent's draft: its content is scoped to this project only.
    pub confined: Option<crate::protocol::ProjectId>,
}

/// Publishes a staged instruction text as a host artifact in the item's own scope (the release
/// of a confined draft's approved content) and answers its `ArtifactRef`.
pub(crate) fn publish_instruction_text(
    tx: &mut dyn crate::ports::storage::WriteTransaction,
    artifacts: &ArtifactService,
    item: &StoredInstructionItem,
    staged: &StagedCapture,
    title: &str,
) -> Result<Value, StorageError> {
    let scope = match &item.scope {
        ConfigScope::Workspace => crate::ports::storage::ArtifactScope::Workspace,
        ConfigScope::Project(p) => crate::ports::storage::ArtifactScope::Project(p.clone()),
    };
    let owner = tx.store().host_actor.clone();
    let version = artifacts.commit_host_bytes_in_scope(
        tx,
        &staged.instruction,
        crate::services::artifacts::host::HostArtifactPublication {
            title: &format!("{title} {}", scan::SKILL_FILE),
            media_type: staged.instruction_media,
            provenance: json!({"kind": "Instruction", "instructionId": item.id.as_str(), "path": scan::SKILL_FILE}),
            scope,
            owner,
        },
    )?;
    serde_json::to_value(version_ref(&version))
        .map_err(|e| StorageError::InvalidRecord(e.to_string()))
}

/// Commits a staged capture as host artifacts in the item's scope and appends its revision
/// (`item.latest_revision + 1`). The caller updates the item in the same unit of work.
pub(crate) fn record_capture(
    tx: &mut dyn crate::ports::storage::WriteTransaction,
    artifacts: &ArtifactService,
    item: &StoredInstructionItem,
    staged: &StagedCapture,
    recorded: RecordedRevision,
) -> Result<StoredInstructionRevision, StorageError> {
    let scope = match (&recorded.confined, &item.scope) {
        (Some(p), _) | (None, ConfigScope::Project(p)) => {
            crate::ports::storage::ArtifactScope::Project(p.clone())
        }
        (None, ConfigScope::Workspace) => crate::ports::storage::ArtifactScope::Workspace,
    };
    let owner = tx.store().host_actor.clone();
    let publish = |tx: &mut dyn crate::ports::storage::WriteTransaction,
                   file: &StagedFile,
                   media: &str,
                   path: &str|
     -> Result<Value, StorageError> {
        let version = artifacts.commit_host_bytes_in_scope(
            tx,
            file,
            crate::services::artifacts::host::HostArtifactPublication {
                title: &format!("{} {path}", recorded.title),
                media_type: media,
                provenance: json!({"kind": "Instruction", "instructionId": item.id.as_str(), "path": path}),
                scope: scope.clone(),
                owner: owner.clone(),
            },
        )?;
        serde_json::to_value(version_ref(&version))
            .map_err(|e| StorageError::InvalidRecord(e.to_string()))
    };
    let content = publish(
        tx,
        &staged.instruction,
        staged.instruction_media,
        scan::SKILL_FILE,
    )?;
    let mut resources = Vec::new();
    for (file, s) in &staged.resources {
        let artifact = publish(tx, s, media_type(&file.path), &file.path)?;
        resources.push(json!({
            "path": file.path, "artifact": artifact, "sha256": file.sha256,
            "byteCount": file.bytes.len(), "script": file.script, "executable": file.executable,
        }));
    }
    let revision = StoredInstructionRevision {
        instruction_id: item.id.clone(),
        revision: item.latest_revision + 1,
        content,
        content_digest: staged.digest.clone(),
        state: recorded.state,
        author: recorded.author,
        origin: recorded.origin,
        resources: recorded
            .carried_resources
            .unwrap_or(Value::Array(resources)),
        applicability: recorded.applicability,
        approval_id: None,
        created_at_ms: tx.now_ms(),
        decided_at_ms: None,
        confined_project: recorded.confined,
    };
    tx.instructions().insert_instruction_revision(&revision)?;
    Ok(revision)
}
