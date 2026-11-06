//! Native workflow and allocation controls. All mutations go through the shared gateway;
//! this module stores only editors, authoritative snapshots, and command progress.
use super::*;

#[derive(Clone, Debug)]
pub enum Draft {
    Create {
        revision: String,
        label: String,
        category: String,
        after_status_id: Option<String>,
    },
    Rename {
        snapshot: StatusRecord,
        label: String,
    },
    Remove {
        snapshot: StatusRecord,
        destination: Option<String>,
    },
}
#[derive(Default)]
pub struct WorkflowUi {
    pub rows: Vec<StatusRecord>,
    pub loaded: bool,
    pub error: Option<String>,
    pub generation: u64,
    pub gathering: Vec<StatusRecord>,
    pub open: bool,
    pub draft: Option<Draft>,
    pub busy: bool,
    pub editable: bool,
}
#[derive(Clone, Debug, Default)]
pub struct AllocationFilters {
    pub repository_id: Option<String>,
    pub task_type: Option<String>,
    pub label: String,
    pub another: bool,
    pub idempotency_key: String,
}
impl AllocationFilters {
    fn apply(&self, body: &mut Value) {
        if let Some(repository) = &self.repository_id {
            body["repositoryId"] = json!(repository);
        }
        if let Some(kind) = &self.task_type {
            body["taskType"] = json!(kind);
        }
        if !self.label.is_empty() {
            body["label"] = json!(self.label);
        }
        if self.another {
            body["another"] = json!(true);
        }
        if !self.idempotency_key.is_empty() {
            body["idempotencyKey"] = json!(self.idempotency_key);
        }
    }
}
impl WorkflowUi {
    pub fn revision(&self) -> Option<String> {
        self.rows
            .first()?
            .workflow_revision
            .as_ref()
            .map(|r| r.as_str().to_owned())
    }
}
#[derive(Clone, Debug)]
pub enum WorkflowAction {
    Submit {
        project: String,
        operation: &'static str,
        body: Value,
    },
    Reload(String),
}

impl Shell {
    pub(super) fn load_workflow(&mut self, project: &str, replace: bool) {
        let generation = self
            .work
            .workflows
            .entry(project.into())
            .or_default()
            .generation;
        if !replace && self.pending(&Slot::WorkWorkflow(project.into(), generation)) {
            return;
        }
        let state = self.work.workflows.entry(project.into()).or_default();
        state.generation += 1;
        state.gathering.clear();
        let generation = state.generation;
        let p = project.to_owned();
        self.query(
            Slot::WorkWorkflow(p.clone(), generation),
            "Loading project workflow",
            "workflows.list",
            move |b| wops::workflow_list(b, &p, None),
        );
    }
    pub(super) fn load_allocation_queue(
        &mut self,
        project: &str,
        feature: Option<String>,
        lease: Option<u64>,
    ) {
        let key = (project.to_owned(), feature.clone());
        if lease.is_some()
            && (self.work.allocation_busy.contains(&key)
                || self.waiting(&Slot::AllocationClaim(project.into(), feature.clone())))
        {
            return;
        }
        let slot = Slot::AllocationQueue(project.into(), feature.clone(), lease);
        if self.waiting(&slot) {
            return;
        }
        let p = project.to_owned();
        if lease.is_some() {
            self.work.allocation_busy.insert(key.clone());
            self.work.allocation_requests.insert(
                key,
                self.work
                    .allocation_filters
                    .get(project)
                    .cloned()
                    .unwrap_or_default(),
            );
        }
        let feature_query = feature.clone();
        let admitted = self.query(
            slot,
            "Reading work queue",
            "next_work.inspect_queue",
            move |b| wops::allocation_queue(b, &p, feature_query.as_deref()),
        );
        if !admitted && lease.is_some() {
            self.work
                .allocation_busy
                .remove(&(project.into(), feature.clone()));
            self.work
                .allocation_requests
                .remove(&(project.into(), feature));
            self.work.message = Some((Tone::Attention,"The selection was not submitted. Choose Next work again when the runtime is available.".into()));
        }
    }
    pub(super) fn inspect_allocation(&mut self, claim: &str) {
        if self.pending(&Slot::AllocationInspect(claim.into())) {
            return;
        }
        let id = claim.to_owned();
        self.work.allocation_inspect_errors.remove(claim);
        self.query(
            Slot::AllocationInspect(id.clone()),
            "Inspecting work claim",
            "next_work.inspect_claim",
            move |b| wops::allocation_inspect(b, &id),
        );
    }
    pub(super) fn refresh_allocation_claims(&mut self) {
        let mut claims: BTreeSet<String> = self
            .work
            .features
            .items
            .iter()
            .chain(&self.work.cards.items)
            .filter_map(|w| w.claim.as_ref().map(|c| c.id.as_str().to_owned()))
            .collect();
        claims.extend(
            self.work
                .allocation_claims
                .values()
                .filter(|c| c.project_id.as_ref().map(|p| p.0.as_str()) == self.work.of.as_deref())
                .map(|c| c.id.as_str().to_owned()),
        );
        for id in claims {
            self.inspect_allocation(&id);
        }
    }
    pub(super) fn release_allocation(&mut self, id: &str) {
        if self.waiting(&Slot::AllocationRelease(id.into())) {
            return;
        }
        let Some(claim) = self.work.allocation_claims.get(id) else {
            return;
        };
        let body = json!({"claimId":claim.id,"expectedFence":claim.fence});
        self.command(
            Slot::AllocationRelease(id.into()),
            "Releasing work claim",
            "next_work.release_claim",
            move |b| wops::allocation_command(b, "next_work.release_claim", body),
        );
    }
    pub(super) fn workflow_action(&mut self, action: WorkflowAction) {
        match action {
            WorkflowAction::Reload(project) => self.load_workflow(&project, true),
            WorkflowAction::Submit {
                project,
                operation,
                body,
            } => {
                if self.waiting(&Slot::WorkWorkflowCommand(project.clone())) {
                    return;
                }
                if self.command(
                    Slot::WorkWorkflowCommand(project.clone()),
                    "Changing project workflow",
                    operation,
                    move |b| wops::workflow_command(b, operation, body),
                ) {
                    self.work.workflows.entry(project).or_default().busy = true;
                } else {
                    self.work.message = Some((
                        Tone::Attention,
                        "The workflow change was not submitted; your draft is kept.".into(),
                    ));
                }
            }
        }
    }
    pub(super) fn apply_workflow(&mut self, slot: Slot, reply: Reply) -> Option<(Slot, Reply)> {
        if let (Slot::AllocationClaim(project, feature), Reply::Command(outcome)) = (&slot, &reply)
            && !is_unknown(outcome)
        {
            self.work
                .allocation_busy
                .remove(&(project.clone(), feature.clone()));
        }
        match (slot, reply) {
            (Slot::WorkWorkflow(project, generation), Reply::Query(query)) => {
                let state = self.work.workflows.entry(project.clone()).or_default();
                if generation != state.generation {
                    return None;
                }
                match wops::workflow_page(query) {
                    Ok((rows, next)) => {
                        state.gathering.extend(rows);
                        if let Some(cursor) = next {
                            self.query(
                                Slot::WorkWorkflow(project.clone(), generation),
                                "Loading project workflow",
                                "workflows.list",
                                move |b| wops::workflow_list(b, &project, Some(&cursor)),
                            );
                        } else {
                            let revision = state
                                .gathering
                                .first()
                                .and_then(|s| s.workflow_revision.as_ref());
                            if revision.is_none()
                                || state
                                    .gathering
                                    .iter()
                                    .any(|s| s.workflow_revision.as_ref() != revision)
                            {
                                state.gathering.clear();
                                state.error = Some("The workflow snapshot is incomplete; refresh it before editing.".into());
                            } else {
                                state.rows = std::mem::take(&mut state.gathering);
                                state.loaded = true;
                                state.error = None;
                                if self.work.of.as_deref() == Some(project.as_str())
                                    && self.work.status_filter.as_ref().is_some_and(|id| {
                                        !state.rows.iter().any(|status| status.id.as_str() == id)
                                    })
                                {
                                    self.work.status_filter = None;
                                }
                            }
                        }
                    }
                    Err(Failure::Refused(ref f)) if f.code == DomainErrorCode::CursorExpired => {
                        self.load_workflow(&project, true)
                    }
                    Err(error) => {
                        state.gathering.clear();
                        state.error = Some(error.describe());
                    }
                }
            }
            (Slot::WorkWorkflowCommand(project), Reply::Command(outcome)) => {
                let unknown = is_unknown(&outcome);
                if !unknown {
                    self.work.workflows.entry(project.clone()).or_default().busy = false;
                }
                match wops::workflow_changed(outcome) {
                    Ok(()) => {
                        self.work
                            .workflows
                            .entry(project.clone())
                            .or_default()
                            .draft = None;
                        self.work.mutations += 1;
                        self.work.message =
                            Some((Tone::Good, "The project workflow was saved.".into()));
                        self.load_workflow(&project, true);
                        self.load_work();
                    }
                    Err(error) => {
                        self.work_failure("Changing the workflow", error);
                        self.load_workflow(&project, true);
                    }
                }
            }
            (Slot::AllocationQueue(project, feature, lease), Reply::Query(query)) => {
                match wops::queue_snapshot(query) {
                    Ok(snapshot) => {
                        self.work
                            .allocation_queues
                            .insert((project.clone(), feature.clone()), snapshot.clone());
                        if let Some(lease) = lease {
                            let (operation, mut body) = match &feature {
                                None => (
                                    "next_work.claim_project_queue",
                                    json!({"projectId":project,"expectedQueueRevision":snapshot.queue_revision,"leaseMs":lease.to_string()}),
                                ),
                                Some(feature) => (
                                    "next_work.claim_feature_task",
                                    json!({"rootId":feature,"expectedTaskSetRevision":snapshot.queue_revision,"leaseMs":lease.to_string()}),
                                ),
                            };
                            let Some(filters) = self
                                .work
                                .allocation_requests
                                .remove(&(project.clone(), feature.clone()))
                            else {
                                self.work.allocation_busy.remove(&(project, feature));
                                self.work_failure("Selecting work",Failure::Other("The selection request is no longer available; choose Next work again.".into()));
                                return None;
                            };
                            filters.apply(&mut body);
                            let key = (project.clone(), feature.clone());
                            if !self.command(
                                Slot::AllocationClaim(project, feature),
                                "Selecting work",
                                operation,
                                move |b| wops::allocation_command(b, operation, body),
                            ) {
                                self.work.allocation_busy.remove(&key);
                                self.work.message = Some((Tone::Attention,"The selection was not submitted. Choose Next work again when the runtime is available.".into()));
                            }
                        }
                    }
                    Err(error) => {
                        self.work.allocation_busy.remove(&(project, feature));
                        self.work_failure("Reading the work queue", error);
                    }
                }
            }
            (Slot::AllocationClaim(_, _), Reply::Command(outcome)) => {
                match wops::allocation_selection(outcome) {
                    Ok(AllocationSelection::Current(AllocationOutcome::Claimed {
                        claim,
                        work,
                        reused,
                    })) => self.adopt_allocation(claim, work, reused, None),
                    Ok(AllocationSelection::Current(AllocationOutcome::Recovered {
                        claim,
                        work,
                        reused,
                        recovery,
                    })) => self.adopt_allocation(claim, work, reused, Some(recovery)),
                    Ok(AllocationSelection::Current(AllocationOutcome::NoEligible {
                        reason,
                        evidence,
                    })) => {
                        let mut text = if reason.0 == "no_ready_work" {
                            "No eligible work is ready.".to_owned()
                        } else {
                            format!("No eligible work: {}", reason.0)
                        };
                        if let Some(fault) = evidence {
                            text.push_str(&format!(" {}", Failure::Refused(fault).describe()));
                        }
                        self.work.message = Some((Tone::Attention, text));
                        self.load_work();
                    }
                    Ok(AllocationSelection::Legacy(claim)) => {
                        self.work.message = Some((Tone::Neutral,"The saved claim was confirmed. Inspecting its current ownership and work.".into()));
                        self.inspect_allocation(claim.id.as_str());
                        self.load_work();
                    }
                    Err(error) => self.work_failure("Selecting work", error),
                }
            }
            (Slot::AllocationRelease(_), Reply::Command(outcome)) => {
                match wops::allocation_released(outcome) {
                    Ok(claim) => {
                        self.work.message = Some((
                            Tone::Good,
                            format!("Work claim {}: {:?}.", claim.id.as_str(), claim.state),
                        ));
                        self.work
                            .allocation_claims
                            .insert(claim.id.as_str().into(), claim);
                        self.load_work();
                    }
                    Err(error) => self.work_failure("Releasing work", error),
                }
            }
            (Slot::AllocationInspect(id), Reply::Query(query)) => {
                match wops::inspected_claim(query) {
                    Ok(claim) => {
                        self.work.allocation_claims.insert(id, claim);
                    }
                    Err(error) => {
                        let text =
                            format!("The claim could not be inspected: {}", error.describe());
                        self.work.allocation_inspect_errors.insert(id, text.clone());
                        self.work.message = Some((Tone::Bad, text));
                    }
                }
            }
            (slot, reply) => return Some((slot, reply)),
        }
        None
    }
    fn adopt_allocation(
        &mut self,
        claim: AllocationClaimRecord,
        work: Option<WorkRecord>,
        reused: bool,
        recovery: Option<AllocationRecovery>,
    ) {
        let title = work
            .as_ref()
            .map(|w| w.title.0.clone())
            .unwrap_or_else(|| claim.work_id.as_str().into());
        if let Some(work) = work {
            self.work.detail.insert(work.id.as_str().into(), work);
        }
        let id = claim.id.as_str().to_owned();
        if reused {
            self.work.allocation_reused.insert(id.clone());
        }
        if let Some(recovery) = recovery {
            self.work.allocation_recovery.insert(id.clone(), recovery);
        }
        self.work.allocation_claims.insert(id, claim);
        self.work.message = Some((
            Tone::Good,
            format!("Selected {title}. Open it to review its current controls; Start is separate."),
        ));
        self.load_work();
    }
    pub(super) fn workflow_dialog(&mut self, ctx: &egui::Context, actions: &mut Vec<Action>) {
        let Some(project) = self.work.of.clone() else {
            return;
        };
        let waiting = self.waiting(&Slot::WorkWorkflowCommand(project.clone()));
        let Some(workflow) = self.work.workflows.get_mut(&project) else {
            return;
        };
        workflow.busy |= waiting;
        if !workflow.open {
            return;
        }
        let mut open = true;
        egui::Window::new("Project workflow")
            .id(egui::Id::unique(("project-workflow", &project)))
            .open(&mut open)
            .default_width(620.0)
            .vscroll(true)
            .show(ctx, |ui| editor(ui, &project, workflow, actions));
        workflow.open = open;
    }
}

fn field(ui: &mut Ui, label: &str, value: &mut String) {
    ui.label(label);
    ui.add(egui::TextEdit::singleline(value))
        .widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, label));
}
fn claim_time(milliseconds: i64) -> String {
    jiff::Timestamp::from_millisecond(milliseconds)
        .map(|time| {
            time.to_zoned(jiff::tz::TimeZone::system())
                .strftime("%Y-%m-%d %H:%M:%S %Z")
                .to_string()
        })
        .unwrap_or_else(|_| "unknown time".into())
}
fn lease(work: &WorkUi) -> Option<u64> {
    work.claim_lease_ms
        .as_deref()
        .unwrap_or("300000")
        .parse()
        .ok()
        .filter(|n| *n > 0)
}
pub(super) fn next_task(
    ui: &mut Ui,
    work: &WorkUi,
    feature: &str,
    enabled: bool,
    actions: &mut Vec<Action>,
) {
    if ui
        .add_enabled(
            enabled
                && lease(work).is_some()
                && work.of.as_ref().is_some_and(|p| {
                    !work
                        .allocation_busy
                        .contains(&(p.clone(), Some(feature.into())))
                }),
            egui::Button::new("Next task"),
        )
        .clicked()
    {
        actions.push(Action::Allocate(
            Some(feature.into()),
            lease(work).expect("enabled lease"),
        ));
    }
}
pub(super) fn controls(
    ui: &mut Ui,
    work: &mut WorkUi,
    enabled: bool,
    runtime_available: bool,
    actions: &mut Vec<Action>,
) {
    let Some(project) = work.of.clone() else {
        return;
    };
    work.workflows.entry(project.clone()).or_default().editable = enabled;
    ui.horizontal_wrapped(|ui| {
        if ui.button("Project workflow").clicked() {
            work.workflows.entry(project.clone()).or_default().open = true;
        }
        if ui
            .add_enabled(
                enabled
                    && lease(work).is_some()
                    && !work.allocation_busy.contains(&(project.clone(), None)),
                egui::Button::new("Next work"),
            )
            .clicked()
        {
            actions.push(Action::Allocate(None, lease(work).expect("enabled lease")));
        }
        let mut value = work
            .claim_lease_ms
            .clone()
            .unwrap_or_else(|| "300000".into());
        ui.label("Claim lease (ms)");
        let response = ui.add(egui::TextEdit::singleline(&mut value).desired_width(90.0));
        response.widget_info(|| {
            egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Claim lease (ms)")
        });
        if response.changed() {
            work.claim_lease_ms = Some(value);
        }
    });
    let repositories = work.allocation_repositories.clone();
    let filters = work.allocation_filters.entry(project.clone()).or_default();
    egui::CollapsingHeader::new("Selection options").show(ui, |ui| {
        let repository = filters
            .repository_id
            .as_ref()
            .map(|id| {
                repositories
                    .iter()
                    .find(|(key, _)| key == id)
                    .map(|(_, name)| name.clone())
                    .unwrap_or_else(|| format!("{id} (no longer in this project)"))
            })
            .unwrap_or_else(|| "Any repository".into());
        egui::ComboBox::from_label("Selection repository")
            .selected_text(repository)
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut filters.repository_id, None, "Any repository");
                for (id, name) in &repositories {
                    ui.selectable_value(&mut filters.repository_id, Some(id.clone()), name);
                }
            });
        egui::ComboBox::from_label("Selection work type")
            .selected_text(filters.task_type.as_deref().unwrap_or("Any work type"))
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut filters.task_type, None, "Any work type");
                for kind in ["Story", "Task", "Bug"] {
                    ui.selectable_value(&mut filters.task_type, Some(kind.into()), kind);
                }
            });
        field(
            ui,
            "Selection label (exact label, optional)",
            &mut filters.label,
        );
        ui.checkbox(&mut filters.another, "Select another item");
        field(
            ui,
            "Selection retry key (optional)",
            &mut filters.idempotency_key,
        );
    });
    if lease(work).is_none() {
        widgets::caption(
            ui,
            "The claim lease must be a positive number of milliseconds.",
        );
    }
    if let Some(queue) = work.allocation_queues.get(&(project.clone(), None)) {
        widgets::caption(
            ui,
            format!(
                "Queue revision {} · workflow revision {}. Selecting work does not start execution.",
                queue.queue_revision.as_str(),
                queue.workflow_revision.as_str()
            ),
        );
    }
    // The legacy WorkRecord claim provides durable discovery after relaunch. Full ownership
    // and scope are then read from the dedicated allocation inspection, never inferred here.
    for claim in work
        .features
        .items
        .iter()
        .chain(&work.cards.items)
        .filter_map(|w| w.claim.as_ref())
    {
        if !work.allocation_claims.contains_key(claim.id.as_str()) {
            if let Some(error) = work.allocation_inspect_errors.get(claim.id.as_str()) {
                chip(ui, error.clone(), Tone::Attention);
                if ui
                    .button(format!("Retry claim {}", claim.id.as_str()))
                    .clicked()
                {
                    actions.push(Action::InspectClaim(claim.id.as_str().into()));
                }
            } else {
                actions.push(Action::InspectClaim(claim.id.as_str().into()));
            }
        }
    }
    let claims: Vec<_> = work
        .allocation_claims
        .values()
        .filter(|c| c.project_id.as_ref().map(|p| p.0.as_str()) == Some(project.as_str()))
        .cloned()
        .collect();
    for claim in claims {
        ui.group(|ui| {
            if let Some(error) = work.allocation_inspect_errors.get(claim.id.as_str()) { chip(ui,format!("Last known claim; {error}"),Tone::Attention); }
            let name = work
                .freshest(claim.work_id.as_str())
                .map(|w| w.title.0)
                .unwrap_or_else(|| claim.work_id.as_str().into());
            ui.label(RichText::new(format!("Selected: {name}")).strong());
            widgets::caption(
                ui,
                format!(
                    "Claim {} · {:?} · owner {} · expires {}",
                    claim.id.as_str(),
                    claim.state,
                    claim.owner_id.0.as_str(),
                    claim_time(claim.expires_at_ms.get())
                ),
            );
            widgets::caption(
                ui,
                format!(
                    "Work {} · selected kind {} · selection work revision {} · claim fence {} · selection queue revision {}",
                    claim.work_id.as_str(),
                    claim
                        .work_kind
                        .as_ref()
                        .map(|k| format!("{k:?}"))
                        .unwrap_or_else(|| "unknown".into()),
                    claim
                        .work_revision
                        .as_ref()
                        .map_or("unknown", |r| r.as_str()),
                    claim.fence.as_str(),
                    claim
                        .queue_revision
                        .as_ref()
                        .map_or("unknown", |r| r.as_str())
                ),
            );
            if let Some(session) = &claim.owner_session_id {
                widgets::caption(ui, format!("Owner session {}", session.as_str()));
            }
            if work.allocation_reused.contains(claim.id.as_str()) {
                widgets::caption(ui, "Existing selection reused.");
            }
            if let Some(recovery) = work.allocation_recovery.get(claim.id.as_str()) {
                egui::CollapsingHeader::new("Recovered work context")
                    .id_salt(claim.id.as_str())
                    .show(ui, |ui| {
                        if let Some(owner) = &recovery.previous_owner_id {
                            widgets::caption(ui, format!("Previous owner {}", owner.0.as_str()));
                        }
                        if let Some(session) = &recovery.previous_session_id
                            && ui
                                .button(format!("Open previous conversation {}", session.as_str()))
                                .clicked()
                        {
                            actions.push(Action::OpenGitSession(session.as_str().into()));
                        }
                        if let Some(handoff) = &recovery.latest_handoff {
                            ui.label(&handoff.summary.0);
                            ui.label(&handoff.next_steps.0);
                            for evidence in &handoff.evidence.0 {
                                widgets::caption(
                                    ui,
                                    format!(
                                        "Evidence artifact {} · version {}",
                                        evidence.artifact_id.as_str(),
                                        evidence.version_id.as_str()
                                    ),
                                );
                            }
                        }
                        if let Some(activity) = &recovery.last_activity {
                            widgets::caption(
                                ui,
                                format!(
                                    "Last activity {} #{} by {} at {}",
                                    activity.kind.0,
                                    activity.sequence.as_str(),
                                    activity.actor_id.0.as_str(),
                                    claim_time(activity.occurred_at_ms.get())
                                ),
                            );
                        }
                        if let Some(activity) = &recovery.last_activity && let Some(payload) = &activity.payload {
                            egui::CollapsingHeader::new("Recorded activity details").show(ui,|ui| {
                                match serde_json::to_string_pretty(&payload.value) {
                                    Ok(text) => { ui.monospace(text); },
                                    Err(_) => { widgets::caption(ui,"The recorded activity details could not be displayed."); },
                                }
                            });
                        }
                        if let Some(activity) = &recovery.last_activity
                            && let Some(why) = &activity.payload_unavailable_reason
                        {
                            widgets::caption(
                                ui,
                                format!("Activity details unavailable: {}", why.0),
                            );
                        }
                        for criterion in &recovery.unmet_acceptance.0 {
                            ui.label(format!(
                                "Unmet {}: {}",
                                criterion.id.as_str(),
                                criterion.text.0
                            ));
                        }
                        for link in &recovery.links.0 {
                            if let Some(url) = &link.url {
                                ui.hyperlink_to(&link.kind.0, &url.0);
                            }
                            if let Some(target) = &link.target_id {
                                widgets::caption(
                                    ui,
                                    format!("{}: {}", link.kind.0, target.as_str()),
                                );
                            }
                            if let Some(artifact) = &link.artifact {
                                widgets::caption(
                                    ui,
                                    format!(
                                        "{} artifact {} · version {}",
                                        link.kind.0,
                                        artifact.artifact_id.as_str(),
                                        artifact.version_id.as_str()
                                    ),
                                );
                            }
                        }
                    });
            }
            ui.horizontal_wrapped(|ui| {
                if ui.button(format!("Open selected {name}")).clicked() {
                    actions.push(Action::OpenWork(claim.work_id.as_str().into()));
                }
                if let Some(root) = &claim.root_id
                    && root.0 != claim.work_id
                    && ui.button("Open feature controls").clicked()
                {
                    actions.push(Action::OpenWork(root.0.as_str().into()));
                }
                if ui
                    .button(format!("Inspect claim {}", claim.id.as_str()))
                    .clicked()
                {
                    actions.push(Action::InspectClaim(claim.id.as_str().into()));
                }
                if ui
                    .add_enabled(
                        runtime_available && !matches!(claim.state,devworks_core::protocol::records::LeaseState::Released),
                        egui::Button::new(format!("Release claim {}", claim.id.as_str())),
                    )
                    .clicked()
                {
                    actions.push(Action::ReleaseClaim(claim.id.as_str().into()));
                }
            });
        });
    }
    egui::CollapsingHeader::new("Inspect another claim").show(ui, |ui| {
        field(ui, "Claim ID", &mut work.inspect_claim_id);
        if ui
            .add_enabled(
                runtime_available && !work.inspect_claim_id.is_empty(),
                egui::Button::new("Inspect claim"),
            )
            .clicked()
        {
            actions.push(Action::InspectClaim(work.inspect_claim_id.clone()));
        }
    });
    if let Some(workflow) = work.workflows.get(&project) {
        if let Some(error) = &workflow.error {
            chip(ui, error.clone(), Tone::Bad);
        } else if !workflow.loaded {
            widgets::caption(ui, "Loading the project's workflow…");
        }
    }
    ui.add_space(6.0);
}

fn editor(ui: &mut Ui, project: &str, workflow: &mut WorkflowUi, actions: &mut Vec<Action>) {
    widgets::caption(
        ui,
        "Names and order belong to this project. Each column keeps its fixed workflow meaning; execution controls are separate.",
    );
    if let Some(error) = &workflow.error {
        chip(ui, error.clone(), Tone::Bad);
    }
    if !workflow.loaded {
        widgets::caption(ui, "Loading workflow…");
        if ui.button("Refresh workflow").clicked() {
            actions.push(Action::Workflow(WorkflowAction::Reload(project.into())));
        }
        return;
    }
    let rows = workflow.rows.clone();
    let revision = workflow.revision();
    if let Some(revision) = &revision {
        widgets::caption(ui, format!("Workflow revision {revision}"));
    }
    let busy = workflow.busy;
    ui.add_enabled_ui(!busy,|ui| {
        for (index,status) in rows.iter().enumerate() {
            ui.group(|ui| {
                ui.label(RichText::new(&status.label.0).strong());
                widgets::caption(ui,format!("Meaning: {} · status revision {}",status.category.0,status.revision.as_str()));
                ui.horizontal_wrapped(|ui| {
                    if ui.button(format!("Rename {}",status.label.0)).clicked() {
                        workflow.draft = Some(Draft::Rename { snapshot:status.clone(),label:status.label.0.clone() });
                    }
                    for (earlier,neighbor) in [(true,index.checked_sub(1)),(false,(index+1 < rows.len()).then_some(index+1))] {
                        let name = format!("Move {} column {}",status.label.0,if earlier {"earlier"} else {"later"});
                        if ui.add_enabled(neighbor.is_some() && revision.is_some(),egui::Button::new(name)).clicked() {
                            let mut ids: Vec<_> = rows.iter().map(|s| s.id.clone()).collect();
                            ids.swap(index,neighbor.expect("enabled neighbor"));
                            actions.push(Action::Workflow(WorkflowAction::Submit {
                                project:project.into(),operation:"workflows.order",
                                body:json!({"projectId":project,"expectedWorkflowRevision":revision,"orderedStatusIds":ids}),
                            }));
                        }
                    }
                    if ui.button(format!("Remove {} column",status.label.0)).clicked() {
                        workflow.draft = Some(Draft::Remove { snapshot:status.clone(),destination:None });
                    }
                });
            });
        }
        if ui.add_enabled(revision.is_some(),egui::Button::new("New status")).clicked() {
            workflow.draft = Some(Draft::Create { revision:revision.clone().expect("enabled revision"),label:String::new(),category:"Backlog".into(),after_status_id:None });
        }
        let mut close = false;
        let Some(draft) = workflow.draft.as_mut() else { return; };
        ui.separator();
        match draft {
            Draft::Create { revision:base,label,category,after_status_id } => {
                ui.label(RichText::new("New project status").strong());
                field(ui,"Status name",label);
                egui::ComboBox::from_label("Status meaning").selected_text(category.as_str()).show_ui(ui,|ui| {
                    for (_,name,semantic) in devworks_core::services::work::workflow::DEFAULT_STATUSES { ui.selectable_value(category,semantic.to_owned(),name); }
                });
                let placement = after_status_id.as_ref().map(|id| rows.iter().find(|row|row.id.as_str()==id).map_or(id.as_str(),|row|row.label.0.as_str())).unwrap_or("End of workflow");
                egui::ComboBox::from_label("Place after").selected_text(placement).show_ui(ui,|ui| {
                    ui.selectable_value(after_status_id,None,"End of workflow");
                    for row in &rows { ui.selectable_value(after_status_id,Some(row.id.as_str().into()),&row.label.0); }
                });
                if Some(base.as_str()) != revision.as_deref() { stale(ui); }
                ui.horizontal_wrapped(|ui| {
                    if ui.add_enabled(!label.trim().is_empty(),egui::Button::new("Create status")).clicked() {
                        let mut body = json!({"projectId":project,"expectedWorkflowRevision":base,"label":label,"category":category});
                        if let Some(after) = after_status_id { body["afterStatusId"] = json!(after); }
                        actions.push(Action::Workflow(WorkflowAction::Submit { project:project.into(),operation:"workflows.create_status",body }));
                    }
                    if ui.button("Review current workflow").clicked() && let Some(current) = &revision { *base = current.clone(); }
                    if ui.button("Cancel status edit").clicked() { close = true; }
                });
            }
            Draft::Rename { snapshot,label } => {
                ui.label(RichText::new(format!("Rename {}",snapshot.label.0)).strong());
                widgets::caption(ui,format!("Meaning stays {}",snapshot.category.0));
                field(ui,"Status name",label);
                let current = rows.iter().find(|s| s.id == snapshot.id);
                if current.is_none_or(|s| s.revision != snapshot.revision) { stale(ui); }
                ui.horizontal_wrapped(|ui| {
                    if ui.add_enabled(!label.trim().is_empty(),egui::Button::new("Save status name")).clicked() {
                        actions.push(Action::Workflow(WorkflowAction::Submit { project:project.into(),operation:"workflows.rename_status",
                            body:json!({"projectId":project,"statusId":snapshot.id,"expectedRevision":snapshot.revision,"label":label}) }));
                    }
                    if ui.add_enabled(current.is_some(),egui::Button::new("Review current workflow")).clicked() && let Some(current) = current { *snapshot = current.clone(); }
                    if ui.button("Cancel status edit").clicked() { close = true; }
                });
            }
            Draft::Remove { snapshot,destination } => {
                ui.label(RichText::new(format!("Remove {}",snapshot.label.0)).strong());
                let count = snapshot.affected_work_count.as_ref().map(|n| n.as_str()).unwrap_or("unknown");
                widgets::caption(ui,format!("{count} work records will move together. Their history is kept. Choose a destination with the same meaning ({}).",snapshot.category.0));
                let current = rows.iter().find(|s| s.id == snapshot.id);
                if current.is_none_or(|s| s.workflow_revision != snapshot.workflow_revision || s.affected_work_digest != snapshot.affected_work_digest) { stale(ui); }
                widgets::caption(ui,format!("Removal snapshot: workflow {} · affected work {}",snapshot.workflow_revision.as_ref().map_or("unknown",|r| r.as_str()),snapshot.affected_work_digest.as_ref().map_or("unknown",|d| d.as_str())));
                let compatible: Vec<_> = rows.iter().filter(|s| s.id != snapshot.id && s.category.0 == snapshot.category.0).collect();
                let selected = destination.as_ref().and_then(|id| compatible.iter().find(|s| s.id.as_str() == id)).map_or("Choose a destination",|s| s.label.0.as_str());
                egui::ComboBox::from_label("Reassign work to").selected_text(selected).show_ui(ui,|ui| {
                    for status in &compatible { ui.selectable_value(destination,Some(status.id.as_str().into()),&status.label.0); }
                });
                if compatible.is_empty() { widgets::caption(ui,"Create another status with this meaning before removing this one."); }
                let ready = destination.as_ref().is_some_and(|id| compatible.iter().any(|s| s.id.as_str() == id)) && snapshot.workflow_revision.is_some() && snapshot.affected_work_digest.is_some();
                ui.horizontal_wrapped(|ui| {
                    if ui.add_enabled(ready,egui::Button::new("Reassign work and remove status")).clicked() {
                        actions.push(Action::Workflow(WorkflowAction::Submit { project:project.into(),operation:"workflows.reassign_and_remove",
                            body:json!({"projectId":project,"expectedWorkflowRevision":snapshot.workflow_revision,"removeStatusId":snapshot.id,"destinationStatusId":destination,"expectedAffectedWorkDigest":snapshot.affected_work_digest}) }));
                    }
                    if ui.add_enabled(current.is_some(),egui::Button::new("Review current workflow")).clicked() && let Some(current) = current { *snapshot = current.clone(); }
                    if ui.button("Cancel status edit").clicked() { close = true; }
                });
            }
        }
        if close { workflow.draft = None; }
    });
    if busy {
        widgets::caption(
            ui,
            "The workflow change is awaiting confirmation. Its exact request is kept if the outcome is unknown.",
        );
    }
}
fn stale(ui: &mut Ui) {
    chip(
        ui,
        "This snapshot changed. Your draft is kept; review the current workflow before retrying.",
        Tone::Attention,
    );
}
