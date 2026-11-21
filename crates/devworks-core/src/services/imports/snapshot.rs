//! Sealed CodeGraph work exports. The manifest binds the exact bytes of the following JSON lines.
use crate::fingerprint::sha256_hex;
use crate::protocol::records::ImportConfig;
use crate::protocol::{Check, Id, ProjectId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

pub(super) const SOURCE_REVISION: &str = "afe73560215bbe7593fc59451de5cc3237e3eeca";
pub const MAX_EXPORT_BYTES: usize = 256 * 1024 * 1024;
const MAX_LINE_BYTES: usize = 2 * 1024 * 1024;
const MAX_RECORDS: usize = 500_000;
const OVERLAP_MS: i64 = 30 * 60 * 1000;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Manifest {
    pub format: String,
    pub version: u32,
    pub namespace: Id,
    pub source_revision: String,
    pub captured_at_ms: i64,
    pub coverage_from_ms: i64,
    pub records_sha256: String,
    pub counts: BTreeMap<String, usize>,
    pub missing: BTreeMap<String, usize>,
    pub selected_projects: Vec<ProjectId>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
pub enum HistoryKind {
    FeatureDocument,
    Comment,
    Link,
    Alias,
    Change,
    Attempt,
    Handoff,
    Acceptance,
    Plan,
    Session,
    SessionClaim,
    ItemThread,
    GitEvidence,
    Attachment,
    TranscriptEvidence,
    CodeReference,
    MissingEvidence,
    LegacyMapping,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SourceStamp {
    pub author: Option<String>,
    pub author_kind: Option<String>,
    pub author_session: Option<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author_harness: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author_host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_version: Option<i64>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Backlog,
    Ready,
    InProgress,
    InReview,
    Complete,
    Closed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    Low,
    Medium,
    High,
    Critical,
}
impl Priority {
    pub fn rank(self) -> i64 {
        match self {
            Self::Low => 0,
            Self::Medium => 1,
            Self::High => 2,
            Self::Critical => 3,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum SourceRecord {
    Project {
        id: Id,
        key: String,
        name: String,
        description: String,
        goals: Vec<String>,
        stamp: SourceStamp,
    },
    Repository {
        id: Id,
        project_id: ProjectId,
        name: String,
        remote: Option<String>,
        role: String,
    },
    Feature {
        id: Id,
        project_id: ProjectId,
        number: i64,
        title: String,
        description: String,
        stamp: SourceStamp,
    },
    Item {
        id: Id,
        project_id: ProjectId,
        feature_id: Option<Id>,
        number: i64,
        item_type: String,
        title: String,
        description: String,
        status: Status,
        priority: Priority,
        order: i64,
        labels: Vec<String>,
        repository_name: Option<String>,
        stamp: SourceStamp,
    },
    Dependency {
        id: Id,
        project_id: ProjectId,
        blocked: Id,
        blocker: Id,
        created_at_ms: i64,
    },
    History {
        id: Id,
        project_id: ProjectId,
        subject_id: Id,
        history_kind: HistoryKind,
        stamp: SourceStamp,
        evidence: Value,
    },
}

impl SourceRecord {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Project { .. } => "Project",
            Self::Repository { .. } => "Repository",
            Self::Feature { .. } => "Feature",
            Self::Item { .. } => "Item",
            Self::Dependency { .. } => "Dependency",
            Self::History { .. } => "History",
        }
    }
    fn phase(&self) -> u8 {
        match self {
            Self::Project { .. } => 0,
            Self::Repository { .. } => 1,
            Self::Feature { .. } => 2,
            Self::Item { .. } => 3,
            Self::Dependency { .. } => 4,
            Self::History { .. } => 5,
        }
    }
    pub fn id(&self) -> &Id {
        match self {
            Self::Project { id, .. }
            | Self::Repository { id, .. }
            | Self::Feature { id, .. }
            | Self::Item { id, .. }
            | Self::Dependency { id, .. }
            | Self::History { id, .. } => id,
        }
    }
    pub fn project(&self) -> &Id {
        match self {
            Self::Project { id, .. } => id,
            Self::Repository { project_id, .. }
            | Self::Feature { project_id, .. }
            | Self::Item { project_id, .. }
            | Self::Dependency { project_id, .. }
            | Self::History { project_id, .. } => &project_id.0,
        }
    }
    pub fn stamp(&self) -> Option<&SourceStamp> {
        match self {
            Self::Project { stamp, .. }
            | Self::Feature { stamp, .. }
            | Self::Item { stamp, .. }
            | Self::History { stamp, .. } => Some(stamp),
            Self::Repository { .. } | Self::Dependency { .. } => None,
        }
    }
    pub fn digest(&self) -> Result<String, SnapshotError> {
        serde_json::to_vec(self)
            .map(|bytes| sha256_hex(&bytes))
            .map_err(|_| SnapshotError::Malformed)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotError {
    TooLarge,
    Malformed,
    UnsupportedFormat,
    IntegrityMismatch,
    NoSelection,
    SelectionOutsideSnapshot,
    WindowNotCovered,
}

pub struct Snapshot {
    pub manifest: Manifest,
    pub digest: String,
    pub records: Vec<SourceRecord>,
}

impl Snapshot {
    pub fn parse(bytes: &[u8]) -> Result<Self, SnapshotError> {
        if bytes.len() > MAX_EXPORT_BYTES {
            return Err(SnapshotError::TooLarge);
        }
        let header_end = bytes
            .iter()
            .position(|byte| *byte == b'\n')
            .filter(|position| *position <= MAX_LINE_BYTES)
            .ok_or(SnapshotError::Malformed)?;
        let (header, body) = bytes.split_at(header_end);
        let body = &body[1..];
        let manifest: Manifest =
            serde_json::from_slice(header).map_err(|_| SnapshotError::Malformed)?;
        if manifest.format != "DevWorksCodeGraphWork"
            || manifest.version != 1
            || manifest.source_revision != SOURCE_REVISION
        {
            return Err(SnapshotError::UnsupportedFormat);
        }
        if manifest.captured_at_ms <= 0
            || manifest.coverage_from_ms < 0
            || manifest.captured_at_ms < manifest.coverage_from_ms
            || manifest.namespace.check().is_err()
            || manifest.selected_projects.is_empty()
            || manifest
                .selected_projects
                .iter()
                .any(|p| p.check().is_err())
            || manifest
                .selected_projects
                .iter()
                .map(|p| p.0.as_str())
                .collect::<BTreeSet<_>>()
                .len()
                != manifest.selected_projects.len()
            || !body.ends_with(b"\n")
        {
            return Err(SnapshotError::Malformed);
        }
        if sha256_hex(body) != manifest.records_sha256 {
            return Err(SnapshotError::IntegrityMismatch);
        }
        let mut counts = BTreeMap::new();
        let mut keys = BTreeSet::new();
        let mut records = Vec::new();
        for line in body[..body.len() - 1].split(|byte| *byte == b'\n') {
            if line.len() > MAX_LINE_BYTES || records.len() == MAX_RECORDS {
                return Err(SnapshotError::TooLarge);
            }
            let record: SourceRecord =
                serde_json::from_slice(line).map_err(|_| SnapshotError::Malformed)?;
            if record.id().check().is_err()
                || record.project().check().is_err()
                || !manifest
                    .selected_projects
                    .iter()
                    .any(|p| &p.0 == record.project())
                || !keys.insert((record.kind(), record.id().as_str().to_owned()))
                || record.stamp().is_some_and(|stamp| {
                    stamp.created_at_ms < 0
                        || stamp.updated_at_ms < stamp.created_at_ms
                        || stamp
                            .author
                            .as_ref()
                            .is_some_and(|author| author.len() > 1024)
                })
            {
                return Err(SnapshotError::Malformed);
            }
            *counts.entry(record.kind().to_owned()).or_insert(0) += 1;
            records.push(record);
        }
        if counts != manifest.counts {
            return Err(SnapshotError::IntegrityMismatch);
        }
        Ok(Self {
            manifest,
            digest: sha256_hex(bytes),
            records,
        })
    }

    /// Produce the exact selected input retained for an admitted operation. The original export
    /// digest remains in the operation's binding, independently of this smaller sealed copy.
    pub(super) fn seal_selection(&self, config: &ImportConfig) -> Result<Vec<u8>, SnapshotError> {
        let selection = self.select(config, config.all_projects.unwrap_or(false))?;
        let mut manifest = self.manifest.clone();
        manifest.counts.clear();
        manifest.selected_projects.clear();
        manifest.missing.clear();
        let mut body = Vec::new();
        for record in selection.records {
            if let SourceRecord::Project { id, .. } = record {
                manifest.selected_projects.push(ProjectId(id.clone()));
            }
            *manifest.counts.entry(record.kind().into()).or_insert(0) += 1;
            let start = body.len();
            serde_json::to_writer(&mut body, record).map_err(|_| SnapshotError::Malformed)?;
            if body.len() - start > MAX_LINE_BYTES || body.len() > MAX_EXPORT_BYTES {
                return Err(SnapshotError::TooLarge);
            }
            body.push(b'\n');
        }
        manifest.records_sha256 = sha256_hex(&body);
        let mut bytes = serde_json::to_vec(&manifest).map_err(|_| SnapshotError::Malformed)?;
        bytes.push(b'\n');
        if bytes.len() + body.len() > MAX_EXPORT_BYTES {
            return Err(SnapshotError::TooLarge);
        }
        bytes.extend(body);
        Ok(bytes)
    }

    pub fn delta_start(
        &self,
        explicit: Option<i64>,
        prior_covered_through: Option<i64>,
    ) -> Result<i64, SnapshotError> {
        let since = explicit
            .or_else(|| prior_covered_through.map(|v| v.saturating_sub(OVERLAP_MS).max(0)))
            .ok_or(SnapshotError::WindowNotCovered)?;
        if since < self.manifest.coverage_from_ms
            || since > self.manifest.captured_at_ms
            || prior_covered_through.is_some_and(|bound| bound > self.manifest.captured_at_ms)
        {
            return Err(SnapshotError::WindowNotCovered);
        }
        Ok(since)
    }

    pub fn select<'a>(
        &'a self,
        config: &ImportConfig,
        all_projects: bool,
    ) -> Result<Selection<'a>, SnapshotError> {
        let projects = &config.project_ids.0;
        let work = &config.work_ids.0;
        if !all_projects && projects.is_empty() && work.is_empty() {
            return Err(SnapshotError::NoSelection);
        }
        let mut selected_projects: BTreeSet<&str> = projects.iter().map(|p| p.0.as_str()).collect();
        if all_projects {
            selected_projects.extend(self.manifest.selected_projects.iter().map(|p| p.0.as_str()));
        }
        let mut selected_work: BTreeSet<&str> = BTreeSet::new();
        let mut needed_projects = selected_projects.clone();
        for record in &self.records {
            if matches!(
                record,
                SourceRecord::Feature { .. } | SourceRecord::Item { .. }
            ) && (selected_projects.contains(record.project().as_str())
                || work.contains(record.id()))
            {
                selected_work.insert(record.id().as_str());
                needed_projects.insert(record.project().as_str());
                if let SourceRecord::Item {
                    feature_id: Some(parent),
                    ..
                } = record
                {
                    selected_work.insert(parent.as_str());
                }
            }
        }
        if projects
            .iter()
            .any(|id| !self.manifest.selected_projects.contains(id))
            || work.iter().any(|id| !selected_work.contains(id.as_str()))
            || needed_projects.iter().any(|id| {
                !self.records.iter().any(|record| {
                    matches!(record, SourceRecord::Project { id: project, .. } if project.as_str() == *id)
                })
            })
        {
            return Err(SnapshotError::SelectionOutsideSnapshot);
        }

        let mut selected_sessions = BTreeSet::new();
        if config.include_history {
            for record in &self.records {
                let relevant = match record {
                    SourceRecord::History { subject_id, .. } => {
                        selected_work.contains(subject_id.as_str())
                    }
                    _ => selected_work.contains(record.id().as_str()),
                };
                if relevant
                    && let Some(session) = record.stamp().and_then(|s| s.author_session.as_ref())
                {
                    selected_sessions.insert(session.as_str());
                }
            }
        }
        let is_selected = |record: &&SourceRecord| {
            if !needed_projects.contains(record.project().as_str()) {
                return false;
            }
            match record {
                SourceRecord::Project { .. } | SourceRecord::Repository { .. } => true,
                SourceRecord::Feature { id, .. } | SourceRecord::Item { id, .. } => {
                    selected_work.contains(id.as_str())
                }
                SourceRecord::Dependency { blocked, .. } => {
                    selected_work.contains(blocked.as_str())
                }
                SourceRecord::History {
                    subject_id,
                    history_kind,
                    evidence,
                    ..
                } => {
                    let selected_session = *history_kind == HistoryKind::Session
                        && evidence["id"]
                            .as_str()
                            .is_some_and(|id| selected_sessions.contains(id));
                    config.include_history
                        && (selected_work.contains(subject_id.as_str())
                            || selected_projects.contains(subject_id.as_str())
                            || selected_session)
                        && (config.include_git_links
                            || !matches!(
                                history_kind,
                                HistoryKind::GitEvidence | HistoryKind::CodeReference
                            ))
                }
            }
        };
        let mut records: Vec<_> = self.records.iter().filter(is_selected).collect();
        records.sort_by_key(|record| (record.phase(), record.id().as_str()));
        Ok(Selection {
            records,
            selected_work,
        })
    }
}

pub struct Selection<'a> {
    pub records: Vec<&'a SourceRecord>,
    pub selected_work: BTreeSet<&'a str>,
}
