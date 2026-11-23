//! Feature and card detail: plans and their requirement editor, readiness, blockers, feature
//! membership, acceptance, assignment, run controls, history and source control.
use crate::frame;
use crate::theme;
use crate::widgets::{self, Tone, chip};
use crate::work_ops as wops;
use devworks_core::operations::{OperationInput as In, features, tasks};
use devworks_core::protocol::helpers::work::MAX_PREREQUISITES;
use devworks_core::protocol::records::{
    ApprovalState, CriterionState, DependencyEdge, DependencyReplacement, FeaturePatch,
    FindingState, ReadinessDisposition, ReadinessFreshness, RequirementStatus, WorkKind,
    WorkRecord,
};
use devworks_core::protocol::{AssignmentId, Event, Id};
use eframe::egui::{self, RichText, Ui};
use std::collections::{BTreeMap, BTreeSet};

use super::*;
fn run_controls(
    ui: &mut Ui,
    work: &WorkUi,
    w: &WorkRecord,
    enabled: bool,
    actions: &mut Vec<Action>,
) {
    if let Some(s) = work.runs.get(w.id.as_str()).cloned() {
        let held = s.blockers.items.iter().any(|b| wops::manual_hold(&b.cause));
        widgets::caption(
            ui,
            format!(
                "Execution: {:?}{}",
                s.root.desired_state,
                if held {
                    " · paused until you continue it"
                } else {
                    ""
                }
            ),
        );
        ui.horizontal(|ui| {
            if w.parent_id.is_none()
                && ui
                    .add_enabled(enabled, egui::Button::new("Start"))
                    .clicked()
            {
                actions.push(Action::Run("runs.start", w.clone()));
            }
            if ui
                .add_enabled(enabled, egui::Button::new("Pause"))
                .clicked()
            {
                actions.push(Action::Run("runs.pause", w.clone()));
            }
            if ui
                .add_enabled(enabled && held, egui::Button::new("Continue"))
                .clicked()
            {
                actions.push(Action::Run("runs.continue", w.clone()));
            }
        });
    } else if w.parent_id.is_none() && ui.button("Show run controls").clicked() {
        actions.push(Action::Inspect(w.id.as_str().to_owned()));
    }
}

/// Feature list tabs and Issues tabs.
use crate::shell::work::*;

#[allow(clippy::too_many_arguments)]
pub(crate) fn feature_detail(
    ui: &mut Ui,
    work: &mut WorkUi,
    f: &WorkRecord,
    cols: &[Column],
    project_repos: &[String],
    enabled: bool,
    actions: &mut Vec<Action>,
    watchers: &mut dyn FnMut(&mut Ui),
) {
    egui::ScrollArea::vertical()
        .id_salt("feature-detail")
        .show(ui, |ui| {
            ui.label(RichText::new(&f.title.0).strong());
            if let Some(budget) = &f.budget {
                crate::usage_screen::budget_status(ui, budget);
            } else {
                widgets::caption(
                    ui,
                    "Feature accounting is unavailable; the effective spending policy is \
            unknown.",
                );
            }
            widgets::caption(
                ui,
                format!(
                    "Revision {} · {}",
                    f.revision.as_str(),
                    status_name(cols, f.status_id.as_str())
                ),
            );
            metadata_summary(ui, f);
            if !f.description.0.is_empty() {
                widgets::text_with_github_links(ui, &f.description.0);
            }
            workflow::next_task(ui, work, f.id.as_str(), enabled, actions);
            run_controls(ui, work, f, enabled, actions);
            source_control(ui, work, f.id.as_str(), actions);
            if f.id.as_str().starts_with("import-") && ui.button("Imported work history").clicked()
            {
                actions.push(Action::OpenImportedHistory(f.id.as_str().to_owned(), false));
            }
            ui.add_space(6.0);
            // Approval and readiness are separate facts, shown apart.
            chip(
                ui,
                approval_label(f),
                if f.approval.is_some() {
                    Tone::Good
                } else {
                    Tone::Attention
                },
            );
            chip(
                ui,
                readiness_label(f),
                if f.readiness.as_ref().is_some_and(|r| r.ready) {
                    Tone::Good
                } else {
                    Tone::Attention
                },
            );
            ui.add_space(6.0);
            // Status (the feature's workflow column; not its execution).
            let mut target = f.status_id.as_str().to_owned();
            egui::ComboBox::from_id_salt(("feature-status", f.id.as_str()))
                .selected_text(status_name(cols, &target))
                .show_ui(ui, |ui| {
                    for c in cols {
                        ui.selectable_value(&mut target, c.id.clone(), &c.name);
                    }
                })
                .response
                .widget_info(|| {
                    egui::WidgetInfo::labeled(egui::Role::ComboBox, enabled, "Feature status")
                });
            if target != f.status_id.as_str()
                && enabled
                && let Some(input) = feature_status_request(f, &target)
            {
                actions.push(Action::Command(f.id.as_str().to_owned(), input));
            }
            edit_section(ui, work, f, enabled, actions);
            ui.add_space(8.0);
            ui.label(RichText::new("Plan").strong());
            match (&f.plan, &f.approval) {
                (Some(_), Some(a)) => widgets::caption(
                    ui,
                    format!(
                        "Active plan (approved, revision {})",
                        a.plan_revision.as_str()
                    ),
                ),
                _ => widgets::caption(
                    ui,
                    "No approved plan yet: implementation is not authorized.",
                ),
            }
            for r in &f.requirements.0 {
                ui.label(format!("{}: {}", r.id.as_str(), r.statement.0));
            }
            for a in &f.acceptance.0 {
                ui.label(format!("Acceptance {}: {}", a.id.as_str(), a.text.0));
            }
            if !f.repository_ids.0.is_empty() {
                widgets::caption(
                    ui,
                    format!(
                        "Repositories: {}",
                        f.repository_ids
                            .0
                            .iter()
                            .map(Id::as_str)
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                );
            }
            if let Some(p) = &f.pending_plan {
                ui.add_space(4.0);
                ui.label(
                    RichText::new(format!("Pending revision {}", p.revision.as_str())).strong(),
                );
                if let Some(budget) = &p.budget {
                    crate::usage_screen::budget_choice(ui, budget);
                }
                for r in &p.draft.requirements.0 {
                    ui.label(format!("{}: {}", r.id.as_str(), r.statement.0));
                }
                let requested = p
                    .approval
                    .as_ref()
                    .is_some_and(|a| matches!(a.state, ApprovalState::Pending));
                if requested {
                    widgets::caption(ui, "Approval requested: decide it in Approvals.");
                } else if ui
                    .add_enabled(
                        enabled && p.plan_digest.is_some(),
                        egui::Button::new("Request approval"),
                    )
                    .clicked()
                {
                    actions.push(Action::RequestApproval(f.clone()));
                }
            }
            ui.push_id(("feature-plan", f.id.as_str()), |ui| {
                plan_form(ui, work, f, project_repos, enabled, actions);
            });
            ui.add_space(8.0);
            ui.label(RichText::new("Readiness").strong());
            settled_finding_history(ui, work, f.id.as_str(), actions);
            if let Some(p) = &f.pending_readiness {
                let open = p
                    .findings
                    .0
                    .iter()
                    .filter(|x| x.blocking && matches!(x.state, FindingState::Open))
                    .count();
                widgets::caption(
                    ui,
                    format!(
                        "Readiness for pending draft {} · {open} open blocking finding(s). \
                        It takes effect when the draft is approved.",
                        p.plan_revision.as_str()
                    ),
                );
                if !p.summary.0.is_empty() {
                    ui.label(&p.summary.0);
                }
            }
            match &f.readiness {
                None => widgets::caption(
                    ui,
                    "No readiness assessment yet. The coordinator assesses readiness before \
                implementation.",
                ),
                Some(r) => {
                    widgets::caption(
                        ui,
                        format!(
                            "Readiness for plan revision {}{} · {}",
                            r.plan_revision.as_str(),
                            if r.reviewed_pending {
                                " (pending draft)"
                            } else {
                                ""
                            },
                            match r.freshness {
                                ReadinessFreshness::Current => "current",
                                ReadinessFreshness::Stale => "stale for changed scope",
                            }
                        ),
                    );
                    if !r.summary.0.is_empty() {
                        ui.label(&r.summary.0);
                    }
                    // Remaining findings together, then settled ones.
                    for open in [true, false] {
                        for x in r
                            .findings
                            .0
                            .iter()
                            .filter(|x| matches!(x.state, FindingState::Open) == open)
                        {
                            egui::Frame::new()
                                .stroke(theme::soft_line())
                                .corner_radius(theme::radius())
                                .inner_margin(theme::margin(8, 6))
                                .show(ui, |ui| {
                                    ui.label(
                                        RichText::new(format!(
                                            "{} · {} · {}{}",
                                            x.id.as_str(),
                                            match x.disposition {
                                                ReadinessDisposition::ResolvedDecision =>
                                                    "Resolved decision",
                                                ReadinessDisposition::BoundedInvestigation =>
                                                    "Bounded investigation",
                                                ReadinessDisposition::NeedsUserDecision =>
                                                    "Needs your decision",
                                            },
                                            match x.state {
                                                FindingState::Open => "open",
                                                FindingState::Resolved => "resolved",
                                                FindingState::RiskAccepted => "risk accepted",
                                            },
                                            if x.blocking { " · blocking" } else { "" }
                                        ))
                                        .strong(),
                                    );
                                    ui.label(&x.summary.0);
                                    if !x.requirement_ids.0.is_empty() {
                                        widgets::caption(
                                            ui,
                                            format!(
                                                "Requirements: {}",
                                                x.requirement_ids
                                                    .0
                                                    .iter()
                                                    .map(Id::as_str)
                                                    .collect::<Vec<_>>()
                                                    .join(", ")
                                            ),
                                        );
                                    }
                                    if !x.affected_work_ids.0.is_empty() {
                                        widgets::caption(
                                            ui,
                                            format!(
                                                "Gates {} item(s) of work",
                                                x.affected_work_ids.0.len()
                                            ),
                                        );
                                    }
                                    for (l, v) in [
                                        ("Missing", &x.missing.0),
                                        ("Consequence", &x.consequence.0),
                                        ("Why not resolved autonomously", &x.insufficiency.0),
                                        ("Closes when", &x.closure_condition.0),
                                    ] {
                                        if !v.is_empty() {
                                            widgets::caption(ui, format!("{l}: {v}"));
                                        }
                                    }
                                    for a in &x.attempts.0 {
                                        widgets::caption(ui, format!("Tried: {}", a.0));
                                    }
                                    for (l, v) in [
                                        ("Decision", &x.decision),
                                        ("Objective", &x.objective),
                                        ("Limits", &x.limits),
                                        ("Question", &x.question),
                                        ("Recommendation", &x.recommendation),
                                        ("Closure", &x.closure_note),
                                    ] {
                                        if let Some(v) = v {
                                            widgets::caption(ui, format!("{l}: {}", v.0));
                                        }
                                    }
                                    if let Some(c) = &x.confidence {
                                        widgets::caption(ui, format!("Confidence: {c:?}"));
                                    }
                                    if !x.evidence.0.is_empty() || !x.closure_evidence.0.is_empty()
                                    {
                                        widgets::caption(
                                            ui,
                                            format!(
                                                "Evidence: {} item(s)",
                                                x.evidence.0.len() + x.closure_evidence.0.len()
                                            ),
                                        );
                                    }
                                });
                        }
                    }
                }
            }
            ui.add_space(8.0);
            let tasks = work
                .cards
                .items
                .iter()
                .filter(|c| c.parent_id.as_ref() == Some(&f.id))
                .count();
            widgets::caption(ui, format!("{tasks} task(s) on its board"));
            if ui.button("Show its activity").clicked() {
                actions.push(Action::ShowActivity(f.id.as_str().to_owned()));
            }
            ui.add_space(8.0);
            ui.label(RichText::new("Watchers").strong());
            watchers(ui);
        });
}

pub(crate) fn edit_section(
    ui: &mut Ui,
    work: &mut WorkUi,
    w: &WorkRecord,
    enabled: bool,
    actions: &mut Vec<Action>,
) {
    let editing = work.edit.as_ref().is_some_and(|e| e.snapshot.id == w.id);
    if !editing {
        if ui.add_enabled(enabled, egui::Button::new("Edit")).clicked() {
            work.edit = Some(EditForm {
                snapshot: w.clone(),
                title: w.title.0.clone(),
                description: w.description.0.clone(),
                priority: w.priority,
                task_type: task_type_of(w),
                labels: labels_of(w),
                budget: crate::usage_screen::BudgetDraft::from_view(w.budget.as_ref()),
                budget_changed: false,
            });
        }
        return;
    }
    let form = work.edit.as_mut().expect("editing");
    if form.snapshot.revision != w.revision {
        // The draft keeps the revision it was opened on: saving it is refused as a conflict.
        widgets::caption(
            ui,
            "This changed since you began editing; saving will be refused. Your draft is kept.",
        );
    }
    let r = ui.add(egui::TextEdit::singleline(&mut form.title));
    r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Work title"));
    let r = ui.add(egui::TextEdit::multiline(&mut form.description).desired_rows(3));
    r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Work description"));
    metadata_fields(
        ui,
        !matches!(w.kind, WorkKind::Feature),
        &mut form.task_type,
        &mut form.labels,
    );
    if matches!(w.kind, WorkKind::Feature) {
        form.budget_changed |= form.budget.ui(ui);
    }
    if !matches!(w.kind, WorkKind::Feature) {
        ui.horizontal(|ui| {
            ui.label("Priority");
            ui.add(egui::DragValue::new(&mut form.priority).range(-1000..=1000))
                .widget_info(|| {
                    egui::WidgetInfo::labeled(egui::Role::SpinButton, true, "Priority")
                });
        });
    }
    let mut cancel = false;
    ui.horizontal(|ui| {
        if ui
            .add_enabled(
                enabled && (!matches!(w.kind, WorkKind::Feature) || form.budget.input().is_ok()),
                egui::Button::new("Save"),
            )
            .clicked()
        {
            actions.push(Action::Edit(form.clone()));
        }
        if widgets::secondary_button(ui, "Cancel").clicked() {
            cancel = true;
        }
    });
    if cancel {
        work.edit = None;
    }
}

pub(crate) fn plan_form(
    ui: &mut Ui,
    work: &mut WorkUi,
    f: &WorkRecord,
    project_repos: &[String],
    enabled: bool,
    actions: &mut Vec<Action>,
) {
    let open = work.plan.as_ref().is_some_and(|p| p.snapshot.id == f.id);
    if !open {
        if ui
            .add_enabled(enabled, egui::Button::new("Edit plan"))
            .clicked()
        {
            work.plan = Some(open_plan_form(f));
        }
        return;
    }
    let form = work.plan.as_mut().expect("open");
    widgets::caption(
        ui,
        "Saving makes this the pending revision; the active plan stays in force \
            until it is approved.",
    );
    let mut propose = form.budget.is_some();
    if ui
        .checkbox(&mut propose, "Propose a spending cap change with this plan")
        .changed()
    {
        form.budget =
            propose.then(|| crate::usage_screen::BudgetDraft::from_view(f.budget.as_ref()));
    }
    if let Some(budget) = &mut form.budget {
        budget.ui(ui);
    }
    let r = ui.add(
        egui::TextEdit::multiline(&mut form.text)
            .hint_text("Plan text (leave empty to keep the current text)")
            .desired_rows(4),
    );
    r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Plan text"));
    requirement_editor(ui, form);
    entry_list(ui, "Acceptance criterion", "A", &mut form.acceptance, &[]);
    entry_list(ui, "Acceptance test", "T", &mut form.acceptance_tests, &[]);
    ui.label(RichText::new("Goals").strong());
    let mut remove: Option<usize> = None;
    for (i, g) in form.goals.iter_mut().enumerate() {
        ui.horizontal(|ui| {
            let r = ui.add(egui::TextEdit::multiline(g).desired_rows(1));
            r.widget_info(|| {
                egui::WidgetInfo::labeled(egui::Role::TextInput, true, format!("Goal {}", i + 1))
            });
            if ui.small_button(format!("Remove goal {}", i + 1)).clicked() {
                remove = Some(i);
            }
        });
    }
    if let Some(i) = remove {
        form.goals.remove(i);
    }
    if ui.small_button("Add goal").clicked() {
        form.goals.push(String::new());
    }
    for repo in project_repos {
        let mut on = form.repositories.contains(repo);
        if ui.checkbox(&mut on, repo).changed() {
            if on {
                form.repositories.insert(repo.clone());
            } else {
                form.repositories.remove(repo);
            }
        }
    }
    if form.text.is_empty() && form.content.is_some() {
        form.original = String::new();
    }
    let mut cancel = false;
    let error = plan_error(form);
    if let Some(error) = &error {
        widgets::caption(ui, error);
    }
    ui.horizontal(|ui| {
        let ok =
            enabled && error.is_none() && (form.content.is_some() || !form.text.trim().is_empty());
        if ui
            .add_enabled(ok, egui::Button::new("Save plan draft"))
            .clicked()
        {
            actions.push(Action::SavePlan(form.clone()));
        }
        if widgets::secondary_button(ui, "Cancel").clicked() {
            cancel = true;
        }
    });
    if cancel {
        work.plan = None;
    }
}

/// Refuse unsupported omissions before submitting, preserving the whole draft.
pub fn plan_error(form: &PlanForm) -> Option<String> {
    if let Some(budget) = &form.budget
        && let Err(error) = budget.input()
    {
        return Some(error);
    }
    if form.snapshot.approval.is_some() {
        for r in &form.snapshot.requirements.0 {
            match form.requirements.iter().find(|e| e.id == r.id.as_str()) {
                None if !form
                    .base
                    .requirements
                    .iter()
                    .any(|b| b.id == r.id && matches!(b.status, RequirementStatus::Dropped)) =>
                {
                    return Some(format!(
                        "Requirement {} belongs to the approved plan; drop it with a decision.",
                        r.id.as_str()
                    ));
                }
                Some(e) if e.text.trim().is_empty() && !form.drops.contains_key(&e.id) => {
                    return Some(format!(
                        "Requirement {} needs its statement, or an explicit drop decision.",
                        r.id.as_str()
                    ));
                }
                _ => {}
            }
        }
    }
    form.drops
        .iter()
        .find(|(_, d)| d.artifact.is_none() && d.reason.trim().is_empty())
        .map(|(id, _)| format!("Give a decision to drop requirement {id}."))
}

pub(crate) fn requirement_editor(ui: &mut Ui, form: &mut PlanForm) {
    ui.label(RichText::new("Requirements").strong());
    let approved: BTreeSet<_> = if form.snapshot.approval.is_some() {
        form.snapshot
            .requirements
            .0
            .iter()
            .map(|r| r.id.as_str().to_owned())
            .collect()
    } else {
        BTreeSet::new()
    };
    let mut remove = None;
    for (i, entry) in form.requirements.iter_mut().enumerate() {
        let dropped = form.drops.contains_key(&entry.id);
        let previous_id = entry.id.clone();
        ui.horizontal(|ui| {
            let r = ui.add_enabled(
                !approved.contains(&entry.id) && !dropped,
                egui::TextEdit::singleline(&mut entry.id).desired_width(56.0),
            );
            r.widget_info(|| {
                egui::WidgetInfo::labeled(
                    egui::Role::TextInput,
                    !approved.contains(&entry.id) && !dropped,
                    format!("Requirement {} ID", i + 1),
                )
            });
            if dropped {
                if ui
                    .small_button(format!("Keep requirement {}", entry.id))
                    .clicked()
                {
                    form.drops.remove(&entry.id);
                }
            } else if approved.contains(&entry.id) {
                if ui
                    .small_button(format!("Drop requirement {}", entry.id))
                    .clicked()
                {
                    let artifact = form
                        .base
                        .requirements
                        .iter()
                        .find(|r| {
                            r.id.as_str() == entry.id
                                && matches!(r.status, RequirementStatus::Dropped)
                        })
                        .and_then(|r| r.drop_decision.clone());
                    form.drops.insert(
                        entry.id.clone(),
                        DropDraft {
                            reason: String::new(),
                            artifact,
                        },
                    );
                }
            } else if ui
                .small_button(format!("Remove requirement {}", i + 1))
                .clicked()
            {
                remove = Some(i);
            }
        });
        let r = ui.add_enabled(
            !form.drops.contains_key(&entry.id),
            egui::TextEdit::multiline(&mut entry.text)
                .desired_rows(2)
                .desired_width(f32::INFINITY),
        );
        r.widget_info(|| {
            egui::WidgetInfo::labeled(
                egui::Role::TextInput,
                !form.drops.contains_key(&entry.id),
                format!("Requirement {} text", i + 1),
            )
        });
        if previous_id != entry.id
            && let Some(status) = form.statuses.remove(&previous_id)
        {
            form.statuses.insert(entry.id.clone(), status);
        }
        if !form.drops.contains_key(&entry.id) {
            let original = form
                .base
                .requirements
                .iter()
                .find(|r| r.id.as_str() == entry.id);
            let status = form
                .statuses
                .get(&entry.id)
                .cloned()
                .or_else(|| original.map(|r| r.status.clone()))
                .unwrap_or(RequirementStatus::Confirmed);
            let mut proposed = matches!(
                status,
                RequirementStatus::Proposed | RequirementStatus::Dropped
            );
            // A restoration whose old status is no longer known defaults to Proposed and
            // visibly offers both choices; it never silently promotes an old proposal.
            if matches!(status, RequirementStatus::Dropped)
                && let Some(r) = form.snapshot.requirements.0.iter().find(|r| {
                    r.id.as_str() == entry.id && !matches!(r.status, RequirementStatus::Dropped)
                })
            {
                proposed = matches!(r.status, RequirementStatus::Proposed);
            }
            let before = proposed;
            egui::ComboBox::from_id_salt(("requirement-status", &entry.id))
                .selected_text(if proposed { "Proposed" } else { "Confirmed" })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut proposed, true, "Proposed");
                    ui.selectable_value(&mut proposed, false, "Confirmed");
                })
                .response
                .widget_info(|| {
                    egui::WidgetInfo::labeled(
                        egui::Role::ComboBox,
                        true,
                        format!("Status of requirement {}", entry.id),
                    )
                });
            if before != proposed {
                form.statuses.insert(
                    entry.id.clone(),
                    if proposed {
                        RequirementStatus::Proposed
                    } else {
                        RequirementStatus::Confirmed
                    },
                );
            }
        }
        if let Some(drop) = form.drops.get_mut(&entry.id) {
            if drop.artifact.is_some() {
                widgets::caption(
                    ui,
                    "The recorded decision is preserved unless you write a replacement.",
                );
            }
            let r = ui.add(
                egui::TextEdit::multiline(&mut drop.reason)
                    .desired_rows(2)
                    .hint_text("Why is this requirement being dropped?"),
            );
            r.widget_info(|| {
                egui::WidgetInfo::labeled(
                    egui::Role::TextInput,
                    true,
                    format!("Decision to drop {}", entry.id),
                )
            });
            if r.changed() {
                drop.artifact = if drop.reason.trim().is_empty() {
                    form.base
                        .requirements
                        .iter()
                        .find(|r| {
                            r.id.as_str() == entry.id
                                && matches!(r.status, RequirementStatus::Dropped)
                        })
                        .and_then(|r| r.drop_decision.clone())
                } else {
                    None
                };
            }
        }
    }
    if let Some(i) = remove {
        let e = form.requirements.remove(i);
        form.drops.remove(&e.id);
        form.statuses.remove(&e.id);
    }
    let hidden: Vec<_> = form
        .base
        .requirements
        .iter()
        .filter(|r| {
            matches!(r.status, RequirementStatus::Dropped)
                && !form.requirements.iter().any(|e| e.id == r.id.as_str())
        })
        .cloned()
        .collect();
    if !hidden.is_empty() {
        ui.collapsing("Dropped requirements", |ui| {
            for r in hidden {
                ui.label(format!("{}: {}", r.id.as_str(), r.statement.0));
                if ui
                    .small_button(format!("Restore requirement {}", r.id.as_str()))
                    .clicked()
                {
                    form.requirements.push(Entry {
                        id: r.id.as_str().to_owned(),
                        text: r.statement.0,
                    });
                }
            }
        });
    }
    if !approved.is_empty() {
        widgets::caption(
            ui,
            "Approved requirement IDs stay stable. Dropping one records a decision \
                with the pending draft.",
        );
    }
    if ui.small_button("Add requirement").clicked() {
        let reserved: Vec<_> = form
            .base
            .requirements
            .iter()
            .map(|r| r.id.as_str().to_owned())
            .collect();
        let id = next_id(&form.requirements, "E", &reserved);
        form.requirements.push(Entry {
            id,
            text: String::new(),
        });
    }
}

/// One change to a card's blockers.
#[derive(Clone, Copy, Debug)]
pub enum BlockerEdit<'a> {
    Add(&'a str),
    Remove(&'a str),
}

/// The `tasks.dependency_links` command applying one blocker change to `w`, at its revision.
/// The operation replaces the whole incoming list, so the change is applied to the record's
/// `dependencyIds`, which is complete: no writer stores more than [`MAX_PREREQUISITES`], the
/// projection's own bound. `None` when the change changes nothing or would exceed the bound.
pub fn blocker_change(w: &WorkRecord, edit: BlockerEdit<'_>) -> Option<In> {
    let mut ids: Vec<Id> = w.dependency_ids.0.clone();
    match edit {
        BlockerEdit::Add(a)
            if !ids.iter().any(|d| d.as_str() == a) && ids.len() < MAX_PREREQUISITES =>
        {
            ids.push(frame::id(a)?)
        }
        BlockerEdit::Remove(r) if ids.iter().any(|d| d.as_str() == r) => {
            ids.retain(|d| d.as_str() != r)
        }
        _ => return None,
    }
    let edges = ids
        .into_iter()
        .map(|predecessor| DependencyEdge {
            predecessor,
            successor: w.id.clone(),
        })
        .collect();
    Some(In::Tasks(tasks::Input::DependencyLinks(
        tasks::DependencyLinksInput {
            task_id: w.id.clone(),
            expected_revision: w.revision.clone(),
            replacement: DependencyReplacement {
                edges: frame::bounded(edges)?,
            },
        },
    )))
}

/// The `features.edit` command moving feature `f` to workflow status `target`.
pub(crate) fn feature_status_request(f: &WorkRecord, target: &str) -> Option<In> {
    Some(In::Features(features::Input::Edit(features::EditInput {
        work_id: f.id.clone(),
        expected_revision: f.revision.clone(),
        changes: FeaturePatch {
            status_id: Some(frame::id(target)?),
            ..wops::empty()?
        },
    })))
}
/// The `tasks.set_feature` command putting `w` in feature `f`.
pub(crate) fn set_feature_request(w: &WorkRecord, f: &str) -> Option<In> {
    Some(In::Tasks(tasks::Input::SetFeature(
        tasks::SetFeatureInput {
            task_id: w.id.clone(),
            expected_revision: w.revision.clone(),
            feature_id: Some(frame::text(f)?),
            clear_feature: false,
        },
    )))
}
/// The `tasks.assign` command recording `assignment` on `w`, or clearing its assignment.
pub(crate) fn assign_request(w: &WorkRecord, assignment: Option<&AssignmentId>) -> In {
    In::Tasks(tasks::Input::Assign(tasks::AssignInput {
        task_id: w.id.clone(),
        expected_revision: w.revision.clone(),
        assignment_id: assignment.cloned(),
        clear_assignment: assignment.is_none(),
    }))
}

/// An editable list of entries, one ID field and one multi-line text field each, with add and
/// remove controls.
pub(crate) fn entry_list(
    ui: &mut Ui,
    what: &str,
    prefix: &str,
    entries: &mut Vec<Entry>,
    reserved: &[String],
) {
    ui.label(RichText::new(format!("{what}s")).strong());
    let mut remove: Option<usize> = None;
    for (i, e) in entries.iter_mut().enumerate() {
        ui.horizontal(|ui| {
            let r = ui.add(egui::TextEdit::singleline(&mut e.id).desired_width(56.0));
            r.widget_info(|| {
                egui::WidgetInfo::labeled(
                    egui::Role::TextInput,
                    true,
                    format!("{what} {} ID", i + 1),
                )
            });
            if ui
                .small_button(format!("Remove {} {}", what.to_lowercase(), i + 1))
                .clicked()
            {
                remove = Some(i);
            }
        });
        let r = ui.add(
            egui::TextEdit::multiline(&mut e.text)
                .desired_rows(2)
                .desired_width(f32::INFINITY),
        );
        r.widget_info(|| {
            egui::WidgetInfo::labeled(
                egui::Role::TextInput,
                true,
                format!("{what} {} text", i + 1),
            )
        });
    }
    if let Some(i) = remove {
        entries.remove(i);
    }
    if ui
        .small_button(format!("Add {}", what.to_lowercase()))
        .clicked()
    {
        let id = next_id(entries, prefix, reserved);
        entries.push(Entry {
            id,
            text: String::new(),
        });
    }
}

pub(crate) fn card_detail(
    ui: &mut Ui,
    work: &mut WorkUi,
    w: &WorkRecord,
    cols: &[Column],
    enabled: bool,
    actions: &mut Vec<Action>,
) {
    let titles: BTreeMap<String, (String, String)> = work
        .cards
        .items
        .iter()
        .map(|c| {
            (
                c.id.as_str().to_owned(),
                (c.title.0.clone(), c.status_id.as_str().to_owned()),
            )
        })
        .collect();
    let features: Vec<(String, String)> = work
        .features
        .items
        .iter()
        .map(|f| (f.id.as_str().to_owned(), f.title.0.clone()))
        .collect();
    egui::ScrollArea::vertical()
        .id_salt("card-detail")
        .show(ui, |ui| {
            ui.label(RichText::new(&w.title.0).strong());
            widgets::caption(
                ui,
                format!(
                    "{} · revision {} · priority {}",
                    kind_label(w),
                    w.revision.as_str(),
                    w.priority
                ),
            );
            metadata_summary(ui, w);
            if !w.description.0.is_empty() {
                widgets::text_with_github_links(ui, &w.description.0);
            }
            if matches!(w.kind, WorkKind::Issue) {
                github_issue_controls(ui, work, w, enabled, actions);
            }
            source_control(ui, work, w.id.as_str(), actions);
            if w.id.as_str().starts_with("import-") && ui.button("Imported work history").clicked()
            {
                actions.push(Action::OpenImportedHistory(w.id.as_str().to_owned(), false));
            }
            // Workflow status: moving it never starts, pauses or completes execution.
            let mut target = w.status_id.as_str().to_owned();
            egui::ComboBox::from_id_salt(("card-status", w.id.as_str()))
                .selected_text(status_name(cols, &target))
                .show_ui(ui, |ui| {
                    for c in cols {
                        ui.selectable_value(&mut target, c.id.clone(), &c.name);
                    }
                })
                .response
                .widget_info(|| egui::WidgetInfo::labeled(egui::Role::ComboBox, enabled, "Status"));
            if target != w.status_id.as_str() && enabled {
                actions.push(Action::Move(w.clone(), target));
            }
            let semantic = semantic_of(cols, w.status_id.as_str());
            if semantic == "Done" {
                let verified = !w.acceptance.0.is_empty()
                    && w.acceptance.0.iter().all(|c| {
                        matches!(c.state, CriterionState::Met) && !c.evidence_ids.0.is_empty()
                    });
                chip(
                    ui,
                    if verified {
                        "Completed with verified acceptance evidence"
                    } else {
                        "Completed manually (no verified evidence)"
                    },
                    if verified {
                        Tone::Good
                    } else {
                        Tone::Attention
                    },
                );
            }
            edit_section(ui, work, w, enabled, actions);
            ui.add_space(6.0);
            if let Some(origin) = &w.previous_inheritance {
                widgets::caption(
                    ui,
                    format!(
                        "Earlier requirements from feature {}: {}",
                        origin.feature_id.as_str(),
                        origin
                            .requirement_ids
                            .0
                            .iter()
                            .map(Id::as_str)
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                );
                if ui.button("Open earlier feature").clicked() {
                    actions.push(Action::OpenWork(origin.feature_id.as_str().into()));
                }
            }
            // Membership: link, transfer or remove (remove pauses).
            let current = w
                .parent_id
                .as_ref()
                .and_then(|p| features.iter().find(|(id, _)| id == p.as_str()))
                .map_or("No feature (standalone issue)".to_owned(), |(_, t)| {
                    t.clone()
                });
            let mut chosen = w.parent_id.as_ref().map(|p| p.as_str().to_owned());
            egui::ComboBox::from_id_salt(("card-feature", w.id.as_str()))
                .selected_text(current)
                .show_ui(ui, |ui| {
                    for (id, t) in &features {
                        ui.selectable_value(&mut chosen, Some(id.clone()), t);
                    }
                })
                .response
                .widget_info(|| {
                    egui::WidgetInfo::labeled(egui::Role::ComboBox, enabled, "Feature membership")
                });
            if chosen != w.parent_id.as_ref().map(|p| p.as_str().to_owned())
                && enabled
                && let Some(f) = chosen
                && let Some(input) = set_feature_request(w, &f)
            {
                actions.push(Action::Command(w.id.as_str().to_owned(), input));
            }
            if w.parent_id.is_some()
                && ui
                    .add_enabled(enabled, egui::Button::new("Remove from feature…"))
                    .clicked()
            {
                work.unlink = Some(w.clone());
            }
            ui.add_space(6.0);
            // Blocking links (one stored edge, both views).
            ui.label(RichText::new("Blocked by").strong());
            if w.dependency_ids.0.is_empty() {
                widgets::caption(ui, "Nothing blocks it.");
            }
            for d in &w.dependency_ids.0 {
                let (t, s) = titles
                    .get(d.as_str())
                    .cloned()
                    .unwrap_or((d.as_str().to_owned(), String::new()));
                ui.horizontal(|ui| {
                    ui.label(format!("{t} ({})", status_name(cols, &s)));
                    if ui
                        .add_enabled(
                            enabled,
                            egui::Button::new(format!("Remove blocker {}", short(&t, 16))).small(),
                        )
                        .clicked()
                        && let Some(input) = blocker_change(w, BlockerEdit::Remove(d.as_str()))
                    {
                        actions.push(Action::Command(w.id.as_str().to_owned(), input));
                    }
                });
            }
            let mut add: Option<String> = None;
            let full = w.dependency_ids.0.len() >= MAX_PREREQUISITES;
            if full {
                widgets::caption(
                    ui,
                    "It has the most blockers an item can have; remove one to add another.",
                );
            }
            egui::ComboBox::from_id_salt(("add-blocker", w.id.as_str()))
                .selected_text("Add a blocker…")
                .show_ui(ui, |ui| {
                    for (id, (t, _)) in &titles {
                        if id != w.id.as_str()
                            && !w.dependency_ids.0.iter().any(|d| d.as_str() == id)
                        {
                            ui.selectable_value(&mut add, Some(id.clone()), t);
                        }
                    }
                })
                .response
                .widget_info(|| {
                    egui::WidgetInfo::labeled(
                        egui::Role::ComboBox,
                        enabled && !full,
                        "Add a blocker",
                    )
                });
            if let Some(a) = add
                && enabled
                && let Some(input) = blocker_change(w, BlockerEdit::Add(&a))
            {
                actions.push(Action::Command(w.id.as_str().to_owned(), input));
            }
            ui.label(RichText::new("Blocks").strong());
            if w.dependent_ids.0.is_empty() {
                widgets::caption(ui, "It blocks nothing.");
            }
            for d in &w.dependent_ids.0 {
                let (t, _) = titles
                    .get(d.as_str())
                    .cloned()
                    .unwrap_or((d.as_str().to_owned(), String::new()));
                ui.label(t);
            }
            ui.add_space(6.0);
            ui.label(RichText::new("Acceptance").strong());
            if w.acceptance.0.is_empty() {
                widgets::caption(ui, "No acceptance criteria.");
            }
            for c in &w.acceptance.0 {
                let state = match (&c.state, c.evidence_ids.0.is_empty()) {
                    (CriterionState::Met, false) => "met with evidence",
                    (CriterionState::Met, true) => "met (manual)",
                    _ => "unmet",
                };
                ui.label(format!("{}: {} · {state}", c.id.as_str(), c.text.0));
            }
            ui.add_space(6.0);
            ui.label(RichText::new("Agents").strong());
            match &w.assigned_agent_id {
                Some(a) => ui.label(format!("Assigned to {}", a.as_str())),
                None => ui.label("No agent assigned."),
            };
            if !w.attempts.0.is_empty() {
                widgets::caption(ui, "An agent is working on it now.");
            }
            // The card records the agent working on it through its live assignment, or clears that.
            let working = w.attempts.0.iter().find_map(|a| a.assignment_id.as_ref());
            if w.assigned_agent_id.is_some() {
                if ui
                    .add_enabled(enabled, egui::Button::new("Clear assignment"))
                    .clicked()
                {
                    actions.push(Action::Command(
                        w.id.as_str().to_owned(),
                        assign_request(w, None),
                    ));
                }
            } else if let Some(assignment) = working {
                if ui
                    .add_enabled(enabled, egui::Button::new("Assign to the working agent"))
                    .clicked()
                {
                    actions.push(Action::Command(
                        w.id.as_str().to_owned(),
                        assign_request(w, Some(assignment)),
                    ));
                }
            } else {
                widgets::caption(ui, "An agent can be assigned while one is working on it.");
            }
            run_controls(ui, work, w, enabled, actions);
            ui.add_space(6.0);
            if ui.button("Show its history").clicked() {
                actions.push(Action::ShowActivity(w.id.as_str().to_owned()));
            }
            let feed = work.histories.get(w.id.as_str());
            let history: Vec<&Event> = feed
                .into_iter()
                .flat_map(|feed| feed.items.iter())
                .collect();
            if let Some(feed) = feed {
                if let Some(error) = &feed.error {
                    widgets::caption(ui, error);
                    if ui.button("Reload card history").clicked() {
                        actions.push(Action::RefreshHistory(Some(w.id.as_str().to_owned())));
                    }
                }
                if feed.more()
                    && ui
                        .add_enabled(
                            !feed.loading(),
                            egui::Button::new("Load older card changes"),
                        )
                        .clicked()
                {
                    actions.push(Action::OlderHistory(Some(w.id.as_str().to_owned())));
                }
            }
            egui::ScrollArea::vertical()
                .id_salt(("card-history", w.id.as_str()))
                .max_height(160.0)
                .show(ui, |ui| {
                    for e in history {
                        widgets::caption(ui, activity_line(e, e.actor_id.0.as_str(), "this item"));
                    }
                });
        });
}

fn settled_finding_history(ui: &mut Ui, work: &WorkUi, root: &str, actions: &mut Vec<Action>) {
    let Some(history) = work.readiness_history.get(root) else {
        if ui.button("Show settled findings history").clicked() {
            actions.push(Action::ReadinessHistory(root.to_owned(), false));
        }
        return;
    };
    if let Some(error) = &history.error {
        widgets::caption(ui, error);
    }
    if history.loading() {
        ui.spinner();
    }
    if history.loaded {
        ui.label(RichText::new("Settled findings history").strong());
        for finding in &history.items {
            ui.label(&finding.summary.0);
        }
        if history.items.is_empty() && !history.more() {
            widgets::caption(ui, "No archived findings.");
        }
        if history.more() && ui.button("Load more finding history").clicked() {
            actions.push(Action::ReadinessHistory(root.to_owned(), true));
        }
    }
    if ui.button("Refresh finding history").clicked() {
        actions.push(Action::ReadinessHistory(root.to_owned(), false));
    }
}

pub(crate) fn source_control(ui: &mut Ui, work: &WorkUi, root: &str, actions: &mut Vec<Action>) {
    egui::CollapsingHeader::new("Source control")
        .id_salt(("work-git", root))
        .default_open(true)
        .show(ui, |ui| {
            if ui.small_button("Refresh source control").clicked() {
                actions.push(Action::RefreshGit(root.to_owned()));
            }
            let Some(state) = work.git.get(root) else {
                widgets::caption(ui, "Source control has not been read.");
                return;
            };
            if let Some(error) = &state.error {
                widgets::caption(ui, error);
            }
            if !state.loaded {
                ui.spinner();
                return;
            }
            if state.items.is_empty() {
                widgets::caption(ui, "No recorded Git associations.");
            }
            // Retain the latest event for each resource, while the full event history remains in Activity.
            let mut latest = BTreeMap::new();
            for link in &state.items {
                latest.insert(
                    (
                        link.repository_id.as_str(),
                        link.worktree_id.as_ref().map(|id| id.as_str()),
                        link.pull_request_url.as_ref().map(|u| u.0.as_str()),
                    ),
                    link,
                );
            }
            for link in latest.values() {
                ui.separator();
                if let Some(url) = &link.repository_url {
                    let repo_label = widgets::github_link_label(&url.0)
                        .unwrap_or_else(|| link.repository_id.as_str().to_owned());
                    ui.hyperlink_to(repo_label, &url.0);
                } else {
                    ui.label(link.repository_id.as_str());
                }
                if let Some(path) = &link.worktree_path {
                    ui.label(&path.0);
                }
                widgets::caption(
                    ui,
                    if link.worktree_removed == Some(true) {
                        "Removed worktree · retained history"
                    } else {
                        "Recorded worktree"
                    },
                );
                if let Some(tree) = &link.worktree_id
                    && link.worktree_removed != Some(true)
                {
                    if ui.small_button("Inspect checkout").clicked() {
                        actions.push(Action::InspectGit(
                            link.repository_id.as_str().to_owned(),
                            tree.as_str().to_owned(),
                        ));
                    }
                    if let Some(observed) = work.git_inspections.get(tree.as_str()) {
                        match observed {
                            Ok(state) => {
                                widgets::caption(
                                    ui,
                                    if state.dirty {
                                        "Observed checkout · uncommitted changes"
                                    } else {
                                        "Observed checkout · clean"
                                    },
                                );
                                if let Some(branch) = &state.branch {
                                    if let Some(url) = link.repository_url.as_ref().and_then(|u| {
                                        devworks_core::protocol::helpers::git::branch_url(
                                            &u.0, &branch.0,
                                        )
                                    }) {
                                        ui.hyperlink_to(&branch.0, url);
                                    } else {
                                        ui.label(&branch.0);
                                    }
                                }
                                if let Some(url) = &link.repository_url {
                                    ui.hyperlink_to(
                                        &state.head.hex,
                                        format!("{}/commit/{}", url.0, state.head.hex),
                                    );
                                } else {
                                    ui.monospace(&state.head.hex);
                                }
                            }
                            Err(error) => {
                                widgets::caption(ui, error);
                            }
                        }
                    }
                }
                widgets::caption(ui, "Recorded association");
                if let Some(branch) = &link.branch {
                    if let Some(url) = &link.repository_url {
                        ui.hyperlink_to(
                            &branch.0,
                            devworks_core::protocol::helpers::git::branch_url(&url.0, &branch.0)
                                .unwrap_or_else(|| url.0.clone()),
                        );
                    } else {
                        ui.label(&branch.0);
                    }
                }
                if let Some(url) = &link.repository_url {
                    ui.hyperlink_to(
                        &link.head.hex,
                        format!("{}/commit/{}", url.0, link.head.hex),
                    );
                } else {
                    ui.monospace(&link.head.hex);
                }
                if let Some(url) = &link.pull_request_url {
                    let pr_label = widgets::github_link_label(&url.0).map_or_else(
                        || "Pull request".to_owned(),
                        |repo| format!("Pull request · {repo}"),
                    );
                    ui.horizontal_wrapped(|ui| {
                        ui.hyperlink_to(pr_label, &url.0);
                        if ui.small_button("Read checks & reviews").clicked() {
                            actions.push(Action::ReadGithubPullObservations {
                                repository_id: link.repository_id.as_str().to_owned(),
                                pull_request_url: url.0.clone(),
                                expected_head: link.head.hex.clone(),
                            });
                        }
                    });
                    for obs in work
                        .github_pr_checks
                        .get(&url.0)
                        .into_iter()
                        .flatten()
                        .chain(work.github_pr_reviews.get(&url.0).into_iter().flatten())
                    {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(format!("{} · {}", obs.kind.0, obs.state.0));
                            if let Some(obs_url) = &obs.url {
                                let label = widgets::github_link_label(&obs_url.0)
                                    .unwrap_or_else(|| obs.subject_id.as_str().to_owned());
                                ui.hyperlink_to(label, &obs_url.0);
                            }
                        });
                    }
                }
                if let Some(session) = &link.agent_session_id
                    && ui
                        .link(format!("Producing session {}", session.as_str()))
                        .clicked()
                {
                    actions.push(Action::OpenGitSession(session.as_str().to_owned()));
                }
            }
        });
}

fn github_issue_controls(
    ui: &mut Ui,
    work: &mut WorkUi,
    w: &WorkRecord,
    enabled: bool,
    actions: &mut Vec<Action>,
) {
    let Some(repo_id) = w.repository_ids.0.first() else {
        return;
    };
    egui::CollapsingHeader::new("GitHub issue")
        .id_salt(("work-github-issue", w.id.as_str()))
        .show(ui, |ui| {
            let mut is_linked = false;
            if let Some(tracked) = &w.github_issue {
                is_linked = true;
                ui.horizontal_wrapped(|ui| {
                    let label = widgets::github_link_label(&tracked.url.0).unwrap_or_else(|| {
                        format!("GitHub issue #{}", tracked.remote_issue_number.0)
                    });
                    ui.hyperlink_to(label, &tracked.url.0);
                    widgets::chip(
                        ui,
                        format!(
                            "remote: {} · intake: {:?}",
                            tracked.remote_state.0, tracked.intake_state
                        ),
                        widgets::Tone::Neutral,
                    );
                });
            } else if let Some(linked) = work.github_links.get(w.id.as_str()) {
                is_linked = true;
                let label = widgets::github_link_label(&linked.url.0)
                    .unwrap_or_else(|| format!("GitHub issue #{}", linked.remote_issue_number.0));
                ui.hyperlink_to(label, &linked.url.0);
            }
            ui.horizontal_wrapped(|ui| {
                if ui
                    .add_enabled(
                        enabled && !is_linked,
                        egui::Button::new("Publish to GitHub"),
                    )
                    .clicked()
                {
                    actions.push(Action::PublishGithubIssue {
                        issue_id: w.id.as_str().to_owned(),
                        expected_revision: w.revision.as_str().to_owned(),
                        repository_id: repo_id.as_str().to_owned(),
                    });
                }
                let draft = work
                    .github_link_number_drafts
                    .entry(w.id.as_str().to_owned())
                    .or_default();
                let input = ui.add(
                    egui::TextEdit::singleline(draft)
                        .hint_text("Issue #")
                        .desired_width(80.0),
                );
                input.widget_info(|| {
                    egui::WidgetInfo::labeled(egui::Role::TextInput, enabled, "GitHub issue number")
                });
                let valid_num = draft.trim().parse::<i64>().is_ok_and(|n| n > 0);
                if ui
                    .add_enabled(enabled && valid_num, egui::Button::new("Link GitHub issue"))
                    .clicked()
                {
                    actions.push(Action::LinkGithubIssue {
                        issue_id: w.id.as_str().to_owned(),
                        expected_revision: w.revision.as_str().to_owned(),
                        repository_id: repo_id.as_str().to_owned(),
                        remote_issue_number: draft.trim().to_owned(),
                    });
                }
            });
        });
}
