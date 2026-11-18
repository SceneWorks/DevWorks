//! Drafts and forms of the work screens, and the pure helpers over them.
use super::*;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MessageDraft {
    pub body: String,
    pub kind: usize,
    pub to_session: String,
    pub to_human: bool,
    pub item: String,
    pub feature: String,
    pub resource: String,
    pub blocking: bool,
    pub reply_to: String,
    pub resolution: usize,
}

#[derive(Default)]
pub struct MessageBoard {
    pub project: Option<String>,
    /// The project's messages, newest first; each older page continues before the last one
    /// (its cursor is that message sequence).
    pub history: Paged<RetainedMessage>,
    /// A re-read was asked for while the history was being re-read: read again when it ends.
    pub history_again: bool,
    pub inbox: Vec<RetainedMessage>,
    pub inbox_cursor: Option<String>,
    pub inbox_loaded: bool,
    pub inbox_refresh_pending: bool,
    pub inbox_has_more: bool,
    pub events: Vec<RetainedMessageEvent>,
    pub event_after: Option<i64>,
    pub event_poll_at: Option<std::time::Instant>,
    pub draft: MessageDraft,
    pub sending: Option<MessageDraft>,
    pub error: Option<String>,
}

/// One board column: a status of the project workflow.
#[derive(Clone, Debug)]
pub struct Column {
    pub id: String,
    pub name: String,
    pub semantic: String,
}

/// Columns are exactly the project's backend workflow, in its stored order.
pub fn columns(statuses: &[StatusRecord]) -> Vec<Column> {
    statuses
        .iter()
        .map(|status| Column {
            id: status.id.as_str().to_owned(),
            name: status.label.0.clone(),
            semantic: status.category.0.clone(),
        })
        .collect()
}

pub(crate) fn semantic_of(cols: &[Column], status: &str) -> String {
    cols.iter()
        .find(|c| c.id == status)
        .map(|c| c.semantic.clone())
        .unwrap_or_default()
}

pub(crate) fn finished(semantic: &str) -> bool {
    semantic == "Done" || semantic == "Closed"
}

pub(crate) fn status_name(cols: &[Column], status: &str) -> String {
    cols.iter()
        .find(|c| c.id == status)
        .map(|c| c.name.clone())
        .unwrap_or_else(|| status.to_owned())
}

/// A new feature, task or issue being written.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CreateForm {
    /// `feature`, `task` or `issue`.
    pub kind: String,
    pub title: String,
    pub description: String,
    pub feature: Option<String>,
    pub priority: i32,
    pub task_type: Option<String>,
    pub labels: Vec<String>,
    pub budget: crate::usage_screen::BudgetDraft,
}

/// An edit of one item's title, description and priority, against the record it was opened on.
#[derive(Clone, Debug)]
pub struct EditForm {
    pub snapshot: WorkRecord,
    pub title: String,
    pub description: String,
    pub priority: i32,
    pub task_type: Option<String>,
    pub labels: Vec<String>,
    pub budget: crate::usage_screen::BudgetDraft,
    pub budget_changed: bool,
}

/// A plan draft being written for a feature, against the record it was opened on.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PlanForm {
    pub snapshot: WorkRecord,
    pub text: String,
    /// One entry per requirement, criterion and test (an ID and its text, which may span
    /// lines): each round-trips exactly, and an entry the user did not change is submitted as
    /// it was read.
    pub requirements: Vec<Entry>,
    pub acceptance: Vec<Entry>,
    /// One goal per entry.
    pub goals: Vec<String>,
    pub repositories: BTreeSet<String>,
    /// The content the draft keeps when its text is unchanged.
    pub content: Option<ArtifactRef>,
    /// The text `content` holds (unchanged text keeps the content).
    pub original: String,
    pub acceptance_tests: Vec<Entry>,
    /// The draft the form started from: what the form does not show (requirement status, drop
    /// decisions and coverage; criterion state and evidence) is kept from it, never dropped.
    pub base: PlanBase,
    /// Explicit drops being authored; their decisions are stored before the draft command.
    #[serde(default)]
    pub drops: BTreeMap<String, DropDraft>,
    #[serde(default)]
    pub statuses: BTreeMap<String, RequirementStatus>,
    /// None withdraws a proposal present in the starting snapshot; Some proposes an exact cap.
    #[serde(default)]
    pub budget: Option<crate::usage_screen::BudgetDraft>,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct DropDraft {
    pub reason: String,
    pub artifact: Option<ArtifactRef>,
}

impl DropDraft {
    fn same_edits(&self, other: &Self) -> bool {
        self.reason == other.reason
            && (self
                .artifact
                .as_ref()
                .map(|a| (&a.artifact_id, &a.version_id))
                == other
                    .artifact
                    .as_ref()
                    .map(|a| (&a.artifact_id, &a.version_id))
                || !self.reason.trim().is_empty())
    }
}

/// One requirement, criterion or test being edited: its ID and its full text.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Entry {
    pub id: String,
    pub text: String,
}

/// The item editor after the save of `saved` was applied (`current` is the saved record):
/// closed when it holds nothing beyond what was saved; otherwise its later edits stay open on
/// the saved revision. An editor of another item is unchanged.
pub fn edit_after_save(
    live: Option<EditForm>,
    saved: &EditForm,
    current: Option<WorkRecord>,
) -> Option<EditForm> {
    match live {
        Some(l) if l.snapshot.id == saved.snapshot.id => {
            let same = l.title == saved.title
                && l.description == saved.description
                && l.priority == saved.priority
                && l.task_type == saved.task_type
                && l.labels == saved.labels
                && l.budget == saved.budget
                && l.budget_changed == saved.budget_changed;
            match current {
                Some(c) if !same => Some(EditForm { snapshot: c, ..l }),
                _ => None,
            }
        }
        other => other,
    }
}

/// Whether two plan forms hold the same edits (what a save submits).
pub fn same_plan_edits(a: &PlanForm, b: &PlanForm) -> bool {
    a.budget == b.budget
        && a.text == b.text
        && a.requirements == b.requirements
        && a.acceptance == b.acceptance
        && a.acceptance_tests == b.acceptance_tests
        && a.goals == b.goals
        && a.repositories == b.repositories
        && a.statuses.len() == b.statuses.len()
        && a.statuses.iter().all(|(id, status)| {
            b.statuses.get(id).is_some_and(|other| {
                std::mem::discriminant(status) == std::mem::discriminant(other)
            })
        })
        && a.drops.len() == b.drops.len()
        && a.drops
            .iter()
            .all(|(id, d)| b.drops.get(id).is_some_and(|b| d.same_edits(b)))
}

/// The plan editor after the save of `saved` was applied as `record`: closed when it holds
/// nothing beyond what was saved; otherwise its later edits stay open as a draft on the saved
/// revision (what the form does not show now comes from the saved draft). An editor of another
/// feature is unchanged.
pub fn plan_after_save(
    live: Option<PlanForm>,
    saved: &PlanForm,
    record: &WorkRecord,
) -> Option<PlanForm> {
    match live {
        Some(mut l) if l.snapshot.id == saved.snapshot.id => {
            if same_plan_edits(&l, saved) {
                return None;
            }
            let fresh = open_plan_form(record);
            for (id, drop) in &mut l.drops {
                if let Some(done) = saved.drops.get(id)
                    && drop.same_edits(done)
                {
                    drop.artifact = done.artifact.clone();
                }
            }
            Some(PlanForm {
                snapshot: fresh.snapshot,
                base: fresh.base,
                content: fresh.content,
                original: saved.text.clone(),
                ..l
            })
        }
        other => other,
    }
}

/// The requirements and criteria a plan form started from.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct PlanBase {
    pub requirements: Vec<RequirementItem>,
    pub acceptance: Vec<AcceptanceCriterion>,
    pub acceptance_tests: Vec<AcceptanceCriterion>,
}

/// A plan form opened on feature `f`: its pending draft when it has one, otherwise its active
/// plan (requirements, acceptance, acceptance tests, goals, repositories, content).
pub fn open_plan_form(f: &WorkRecord) -> PlanForm {
    let (base, goals, repos, content) = match f.pending_plan.as_ref().map(|p| &p.draft) {
        Some(d) => (
            PlanBase {
                requirements: d.requirements.0.clone(),
                acceptance: d.acceptance.0.clone(),
                acceptance_tests: d.acceptance_tests.0.clone(),
            },
            d.goals.0.iter().map(|g| g.0.clone()).collect::<Vec<_>>(),
            d.repository_ids
                .0
                .iter()
                .map(|r| r.as_str().to_owned())
                .collect::<BTreeSet<_>>(),
            Some(d.content.clone()),
        ),
        None => (
            PlanBase {
                requirements: f.requirements.0.clone(),
                acceptance: f.acceptance.0.clone(),
                acceptance_tests: f
                    .acceptance_tests
                    .as_ref()
                    .map(|t| t.0.clone())
                    .unwrap_or_default(),
            },
            f.goals.0.iter().map(|g| g.0.clone()).collect(),
            f.repository_ids
                .0
                .iter()
                .map(|r| r.as_str().to_owned())
                .collect(),
            f.plan.clone(),
        ),
    };
    let criteria = |c: &[AcceptanceCriterion]| {
        c.iter()
            .map(|a| Entry {
                id: a.id.as_str().to_owned(),
                text: a.text.0.clone(),
            })
            .collect::<Vec<_>>()
    };
    PlanForm {
        snapshot: f.clone(),
        budget: f
            .pending_plan
            .as_ref()
            .and_then(|pending| pending.budget.as_ref())
            .map(crate::usage_screen::BudgetDraft::from_input),
        text: String::new(),
        requirements: base
            .requirements
            .iter()
            .filter(|r| !matches!(r.status, RequirementStatus::Dropped))
            .map(|r| Entry {
                id: r.id.as_str().to_owned(),
                text: r.statement.0.clone(),
            })
            .collect(),
        acceptance: criteria(&base.acceptance),
        acceptance_tests: criteria(&base.acceptance_tests),
        goals,
        repositories: repos,
        original: String::new(),
        content,
        base,
        drops: BTreeMap::new(),
        statuses: BTreeMap::new(),
    }
}

/// The criteria of `entries`, keeping the stored state and evidence of each one whose ID and
/// text are unchanged (an unchanged entry is submitted exactly as it was read).
pub(super) fn merged_criteria(
    entries: &[Entry],
    base: &[AcceptanceCriterion],
) -> Option<BoundedList<AcceptanceCriterion, 0, 128>> {
    frame::bounded(
        entries
            .iter()
            .filter(|e| !e.text.trim().is_empty())
            .map(|e| match base.iter().find(|b| b.id.as_str() == e.id) {
                Some(b) if b.text.0 == e.text => Some(b.clone()),
                _ => Some(AcceptanceCriterion {
                    id: frame::id(e.id.trim())?,
                    text: frame::text(&e.text)?,
                    evidence_ids: BoundedList(Vec::new()),
                    state: CriterionState::Unmet,
                }),
            })
            .collect::<Option<Vec<_>>>()?,
    )
}

/// The `features.edit_plan` draft of a plan form: what the user wrote, with what the form does
/// not show kept from the draft it started from — each requirement's status, drop decision and
/// covering tasks, dropped requirements (their decision stays recorded), and each unchanged
/// criterion's state and evidence. None when a value is outside its contract.
pub fn plan_body(form: &PlanForm, content: &ArtifactRef) -> Option<PlanDraft> {
    let written: Vec<&Entry> = form
        .requirements
        .iter()
        .filter(|e| !e.text.trim().is_empty() || form.drops.contains_key(&e.id))
        .collect();
    let mut reqs = written
        .iter()
        .map(|e| {
            Some(
                match form
                    .base
                    .requirements
                    .iter()
                    .find(|b| b.id.as_str() == e.id)
                {
                    Some(b) => {
                        let mut v = b.clone();
                        if !e.text.trim().is_empty() {
                            v.statement = frame::text(&e.text)?;
                        }
                        if let Some(status) = form.statuses.get(&e.id) {
                            v.status = status.clone();
                        }
                        if let Some(drop) = form.drops.get(&e.id) {
                            v.status = RequirementStatus::Dropped;
                            v.drop_decision = drop.artifact.clone();
                        } else if matches!(b.status, RequirementStatus::Dropped) {
                            // A dropped entry is normally hidden and retained below. Making it
                            // editable is the explicit Restore action, never an implicit rewrite.
                            let status = form
                                .snapshot
                                .requirements
                                .0
                                .iter()
                                .find(|r| {
                                    r.id == b.id && !matches!(r.status, RequirementStatus::Dropped)
                                })
                                .map(|r| r.status.clone())
                                .unwrap_or(RequirementStatus::Proposed);
                            v.status = form.statuses.get(&e.id).cloned().unwrap_or(status);
                            v.drop_decision = None;
                        }
                        v
                    }
                    None => RequirementItem {
                        id: frame::id(e.id.trim())?,
                        statement: frame::text(&e.text)?,
                        status: form
                            .statuses
                            .get(&e.id)
                            .cloned()
                            .unwrap_or(RequirementStatus::Confirmed),
                        drop_decision: None,
                        covered_by_task_ids: BoundedList(Vec::new()),
                    },
                },
            )
        })
        .collect::<Option<Vec<_>>>()?;
    for b in &form.base.requirements {
        if matches!(b.status, RequirementStatus::Dropped)
            && !written.iter().any(|e| e.id == b.id.as_str())
        {
            reqs.push(b.clone());
        }
    }
    let goals: Vec<String> = form
        .goals
        .iter()
        .filter(|g| !g.trim().is_empty())
        .cloned()
        .collect();
    let repositories: Vec<String> = form.repositories.iter().cloned().collect();
    Some(PlanDraft {
        content: content.clone(),
        acceptance: merged_criteria(&form.acceptance, &form.base.acceptance)?,
        repository_ids: frame::ids(&repositories)?,
        goals: frame::texts(&goals)?,
        requirements: frame::bounded(reqs)?,
        acceptance_tests: merged_criteria(&form.acceptance_tests, &form.base.acceptance_tests)?,
    })
}
