//! Signed app update discovery, exact human approval, and durable outcome reporting.
use crate::context::InvocationContext;
use crate::gateway::{AdmittedCommand, AdmittedQuery};
use crate::locks::LockExt;
use crate::operations::{OperationInput, workspace::Input};
use crate::ports::storage::{
    ApplicationStore, AttemptBinding, Cancellation, NewOperation, ReadOptions, ReadSnapshot,
    StorageError, WriteOptions, WriteTransaction, read_snapshot, run_write,
};
use crate::ports::update::{
    DownloadedUpdate, InstallApproval, Release, ReleaseError, ReleaseSource, RunningApplication,
    StagedRelease, UpdateState, asset_name,
};
use crate::ports::{
    Clock, CommandHandler, Decided, HandlerFailure, IdKind, QueryAnswer, QueryHandler,
};
use crate::protocol::provider::Digest;
use crate::protocol::records::{UpdateChannel, UpdateRecord};
use crate::protocol::results::{DomainDecision, OperationResult};
use crate::protocol::support::{BoundedList, Page};
use crate::protocol::{
    CorrelationId, DomainErrorCode, DomainFault, Id, OperationState, RetryAdvice,
};
use crate::services::kernel::{Failure, Refusal, decided, label, rev};
use semver::Version;
use serde_json::json;
use std::sync::{Arc, Mutex};

pub const STATUS: &str = "workspace.update_status";
pub const APPROVE: &str = "workspace.approve_install";
const HOUR: i64 = 3_600_000;
const APPROVAL_WINDOW: i64 = 10 * 60_000;

#[derive(Clone, Debug, Default)]
pub enum Progress {
    #[default]
    Idle,
    Checking,
    Downloading {
        version: String,
        received: u64,
        total: u64,
    },
}

#[derive(Clone, Debug)]
pub struct UpdateStatus {
    pub channel: UpdateChannel,
    pub checked_at_ms: Option<i64>,
    pub issue: Option<String>,
    pub current: Option<DownloadedUpdate>,
    pub last: Option<UpdateRecord>,
    pub last_detail: Option<String>,
    pub progress: Progress,
    pub can_install: bool,
    pub build: String,
}

#[derive(Default)]
struct ActivePoll {
    cancellation: Option<Cancellation>,
    progress: Progress,
    stopped: bool,
}

pub struct UpdateService {
    store: Arc<dyn ApplicationStore>,
    source: Arc<dyn ReleaseSource>,
    running: RunningApplication,
    clock: Arc<dyn Clock>,
    poll: Mutex<ActivePoll>,
}

impl UpdateService {
    pub fn new(
        store: Arc<dyn ApplicationStore>,
        source: Arc<dyn ReleaseSource>,
        running: RunningApplication,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            store,
            source,
            running,
            clock,
            poll: Mutex::default(),
        }
    }

    fn state(&self) -> Result<UpdateState, StorageError> {
        read_snapshot(&*self.store, &ReadOptions::default(), |read| {
            read.updates()?.update_state()
        })
    }

    /// A workspace restore advances the profile epoch. A restored historical update
    /// approval cannot authorize activation, and its stage files do not come from the backup.
    pub fn bind_profile(&self) -> Result<(), StorageError> {
        let info = self.store.info();
        let profile = (info.store_id, info.restore_epoch);
        if self.state()?.profile.as_ref() == Some(&profile) {
            return Ok(());
        }
        run_write(&*self.store, WriteOptions::host_state(), move |tx| {
            let repo = tx.updates()?;
            let previous = repo.update_state()?;
            if previous.profile.as_ref() != Some(&profile) {
                repo.save_update_state(&UpdateState {
                    generation: previous.generation.saturating_add(1),
                    profile: Some(profile),
                    channel: previous.channel,
                    ..UpdateState::default()
                })?;
            }
            Ok(())
        })
    }

    pub fn status(&self) -> Result<UpdateStatus, StorageError> {
        let state = self.state()?;
        let can_install = self.qualifies(&state, self.clock.now_ms().get());
        Ok(UpdateStatus {
            channel: state.channel,
            checked_at_ms: state.checked_at_ms,
            issue: state.issue,
            current: state.update,
            last: state.last,
            last_detail: state.last_detail,
            progress: self.poll.locked().progress.clone(),
            can_install,
            build: self.running.build.clone(),
        })
    }

    fn qualifies(&self, state: &UpdateState, now: i64) -> bool {
        self.running.installed_binary_digest.is_some()
            && self.poll.locked().cancellation.is_none()
            && state.approval.is_none()
            && state.issue.is_none()
            && state
                .checked_at_ms
                .is_some_and(|last| last <= now && now < last.saturating_add(HOUR))
            && state
                .update
                .as_ref()
                .is_some_and(|current| self.eligible_stage(&current.stage))
    }

    fn eligible_stage(&self, stage: &StagedRelease) -> bool {
        let plan = &stage.manifest.migrations;
        stage.manifest.target == self.running.target
            && self
                .running
                .installed_binary_digest
                .as_ref()
                .is_none_or(|old| old != &stage.manifest.entrypoint().content.sha256)
            && !stage.manifest.destructive_migration
            && plan.starts_with(&self.running.migrations)
            && Version::parse(stage.manifest.tag.trim_start_matches('v'))
                .is_ok_and(|version| version.cmp_precedence(&self.running.version).is_gt())
    }

    pub fn set_channel(
        &self,
        ctx: &InvocationContext,
        channel: UpdateChannel,
    ) -> Result<(), StorageError> {
        let ctx = ctx.clone();
        run_write(&*self.store, WriteOptions::host_state(), move |tx| {
            super::workspace_archive::authority_in(tx, &ctx)?;
            let repo = tx.updates()?;
            let mut state = repo.update_state()?;
            if state.approval.is_some() {
                return Err(StorageError::ScopeDenied);
            }
            if std::mem::discriminant(&state.channel) != std::mem::discriminant(&channel) {
                state.channel = channel;
                state.generation += 1;
                state.checked_at_ms = None;
                state.retry_at_ms = 0;
                state.failures = 0;
                state.issue = None;
                state.update = None;
                repo.save_update_state(&state)?;
            }
            Ok(())
        })?;
        self.cancel_poll();
        Ok(())
    }

    pub fn cancel_poll(&self) {
        if let Some(cancellation) = &self.poll.locked().cancellation {
            cancellation.cancel();
        }
    }

    pub fn stop_polls(&self) {
        let mut poll = self.poll.locked();
        poll.stopped = true;
        if let Some(cancellation) = &poll.cancellation {
            cancellation.cancel();
        }
    }

    /// Called by one native worker after the first frame, then while the desktop is open.
    /// A manual request bypasses freshness, but still honors backoff and single flight.
    pub fn poll(&self, manual: bool) -> Result<(), StorageError> {
        let state = self.state()?;
        let now = self.clock.now_ms().get();
        if !poll_due(&state, now, manual) {
            return Ok(());
        }
        let cancel = {
            let mut poll = self.poll.locked();
            if poll.stopped || poll.cancellation.is_some() {
                return Ok(());
            }
            let cancellation = Cancellation::default();
            poll.cancellation = Some(cancellation.clone());
            poll.progress = Progress::Checking;
            cancellation
        };
        let result = self.refresh(&state, &cancel);
        let committed = self.commit_poll(state.generation, result, &cancel);
        if committed.is_ok()
            && let Ok(state) = self.state()
        {
            let keep = state
                .update
                .as_ref()
                .map(|update| update.stage.manifest_digest.as_str());
            let approval = state
                .approval
                .as_ref()
                .map(|approval| approval.stage.manifest_digest.as_str());
            let retained: Vec<_> = keep.into_iter().chain(approval).collect();
            self.source.retain(&retained);
        }
        let mut poll = self.poll.locked();
        poll.progress = Progress::Idle;
        poll.cancellation = None;
        committed
    }

    fn refresh(
        &self,
        state: &UpdateState,
        cancel: &Cancellation,
    ) -> Result<PollResult, ReleaseError> {
        let pages = self.source.discover(&state.pages, cancel)?;
        let selected = select(&pages, &self.running, &state.channel);
        let mut issue = None;
        let stage = match selected {
            Selection::None => None,
            Selection::Rejected(reason) => {
                issue = Some(ReleaseError::rejected(reason));
                None
            }
            Selection::Target(release, version) => {
                self.poll.locked().progress = Progress::Downloading {
                    version: version.to_string(),
                    received: 0,
                    total: 0,
                };
                let progress = |received, total| {
                    self.poll.locked().progress = Progress::Downloading {
                        version: version.to_string(),
                        received,
                        total,
                    };
                };
                let stage = self
                    .source
                    .stage(release, &self.running.target, cancel, &progress)?;
                if !self.eligible_stage(&stage) || self.running.installed_binary_digest.is_none() {
                    issue = Some(ReleaseError::rejected(
                        "This release is staged. The installed owner or its storage migration is unqualified.",
                    ));
                }
                Some(stage)
            }
        };
        Ok(PollResult {
            pages,
            stage,
            issue,
        })
    }

    fn commit_poll(
        &self,
        generation: i64,
        result: Result<PollResult, ReleaseError>,
        cancel: &Cancellation,
    ) -> Result<(), StorageError> {
        let cancelled = cancel.is_cancelled();
        let version = self.running.version.clone();
        run_write(&*self.store, WriteOptions::host_state(), move |tx| {
            let now = tx.now_ms();
            let mut state = tx.updates()?.update_state()?;
            if state.generation != generation || state.approval.is_some() {
                return Ok(());
            }
            match result {
                _ if cancelled => {
                    update_failure(&mut state, ReleaseError::cancelled(), now);
                }
                Ok(PollResult {
                    pages,
                    stage,
                    mut issue,
                }) => {
                    let profile = tx.updates()?.update_profile()?;
                    if let Some(selected) = &stage
                        && profile
                            .reviewed_prefix(&selected.manifest.migrations)
                            .is_none()
                    {
                        issue = Some(ReleaseError::rejected(
                            "The signed migration plan is incompatible with the current workspace.",
                        ));
                    }
                    let had_stage = state.update.is_some();
                    state.checked_at_ms = Some(now);
                    state.pages = pages;
                    state.retry_at_ms = 0;
                    let unchanged = state
                        .update
                        .as_ref()
                        .zip(stage.as_ref())
                        .is_some_and(|(old, new)| &old.stage == new);
                    if !unchanged {
                        state.update = stage.map(|stage| {
                            let id = tx.new_id(IdKind::Operation);
                            let proposal_id = tx.new_id(IdKind::Operation);
                            let tag = stage.manifest.tag.trim_start_matches('v');
                            let migration_required =
                                stage.manifest.migrations.len() as i64 > profile.schema;
                            let record = UpdateRecord {
                                id,
                                revision: rev(1),
                                version: label(tag, "DevWorks"),
                                channel: if Version::parse(tag)
                                    .is_ok_and(|version| !version.pre.is_empty())
                                {
                                    UpdateChannel::Prerelease
                                } else {
                                    UpdateChannel::Stable
                                },
                                digest: Digest(stage.manifest.entrypoint().content.sha256.clone()),
                                signature_verified: true,
                                state: OperationState::NeedsInput,
                                migration_required,
                            };
                            DownloadedUpdate {
                                record,
                                stage,
                                proposal_id,
                            }
                        });
                    }
                    state.failures = 0;
                    state.issue = issue.map(|issue| issue.message).or_else(|| {
                        (had_stage && state.update.is_none()).then(|| {
                            format!("The previously staged update is no longer offered for DevWorks {version}.")
                        })
                    });
                }
                Err(error) => update_failure(&mut state, error, now),
            }
            tx.updates()?.save_update_state(&state)
        })
    }

    pub fn approval(&self, proposal: &Id) -> Result<InstallApproval, StorageError> {
        self.state()?
            .approval
            .filter(|approval| {
                &approval.proposal == proposal
                    && self.clock.now_ms().get() < approval.expires_at_ms
                    && self.running.build == approval.old_build
                    && self.running.installed_binary_digest.as_ref()
                        == Some(&approval.old_binary_digest)
                    && self.running.version.to_string() == approval.old_version
                    && self.store.info().boot_id.as_ref() == Some(&approval.approved_boot)
            })
            .ok_or(StorageError::ScopeDenied)
    }

    pub fn pending_approval(&self) -> Result<Option<InstallApproval>, StorageError> {
        Ok(self.state()?.approval)
    }

    /// Arm only the exact approval selected by the native close flow, after runtime quiescence.
    pub fn begin_install(&self, approval: &InstallApproval) -> Result<(), StorageError> {
        let approval = approval.clone();
        run_write(&*self.store, WriteOptions::host_state(), move |tx| {
            let mut state = tx.updates()?.update_state()?;
            if state.approval.as_ref() != Some(&approval)
                || tx.now_ms() >= approval.expires_at_ms
                || tx.store().boot_id.as_ref() != Some(&approval.approved_boot)
                || tx.actors().actor_kind(&approval.actor)?
                    != Some(crate::context::PrincipalKind::Human)
            {
                return Err(StorageError::ScopeDenied);
            }
            let op = tx
                .operations()
                .operation(&approval.operation)?
                .ok_or(StorageError::ViewUnavailable)?;
            let attempt =
                tx.operations()
                    .begin_attempt(&op.id, op.revision, AttemptBinding::default())?;
            tx.operations().transition(
                &op.id,
                op.revision + 1,
                Some(&attempt.fence()),
                OperationState::Dispatching,
                Some(json!({"kind":"VerifiedUpdateShutdown"})),
            )?;
            let current = state.update.as_mut().ok_or(StorageError::ViewUnavailable)?;
            current.record.state = OperationState::Dispatching;
            current.record.revision = rev(current.record.revision.get() + 1);
            tx.updates()?.save_update_state(&state)
        })
    }

    /// The next boot reconciles native effect facts. A missing or uncertain effect never creates
    /// a replacement attempt. The caller provides evidence from the installation journal.
    pub fn finish(
        &self,
        approval: &InstallApproval,
        outcome: InstallOutcome,
        detail: &str,
    ) -> Result<(), StorageError> {
        let approval = approval.clone();
        let detail: String = detail.chars().take(1024).collect();
        run_write(&*self.store, WriteOptions::host_state(), move |tx| {
            let mut state = tx.updates()?.update_state()?;
            if state.approval.as_ref() != Some(&approval) {
                return Ok(());
            }
            if tx.store().store_id != approval.store || tx.store().restore_epoch != approval.epoch {
                return Err(StorageError::ScopeDenied);
            }
            let op = tx
                .operations()
                .operation(&approval.operation)?
                .ok_or(StorageError::ViewUnavailable)?;
            let mut record = state
                .update
                .as_ref()
                .ok_or(StorageError::ViewUnavailable)?
                .record
                .clone();
            let no_attempt = op.latest_attempt().is_none();
            let terminal = match outcome {
                InstallOutcome::Installed => OperationState::ObservedComplete,
                InstallOutcome::NotStarted => OperationState::Failed,
                InstallOutcome::Cancelled => OperationState::CancelledBeforeDispatch,
                InstallOutcome::Failed => OperationState::Failed,
                InstallOutcome::Uncertain if no_attempt => OperationState::Failed,
                InstallOutcome::Uncertain => OperationState::OutcomeUnknown,
            };
            record.revision = rev(record.revision.get() + 1);
            record.state = terminal;
            if terminal != op.state {
                let observation = if matches!(outcome, InstallOutcome::Installed) {
                    json!({"result":OperationResult::WorkspaceApproveInstall{value:record.clone()}, "detail":detail})
                } else {
                    json!({"detail":detail})
                };
                tx.operations().transition(
                    &op.id,
                    op.revision,
                    op.latest_attempt().map(|attempt| attempt.fence()).as_ref(),
                    terminal,
                    (!no_attempt).then_some(observation),
                )?;
            }
            state.last_detail = Some(detail);
            if terminal == OperationState::OutcomeUnknown {
                if let Some(active) = &mut state.update {
                    active.record = record;
                }
            } else {
                state.last = Some(record);
                state.approval = None;
                state.update = None;
                state.checked_at_ms = None;
                state.retry_at_ms = 0;
                state.issue = None;
            }
            tx.updates()?.save_update_state(&state)
        })
    }

    fn approve(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, Failure> {
        let OperationInput::Workspace(Input::ApproveInstall(input)) = command.input() else {
            return Err(StorageError::ScopeDenied.into());
        };
        super::workspace_archive::authority_in(tx, ctx)?;
        if !ctx.workspace_maintenance_matches(command.input()) {
            return Err(Refusal::new(
                DomainErrorCode::PermissionDenied,
                "desktop-update-confirmation",
                None,
            )
            .into());
        }
        let mut state = tx.updates()?.update_state()?;
        if !self.qualifies(&state, tx.now_ms()) {
            return Err(Refusal::new(
                DomainErrorCode::SourceChanged,
                "staged-update",
                Some(input.update_id.clone()),
            )
            .into());
        }
        let profile = tx.updates()?.update_profile()?;
        let current = state.update.as_mut().ok_or(StorageError::ViewUnavailable)?;
        if current.record.id != input.update_id
            || current.record.revision != input.expected_revision
            || current.record.digest != input.binary_digest
            || current.proposal_id != input.proposal_id
        {
            return Err(Refusal::new(
                DomainErrorCode::SourceChanged,
                "staged-update",
                Some(input.update_id.clone()),
            )
            .into());
        }
        let approved_boot = tx
            .store()
            .boot_id
            .clone()
            .ok_or(StorageError::ScopeDenied)?;
        let source_migrations = profile
            .reviewed_prefix(&current.stage.manifest.migrations)
            .ok_or_else(|| {
                Refusal::new(
                    DomainErrorCode::SourceChanged,
                    "workspace-migration-plan",
                    Some(input.update_id.clone()),
                )
            })?;
        let approval = InstallApproval {
            operation: crate::protocol::OperationId(tx.new_id(IdKind::Operation)),
            proposal: input.proposal_id.clone(),
            update: input.update_id.clone(),
            revision: input.expected_revision.get(),
            store: tx.store().store_id.clone(),
            epoch: tx.store().restore_epoch,
            actor: ctx.principal().clone(),
            approved_boot,
            request_sha256: command.fingerprint().as_str().into(),
            expires_at_ms: tx
                .now_ms()
                .saturating_add(APPROVAL_WINDOW)
                .min(state.checked_at_ms.unwrap_or_default().saturating_add(HOUR)),
            old_build: self.running.build.clone(),
            old_binary_digest: self
                .running
                .installed_binary_digest
                .clone()
                .ok_or(StorageError::ScopeDenied)?,
            old_version: self.running.version.to_string(),
            old_schema_capability: self
                .running
                .migrations
                .last()
                .ok_or(StorageError::ViewUnavailable)?
                .version,
            profile_schema: profile.schema,
            profile_migrations: source_migrations.to_vec(),
            stage: current.stage.clone(),
        };
        tx.operations().record_intent(NewOperation {
            id: approval.operation.clone(),
            root_id: None,
            assignment_id: None,
            capability: APPROVE.to_owned(),
            schema_version: command.schema_revision().0 as i64,
            args_digest: approval.request_sha256.clone(),
            payload: json!({"approval":approval}),
        })?;
        current.record.state = OperationState::IntentRecorded;
        current.record.revision = rev(current.record.revision.get() + 1);
        state.approval = Some(approval.clone());
        tx.updates()?.save_update_state(&state)?;
        Ok(Decided {
            decision: DomainDecision::Accepted {
                operation_id: approval.operation.clone(),
            },
            operation_id: Some(approval.operation),
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub enum InstallOutcome {
    Installed,
    NotStarted,
    Cancelled,
    Failed,
    Uncertain,
}

impl CommandHandler for UpdateService {
    fn decide(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        match self.approve(ctx, command, tx) {
            Ok(reply) => Ok(reply),
            Err(error) => decided(tx, Err(error)),
        }
    }
}

impl QueryHandler for UpdateService {
    fn query(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        snapshot: &dyn ReadSnapshot,
    ) -> Result<QueryAnswer, DomainFault> {
        let correlation = CorrelationId(ctx.invocation_id().clone());
        if !matches!(
            query.input(),
            OperationInput::Workspace(Input::UpdateStatus(_))
        ) {
            return Err(super::fault(
                DomainErrorCode::InvalidArgument,
                RetryAdvice::Never,
                &correlation,
            ));
        }
        let state = snapshot
            .updates()
            .and_then(|view| view.update_state())
            .map_err(|error| {
                let (code, retry) = crate::services::failure::storage_fault(&error);
                super::fault(code, retry, &correlation)
            })?;
        let items = BoundedList(
            state
                .update
                .into_iter()
                .map(|current| current.record)
                .chain(state.last)
                .collect(),
        );
        Ok(QueryAnswer {
            stamp: snapshot
                .stamp()
                .read_stamp(ctx.policy_revision().clone(), rev(1)),
            result: OperationResult::WorkspaceUpdateStatus {
                value: Page {
                    items,
                    next_cursor: None,
                    has_more: false,
                },
            },
        })
    }
}

struct PollResult {
    pages: Vec<crate::ports::update::ReleasePage>,
    stage: Option<StagedRelease>,
    issue: Option<ReleaseError>,
}

enum Selection<'a> {
    None,
    Target(&'a Release, Version),
    Rejected(&'static str),
}

fn select<'a>(
    pages: &'a [crate::ports::update::ReleasePage],
    running: &RunningApplication,
    channel: &UpdateChannel,
) -> Selection<'a> {
    let names = ["manifest.json", "manifest.sig", "bundle"]
        .map(|suffix| asset_name(&running.target, suffix));
    let mut selected: Option<(&Release, Version)> = None;
    let mut missing = false;
    let mut invalid = false;
    for release in pages.iter().flat_map(|page| &page.releases) {
        if release.prerelease && matches!(channel, UpdateChannel::Stable) {
            continue;
        }
        let Some(version) = release.version() else {
            invalid = true;
            continue;
        };
        if version.cmp_precedence(&running.version).is_le() {
            continue;
        }
        if names.iter().any(|name| release.asset(name).is_none()) {
            missing = true;
            continue;
        }
        if let Some((existing, best)) = &selected {
            match version.cmp_precedence(best) {
                std::cmp::Ordering::Less => continue,
                std::cmp::Ordering::Equal if existing != &release => {
                    return Selection::Rejected(
                        "The feed returned ambiguous assets for one application version.",
                    );
                }
                std::cmp::Ordering::Equal => continue,
                std::cmp::Ordering::Greater => {}
            }
        }
        selected = Some((release, version));
    }
    if let Some((release, version)) = selected {
        Selection::Target(release, version)
    } else if missing {
        Selection::Rejected("No newer release has a complete signed asset for this OS and CPU.")
    } else if invalid {
        Selection::Rejected("The release feed returned an invalid DevWorks version.")
    } else {
        Selection::None
    }
}

fn poll_due(state: &UpdateState, now: i64, manual: bool) -> bool {
    state.approval.is_none()
        && now >= state.retry_at_ms
        && (manual
            || state.failures > 0
            || state
                .checked_at_ms
                .is_none_or(|last| now < last || now >= last.saturating_add(HOUR)))
}

fn update_failure(state: &mut UpdateState, error: ReleaseError, now: i64) {
    state.failures = (state.failures + 1).min(8);
    let delay = if error.cancelled {
        60_000
    } else {
        (30_000_i64 << state.failures).min(HOUR)
    };
    let jitter = now.rem_euclid(17_005);
    state.retry_at_ms = (now + delay + jitter).max(error.retry_after_ms.unwrap_or_default());
    state.issue = Some(error.message);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn due_checks_retry_an_offline_attempt_without_replaying_missed_hours() {
        let mut state = UpdateState {
            checked_at_ms: Some(10_000),
            ..UpdateState::default()
        };
        assert!(!poll_due(&state, 20_000, false));
        assert!(poll_due(&state, 20_000, true));
        assert!(poll_due(&state, 10_000 + HOUR, false));
        assert!(poll_due(&state, 10_000 + 8 * HOUR, false));
        update_failure(
            &mut state,
            ReleaseError {
                message: "rate limited".into(),
                retry_after_ms: Some(500_000),
                cancelled: false,
            },
            20_000,
        );
        assert!(!poll_due(&state, 499_999, true));
        assert!(poll_due(&state, 500_000, false));
        assert_eq!(state.checked_at_ms, Some(10_000));
    }
}
