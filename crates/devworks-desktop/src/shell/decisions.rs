//! Approval and notification view state. All eligibility, promotion, and wakeups are server-owned.
use super::*;
use crate::decision_ops as dops;
use devworks_core::protocol::records::{
    ApprovalContent, ApprovalReviewedContent, ApprovalState, ApprovalViewRecord,
    KnowledgeReviewItem, NotificationConfig, NotificationLink, NotificationRecord,
};
use devworks_core::protocol::results::OperationResult;
use serde_json::json;

#[derive(Default)]
pub struct DecisionsUi {
    pub project: Option<String>,
    pub tab: usize,
    pub history_state: Option<String>,
    pub approvals: Vec<ApprovalViewRecord>,
    gathering: Vec<ApprovalViewRecord>,
    pub loaded: bool,
    pub error: Option<String>,
    /// The inspected proposal is the exact displayed snapshot; a refresh never replaces a draft.
    pub selected: Option<ApprovalViewRecord>,
    inspecting: Option<String>,
    pub rationale: String,
    pub text: Option<ApprovalText>,
    pub decision_error: Option<String>,
    pub reviews: Vec<KnowledgeReviewItem>,
    review_gathering: Vec<KnowledgeReviewItem>,
    pub reviews_loaded: bool,
    pub notifications: Vec<NotificationRecord>,
    notification_gathering: Vec<NotificationRecord>,
    pub notifications_loaded: bool,
    pub notification_error: Option<String>,
    pub notification_project: Option<String>,
    pub unread_only: bool,
    pub unacknowledged_only: bool,
    pub category: String,
    pub notification: Option<NotificationRecord>,
    reading_notification: Option<String>,
    /// Complete authoritative unacknowledged verified-completion feed, independent of filters.
    pub completions: Vec<NotificationRecord>,
    completion_gathering: Vec<NotificationRecord>,
    pub completion_error: Option<String>,
    pub permission: Option<devworks_core::ports::os_notifications::OsNotificationPermission>,
    pub config: Option<(NotificationConfig, String)>,
    pub config_draft: Option<NotificationConfig>,
    config_saving: Option<NotificationConfig>,
}

pub struct ApprovalText {
    artifact: devworks_core::protocol::provider::ArtifactRef,
    pages: Vec<String>,
    page: usize,
    next: Option<String>,
    error: Option<String>,
}

#[derive(Clone)]
enum Action {
    Reload,
    Inspect(String),
    Decide(bool),
    ReloadNotifications,
    Read(String),
    MarkRead(NotificationRecord, bool),
    Acknowledge(NotificationRecord),
    Link(NotificationLink),
    Approval(String),
    OpenKnowledge(devworks_core::protocol::Id),
    SaveConfig,
    RequestPermission,
    NextText,
    PreviousText,
    OpenWork(String),
    ReloadConfig,
}

impl Shell {
    fn unresolved(&self, slot: &Slot) -> bool {
        self.client
            .as_ref()
            .is_some_and(|c| c.unresolved().iter().any(|u| &u.slot == slot))
    }
    pub(super) fn load_approvals(&mut self) {
        self.decisions.gathering.clear();
        self.approval_page(None);
        self.decisions.review_gathering.clear();
        self.review_page(None);
    }
    fn approval_page(&mut self, cursor: Option<String>) {
        let mut body = json!({"scope":dops::scope(self.decisions.project.as_deref()), "page":dops::page(cursor.as_deref())});
        if self.decisions.tab == 0 {
            body["state"] = json!("Pending");
        } else if let Some(state) = &self.decisions.history_state {
            body["state"] = json!(state);
        }
        self.query(
            Slot::ApprovalsList,
            "Loading approvals",
            "approvals.list",
            move |b| dops::query(b, "approvals.list", body),
        );
    }
    fn review_page(&mut self, cursor: Option<String>) {
        let mut body = json!({"page":dops::page(cursor.as_deref())});
        if let Some(p) = &self.decisions.project {
            body["projectId"] = json!(p);
        }
        self.query(
            Slot::ReviewQueue,
            "Loading Knowledge review queue",
            "approvals.review_queue",
            move |b| dops::query(b, "approvals.review_queue", body),
        );
    }
    pub(super) fn load_completion_notifications(&mut self) {
        if self.pending(&Slot::CompletionList) {
            return;
        }
        self.decisions.completion_gathering.clear();
        self.notification_page(true, None);
    }
    pub(super) fn load_notification_history(&mut self) {
        self.decisions.notification_gathering.clear();
        self.notification_page(false, None);
        if self.decisions.config.is_none() && !self.pending(&Slot::NotificationSettings) {
            self.query(
                Slot::NotificationSettings,
                "Loading notification settings",
                "notifications.settings",
                |b| ops::query_frame(b, "notifications.settings", json!({})),
            );
        }
    }
    fn notification_page(&mut self, completion: bool, cursor: Option<String>) {
        let mut body = if completion {
            json!({"scope":dops::scope(None),"page":dops::page(cursor.as_deref()),"unacknowledgedOnly":true,"completionOnly":true})
        } else {
            json!({"scope":dops::scope(self.decisions.notification_project.as_deref()),"page":dops::page(cursor.as_deref()),"unacknowledgedOnly":self.decisions.unacknowledged_only})
        };
        if !completion {
            if self.decisions.unread_only {
                body["read"] = json!(false);
            }
            if !self.decisions.category.trim().is_empty() {
                body["categories"] = json!([self.decisions.category.trim()]);
            }
        }
        let slot = if completion {
            Slot::CompletionList
        } else {
            Slot::NotificationsList
        };
        self.query(
            slot,
            "Loading notifications",
            "notifications.list",
            move |b| dops::query(b, "notifications.list", body),
        );
    }
    fn inspect_approval(&mut self, id: String) {
        let key = id.clone();
        if self
            .decisions
            .selected
            .as_ref()
            .is_some_and(|p| p.id.as_str() != id)
        {
            self.decisions.selected = None;
            self.decisions.rationale.clear();
            self.decisions.text = None;
        }
        self.decisions.inspecting = Some(id.clone());
        self.query(
            Slot::ApprovalInspect(key),
            "Inspecting exact approval content",
            "approvals.inspect",
            move |b| dops::query(b, "approvals.inspect", json!({"proposalId":id})),
        );
    }
    fn load_approval_text(&mut self) {
        let (Some(p), Some(text)) = (&self.decisions.selected, &self.decisions.text) else {
            return;
        };
        let id = p.id.as_str().to_owned();
        let artifact = text.artifact.clone();
        let cursor = text.next.clone();
        if self.pending(&Slot::ApprovalContent(id.clone())) {
            return;
        }
        self.query(
            Slot::ApprovalContent(id),
            "Reading exact proposal artifact",
            "artifacts.read",
            move |b| {
                let mut frame: serde_json::Value =
                    serde_json::from_slice(&ops::artifact_read(b, &artifact))
                        .expect("typed artifact frame");
                if let Some(cursor) = cursor {
                    frame["cursor"] = json!(cursor);
                }
                serde_json::to_vec(&frame).expect("typed artifact frame")
            },
        );
    }
    fn decide(&mut self, approve: bool) {
        let Some(p) = &self.decisions.selected else {
            return;
        };
        let slot = Slot::ApprovalDecision(p.id.as_str().to_owned());
        if self.online().is_err()
            || self.pending(&slot)
            || self.unresolved(&slot)
            || self.decisions.rationale.trim().is_empty()
        {
            return;
        }
        let body = json!({"proposalId":p.id,"expectedRevision":p.revision,"contentDigest":p.content_digest,
            "decision":if approve {"Approve"} else {"Reject"},"rationale":self.decisions.rationale});
        let Some(b) = self.backend() else {
            return;
        };
        let frame = dops::command(&*b, "approvals.record_human_decision", body);
        if let Some(c) = &mut self.client {
            c.human_decision(slot, "Recording your approval decision", frame);
        }
    }
    fn change_notification(&mut self, record: NotificationRecord, read: Option<bool>) {
        let id = record.id.as_str().to_owned();
        let slot = Slot::NotificationChange(id.clone());
        if self.pending(&slot) || self.unresolved(&slot) {
            return;
        }
        let op = if read.is_some() {
            "notifications.mark_read"
        } else {
            "notifications.acknowledge"
        };
        let mut body = json!({"notificationId":record.id,"expectedRevision":record.revision});
        if let Some(read) = read {
            body["read"] = json!(read);
        }
        self.command(slot, "Updating notification", op, move |b| {
            dops::command(b, op, body)
        });
    }
    fn notification_updated(&mut self, record: NotificationRecord) {
        for rows in [
            &mut self.decisions.notifications,
            &mut self.decisions.completions,
        ] {
            if let Some(row) = rows
                .iter_mut()
                .find(|n| n.id.as_str() == record.id.as_str())
            {
                *row = record.clone();
            }
        }
        if self
            .decisions
            .notification
            .as_ref()
            .is_some_and(|n| n.id.as_str() == record.id.as_str())
        {
            self.decisions.notification = Some(record);
        }
        self.decisions.completions.retain(|n| !n.acknowledged);
        self.sync_completion_cards();
    }
    fn sync_completion_cards(&mut self) {
        let cards = self
            .decisions
            .completions
            .iter()
            .filter(|n| !n.acknowledged)
            .map(|n| {
                let project = n
                    .project_id
                    .as_ref()
                    .map(|p| {
                        self.projects
                            .list
                            .items
                            .iter()
                            .find(|x| x.id.0.as_str() == p.0.as_str())
                            .map_or_else(|| p.0.as_str().to_owned(), |x| x.name.0.clone())
                    })
                    .unwrap_or_else(|| "Workspace".into());
                crate::notifications::CompletionCard {
                    id: n.id.as_str().into(),
                    project,
                    kind: n
                        .category
                        .as_ref()
                        .map_or("Verified completion", |c| c.0.as_str())
                        .into(),
                    title: n.title.0.clone(),
                    at: display_time(n.created_at_ms.get()),
                    link: "completed work".into(),
                }
            })
            .collect();
        self.completions.reconcile(cards);
    }
    pub(super) fn apply_decisions(&mut self, slot: Slot, reply: Reply) -> Option<(Slot, Reply)> {
        match (slot, reply) {
            (Slot::ApprovalsList, Reply::Query(q)) => match dops::approvals(q) {
                Ok((rows, next)) => {
                    self.decisions.gathering.extend(rows);
                    if next.is_some() {
                        self.approval_page(next);
                    } else {
                        self.decisions.approvals = std::mem::take(&mut self.decisions.gathering);
                        self.decisions.loaded = true;
                        self.decisions.error = None;
                    }
                }
                Err(e) => self.decisions.error = Some(e.describe()),
            },
            (Slot::ApprovalInspect(id), Reply::Query(_))
                if self.decisions.inspecting.as_deref() != Some(id.as_str()) => {}
            (Slot::ApprovalInspect(_), Reply::Query(q)) => match dops::approval(q) {
                Ok(p) => {
                    let artifact = match &p.reviewed_source {
                        Some(ApprovalReviewedContent::FeaturePlan { plan }) => {
                            Some(plan.content.clone())
                        }
                        _ => p.artifact.clone(),
                    };
                    self.decisions.text = artifact.map(|artifact| ApprovalText {
                        artifact,
                        pages: Vec::new(),
                        page: 0,
                        next: None,
                        error: None,
                    });
                    self.decisions.selected = Some(p);
                    self.decisions.inspecting = None;
                    self.decisions.decision_error = None;
                    self.load_approval_text();
                }
                Err(e) => {
                    self.decisions.inspecting = None;
                    self.decisions.decision_error = Some(e.describe());
                }
            },
            (Slot::ApprovalContent(id), Reply::Query(q)) => {
                if self
                    .decisions
                    .selected
                    .as_ref()
                    .is_some_and(|p| p.id.as_str() == id)
                {
                    if let Some(text) = &mut self.decisions.text {
                        match ops::read_content(q) {
                            Ok(devworks_core::protocol::records::ArtifactReadContent::Text {
                                artifact,
                                content,
                                next_byte,
                                truncated,
                                ..
                            }) if artifact.version_id.as_str()
                                == text.artifact.version_id.as_str()
                                && artifact.sha256.as_str() == text.artifact.sha256.as_str() =>
                            {
                                text.pages.push(content.0);
                                text.page = text.pages.len() - 1;
                                text.next = truncated.then(|| next_byte.as_str().to_owned());
                                text.error = None;
                            }
                            Ok(_) => {
                                text.error = Some(
                                    "The exact proposal artifact could not be displayed as text."
                                        .into(),
                                )
                            }
                            Err(e) => text.error = Some(e.describe()),
                        }
                    }
                }
            }
            (Slot::ApprovalDecision(_), Reply::Command(q)) => match dops::decided(q) {
                Ok(p) => {
                    self.decisions.selected = Some(p);
                    self.decisions.decision_error = None;
                    self.decisions.rationale.clear();
                    self.load_approvals();
                    self.load_completion_notifications();
                }
                Err(e) => self.decisions.decision_error = Some(e.describe()),
            },
            (Slot::ReviewQueue, Reply::Query(q)) => match dops::reviews(q) {
                Ok((rows, next)) => {
                    self.decisions.review_gathering.extend(rows);
                    if next.is_some() {
                        self.review_page(next);
                    } else {
                        self.decisions.reviews =
                            std::mem::take(&mut self.decisions.review_gathering);
                        self.decisions.reviews_loaded = true;
                    }
                }
                Err(e) => self.decisions.error = Some(e.describe()),
            },
            (Slot::CompletionList, Reply::Query(q)) => match dops::notifications(q) {
                Ok((rows, next)) => {
                    self.decisions.completion_gathering.extend(rows);
                    if next.is_some() {
                        self.notification_page(true, next);
                    } else {
                        self.decisions.completions =
                            std::mem::take(&mut self.decisions.completion_gathering);
                        self.decisions.completions.sort_by(|a, b| {
                            b.created_at_ms
                                .get()
                                .cmp(&a.created_at_ms.get())
                                .then_with(|| b.id.as_str().cmp(a.id.as_str()))
                        });
                        self.decisions.completion_error = None;
                        self.sync_completion_cards();
                    }
                }
                Err(e) => self.decisions.completion_error = Some(e.describe()),
            },
            (Slot::NotificationsList, Reply::Query(q)) => match dops::notifications(q) {
                Ok((rows, next)) => {
                    self.decisions.notification_gathering.extend(rows);
                    if next.is_some() {
                        self.notification_page(false, next);
                    } else {
                        self.decisions.notifications =
                            std::mem::take(&mut self.decisions.notification_gathering);
                        self.decisions.notifications_loaded = true;
                        self.decisions.notification_error = None;
                    }
                }
                Err(e) => {
                    self.decisions.notifications_loaded = true;
                    self.decisions.notification_error = Some(e.describe());
                }
            },
            (Slot::NotificationRead(id), Reply::Query(_))
                if self.decisions.reading_notification.as_deref() != Some(id.as_str()) => {}
            (Slot::NotificationRead(_), Reply::Query(q)) => match dops::notification(q) {
                Ok(n) => self.decisions.notification = Some(n),
                Err(e) => self.decisions.notification_error = Some(e.describe()),
            },
            (Slot::NotificationChange(id), Reply::Command(q)) => match dops::notified(q) {
                Ok(n) => {
                    self.completions.set_status(&id, None);
                    self.notification_updated(n);
                    self.decisions.notification_error = None;
                    self.load_notification_history();
                    self.decisions.completion_gathering.clear();
                    self.notification_page(true, None);
                }
                Err(e) => {
                    self.completions.set_status(&id, Some(e.describe()));
                    self.decisions.notification_error = Some(e.describe());
                }
            },
            (Slot::NotificationSettings, Reply::Query(q)) => match ops::query_result(q) {
                Ok((OperationResult::NotificationsSettings { value }, _)) => {
                    if self.decisions.config_draft.is_none() {
                        self.decisions.config_draft = Some(value.config.clone());
                    }
                    self.decisions.config =
                        Some((value.config, value.revision.as_str().to_owned()));
                }
                Ok(_) => {
                    self.decisions.notification_error =
                        Some("Unexpected notification settings reply".into())
                }
                Err(e) => self.decisions.notification_error = Some(e.describe()),
            },
            (Slot::NotificationSettingsSave, Reply::Command(q)) => match ops::command_result(q) {
                Ok(OperationResult::NotificationsSettings { value }) => {
                    if serde_json::to_value(&self.decisions.config_draft).ok()
                        == serde_json::to_value(&self.decisions.config_saving).ok()
                    {
                        self.decisions.config_draft = Some(value.config.clone());
                    }
                    self.decisions.config_saving = None;
                    self.decisions.config =
                        Some((value.config, value.revision.as_str().to_owned()));
                }
                Ok(_) => {
                    self.decisions.notification_error =
                        Some("Unexpected notification settings reply".into())
                }
                Err(e) => self.decisions.notification_error = Some(e.describe()),
            },
            (Slot::OsNotificationPermission, Reply::OsNotificationPermission(value)) => {
                self.decisions.permission = Some(value);
            }
            (s, r) => return Some((s, r)),
        }
        None
    }
    fn decision_actions(&mut self, ctx: &egui::Context, actions: Vec<Action>) {
        for a in actions {
            match a {
                Action::Reload => {
                    self.decisions.approvals.clear();
                    self.decisions.loaded = false;
                    self.decisions.reviews.clear();
                    self.decisions.reviews_loaded = false;
                    self.load_approvals();
                }
                Action::Inspect(id) => self.inspect_approval(id),
                Action::Decide(approve) => self.decide(approve),
                Action::ReloadNotifications => {
                    self.decisions.notifications.clear();
                    self.decisions.notifications_loaded = false;
                    self.load_notification_history();
                }
                Action::Read(id) => {
                    self.decisions.reading_notification = Some(id.clone());
                    let key = id.clone();
                    self.query(
                        Slot::NotificationRead(key),
                        "Reading notification",
                        "notifications.read",
                        move |b| dops::query(b, "notifications.read", json!({"notificationId":id})),
                    );
                }
                Action::MarkRead(n, read) => self.change_notification(n, Some(read)),
                Action::Acknowledge(n) => self.change_notification(n, None),
                Action::Approval(id) => {
                    self.navigate(Route::Approvals);
                    self.inspect_approval(id);
                }
                Action::OpenKnowledge(id) => {
                    self.navigate(Route::Knowledge);
                    self.knowledge_actions(vec![KnowledgeAction::Select(id)]);
                }
                Action::Link(link) => self.open_notification_link(ctx, link),
                Action::NextText => {
                    if let Some(text) = &mut self.decisions.text {
                        if text.page + 1 < text.pages.len() {
                            text.page += 1;
                        } else if text.next.is_some() {
                            self.load_approval_text();
                        }
                    }
                }
                Action::OpenWork(id) => self.open_work(&id),
                Action::PreviousText => {
                    if let Some(text) = &mut self.decisions.text {
                        text.page = text.page.saturating_sub(1);
                    }
                }
                Action::RequestPermission => {
                    if let Some(c) = &mut self.client {
                        c.request_notification_permission();
                    }
                }
                Action::ReloadConfig => {
                    self.decisions.config = None;
                    self.decisions.config_draft = None;
                    self.load_notification_history();
                }
                Action::SaveConfig => {
                    if let (Some((_, revision)), Some(config)) =
                        (&self.decisions.config, &self.decisions.config_draft)
                    {
                        let body = json!({"expectedRevision":revision,"config":config});
                        self.decisions.config_saving = Some(config.clone());
                        self.command(
                            Slot::NotificationSettingsSave,
                            "Saving notification settings",
                            "notifications.settings",
                            move |b| ops::command_frame(b, "notifications.settings", body),
                        );
                    }
                }
            }
        }
    }
    fn open_notification_link(&mut self, ctx: &egui::Context, link: NotificationLink) {
        if let Some(artifact) = &link.artifact {
            self.routes.entry(Route::Artifacts).or_default().selection =
                Some(artifact.artifact_id.as_str().to_owned());
        }
        if let Some(id) = link.target_id {
            match link.kind.0.to_ascii_lowercase().as_str() {
                "approval" | "decision" => {
                    self.navigate(Route::Approvals);
                    self.inspect_approval(id.as_str().into());
                }
                "knowledge" => {
                    self.navigate(Route::Knowledge);
                    self.knowledge_actions(vec![KnowledgeAction::Select(id)]);
                }
                "artifact" => {
                    self.navigate(Route::Artifacts);
                }
                "feature" | "issue" | "task" | "work" | "root" => self.open_work(id.as_str()),
                "session" | "chat" => {
                    let id = id.as_str().to_owned();
                    self.chat.selected = Some(id.clone());
                    self.load_transcript(&id);
                    self.navigate(Route::Chat);
                }
                "project" => {
                    self.select_project(Some(id.as_str().into()));
                    self.navigate(Route::Overview);
                }
                _ => {
                    self.status = Some(format!(
                        "No native view is registered for {} target {}.",
                        link.kind.0,
                        id.as_str()
                    ))
                }
            }
        } else if link.artifact.is_some() {
            self.navigate(Route::Artifacts);
        } else if let Some(url) = link.url {
            ctx.open_url(egui::OpenUrl::new_tab(url.0));
        }
    }
    pub(super) fn completion_action(&mut self, ctx: &egui::Context, action: CardAction) {
        let id = match &action {
            CardAction::Open(id) | CardAction::Dismiss(id) => id,
        };
        let Some(n) = self
            .decisions
            .completions
            .iter()
            .find(|n| n.id.as_str() == id)
            .cloned()
        else {
            return;
        };
        match action {
            CardAction::Dismiss(_) => {
                self.completions
                    .set_status(n.id.as_str(), Some("Confirming dismissal…".into()));
                self.change_notification(n, None);
            }
            CardAction::Open(_) => {
                if let Some(root) = n.root_id {
                    self.open_work(root.0.as_str());
                } else if let Some(link) = n.links.as_ref().and_then(|x| x.0.first()).cloned() {
                    self.open_notification_link(ctx, link);
                } else {
                    self.notifications_open = true;
                    self.decision_actions(ctx, vec![Action::Read(n.id.as_str().into())]);
                }
            }
        }
    }
    pub(super) fn approvals_screen(&mut self, ui: &mut Ui) {
        let online = self.online().is_ok();
        let mut actions = Vec::new();
        let projects: Vec<_> = self
            .projects
            .list
            .items
            .iter()
            .map(|p| (p.id.0.as_str().to_owned(), p.name.0.clone()))
            .collect();
        let pending = self.decisions.inspecting.is_some()
            || self.decisions.selected.as_ref().is_some_and(|p| {
                self.pending(&Slot::ApprovalDecision(p.id.as_str().into()))
                    || self.unresolved(&Slot::ApprovalDecision(p.id.as_str().into()))
            });
        let tab = self.decisions.tab;
        let mut newtab = tab;
        widgets::with_right_sidebar(
            ui,
            theme::SIDEBAR,
            &mut (&mut self.decisions, &mut actions),
            |ui, state| {
                let (_, selected_tab) = widgets::pane(
                    ui,
                    Header {
                        title: "Approvals",
                        summary: "Decisions waiting for you across every project.",
                        tabs: &["Inbox", "History", "Knowledge review"],
                        tab,
                    },
                    state,
                    |ui, state| {
                        let old = state.0.project.clone();
                        project_filter(
                            ui,
                            "Approval project filter",
                            &projects,
                            &mut state.0.project,
                        );
                        if state.0.project != old {
                            state.1.push(Action::Reload);
                        }
                        if tab == 1 {
                            let old = state.0.history_state.clone();
                            let filter = egui::ComboBox::from_id_salt("approval-history-state")
                                .selected_text(
                                    state.0.history_state.as_deref().unwrap_or("All states"),
                                )
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(
                                        &mut state.0.history_state,
                                        None,
                                        "All states",
                                    );
                                    for name in
                                        ["Pending", "Approved", "Rejected", "Superseded", "Expired"]
                                    {
                                        ui.selectable_value(
                                            &mut state.0.history_state,
                                            Some(name.into()),
                                            name,
                                        );
                                    }
                                });
                            filter.response.widget_info(|| {
                                egui::WidgetInfo::labeled(
                                    egui::Role::ComboBox,
                                    true,
                                    "Approval state filter",
                                )
                            });
                            if old != state.0.history_state {
                                state.1.push(Action::Reload);
                            }
                        }

                        if ui.button("Refresh approvals").clicked() {
                            state.1.push(Action::Reload);
                        }
                    },
                    |ui, state, tab| {
                        let (d, actions) = state;
                        if !online {
                            ui.label(
                                "Application offline. Decisions require a connected native host.",
                            );
                        }
                        if let Some(e) = &d.error {
                            ui.colored_label(theme::RED, e);
                        }
                        egui::ScrollArea::vertical()
                            .id_salt("approval-inbox")
                            .show(ui, |ui| {
                                if tab == 2 {
                                    if !d.reviews_loaded {
                                        ui.label("Loading Knowledge review queue…");
                                    }
                                    if d.reviews_loaded && d.reviews.is_empty() {
                                        ui.label("No Knowledge entries need review.");
                                    }
                                    for r in &d.reviews {
                                        ui.group(|ui| {
                                            ui.label(format!(
                                                "{:?} · {} · {} occurrences",
                                                r.kind,
                                                r.priority.0,
                                                r.recurrence.as_str()
                                            ));
                                            json_view(ui, "Review evidence", r);
                                            if ui
                                                .button(format!(
                                                    "Review Knowledge {}",
                                                    r.entry.id.as_str()
                                                ))
                                                .clicked()
                                            {
                                                actions.push(Action::OpenKnowledge(
                                                    r.entry.id.clone(),
                                                ));
                                            }
                                        });
                                    }
                                } else {
                                    if !d.loaded {
                                        ui.label("Loading approvals…");
                                    }
                                    if d.loaded && d.approvals.is_empty() {
                                        ui.label("No approvals in this view.");
                                    }
                                    for p in &d.approvals {
                                        ui.push_id(p.id.as_str(), |ui| {
                                            ui.group(|ui| {
                                                ui.label(RichText::new(&p.summary.0).strong());
                                                ui.label(format!(
                                                    "{} · {:?} · target {} @ {}",
                                                    p.operation_kind.0,
                                                    p.state,
                                                    p.target_id.as_str(),
                                                    p.target_revision.as_str()
                                                ));
                                                if ui
                                                    .button(format!(
                                                        "Inspect proposal {}",
                                                        p.id.as_str()
                                                    ))
                                                    .clicked()
                                                {
                                                    actions.push(Action::Inspect(
                                                        p.id.as_str().into(),
                                                    ));
                                                }
                                            });
                                        });
                                    }
                                }
                            });
                    },
                );
                newtab = selected_tab;
            },
            |ui, state| {
                Frame::new()
                    .fill(theme::PANEL)
                    .inner_margin(theme::margin(14, 12))
                    .show(ui, |ui| {
                        ui.heading("Decision");
                        egui::ScrollArea::vertical()
                            .id_salt("approval-details")
                            .show(ui, |ui| {
                                approval_detail(ui, state.0, state.1, online && !pending);
                            });
                    });
            },
        );
        if newtab != tab {
            self.decisions.tab = newtab;
            self.load_approvals();
        }
        self.routes.entry(Route::Approvals).or_default().tab = self.decisions.tab;
        self.decision_actions(ui.ctx(), actions);
    }
    pub(super) fn notification_center(&mut self, ctx: &egui::Context) {
        if !self.notifications_open {
            return;
        }
        if !self.decisions.notifications_loaded && !self.pending(&Slot::NotificationsList) {
            self.load_notification_history();
        }
        let mut open = true;
        let mut actions = Vec::new();
        if let Some(b) = self.backend() {
            self.decisions.permission = Some(b.os_notification_permission());
        }
        let requesting_permission = self.pending(&Slot::OsNotificationPermission);
        let preferences_busy = self.pending(&Slot::NotificationSettingsSave)
            || self.unresolved(&Slot::NotificationSettingsSave)
            || self.online().is_err();
        let projects: Vec<_> = self
            .projects
            .list
            .items
            .iter()
            .map(|p| (p.id.0.as_str().to_owned(), p.name.0.clone()))
            .collect();
        let busy: std::collections::BTreeSet<_> = self
            .decisions
            .notifications
            .iter()
            .filter(|n| {
                self.pending(&Slot::NotificationChange(n.id.as_str().into()))
                    || self.unresolved(&Slot::NotificationChange(n.id.as_str().into()))
            })
            .map(|n| n.id.as_str().to_owned())
            .collect();
        egui::Window::new("Notifications").open(&mut open).collapsible(false).resizable(true).default_width(520.0).max_height((ctx.content_rect().height()-TOOLBAR-40.0).max(180.0)).anchor(egui::Align2::RIGHT_TOP,egui::vec2(-12.0,TOOLBAR+4.0)).show(ctx,|ui|{
            let d=&mut self.decisions;
            let old=(d.notification_project.clone(),d.unread_only,d.unacknowledged_only);
            project_filter(ui,"Notification project filter",&projects,&mut d.notification_project);
            ui.horizontal(|ui| {ui.checkbox(&mut d.unread_only,"Unread only");ui.checkbox(&mut d.unacknowledged_only,"Unacknowledged only");});
            ui.horizontal(|ui|{ui.label("Category");ui.add(egui::TextEdit::singleline(&mut d.category).hint_text("All categories").desired_width(160.0));if ui.button("Filter notifications").clicked(){actions.push(Action::ReloadNotifications);}});
            if old!=(d.notification_project.clone(),d.unread_only,d.unacknowledged_only){actions.push(Action::ReloadNotifications);}
            ui.label("Reading, acknowledging, and opening links never approve or resume work.");
            if let Some(e)=&d.notification_error {ui.colored_label(theme::RED,e);}
            if let Some(e)=&d.completion_error {ui.colored_label(theme::RED,format!("Completion feed: {e}"));}
            egui::ScrollArea::vertical().id_salt("notification-history").max_height(340.0).show(ui,|ui|{
                if d.notifications_loaded&&d.notifications.is_empty(){ui.label("No notifications in this view.");}
                for n in &d.notifications {
                    ui.push_id(n.id.as_str(),|ui|{ui.group(|ui|{
                        ui.label(RichText::new(&n.title.0).strong());
                    if n.requires_input==Some(true){ui.label("Input required");}
                    ui.label(format!("{} · {} · {}",n.category.as_ref().map_or("Notification",|c|c.0.as_str()),if n.read==Some(true){"Read"}else{"Unread"},if n.acknowledged{"Acknowledged"}else{"Unacknowledged"}));
                        if ui.button(format!("Read {}",n.title.0)).clicked(){actions.push(Action::Read(n.id.as_str().into()));}
                        ui.add_enabled_ui(!busy.contains(n.id.as_str()),|ui|{
                            if ui.button(if n.read==Some(true){"Mark unread"}else{"Mark read"}).clicked(){actions.push(Action::MarkRead(n.clone(),n.read!=Some(true)));}
                            if !n.acknowledged&&ui.button(format!("Acknowledge {}",n.title.0)).clicked(){actions.push(Action::Acknowledge(n.clone()));}
                        });
                    });});
                }
            });
            if let Some(n)=&d.notification {
                ui.separator();ui.heading(&n.title.0);ui.label(&n.body.0);ui.label(format!("Notification {} @ {}",n.id.as_str(),n.revision.as_str()));
                if let Some(id)=&n.decision_id {if ui.button("Open linked decision").clicked(){actions.push(Action::Approval(id.as_str().into()));}}
                if let Some(links)=&n.links {for link in &links.0 {if let Some(url)=&link.url {ui.hyperlink_to(&link.kind.0,&url.0);}if link.target_id.is_some()||link.artifact.is_some(){if ui.button(format!("Open {}",link.kind.0)).clicked(){actions.push(Action::Link(link.clone()));}}}}
            }
            ui.collapsing("Notification preferences",|ui|{
                if let Some(config)=&mut d.config_draft {
                    ui.checkbox(&mut config.desktop_enabled,"Desktop notifications");
                    ui.label("Required-input desktop notifications are attempted only while the app is unfocused and permission allows them. In-app history remains available.");
                    if let Some(permission)=&d.permission { ui.label(permission_label(permission)); }
                    if ui.add_enabled(!requesting_permission,egui::Button::new("Request notification permission")).clicked() { actions.push(Action::RequestPermission); }
                    let mut kinds=config.kinds.0.iter().map(|k|k.0.clone()).collect::<Vec<_>>().join(", ");
                    ui.label("Notification kinds (comma separated)");
                    if ui.text_edit_singleline(&mut kinds).changed(){config.kinds.0=kinds.split(',').map(str::trim).filter(|x|!x.is_empty()).map(|x|devworks_core::protocol::support::BoundedText(x.into())).collect();}
                    if ui.add_enabled(!preferences_busy,egui::Button::new("Save notification preferences")).clicked(){actions.push(Action::SaveConfig);}
                    if ui.button("Discard preference draft and reload").clicked(){actions.push(Action::ReloadConfig);}
                    if let Some((_,revision))=&d.config {ui.label(format!("Preferences revision {revision}"));}
                }
            });
        });
        self.notifications_open = open;
        self.decision_actions(ctx, actions);
    }
}
fn project_filter(
    ui: &mut Ui,
    id: &str,
    projects: &[(String, String)],
    selected: &mut Option<String>,
) {
    let label = selected
        .as_ref()
        .and_then(|s| projects.iter().find(|(id, _)| id == s))
        .map_or("All projects", |(_, name)| name.as_str());
    let filter = egui::ComboBox::from_id_salt(id)
        .selected_text(label)
        .show_ui(ui, |ui| {
            ui.selectable_value(selected, None, "All projects");
            for (id, name) in projects {
                ui.selectable_value(selected, Some(id.clone()), name);
            }
        });
    filter
        .response
        .widget_info(|| egui::WidgetInfo::labeled(egui::Role::ComboBox, true, id));
}
fn json_view(ui: &mut Ui, label: &str, value: &impl serde::Serialize) {
    ui.collapsing(label, |ui| {
        let text = serde_json::to_string_pretty(value)
            .unwrap_or_else(|e| format!("Content unavailable: {e}"));
        ui.add(
            egui::Label::new(RichText::new(text).monospace())
                .wrap()
                .selectable(true),
        );
    });
}
fn approval_detail(ui: &mut Ui, d: &mut DecisionsUi, actions: &mut Vec<Action>, enabled: bool) {
    if let Some(e) = &d.decision_error {
        ui.colored_label(theme::RED, e);
    }
    let Some(p) = &d.selected else {
        ui.label("Inspect a proposal to review its exact content and binding.");
        return;
    };
    ui.label(RichText::new(&p.summary.0).strong());
    ui.label(format!(
        "Proposal {} @ {}",
        p.id.as_str(),
        p.revision.as_str()
    ));
    ui.label(format!("{} · {:?}", p.operation_kind.0, p.state));
    ui.label(format!(
        "Target {} @ {}",
        p.target_id.as_str(),
        p.target_revision.as_str()
    ));
    ui.label(format!("Content digest: {}", p.content_digest.as_str()));
    if let Some(actor) = &p.requested_by {
        ui.label(format!("Requested by {}", actor.0.as_str()));
    }
    if let Some(at) = &p.created_at_ms {
        ui.label(format!("Requested {}", display_time(at.get())));
    }
    if let Some(scope) = &p.scope {
        json_view(ui, "Scope", scope);
    }
    if let Some(digest) = &p.scope_digest {
        ui.label(format!("Scope digest: {}", digest.as_str()));
    }
    let changed = d
        .approvals
        .iter()
        .find(|x| x.id.as_str() == p.id.as_str())
        .is_some_and(|x| x.revision.as_str() != p.revision.as_str());
    if changed {
        ui.colored_label(theme::AMBER,"A newer proposal revision is available. This draft remains bound to the inspected revision.");
    }
    let stale = p.stale == Some(true)
        || d.approvals
            .iter()
            .find(|x| x.id.as_str() == p.id.as_str())
            .is_some_and(|x| x.stale == Some(true));
    if stale {
        ui.colored_label(
            theme::AMBER,
            "This proposal is stale. Review the current target before deciding.",
        );
    }
    if let Some(payload) = &p.payload {
        match payload {
            ApprovalContent::FindingRisk {
                feature_id,
                finding_id,
                assessment_revision,
            } => {
                if ui.button("Open finding's feature").clicked() {
                    actions.push(Action::OpenWork(feature_id.as_str().into()));
                }
                ui.label(format!(
                    "Accept risk only for finding {} in feature {} at assessment {}.",
                    finding_id.as_str(),
                    feature_id.as_str(),
                    assessment_revision.as_str()
                ));
                ui.label("Risk acceptance does not waive prerequisites, permissions, verification, or other readiness findings.");
            }
            ApprovalContent::FeaturePlan { .. } => {
                if ui.button("Open feature").clicked() {
                    actions.push(Action::OpenWork(p.target_id.as_str().into()));
                }
                ui.label("Approval promotes this exact pending plan. It does not clear pauses or accept readiness risks.");
            }
            _ => {}
        }
        json_view(ui, "Proposal payload", payload);
    }
    if let Some(source) = &p.reviewed_source {
        match source {
            ApprovalReviewedContent::Settings { entries } => {
                ui.label("Exact proposed settings");
                for (i, entry) in entries.0.iter().enumerate() {
                    ui.push_id(i, |ui| json_view(ui, &format!("Setting {}", i + 1), entry));
                }
            }
            ApprovalReviewedContent::FeaturePlan { plan } => {
                ui.label("Exact proposed plan");
                json_view(ui, "Plan artifact identity", &plan.content);
                json_view(ui, "Repositories", &plan.repository_ids);
                for (i, goal) in plan.goals.0.iter().enumerate() {
                    ui.collapsing(format!("Goal {}", i + 1), |ui| {
                        ui.add(egui::Label::new(&goal.0).wrap().selectable(true));
                    });
                }
                for requirement in &plan.requirements.0 {
                    ui.push_id(requirement.id.as_str(), |ui| {
                        json_view(
                            ui,
                            &format!("Requirement {}", requirement.id.as_str()),
                            requirement,
                        )
                    });
                }
                for criterion in &plan.acceptance.0 {
                    ui.push_id(("acceptance", criterion.id.as_str()), |ui| {
                        json_view(
                            ui,
                            &format!("Acceptance {}", criterion.id.as_str()),
                            criterion,
                        )
                    });
                }
                for criterion in &plan.acceptance_tests.0 {
                    ui.push_id(("test", criterion.id.as_str()), |ui| {
                        json_view(
                            ui,
                            &format!("Acceptance test {}", criterion.id.as_str()),
                            criterion,
                        )
                    });
                }
            }
        }
    }
    if let Some(content) = &p.reviewed_content {
        json_view(ui, "Reviewed binding and evidence", content);
    }
    let exact_content = match &p.payload {
        Some(ApprovalContent::FeaturePlan { .. } | ApprovalContent::Settings { .. }) => {
            p.reviewed_source.is_some()
        }
        _ => p.reviewed_source.is_some() || p.reviewed_content.is_some(),
    };
    if !exact_content {
        ui.colored_label(
            theme::AMBER,
            "Exact reviewed content is unavailable. Refresh the proposal before approving.",
        );
    }
    if let Some(text) = &d.text {
        ui.collapsing("Exact proposal artifact text", |ui| {
            if let Some(error) = &text.error {
                ui.colored_label(theme::RED, error);
            }
            if let Some(page) = text.pages.get(text.page) {
                ui.label(format!(
                    "Page {}{}",
                    text.page + 1,
                    if text.next.is_some() {
                        " · more text available"
                    } else {
                        ""
                    }
                ));
                ui.add(egui::Label::new(page).wrap().selectable(true));
            } else {
                ui.label("Loading exact artifact text…");
            }
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(text.page > 0, egui::Button::new("Previous text page"))
                    .clicked()
                {
                    actions.push(Action::PreviousText);
                }
                if ui
                    .add_enabled(
                        text.page + 1 < text.pages.len() || text.next.is_some(),
                        egui::Button::new("Next text page"),
                    )
                    .clicked()
                {
                    actions.push(Action::NextText);
                }
            });
        });
    }
    if let Some(preview) = &p.operation_preview {
        json_view(ui, "Saved operation preview", preview);
    }
    if let Some(rationale) = &p.rationale {
        ui.label(format!("Recorded rationale: {}", rationale.0));
    }
    if let Some(status) = &p.resume_status {
        ui.label(format!("Saved operation: {}", status.0));
    }
    if let Some(id) = &p.resumed_operation_id {
        ui.label(format!("Operation {}", id.0.as_str()));
    }
    if ui.button("Refresh inspected proposal").clicked() {
        actions.push(Action::Inspect(p.id.as_str().into()));
    }
    ui.label("Decision rationale");
    let rationale_input = ui.add(
        egui::TextEdit::multiline(&mut d.rationale)
            .id_salt("approval-rationale")
            .desired_rows(3)
            .hint_text("Explain your decision for this exact proposal"),
    );
    rationale_input.widget_info(|| {
        egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Decision rationale")
    });
    let can =
        enabled && matches!(p.state, ApprovalState::Pending) && !d.rationale.trim().is_empty();
    let approve = can
        && !stale
        && !changed
        && exact_content
        && d.text
            .as_ref()
            .is_none_or(|t| !t.pages.is_empty() && t.error.is_none());
    if ui
        .add_enabled(
            approve,
            egui::Button::new(
                if matches!(p.payload, Some(ApprovalContent::FindingRisk { .. })) {
                    "Accept this finding's risk"
                } else {
                    "Approve exact proposal"
                },
            ),
        )
        .clicked()
    {
        actions.push(Action::Decide(true));
    }
    if ui
        .add_enabled(can, egui::Button::new("Reject proposal"))
        .clicked()
    {
        actions.push(Action::Decide(false));
    }
    if !enabled {
        ui.label("Decisions require an online host and confirmation of any earlier outcome.");
    }
}

fn permission_label(
    permission: &devworks_core::ports::os_notifications::OsNotificationPermission,
) -> String {
    use devworks_core::ports::os_notifications::OsNotificationPermission as P;
    match permission {
        P::Allowed => "OS permission allowed; submission does not guarantee visibility.".into(),
        P::SystemManaged => {
            "The desktop notification service manages delivery; visibility is unconfirmed.".into()
        }
        P::NotDetermined => {
            "OS permission has not been requested. In-app notifications remain available.".into()
        }
        P::Denied => "OS permission denied. In-app notifications remain available.".into(),
        P::Suppressed(reason) => {
            format!("OS notifications suppressed: {reason}. In-app notifications remain available.")
        }
        P::Unavailable(reason) => format!(
            "OS notifications unavailable: {reason}. In-app notifications remain available."
        ),
    }
}

fn display_time(ms: i64) -> String {
    jiff::Timestamp::from_millisecond(ms)
        .map(|at| {
            at.to_zoned(jiff::tz::TimeZone::system())
                .strftime("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|_| "Time unavailable".into())
}
