//! Builds the operation inputs for the work screens' commands.
use super::*;

/// The command that creates what `form` describes in project `p`, whose backlog status is
/// `backlog`: its operation, and the input (none when a value is outside its contract, which
/// the gateway then rejects as a malformed request).
pub(crate) fn create_request(
    form: &CreateForm,
    p: &str,
    backlog: &str,
) -> Result<(&'static str, Option<In>), String> {
    let feature = form.kind == "feature";
    let budget = if feature {
        Some(form.budget.input()?)
    } else {
        None
    };
    let op = match form.kind.as_str() {
        "feature" => "features.create",
        "issue" => "issues.create",
        _ => "tasks.create",
    };
    let input = || {
        let project_id = ProjectId(frame::id(p)?);
        let title = frame::text(&form.title)?;
        let description = frame::text(&form.description)?;
        let repository_ids = BoundedList(Vec::new());
        let status_id = frame::id(backlog)?;
        let priority = form.priority;
        let labels = Some(frame::texts::<Label, 128>(&form.labels)?);
        Some(match op {
            "features.create" => In::Features(features::Input::Create(features::CreateInput {
                project_id,
                creation: FeatureCreation::Single {
                    title,
                    description,
                    repository_ids,
                    status_id,
                    priority,
                    plan: None,
                    labels,
                    budget,
                },
            })),
            "issues.create" => In::Issues(issues::Input::Create(issues::CreateInput {
                project_id,
                title,
                description,
                repository_ids,
                status_id,
                priority,
                feature_id: frame::lift(form.feature.as_deref().map(frame::text::<RootId>))?,
                task_type: frame::lift(form.task_type.as_deref().map(frame::text))?,
                labels,
            })),
            _ => In::Tasks(tasks::Input::Create(tasks::CreateInput {
                project_id,
                title,
                description,
                repository_ids,
                parent_id: frame::lift(form.feature.as_deref().map(frame::text::<RootId>))?,
                status_id,
                priority,
                task_type: frame::lift(form.task_type.as_deref().map(frame::text))?,
                labels,
            })),
        })
    };
    Ok((op, input()))
}

/// The command that saves `form`'s changes to its item (its operation and input, as
/// [`create_request`]), or none when nothing changed.
pub(crate) fn edit_request(form: &EditForm) -> Result<Option<(&'static str, Option<In>)>, String> {
    let w = &form.snapshot;
    let feature = matches!(w.kind, WorkKind::Feature);
    let title = (form.title != w.title.0).then_some(form.title.as_str());
    let description = (form.description != w.description.0).then_some(form.description.as_str());
    let labels = (form.labels != labels_of(w)).then_some(form.labels.as_slice());
    let task_type = (!feature && form.task_type != task_type_of(w)).then_some(&form.task_type);
    let budget = if feature && form.budget_changed {
        Some(form.budget.input()?)
    } else {
        None
    };
    let priority = (!feature && form.priority != w.priority).then_some(form.priority);
    if title.is_none()
        && description.is_none()
        && labels.is_none()
        && task_type.is_none()
        && budget.is_none()
        && priority.is_none()
    {
        return Ok(None);
    }
    let input = || {
        let work_id = w.id.clone();
        let expected_revision = w.revision.clone();
        let title = frame::lift(title.map(frame::text))?;
        let description = frame::lift(description.map(frame::text))?;
        let labels = frame::lift(labels.map(frame::texts::<Label, 128>))?;
        Some(if feature {
            In::Features(features::Input::Edit(features::EditInput {
                work_id,
                expected_revision,
                changes: FeaturePatch {
                    title,
                    description,
                    labels,
                    budget,
                    ..wops::empty()?
                },
            }))
        } else {
            let (task_type, clear_task_type) = match task_type {
                Some(Some(kind)) => (Some(frame::text(kind)?), None),
                Some(None) => (None, Some(true)),
                None => (None, None),
            };
            In::Tasks(tasks::Input::Edit(tasks::EditInput {
                work_id,
                expected_revision,
                changes: TaskPatch {
                    title,
                    description,
                    labels,
                    priority,
                    task_type,
                    clear_task_type,
                    ..wops::empty()?
                },
            }))
        })
    };
    Ok(Some((
        if feature {
            "features.edit"
        } else {
            "tasks.edit"
        },
        input(),
    )))
}

/// The `tasks.move` command moving `w` to workflow status `status`.
pub(crate) fn move_request(w: &WorkRecord, status: &str) -> Option<In> {
    Some(In::Tasks(tasks::Input::Move(tasks::MoveInput {
        task_id: w.id.clone(),
        expected_revision: w.revision.clone(),
        status_id: frame::id(status)?,
        expected_workflow_revision: match &w.workflow_revision {
            Some(revision) => revision.clone(),
            None => frame::number(1)?,
        },
    })))
}

/// The queue placement command of `op` for one bounded `placement`.
pub(crate) fn order_request(
    op: &str,
    p: &str,
    feature: Option<&str>,
    priority: Option<i32>,
    placement: QueuePlacement,
    expected: &str,
) -> Option<In> {
    let project_id = ProjectId(frame::id(p)?);
    let expected_queue_revision = frame::text(expected)?;
    let placement = Some(placement);
    Some(match op {
        "features.prioritize" => {
            In::Features(features::Input::Prioritize(features::PrioritizeInput {
                project_id,
                expected_queue_revision,
                ordered_work_ids: None,
                placement,
            }))
        }
        "issues.prioritize" => In::Issues(issues::Input::Prioritize(issues::PrioritizeInput {
            project_id,
            expected_queue_revision,
            ordered_work_ids: None,
            placement,
        })),
        _ => In::Tasks(tasks::Input::Prioritize(tasks::PrioritizeInput {
            project_id,
            feature_id: frame::lift(feature.map(frame::text::<RootId>))?,
            expected_queue_revision,
            priority: priority.unwrap_or(0),
            ordered_task_ids: None,
            placement,
        })),
    })
}

/// The `features.request_plan_approval` command for the pending plan of `w`.
pub(crate) fn approval_request(
    w: &WorkRecord,
    pending: &PendingFeaturePlan,
    digest: &Digest,
) -> In {
    In::Features(features::Input::RequestPlanApproval(
        features::RequestPlanApprovalInput {
            feature_id: RootId(w.id.clone()),
            expected_revision: w.revision.clone(),
            plan_revision: pending.revision.clone(),
            plan_digest: digest.clone(),
        },
    ))
}

/// The `tasks.set_feature` command making `w` a standalone issue.
pub(crate) fn unlink_request(w: &WorkRecord) -> In {
    In::Tasks(tasks::Input::SetFeature(tasks::SetFeatureInput {
        task_id: w.id.clone(),
        expected_revision: w.revision.clone(),
        feature_id: None,
        clear_feature: true,
    }))
}

/// The `features.edit_plan` command saving `form` (as [`create_request`]). Every live
/// requirement of the active plan stays in the draft (dropping one needs an explicit decision);
/// what the form does not show is kept from its starting draft.
pub(crate) fn plan_request(form: &PlanForm, content: &ArtifactRef) -> Result<Option<In>, String> {
    let w = &form.snapshot;
    let (budget, clear_budget_proposal) = match &form.budget {
        Some(budget) => (Some(budget.input()?), None),
        None if w
            .pending_plan
            .as_ref()
            .is_some_and(|pending| pending.budget.is_some()) =>
        {
            (None, Some(true))
        }
        None => (None, None),
    };
    Ok(plan_body(form, content).map(|plan| {
        In::Features(features::Input::EditPlan(features::EditPlanInput {
            feature_id: RootId(w.id.clone()),
            expected_revision: w.revision.clone(),
            plan,
            budget,
            clear_budget_proposal,
        }))
    }))
}

/// The desktop user's actor (the host binds it), for "You" in Activity.
pub(crate) fn devworks_desktop_user() -> String {
    crate::host::USER_ACTOR.to_owned()
}

/// The `agents.post` command of `draft` in `project` (none when a value is outside its
/// contract).
pub(crate) fn message_post_body(draft: &MessageDraft, project: &str) -> Option<In> {
    let kind = ["Info", "Question", "Request", "Blocker", "Announcement"]
        .get(draft.kind)
        .copied()
        .unwrap_or("Info");
    fn field(value: &str) -> Option<&str> {
        Some(value.trim()).filter(|v| !v.is_empty())
    }
    let reply = field(&draft.reply_to);
    let resolution = match (reply, draft.resolution) {
        (Some(_), 1) => Some("Answered"),
        (Some(_), 2) => Some("Declined"),
        _ => None,
    };
    Some(In::Agents(agents::Input::Post(agents::PostInput {
        session_id: None,
        project_id: Some(ProjectId(frame::id(project)?)),
        body: frame::text(&draft.body)?,
        message_kind: Some(frame::text(kind)?),
        item_id: frame::lift(field(&draft.item).map(frame::text))?,
        feature_id: frame::lift(field(&draft.feature).map(frame::text))?,
        resource_name: frame::lift(field(&draft.resource).map(frame::text))?,
        to_session_id: frame::lift(field(&draft.to_session).map(frame::text))?,
        to_human: draft.to_human.then_some(true),
        blocking: draft.blocking.then_some(true),
        in_reply_to_id: frame::lift(reply.map(frame::text))?,
        resolves: frame::lift(resolution.map(frame::text))?,
        idempotency_key: None,
    })))
}
