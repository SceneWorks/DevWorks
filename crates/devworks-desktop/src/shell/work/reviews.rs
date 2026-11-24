//! Code reviews on the work detail. All writes use the same revision-bound operation as agents.
use super::*;
use crate::frame::Negotiate;
use devworks_core::operations::reviews;
use devworks_core::protocol::data::DataScope;
use devworks_core::protocol::records::{
    CodeReviewResolution, PageRequest, ReviewFinding, ReviewRecord,
};
use devworks_core::protocol::results::OperationResult;
use devworks_core::protocol::support::{BoundedText, Limit};

#[derive(Default)]
pub struct ReviewPane {
    records: Vec<ReviewRecord>,
    selected: Option<Id>,
    findings: Vec<ReviewFinding>,
    drafts: BTreeMap<String, String>,
    status: Option<String>,
    reviews_after: Option<devworks_core::protocol::support::Cursor>,
    findings_after: Option<devworks_core::protocol::support::Cursor>,
}

pub(crate) enum ReviewAction {
    List(String, bool),
    Findings(String, Id, bool),
    Decide(String, ReviewFinding, String, bool),
}

pub(crate) fn section(ui: &mut Ui, work: &mut WorkUi, root: &str, actions: &mut Vec<Action>) {
    let pane = work.code_reviews.entry(root.to_owned()).or_default();
    ui.collapsing("Code reviews", |ui| {
        if ui.button("Refresh reviews").clicked() {
            actions.push(Action::CodeReview(ReviewAction::List(root.into(), false)));
        }
        if let Some(status) = &pane.status {
            ui.label(status);
        }
        for record in &pane.records {
            let description = if let Some(local) = &record.local_review {
                format!(
                    "Local task {} · {:?}",
                    local.target.task_id.as_str(),
                    record.state
                )
            } else {
                format!("Pull request review · {:?}", record.state)
            };
            if ui
                .selectable_label(pane.selected.as_ref() == Some(&record.id), description)
                .clicked()
            {
                pane.selected = Some(record.id.clone());
                pane.findings.clear();
                actions.push(Action::CodeReview(ReviewAction::Findings(
                    root.into(),
                    record.id.clone(),
                    false,
                )));
            }
        }
        if pane.reviews_after.is_some() && ui.button("More reviews").clicked() {
            actions.push(Action::CodeReview(ReviewAction::List(root.into(), true)));
        }
        if let Some(record) = pane
            .records
            .iter()
            .find(|record| Some(&record.id) == pane.selected.as_ref())
        {
            if let Some(local) = &record.local_review {
                for diff in &local.target.diffs.0 {
                    ui.label(format!(
                        "{} · {} → {}",
                        diff.repository_id.as_str(),
                        &diff.base.hex[..12],
                        &diff.head.hex[..12]
                    ));
                }
                if let Some(session) = &local.reviewer_session_id {
                    ui.label(format!("Independent reviewer session {}", session.as_str()));
                }
            }
            for source in &record.sources.0 {
                ui.label(format!(
                    "{} · PR #{} · {} · round {} · {:?} · {:?}",
                    source.repository_id.as_str(),
                    source.pull_request_number.0,
                    &source.head.hex[..12],
                    source.cycle.get(),
                    source.source_kind,
                    source.state,
                ));
            }
            if let Some(candidate) = &record.candidate_repair {
                ui.label(format!(
                    "Verified repair {}",
                    &candidate.candidate_head.hex[..12]
                ));
            }
            for finding in &pane.findings {
                ui.separator();
                ui.strong(format!(
                    "{:?} · {}",
                    finding
                        .code_severity
                        .as_ref()
                        .unwrap_or(&devworks_core::protocol::records::CodeReviewSeverity::Minor),
                    finding.summary.0
                ));
                if let Some(location) = &finding.location {
                    ui.label(&location.0);
                }
                if let Some(response) = &finding.response {
                    ui.label(&response.0);
                }
                if let Some(resolution) = &finding.resolution {
                    ui.label(format!("{resolution:?}"));
                }
                let draft = pane
                    .drafts
                    .entry(finding.id.as_str().to_owned())
                    .or_default();
                ui.add(
                    egui::TextEdit::multiline(draft)
                        .hint_text("Response or reason")
                        .desired_rows(2),
                );
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            !draft.trim().is_empty(),
                            egui::Button::new("Record response"),
                        )
                        .clicked()
                    {
                        actions.push(Action::CodeReview(ReviewAction::Decide(
                            root.into(),
                            finding.clone(),
                            draft.clone(),
                            false,
                        )));
                    }
                    if ui
                        .add_enabled(
                            finding.actionable && !draft.trim().is_empty(),
                            egui::Button::new("Mark not actionable"),
                        )
                        .clicked()
                    {
                        actions.push(Action::CodeReview(ReviewAction::Decide(
                            root.into(),
                            finding.clone(),
                            draft.clone(),
                            true,
                        )));
                    }
                });
            }
            if pane.findings_after.is_some() && ui.button("More findings").clicked() {
                actions.push(Action::CodeReview(ReviewAction::Findings(
                    root.into(),
                    record.id.clone(),
                    true,
                )));
            }
        }
    });
}

impl WorkCtl<'_, '_> {
    pub(super) fn code_review_act(&mut self, action: ReviewAction) {
        let Some(project) = self.selected_project.clone() else {
            return;
        };
        match action {
            ReviewAction::List(root, more) => {
                let pane = self.work.code_reviews.entry(root.clone()).or_default();
                let cursor = if more {
                    pane.reviews_after.clone()
                } else {
                    None
                };
                let query_root = root.clone();
                self.query(
                    WorkSlot::CodeReviews(root, more),
                    "Reading code reviews",
                    "reviews.list",
                    move |be| {
                        frame::with(be, Negotiate::Latest, || {
                            Some(In::Reviews(reviews::Input::List(reviews::ListInput {
                                scope: DataScope::Project {
                                    project_id: ProjectId(frame::id(&project)?),
                                },
                                root_id: Some(RootId(frame::id(&query_root)?)),
                                page: PageRequest {
                                    cursor,
                                    limit: Limit(25),
                                },
                            })))
                        })
                    },
                );
            }
            ReviewAction::Findings(root, review, more) => {
                let pane = self.work.code_reviews.entry(root.clone()).or_default();
                let cursor = if more {
                    pane.findings_after.clone()
                } else {
                    None
                };
                self.query(
                    WorkSlot::CodeReviewFindings(root, review.as_str().into(), more),
                    "Reading code review findings",
                    "reviews.findings",
                    move |be| {
                        frame::with(be, Negotiate::Latest, || {
                            Some(In::Reviews(reviews::Input::Findings(
                                reviews::FindingsInput {
                                    review_id: review,
                                    page: PageRequest {
                                        cursor,
                                        limit: Limit(25),
                                    },
                                },
                            )))
                        })
                    },
                );
            }
            ReviewAction::Decide(root, finding, text, dismiss) => {
                let operation = if dismiss {
                    "reviews.adjudicate"
                } else {
                    "reviews.respond"
                };
                let slot =
                    WorkSlot::CodeReviewFindingCommand(root, finding.review_id.as_str().into());
                self.command(slot, "Saving the review finding", operation, move |be| {
                    frame::with(be, Negotiate::Latest, || {
                        Some(In::Reviews(if dismiss {
                            reviews::Input::Adjudicate(reviews::AdjudicateInput {
                                finding_id: finding.id,
                                expected_revision: finding.revision,
                                actionable: false,
                                rationale: BoundedText(text),
                                evidence: finding.evidence,
                                resolution: Some(CodeReviewResolution::NotActionable {}),
                            })
                        } else {
                            reviews::Input::Respond(reviews::RespondInput {
                                finding_id: finding.id,
                                expected_revision: finding.revision,
                                response: BoundedText(text),
                                evidence: finding.evidence,
                            })
                        }))
                    })
                });
            }
        }
    }

    pub(super) fn apply_code_review(
        &mut self,
        slot: WorkSlot,
        reply: Reply,
    ) -> Option<(WorkSlot, Reply)> {
        match (slot, reply) {
            (WorkSlot::CodeReviews(root, more), Reply::Query(reply)) => {
                let pane = self.work.code_reviews.entry(root).or_default();
                match ops::query_result(reply) {
                    Ok((OperationResult::ReviewsList { value }, _)) => {
                        if !more {
                            pane.records.clear();
                        }
                        pane.records.extend(value.items.0);
                        pane.reviews_after = value.next_cursor;
                        pane.status = pane
                            .records
                            .is_empty()
                            .then(|| "No code reviews for this work.".into());
                    }
                    Err(error) => pane.status = Some(error.describe()),
                    _ => {
                        pane.status = Some("DevWorks returned an unexpected review result.".into())
                    }
                }
            }
            (WorkSlot::CodeReviewFindings(root, review, more), Reply::Query(reply)) => {
                let pane = self.work.code_reviews.entry(root).or_default();
                if pane
                    .selected
                    .as_ref()
                    .is_some_and(|id| id.as_str() == review)
                {
                    match ops::query_result(reply) {
                        Ok((OperationResult::ReviewsFindings { value }, _)) => {
                            if !more {
                                pane.findings.clear();
                            }
                            pane.findings.extend(value.items.0);
                            pane.findings_after = value.next_cursor;
                        }
                        Err(error) => pane.status = Some(error.describe()),
                        _ => {
                            pane.status =
                                Some("DevWorks returned an unexpected finding result.".into())
                        }
                    }
                }
            }
            (WorkSlot::CodeReviewFindingCommand(root, review), Reply::Command(reply)) => {
                let pane = self.work.code_reviews.entry(root.clone()).or_default();
                match ops::command_result(reply) {
                    Ok(
                        OperationResult::ReviewsRespond { value }
                        | OperationResult::ReviewsAdjudicate { value },
                    ) => {
                        pane.drafts.remove(value.id.as_str());
                        if let Some(old) = pane.findings.iter_mut().find(|old| old.id == value.id) {
                            *old = value;
                        }
                        pane.status = None;
                        self.code_review_act(ReviewAction::List(root, false));
                    }
                    Err(error) => pane.status = Some(error.describe()),
                    _ => {
                        pane.status = Some(format!("Refresh review {review} to verify the update."))
                    }
                }
            }
            (slot, reply) => return Some((slot, reply)),
        }
        None
    }
}
