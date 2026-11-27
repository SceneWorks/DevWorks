//! Review-round admission and the evidence required for a candidate after the cap.
use crate::protocol::Id;
use crate::protocol::data::GitOid;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ReviewStage {
    LocalTask,
    FinalFeature,
    StandaloneIssue,
}

impl ReviewStage {
    /// A lower setting can reduce the final PR cap. A source change never opens a round.
    pub fn round_limit(self, configured: Option<u8>) -> u8 {
        let maximum = match self {
            Self::LocalTask => return 1,
            Self::FinalFeature => 5,
            Self::StandaloneIssue => 3,
        };
        configured.map_or(maximum, |value| value.clamp(1, maximum))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RoundAdmission {
    Start(u8),
    Existing,
    WaitForOutcome,
    CapReached,
}

/// Already admitted rounds belong to the task or PR across head and policy changes.
/// An exact duplicate reuses the current record, including after restart.
pub fn admit_round(
    stage: ReviewStage,
    configured: Option<u8>,
    admitted: u8,
    same_witness: bool,
    current_round_finished: bool,
) -> RoundAdmission {
    if admitted == 0 {
        RoundAdmission::Start(1)
    } else if same_witness {
        RoundAdmission::Existing
    } else if admitted >= stage.round_limit(configured) {
        RoundAdmission::CapReached
    } else if !current_round_finished {
        RoundAdmission::WaitForOutcome
    } else {
        RoundAdmission::Start(admitted + 1)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum FindingSeverity {
    Blocker,
    Major,
    Minor,
    Informational,
}

impl FindingSeverity {
    fn requires_fix(self) -> bool {
        matches!(self, Self::Blocker | Self::Major)
    }
}

/// Digest of an exact, host-read Git change (old/new path, modes and blob identities).
/// A rename carries both paths inside that digest.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChangeDigest(pub String);

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum RepairReason {
    RequiredCheck { failed_check_id: Id },
    Finding { finding_id: Id },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CandidateRepair {
    pub reviewed_head: GitOid,
    pub candidate_head: GitOid,
    pub diagnosed_failed_checks: HashMap<Id, String>,
    pub changes: BTreeMap<ChangeDigest, RepairReason>,
    pub focused_check_ids: HashSet<Id>,
}

#[derive(Debug)]
pub struct RequiredCheckFailure {
    pub head: GitOid,
    pub required: bool,
}

/// Host-verified test result. Preservation is a review assertion backed by the saved check
/// evidence, never inferred from an unrelated successful workflow.
#[derive(Debug)]
pub struct FocusedCheck {
    pub head: GitOid,
    pub passed: bool,
    pub preserves_product_behavior: bool,
    pub preserves_coverage: bool,
    pub preserves_required_gates: bool,
    pub findings_verified: HashSet<Id>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepairFault {
    CapOpen,
    LocalTask,
    Stale,
    FreshReviewRequired,
    IncompleteDelta,
    InvalidFailedCheck,
    InvalidFinding,
    MissingFocusedVerification,
    MissingFindingVerification,
}

/// Current facts supplied by the service from Git, the stored review and check observations.
/// This validates review evidence. Merge authority, current required CI and approvals remain
/// part of the separate delivery barrier.
pub struct CurrentCandidate<'a> {
    pub stage: ReviewStage,
    pub configured_cap: Option<u8>,
    pub admitted_rounds: u8,
    pub reviewed_head: &'a GitOid,
    pub head: &'a GitOid,
    pub complete_delta: &'a BTreeSet<ChangeDigest>,
    pub failed_checks: &'a HashMap<Id, RequiredCheckFailure>,
    pub actionable_findings: &'a HashMap<Id, FindingSeverity>,
    pub focused_checks: &'a HashMap<Id, FocusedCheck>,
    pub requires_fresh_review: bool,
}

impl CandidateRepair {
    pub fn validate(&self, observed: &CurrentCandidate<'_>) -> Result<(), RepairFault> {
        if observed.stage == ReviewStage::LocalTask {
            return Err(RepairFault::LocalTask);
        }
        if observed.admitted_rounds < observed.stage.round_limit(observed.configured_cap) {
            return Err(RepairFault::CapOpen);
        }
        if observed.requires_fresh_review {
            return Err(RepairFault::FreshReviewRequired);
        }
        if &self.reviewed_head != observed.reviewed_head
            || &self.candidate_head != observed.head
            || self.reviewed_head == self.candidate_head
        {
            return Err(RepairFault::Stale);
        }
        if self.changes.is_empty()
            || self.changes.keys().collect::<BTreeSet<_>>()
                != observed.complete_delta.iter().collect()
        {
            return Err(RepairFault::IncompleteDelta);
        }

        let mut fixes = HashSet::new();
        for reason in self.changes.values() {
            match reason {
                RepairReason::RequiredCheck { failed_check_id } => {
                    let known = observed
                        .failed_checks
                        .get(failed_check_id)
                        .is_some_and(|check| check.required && check.head == self.reviewed_head);
                    let diagnosed = self
                        .diagnosed_failed_checks
                        .get(failed_check_id)
                        .is_some_and(|diagnosis| !diagnosis.trim().is_empty());
                    if !known || !diagnosed {
                        return Err(RepairFault::InvalidFailedCheck);
                    }
                }
                RepairReason::Finding { finding_id } => {
                    if !observed
                        .actionable_findings
                        .get(finding_id)
                        .is_some_and(|severity| severity.requires_fix())
                    {
                        return Err(RepairFault::InvalidFinding);
                    }
                    fixes.insert(finding_id);
                }
            }
        }

        let focused = self
            .focused_check_ids
            .iter()
            .map(|id| observed.focused_checks.get(id))
            .collect::<Option<Vec<_>>>()
            .filter(|checks| !checks.is_empty())
            .ok_or(RepairFault::MissingFocusedVerification)?;
        if focused.iter().any(|check| {
            check.head != self.candidate_head
                || !check.passed
                || !check.preserves_product_behavior
                || !check.preserves_coverage
                || !check.preserves_required_gates
        }) {
            return Err(RepairFault::MissingFocusedVerification);
        }
        if fixes.iter().any(|id| {
            !focused
                .iter()
                .any(|check| check.findings_verified.contains(*id))
        }) {
            return Err(RepairFault::MissingFindingVerification);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowering_the_cap_holds_all_sources_at_the_same_round_limit() {
        assert_eq!(
            admit_round(ReviewStage::FinalFeature, Some(2), 2, false, true),
            RoundAdmission::CapReached
        );
        assert_eq!(
            admit_round(ReviewStage::FinalFeature, Some(2), 2, true, false),
            RoundAdmission::Existing
        );
        assert_eq!(
            admit_round(ReviewStage::FinalFeature, Some(50), 4, false, true),
            RoundAdmission::Start(5)
        );
        assert_eq!(
            admit_round(ReviewStage::LocalTask, None, 1, false, true),
            RoundAdmission::CapReached
        );
    }

    #[test]
    fn post_cap_repairs_must_cover_the_exact_current_candidate_and_preserve_gates() {
        use crate::protocol::data::ObjectFormat;
        let head = |value: &str| GitOid {
            format: ObjectFormat::Sha1,
            hex: value.repeat(40),
        };
        let reviewed = head("a");
        let candidate = head("b");
        let failure = Id::new("failed-check").unwrap();
        let focused = Id::new("focused-check").unwrap();
        let change = ChangeDigest("change-digest".into());
        let other_change = ChangeDigest("unclassified-change".into());
        let complete = BTreeSet::from([change.clone()]);
        let incomplete = BTreeSet::from([change.clone(), other_change]);
        let failed_checks = HashMap::from([(
            failure.clone(),
            RequiredCheckFailure {
                head: reviewed.clone(),
                required: true,
            },
        )]);
        let focus = |preserves_required_gates| {
            HashMap::from([(
                focused.clone(),
                FocusedCheck {
                    head: candidate.clone(),
                    passed: true,
                    preserves_product_behavior: true,
                    preserves_coverage: true,
                    preserves_required_gates,
                    findings_verified: HashSet::new(),
                },
            )])
        };
        let green = focus(true);
        let weakened = focus(false);
        let repair = CandidateRepair {
            reviewed_head: reviewed.clone(),
            candidate_head: candidate.clone(),
            diagnosed_failed_checks: HashMap::from([(failure.clone(), "fixture failure".into())]),
            changes: BTreeMap::from([(
                change,
                RepairReason::RequiredCheck {
                    failed_check_id: failure,
                },
            )]),
            focused_check_ids: HashSet::from([focused.clone()]),
        };
        let mut current = CurrentCandidate {
            stage: ReviewStage::FinalFeature,
            configured_cap: Some(2),
            admitted_rounds: 2,
            reviewed_head: &reviewed,
            head: &candidate,
            complete_delta: &complete,
            failed_checks: &failed_checks,
            actionable_findings: &HashMap::new(),
            focused_checks: &green,
            requires_fresh_review: false,
        };
        assert_eq!(repair.validate(&current), Ok(()));
        current.complete_delta = &incomplete;
        assert_eq!(repair.validate(&current), Err(RepairFault::IncompleteDelta));
        current.complete_delta = &complete;
        current.focused_checks = &weakened;
        assert_eq!(
            repair.validate(&current),
            Err(RepairFault::MissingFocusedVerification)
        );
        current.focused_checks = &green;
        current.requires_fresh_review = true;
        assert_eq!(
            repair.validate(&current),
            Err(RepairFault::FreshReviewRequired)
        );
    }
}
