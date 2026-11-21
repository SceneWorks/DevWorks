//! Applies the actions a work screen raises: commands, moves and plan saves.
use super::*;

impl WorkCtl<'_, '_> {
    pub(crate) fn work_act(&mut self, actions: Vec<Action>) {
        for a in actions {
            match a {
                Action::KeepWorking(enabled) => {
                    let Some(project) = self.selected_project.clone() else {
                        continue;
                    };
                    let Ok(id) = Id::new(&project) else {
                        continue;
                    };
                    let Some(client) = self.cx.client.as_deref_mut() else {
                        continue;
                    };
                    let received = client.backend().control_epoch();
                    let id = ProjectId(id);
                    client.task(
                        WorkSlot::KeepWorking(project).into(),
                        "Changing project keep-working",
                        move |backend| {
                            backend.set_keep_working(&id, enabled, received)?;
                            Ok(TaskOutput::Refreshed)
                        },
                    );
                }
                Action::RefreshMessages => self.load_messages(true),
                Action::MoreInbox => {
                    let Some(project) = self.selected_project.clone() else {
                        continue;
                    };
                    let Some(cursor) = self.work.messages.inbox_cursor.clone() else {
                        continue;
                    };
                    if self.work.messages.project.as_deref() != Some(project.as_str())
                        || !self.work.messages.inbox_has_more
                        || self.pending(&WorkSlot::WorkMessagesInbox(project.clone()))
                    {
                        continue;
                    }
                    let p = project.clone();
                    self.command(
                        WorkSlot::WorkMessagesInbox(project),
                        "Reading more messages",
                        "agents.receive",
                        move |be| wops::message_inbox(be, &p, Some(&cursor)),
                    );
                }
                Action::PollMessages => {
                    let Some(project) = self.selected_project.clone() else {
                        continue;
                    };
                    if self.work.messages.project.as_deref() != Some(project.as_str())
                        || self.pending(&WorkSlot::WorkMessagesEvents(project.clone()))
                    {
                        continue;
                    }
                    self.work.messages.event_poll_at =
                        Some(std::time::Instant::now() + std::time::Duration::from_secs(1));
                    let p = project.clone();
                    let after = self.work.messages.event_after;
                    self.query(
                        WorkSlot::WorkMessagesEvents(project),
                        "Reading message changes",
                        "agents.events",
                        move |be| wops::message_events(be, &p, after),
                    );
                }
                Action::OlderMessages => self.older_messages(),
                Action::PostMessage(draft) => {
                    let Some(project) = self.selected_project.clone() else {
                        continue;
                    };
                    if self.work.messages.project.as_deref() != Some(project.as_str()) {
                        continue;
                    }
                    if draft.body.trim().is_empty()
                        || self.pending(&WorkSlot::WorkMessagePost(project.clone()))
                    {
                        continue;
                    }
                    let input = message_post_body(&draft, &project);
                    if self.command(
                        WorkSlot::WorkMessagePost(project),
                        "Posting message",
                        "agents.post",
                        move |be| wops::message_post(be, input),
                    ) {
                        self.work.messages.sending = Some(draft);
                    }
                }
                Action::AckMessage(id, resolution) => {
                    let Some(project) = self.selected_project.clone() else {
                        continue;
                    };
                    if self.work.messages.project.as_deref() != Some(project.as_str()) {
                        continue;
                    }
                    if self.pending(&WorkSlot::WorkMessageAck(project.clone(), id.clone())) {
                        continue;
                    }
                    let p = project.clone();
                    let key = id.clone();
                    self.command(
                        WorkSlot::WorkMessageAck(project, id),
                        "Acknowledging message",
                        "agents.ack",
                        move |be| wops::message_ack(be, &p, &key, resolution),
                    );
                }
                Action::OlderHistory(root) => self.older_history(root),
                Action::RefreshHistory(root) => self.refresh_history(root),

                Action::Workflow(action) => self.workflow_action(action),
                Action::Allocate(feature, lease) => {
                    if let Some(project) = self.work.of.clone() {
                        self.load_allocation_queue(&project, feature, Some(lease));
                    }
                }
                Action::InspectClaim(claim) => self.inspect_allocation(&claim),
                Action::ReleaseClaim(claim) => self.release_allocation(&claim),
                Action::OpenWork(id) => self.open_work(&id),
                Action::Reload => {
                    self.load_work();
                    self.refresh_history(self.work.activity.item.clone());
                }
                Action::Create(form) => {
                    let Some(p) = self.work.of.clone() else {
                        continue;
                    };
                    let Some(backlog) = self
                        .work
                        .workflows
                        .get(&p)
                        .filter(|w| w.rows.loaded)
                        .and_then(|w| w.rows.items.iter().find(|s| s.category.0 == "Backlog"))
                        .map(|s| s.id.as_str().to_owned())
                    else {
                        self.work.message = Some((
                            Tone::Attention,
                            "Wait for the project's workflow before creating work.".into(),
                        ));
                        continue;
                    };
                    let (op, input) = match create_request(&form, &p, &backlog) {
                        Ok(request) => request,
                        Err(error) => {
                            self.work.message = Some((Tone::Bad, error));
                            continue;
                        }
                    };
                    if self.command(
                        WorkSlot::WorkCreate(p.clone()),
                        "Creating work",
                        op,
                        move |be| wops::work_command(be, input),
                    ) {
                        self.work.create_saving.insert(p, form);
                    }
                }
                Action::Edit(form) => {
                    let w = form.snapshot.clone();
                    let (op, input) = match edit_request(&form) {
                        Ok(Some(request)) => request,
                        Ok(None) => {
                            self.work.edit = None;
                            continue;
                        }
                        Err(error) => {
                            self.work.message = Some((Tone::Bad, error));
                            continue;
                        }
                    };
                    self.work.edit_saving.insert(w.id.as_str().to_owned(), form);
                    self.command(
                        WorkSlot::WorkCommand(w.id.as_str().to_owned()),
                        "Saving work",
                        op,
                        move |be| wops::work_command(be, input),
                    );
                }
                Action::Command(id, input) => {
                    self.command(
                        WorkSlot::WorkCommand(id),
                        "Changing work",
                        input.operation(),
                        move |be| wops::work_command(be, input),
                    );
                }
                Action::Move(w, status) => {
                    let id = w.id.as_str().to_owned();
                    let input = move_request(&w, &status);
                    // The marker shows the move as submitted; the card stays where it is until
                    // the outcome is confirmed.
                    if self.command(
                        WorkSlot::WorkCommand(id.clone()),
                        "Moving a card",
                        "tasks.move",
                        move |be| wops::work_command(be, input),
                    ) {
                        self.work.moving.insert(id, status);
                    }
                }
                Action::Order(op, feature, priority, placement, expected) => {
                    let Some(p) = self.work.of.clone() else {
                        continue;
                    };
                    // One bounded placement, whatever the bucket's size.
                    let input =
                        order_request(op, &p, feature.as_deref(), priority, placement, &expected);
                    let key = format!("{p}/{}", feature.unwrap_or_default());
                    self.command(WorkSlot::WorkOrder(key), "Reordering work", op, move |be| {
                        wops::work_command(be, input)
                    });
                }
                Action::SavePlan(form) => self.save_plan(form),
                Action::ShowActivity(id) => {
                    if !self
                        .work
                        .histories
                        .get(&id)
                        .is_some_and(|feed| feed.feed.loaded || feed.feed.loading())
                    {
                        self.refresh_history(Some(id.clone()));
                    }
                    self.work.activity.item = Some(id);
                    // Overview's second tab is Activity.
                    self.routes.entry(Route::Overview).or_default().tab = 1;
                    self.navigate(Route::Overview);
                }
                Action::RequestApproval(w) => {
                    let Some(pending) = w.pending_plan.clone() else {
                        continue;
                    };
                    let Some(digest) = pending.plan_digest.as_ref() else {
                        self.work.message = Some((
                            Tone::Attention,
                            "Refresh the feature to read its captured pending plan token before \
                            requesting approval."
                                .into(),
                        ));
                        continue;
                    };
                    let input = approval_request(&w, &pending, digest);
                    self.command(
                        WorkSlot::WorkCommand(w.id.as_str().to_owned()),
                        "Requesting plan approval",
                        "features.request_plan_approval",
                        move |be| wops::work_command(be, input),
                    );
                }
                Action::InspectGit(repository, tree) => {
                    self.query(
                        WorkSlot::WorkGitInspect(repository.clone(), tree.clone()),
                        "Inspecting checkout",
                        "git.inspect",
                        move |be| ops::git_inspect_worktree(be, &repository, Some(&tree)),
                    );
                }
                Action::RefreshGit(root) => self.load_git_links(&root),
                Action::ReadinessHistory(root, more) => self.load_readiness_history(&root, more),
                Action::OpenGitSession(session) => {
                    self.cx.effects.push(Effect::OpenGitSession(session));
                }
                Action::PublishGithubIssue {
                    issue_id,
                    expected_revision,
                    repository_id,
                } => {
                    self.command(
                        WorkSlot::GithubIssuePublish(issue_id.clone()),
                        "Publishing issue to GitHub",
                        "issues.publish",
                        move |be| {
                            let input = (|| {
                                let rev: i64 = expected_revision.parse().ok()?;
                                Some(In::Issues(issues::Input::Publish(issues::PublishInput {
                                    issue_id: RootId(frame::id(&issue_id)?),
                                    expected_revision: Revision::new(rev).ok()?,
                                    repository_id: frame::id(&repository_id)?,
                                    expected_remote_revision: None,
                                })))
                            })();
                            frame::build(be, frame::Negotiate::Latest, input)
                        },
                    );
                }
                Action::LinkGithubIssue {
                    issue_id,
                    expected_revision,
                    repository_id,
                    remote_issue_number,
                } => {
                    self.command(
                        WorkSlot::GithubIssueLink(issue_id.clone()),
                        "Linking GitHub issue",
                        "issues.link",
                        move |be| {
                            use devworks_core::protocol::support::PositiveCounter;
                            let input = (|| {
                                let num: i64 = remote_issue_number.trim().parse().ok()?;
                                if num <= 0 {
                                    return None;
                                }
                                let rev: i64 = expected_revision.parse().ok()?;
                                Some(In::Issues(issues::Input::Link(issues::LinkInput {
                                    issue_id: RootId(frame::id(&issue_id)?),
                                    expected_revision: Revision::new(rev).ok()?,
                                    repository_id: frame::id(&repository_id)?,
                                    remote_issue_number: PositiveCounter(num.to_string()),
                                })))
                            })();
                            frame::build(be, frame::Negotiate::Latest, input)
                        },
                    );
                }
                Action::ReadGithubPullObservations {
                    repository_id,
                    pull_request_url,
                    expected_head,
                } => {
                    use devworks_core::operations::github;
                    use devworks_core::protocol::{
                        data::{GitOid, ObjectFormat},
                        records::PageRequest,
                        support::{Limit, PositiveCounter},
                    };
                    let pr_number = pull_request_url
                        .rsplit_once("/pull/")
                        .and_then(|(_, tail)| {
                            let digits = tail.split(['/', '?', '#']).next()?;
                            let n: i64 = digits.parse().ok()?;
                            (n > 0).then(|| n.to_string())
                        });
                    let Some(pr_number) = pr_number else {
                        continue;
                    };
                    let head_oid = |hex: String| GitOid {
                        format: if hex.len() == 64 {
                            ObjectFormat::Sha256
                        } else {
                            ObjectFormat::Sha1
                        },
                        hex,
                    };
                    let repo_checks = repository_id.clone();
                    let pr_num_checks = pr_number.clone();
                    let head_checks = expected_head.clone();
                    self.query(
                        WorkSlot::GithubPullChecks(pull_request_url.clone()),
                        "Reading GitHub checks",
                        "github.checks",
                        move |be| {
                            let input = Id::new(repo_checks).ok().map(|repository_id| {
                                In::Github(github::Input::Checks(github::ChecksInput {
                                    repository_id,
                                    pull_request_number: PositiveCounter(pr_num_checks),
                                    head: head_oid(head_checks),
                                    page: PageRequest {
                                        limit: Limit::new(50).expect("bounded checks limit"),
                                        cursor: None,
                                    },
                                }))
                            });
                            frame::build(be, frame::Negotiate::Latest, input)
                        },
                    );
                    self.query(
                        WorkSlot::GithubPullReviews(pull_request_url),
                        "Reading GitHub reviews",
                        "github.reviews",
                        move |be| {
                            let input = Id::new(repository_id).ok().map(|repository_id| {
                                In::Github(github::Input::Reviews(github::ReviewsInput {
                                    repository_id,
                                    pull_request_number: PositiveCounter(pr_number),
                                    head: head_oid(expected_head),
                                    page: PageRequest {
                                        limit: Limit::new(50).expect("bounded reviews limit"),
                                        cursor: None,
                                    },
                                }))
                            });
                            frame::build(be, frame::Negotiate::Latest, input)
                        },
                    );
                }
                Action::Inspect(root) => self.inspect(&root),
                Action::Run(op, w) => {
                    let Some(snapshot) = self.work.runs.get(w.id.as_str()).cloned() else {
                        self.inspect(w.id.as_str());
                        continue;
                    };
                    let id = w.id.as_str().to_owned();
                    self.command(
                        WorkSlot::WorkRun(id),
                        "Changing run controls",
                        op,
                        move |be| wops::run_control(be, op, &w, &snapshot),
                    );
                }
                Action::SelectCard(c) => {
                    self.work.card = c.clone();
                    if let Some(c) = c
                        && let Some(w) = self.record(&c)
                    {
                        self.load_detail(&w);
                        self.inspect(&c);
                    }
                }
                Action::OpenDetail(id) => {
                    if let Some(w) = self.record(&id) {
                        self.load_detail(&w);
                        if w.parent_id.is_none() {
                            self.inspect(&id);
                        }
                    }
                }
            }
        }
    }

    /// Saves a plan draft: a changed (or first) plan text is stored as an artifact of the
    /// feature's project first (so its agents can read it), then the draft is submitted at the
    /// revision its form was opened on. The save is bound to `form` exactly as submitted; the
    /// editor stays open, and edits made meanwhile are kept as a draft (lock-free: the user
    /// never waits on an upload to keep writing, and nothing they write is lost or mixed in).
    pub(super) fn save_plan(&mut self, form: PlanForm) {
        if let Some(error) = plan_error(&form) {
            self.work.message = Some((Tone::Bad, error));
            if self.work.plan.is_none() {
                self.work.plan = Some(form);
            }
            return;
        }
        let decisions: Vec<_> = form
            .drops
            .iter()
            .filter(|(_, d)| d.artifact.is_none())
            .map(|(id, d)| format!("## {id}\n\n{}", d.reason))
            .collect();
        let unchanged = form.content.is_some() && form.text == form.original;
        if decisions.is_empty() && unchanged {
            let content = form.content.clone().expect("checked");
            self.work
                .plan_saving
                .insert(form.snapshot.id.as_str().to_owned(), form.clone());
            self.submit_plan(&form, &content);
            return;
        }
        let id = form.snapshot.id.as_str().to_owned();
        let project = form.snapshot.project_id.clone();
        let (slot, title, text) = if decisions.is_empty() {
            (
                WorkSlot::WorkPlanText(id.clone()),
                "Feature plan",
                form.text.clone(),
            )
        } else {
            (
                WorkSlot::WorkPlanDrop(id.clone()),
                "Requirement drop decisions",
                format!("# Requirement drop decisions\n\n{}", decisions.join("\n\n")),
            )
        };
        let started = self.cx.client.as_deref_mut().is_some_and(|c| {
            c.task(slot.into(), "Storing the plan and decisions", move |b| {
                let dir = b.scratch_dir();
                let path = b.stage_project_text(Some(&project), "md", text.as_bytes())?;
                let digest = devworks_core::fingerprint::sha256_hex(text.as_bytes());
                let ticket = b.mint_project_upload(&project, &path)?;
                let frame = ops::save_project_artifact(
                    b,
                    project.0.as_str(),
                    ticket.as_str(),
                    title,
                    "text/markdown",
                    &digest,
                    text.len() as u64,
                );
                let outcome = resolve_command(b, "artifacts.save", &frame);
                if ops::saved_artifact(outcome.clone()).is_ok() {
                    crate::attachments::remove_owned(&path, &dir);
                }
                Ok(TaskOutput::Stored { outcome, frame })
            })
        });
        if started {
            self.work.plan_saving.insert(id, form);
        } else if self.work.plan.is_none() {
            // Not started (offline): the draft goes back to the editor, never dropped.
            self.work.plan = Some(form);
        }
    }

    /// A plan save of `feature` that ended without saving: its draft goes back to the editor
    /// when the editor is free (an open editor keeps what it holds).
    pub(super) fn unsaved_plan(&mut self, feature: &str) {
        if let Some(saved) = self.work.plan_saving.remove(feature)
            && self.work.plan.is_none()
        {
            self.work.plan = Some(saved);
        }
    }

    /// Puts back the plan saves a close kept: one whose `features.edit_plan` is pending waits
    /// for its confirmation; one still storing its text is saved again from its snapshot.
    pub(super) fn restore_plan_saves(&mut self, saves: Vec<PlanForm>, confirming: &[String]) {
        for form in saves {
            let id = form.snapshot.id.as_str().to_owned();
            if confirming.contains(&id) {
                self.work.plan_saving.insert(id, form);
            } else {
                self.save_plan(form);
            }
        }
    }

    pub(super) fn submit_plan(&mut self, form: &PlanForm, content: &ArtifactRef) {
        let w = &form.snapshot;
        let input = match plan_request(form, content) {
            Ok(input) => input,
            Err(error) => {
                self.work.message = Some((Tone::Bad, error));
                return;
            }
        };
        self.command(
            WorkSlot::WorkPlan(w.id.as_str().to_owned()),
            "Saving the plan draft",
            "features.edit_plan",
            move |be| wops::work_command(be, input),
        );
    }
}
