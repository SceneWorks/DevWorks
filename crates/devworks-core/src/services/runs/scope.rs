//! The plan and readiness port run control evaluates dispatch against (AD79, AR-02).
//!
//! Plans belong to features and are owned by the planning item (DW-012); readiness assessments
//! and their findings by the coordinator item (DW-015). Run control owns neither: it asks this
//! port for the **active approved** scope of a root and the readiness of each affected scope
//! unit, and records what it dispatched under (the assignment's bound unit, digest and plan
//! revision). A pending plan revision is reported only so it can be shown; it never authorizes
//! anything.
//!
//! * [`RecordedScope`] is the production port: it reads the stored feature plan slot
//!   (`feature_plans`). For an active plan the planning service wrote, the approved scope units
//!   (the feature-wide unit and one per requirement) and each unit's readiness come from the
//!   feature's recorded assessment ([`crate::services::work::readiness::scope_plan`]): `Missing`
//!   without one, `Stale` for a unit whose digest changed since it was reviewed, `Blocked` by an
//!   open blocking finding of the unit (or a feature-wide one), `Current` otherwise. Any other
//!   approved slot reports the feature-wide unit with no readiness evidence (fail closed, never
//!   manufactured readiness).
//! * [`ScriptedScope`] is the deterministic double tests and later items' fixtures script.
use crate::ports::storage::{RunRead, StorageError, WorkRoot};
use crate::protocol::RootId;
use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

/// Whether `key` is a valid named scope unit key: 1–256 characters, no NUL. The feature-wide
/// scope (the coordinator's own) has no key: it is the absent key (`None`, stored as NULL), never
/// a sentinel string.
pub fn valid_scope_key(key: &str) -> bool {
    (1..=256).contains(&key.chars().count()) && !key.contains('\0')
}

/// Readiness evidence of one approved scope unit (AD79). Only `Current` permits implementation
/// dispatch of that unit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Readiness {
    /// A current assessment covers the unit with no unresolved applicable blocking finding.
    Current { assessment_revision: i64 },
    /// No assessment covers the unit (yet): activation may assess, never implement.
    Missing,
    /// Scope or evidence changed since the assessment: reassess before implementation.
    Stale { assessment_revision: i64 },
    /// An unresolved blocking finding gates the unit.
    Blocked { finding: String },
}
impl Readiness {
    pub fn is_current(&self) -> bool {
        matches!(self, Self::Current { .. })
    }
    pub fn revision(&self) -> Option<i64> {
        match self {
            Self::Current {
                assessment_revision,
            }
            | Self::Stale {
                assessment_revision,
            } => Some(*assessment_revision),
            _ => None,
        }
    }
    pub fn describe(&self) -> String {
        match self {
            Self::Current { .. } => "readiness assessment is current".into(),
            Self::Missing => "no readiness assessment covers this scope".into(),
            Self::Stale { .. } => "the readiness assessment is stale for the changed scope".into(),
            Self::Blocked { finding } => format!("readiness finding {finding} is unresolved"),
        }
    }
}

/// One unit of the active approved scope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScopeUnit {
    /// Digest of the unit's approved content: unchanged across a plan replacement means the
    /// work bound to it stays compatible.
    pub digest: String,
    pub readiness: Readiness,
}

/// The execution authority of a root's plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlanState {
    /// A standalone issue: no separate feature plan or readiness gate (AGENT-15).
    NotRequired,
    /// A feature without an active approved plan (initial planning has only a pending plan).
    Unapproved { pending_revision: Option<i64> },
    /// The active approved plan: the feature-wide scope (`feature`) and the named scope units
    /// (`units`, keyed by [`valid_scope_key`] keys), with their approved content and readiness.
    Approved {
        revision: i64,
        pending_revision: Option<i64>,
        feature: ScopeUnit,
        units: BTreeMap<String, ScopeUnit>,
    },
}
impl PlanState {
    pub fn approved_revision(&self) -> Option<i64> {
        match self {
            Self::Approved { revision, .. } => Some(*revision),
            _ => None,
        }
    }
    pub fn pending_revision(&self) -> Option<i64> {
        match self {
            Self::Unapproved { pending_revision }
            | Self::Approved {
                pending_revision, ..
            } => *pending_revision,
            Self::NotRequired => None,
        }
    }
    /// The approved unit of `key` (`None`: the feature-wide scope).
    pub fn unit(&self, key: Option<&str>) -> Option<&ScopeUnit> {
        match (self, key) {
            (Self::Approved { feature, .. }, None) => Some(feature),
            (Self::Approved { units, .. }, Some(k)) => units.get(k),
            _ => None,
        }
    }
}

/// Supplies the active approved scope and readiness of a root, evaluated inside the caller's
/// read snapshot or write transaction so dispatch checks and the state they read are atomic.
pub trait ExecutionScope: Send + Sync {
    fn plan(&self, runs: &dyn RunRead, root: &WorkRoot) -> Result<PlanState, StorageError>;
}

/// Whether a root kind has a feature plan (features) or the issue policy (standalone issues and
/// tasks).
pub fn is_feature(root: &WorkRoot) -> bool {
    root.kind == "feature"
}

/// The production port: approval and scope units from the stored feature plan slot, readiness
/// from the feature's recorded assessment.
#[derive(Clone, Copy, Debug, Default)]
pub struct RecordedScope;
impl ExecutionScope for RecordedScope {
    fn plan(&self, runs: &dyn RunRead, root: &WorkRoot) -> Result<PlanState, StorageError> {
        if !is_feature(root) {
            return Ok(PlanState::NotRequired);
        }
        Ok(match runs.feature_plan(&root.id)? {
            // The planning service's active plan: its scope units and their recorded readiness.
            Some(p)
                if let Some(state) = p.approved_plan.as_ref().and_then(|a| {
                    crate::services::work::readiness::scope_plan(
                        a,
                        p.pending_revision,
                        p.readiness.as_ref(),
                    )
                }) =>
            {
                state
            }
            Some(p) if p.approved => PlanState::Approved {
                revision: p.revision,
                pending_revision: p.pending_revision,
                feature: ScopeUnit {
                    digest: format!("plan-{}", p.revision),
                    readiness: Readiness::Missing,
                },
                units: BTreeMap::new(),
            },
            Some(p) => PlanState::Unapproved {
                pending_revision: p.pending_revision,
            },
            None => PlanState::Unapproved {
                pending_revision: None,
            },
        })
    }
}

/// A deterministic, scripted [`ExecutionScope`]. Roots it has no script for behave as
/// [`RecordedScope`] does.
#[derive(Debug, Default)]
pub struct ScriptedScope {
    plans: Mutex<HashMap<RootId, PlanState>>,
}
impl ScriptedScope {
    pub fn new() -> Self {
        Self::default()
    }
    /// Scripts the plan state of `root`.
    pub fn set(&self, root: &RootId, plan: PlanState) {
        self.plans
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(root.clone(), plan);
    }
    /// Scripts an approved plan whose feature-wide scope and named units are all `Current`
    /// (assessment revision 1).
    pub fn approve(&self, root: &RootId, revision: i64, units: &[(&str, &str)]) {
        self.set(
            root,
            PlanState::Approved {
                revision,
                pending_revision: None,
                feature: ScopeUnit {
                    digest: format!("feature-{revision}"),
                    readiness: Readiness::Current {
                        assessment_revision: 1,
                    },
                },
                units: units
                    .iter()
                    .map(|(k, d)| {
                        (
                            (*k).to_owned(),
                            ScopeUnit {
                                digest: (*d).to_owned(),
                                readiness: Readiness::Current {
                                    assessment_revision: 1,
                                },
                            },
                        )
                    })
                    .collect(),
            },
        );
    }
    /// Changes one unit's readiness (`None`: the feature-wide scope) in the scripted plan.
    pub fn set_readiness(&self, root: &RootId, key: Option<&str>, readiness: Readiness) {
        let mut plans = self.plans.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(PlanState::Approved { feature, units, .. }) = plans.get_mut(root) {
            let unit = match key {
                None => Some(feature),
                Some(k) => units.get_mut(k),
            };
            if let Some(unit) = unit {
                unit.readiness = readiness;
            }
        }
    }
    /// Records a pending revision beside the active plan (it authorizes nothing).
    pub fn set_pending(&self, root: &RootId, pending: Option<i64>) {
        let mut plans = self.plans.lock().unwrap_or_else(|p| p.into_inner());
        match plans.get_mut(root) {
            Some(PlanState::Approved {
                pending_revision, ..
            })
            | Some(PlanState::Unapproved { pending_revision }) => *pending_revision = pending,
            _ => {}
        }
    }
}
impl ExecutionScope for ScriptedScope {
    fn plan(&self, runs: &dyn RunRead, root: &WorkRoot) -> Result<PlanState, StorageError> {
        let scripted = self
            .plans
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&root.id)
            .cloned();
        match scripted {
            Some(p) => Ok(p),
            None => RecordedScope.plan(runs, root),
        }
    }
}
