//! The Approvals and notifications controller: approvals, the Knowledge review queue, the
//! notification center, completion cards and notification preferences, read and changed
//! through the shared services' frames. It implements [`Screen`] for [`DecisionsUi`]; state
//! and views are in [`crate::decisions_screen`]. All eligibility, promotion, and wakeups are
//! server-owned.
use super::*;
use crate::decision_ops::{self as dops, DecisionsSlot};
pub use crate::decisions_screen::*;
use crate::screen::{Screen, ScreenContext, controller};
use devworks_core::protocol::records::{NotificationLink, NotificationRecord};
use devworks_core::protocol::results::OperationResult;
use serde_json::json;

controller!(DecisionsCtl, decisions: DecisionsUi);

impl Screen for DecisionsUi {
    type Slot = DecisionsSlot;
    const ID: ScreenId = ScreenId::Decisions;
    fn load(&mut self, cx: &mut Cx, _route: Route) {
        self.tab = cx.routes.entry(Route::Approvals).or_default().tab;
        DecisionsCtl {
            decisions: self,
            cx,
        }
        .load_approvals();
    }
    /// The shell refreshes approvals and notifications on every route
    /// ([`DecisionsUi::refresh_decisions`]): completion cards show everywhere.
    fn refresh(&mut self, _cx: &mut Cx, _route: Route) {}
    fn apply(&mut self, cx: &mut Cx, slot: DecisionsSlot, reply: Reply) {
        if let Some((slot, reply)) = (DecisionsCtl {
            decisions: self,
            cx,
        })
        .apply_decisions(slot, reply)
        {
            cx.unhandled(&slot, reply);
        }
    }
    fn ui(&mut self, cx: &mut Cx, _route: Route, ui: &mut Ui) {
        DecisionsCtl {
            decisions: self,
            cx,
        }
        .approvals_screen(ui);
    }
    fn purge_live(&self, out: &mut crate::project_purge::Live) {
        if let Some(devworks_core::protocol::data::DataScope::Project { project_id }) =
            self.selected.as_ref().and_then(|r| r.scope.as_ref())
        {
            out.projects
                .entry(project_id.0.as_str().into())
                .or_insert_with(|| json!({}))["decision"] = json!(self.rationale);
        }
    }
    fn remove_purged(&mut self, scope: &devworks_core::ports::storage::ProjectPurgeClientScope) {
        self.purge_owned(scope);
    }
    fn context(&self, _route: Route, _mem: &RouteMemory) -> ScreenContext {
        ScreenContext {
            tab: self.tab,
            selection: self.selected.as_ref().map(|v| v.id.as_str().to_owned()),
        }
    }
}

/// What the shell asks of the approvals and notifications controller.
impl DecisionsUi {
    pub(crate) fn load_completion_notifications(&mut self, cx: &mut Cx) {
        DecisionsCtl {
            decisions: self,
            cx,
        }
        .load_completion_notifications();
    }
    /// A hint or the periodic repair: completion cards, the open Approvals screen and the open
    /// notification center are read again.
    pub(crate) fn refresh_decisions(&mut self, cx: &mut Cx) {
        let mut ctl = DecisionsCtl {
            decisions: self,
            cx,
        };
        ctl.load_completion_notifications();
        if ctl.route() == Route::Approvals
            && !ctl.pending(&DecisionsSlot::ApprovalsList)
            && !ctl.pending(&DecisionsSlot::ReviewQueue)
        {
            ctl.load_approvals();
        }
        if ctl.decisions.notifications_open && !ctl.pending(&DecisionsSlot::NotificationsList) {
            ctl.load_notification_history();
        }
    }
    pub(crate) fn inspect_approval(&mut self, cx: &mut Cx, id: String) {
        DecisionsCtl {
            decisions: self,
            cx,
        }
        .inspect_approval(id);
    }
    pub(crate) fn notification_center(&mut self, cx: &mut Cx, ctx: &egui::Context) {
        DecisionsCtl {
            decisions: self,
            cx,
        }
        .notification_center(ctx);
    }
    pub(crate) fn completion_action(&mut self, cx: &mut Cx, ctx: &egui::Context, a: CardAction) {
        DecisionsCtl {
            decisions: self,
            cx,
        }
        .completion_action(ctx, a);
    }
}

impl DecisionsCtl<'_, '_> {
    fn set_card_status(&mut self, id: &str, status: Option<String>) {
        if let Some(c) = self.cx.completions.as_deref_mut() {
            c.set_status(id, status);
        }
    }
    fn unresolved(&self, slot: &DecisionsSlot) -> bool {
        let slot = Slot::from(slot.clone());
        self.client
            .as_ref()
            .is_some_and(|c| c.unresolved().iter().any(|u| u.slot == slot))
    }
    fn load_approvals(&mut self) {
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
            DecisionsSlot::ApprovalsList,
            "Loading approvals",
            "approvals.list",
            move |b| dops::query(b, "approvals.list", body),
        );
    }
    fn review_page(&mut self, cursor: Option<String>) {
        let Some(project) = &self.decisions.project else {
            self.decisions.reviews.clear();
            self.decisions.review_gathering.clear();
            self.decisions.reviews_loaded = false;
            return;
        };
        // The retained informational queue requires a project and permits at most 50 per page.
        let mut page = dops::page(cursor.as_deref());
        page["limit"] = json!(50);
        let body = json!({"projectId":project,"page":page});
        self.query(
            DecisionsSlot::ReviewQueue,
            "Loading Knowledge review queue",
            "approvals.review_queue",
            move |b| dops::query(b, "approvals.review_queue", body),
        );
    }
    fn load_completion_notifications(&mut self) {
        if self.pending(&DecisionsSlot::CompletionList) {
            return;
        }
        self.decisions.completion_gathering.clear();
        self.notification_page(true, None);
    }
    fn load_notification_history(&mut self) {
        self.decisions.notification_gathering.clear();
        self.notification_page(false, None);
        if self.decisions.config.is_none() && !self.pending(&DecisionsSlot::NotificationSettings) {
            self.query(
                DecisionsSlot::NotificationSettings,
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
            DecisionsSlot::CompletionList
        } else {
            DecisionsSlot::NotificationsList
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
            DecisionsSlot::ApprovalInspect(key),
            "Inspecting exact approval content",
            "approvals.inspect",
            move |b| dops::query(b, "approvals.inspect", json!({"proposalId":id})),
        );
    }
    fn refresh_selected_approval(&mut self) {
        let Some(p) = &self.decisions.selected else {
            return;
        };
        if let Some(row) = self
            .decisions
            .approvals
            .iter()
            .find(|row| row.id.as_str() == p.id.as_str())
        {
            self.decisions.current = Some(row.into());
            return;
        }
        let id = p.id.as_str().to_owned();
        self.query(
            DecisionsSlot::ApprovalRefresh(id.clone()),
            "Checking inspected proposal status",
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
        self.query(
            DecisionsSlot::ApprovalContent(id),
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
        let slot = DecisionsSlot::ApprovalDecision(p.id.as_str().to_owned());
        let desktop_takeover = p.operation_kind.0 == "desktop.request_control";
        if self.online().is_err()
            || self.pending(&slot)
            || self.unresolved(&slot)
            || (!desktop_takeover && self.decisions.rationale.trim().is_empty())
        {
            return;
        }
        let rationale = if desktop_takeover && self.decisions.rationale.trim().is_empty() {
            if approve {
                "OK: desktop takeover approved"
            } else {
                "Desktop takeover declined"
            }
        } else {
            self.decisions.rationale.as_str()
        };
        let body = json!({"proposalId":p.id,"expectedRevision":p.revision,"contentDigest":p.content_digest,
            "decision":if approve {"Approve"} else {"Reject"},"rationale":rationale});
        let Some(b) = self.backend() else {
            return;
        };
        let frame = dops::command(&*b, "approvals.record_human_decision", body);
        if let Some(c) = &mut self.client {
            c.human_decision(slot.into(), "Recording your approval decision", frame);
        }
    }
    fn change_notification(&mut self, record: NotificationRecord, read: Option<bool>) {
        let id = record.id.as_str().to_owned();
        let slot = DecisionsSlot::NotificationChange(id.clone());
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
        if let Some(c) = self.cx.completions.as_deref_mut() {
            c.reconcile(cards);
        }
    }
    fn apply_decisions(
        &mut self,
        slot: DecisionsSlot,
        reply: Reply,
    ) -> Option<(DecisionsSlot, Reply)> {
        match (slot, reply) {
            (DecisionsSlot::ApprovalsList, Reply::Query(q)) => match dops::approvals(q) {
                Ok((rows, next)) => {
                    self.decisions.gathering.extend(rows);
                    if next.is_some() {
                        self.approval_page(next);
                    } else {
                        self.decisions.approvals = std::mem::take(&mut self.decisions.gathering);
                        self.decisions.loaded = true;
                        self.decisions.error = None;
                        self.refresh_selected_approval();
                    }
                }
                Err(e) => self.decisions.error = Some(e.describe()),
            },
            (DecisionsSlot::ApprovalInspect(id), Reply::Query(_))
                if self.decisions.inspecting.as_deref() != Some(id.as_str()) => {}
            (DecisionsSlot::ApprovalInspect(_), Reply::Query(q)) => match dops::approval(q) {
                Ok(p) => {
                    let artifact = required_approval_text(&p);
                    self.decisions.text = artifact.map(|artifact| ApprovalText {
                        artifact,
                        pages: Vec::new(),
                        page: 0,
                        next: None,
                        error: None,
                    });
                    self.decisions.current = Some((&p).into());
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
            (DecisionsSlot::ApprovalRefresh(id), Reply::Query(q)) => {
                if self
                    .decisions
                    .selected
                    .as_ref()
                    .is_some_and(|p| p.id.as_str() == id)
                {
                    match dops::approval(q) {
                        Ok(p) => self.decisions.current = Some((&p).into()),
                        Err(e) => self.decisions.decision_error = Some(e.describe()),
                    }
                }
            }
            (DecisionsSlot::ApprovalContent(id), Reply::Query(q)) => {
                if self
                    .decisions
                    .selected
                    .as_ref()
                    .is_some_and(|p| p.id.as_str() == id)
                    && let Some(text) = &mut self.decisions.text
                {
                    match ops::read_content(q) {
                        Ok(devworks_core::protocol::records::ArtifactReadContent::Text {
                            artifact,
                            content,
                            next_byte,
                            truncated,
                            ..
                        }) if artifact.version_id.as_str() == text.artifact.version_id.as_str()
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
            (DecisionsSlot::ApprovalDecision(id), Reply::Command(q)) => {
                let selected = self
                    .decisions
                    .selected
                    .as_ref()
                    .is_some_and(|p| p.id.as_str() == id);
                match dops::decided(q) {
                    Ok(p) => {
                        // A decision can finish after the user has opened another proposal.
                        if selected && p.id.as_str() == id {
                            self.decisions.current = Some((&p).into());
                            self.decisions.selected = Some(p);
                            self.decisions.decision_error = None;
                            self.decisions.rationale.clear();
                        }
                    }
                    Err(e) if selected => self.decisions.decision_error = Some(e.describe()),
                    Err(_) => {}
                }
                self.load_approvals();
                self.load_completion_notifications();
            }
            // Clearing the project may leave an older project's reply in flight. It cannot
            // restore hidden rows or continue paging without a selected project.
            (DecisionsSlot::ReviewQueue, Reply::Query(_)) if self.decisions.project.is_none() => {}
            (DecisionsSlot::ReviewQueue, Reply::Query(q)) => match dops::reviews(q) {
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
            (DecisionsSlot::CompletionList, Reply::Query(q)) => match dops::notifications(q) {
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
            (DecisionsSlot::NotificationsList, Reply::Query(q)) => match dops::notifications(q) {
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
            (DecisionsSlot::NotificationRead(id), Reply::Query(_))
                if self.decisions.reading_notification.as_deref() != Some(id.as_str()) => {}
            (DecisionsSlot::NotificationRead(_), Reply::Query(q)) => match dops::notification(q) {
                Ok(n) => self.decisions.notification = Some(n),
                Err(e) => self.decisions.notification_error = Some(e.describe()),
            },
            (DecisionsSlot::NotificationChange(id), Reply::Command(q)) => match dops::notified(q) {
                Ok(n) => {
                    self.set_card_status(&id, None);
                    self.notification_updated(n);
                    self.decisions.notification_error = None;
                    self.load_notification_history();
                    self.decisions.completion_gathering.clear();
                    self.notification_page(true, None);
                }
                Err(e) => {
                    self.set_card_status(&id, Some(e.describe()));
                    self.decisions.notification_error = Some(e.describe());
                }
            },
            (DecisionsSlot::NotificationSettings, Reply::Query(q)) => match ops::query_result(q) {
                Ok((OperationResult::NotificationsSettings { value }, _)) => {
                    if self.decisions.config_draft.is_none() {
                        self.decisions.config_kinds = value
                            .config
                            .kinds
                            .0
                            .iter()
                            .map(|k| k.0.as_str())
                            .collect::<Vec<_>>()
                            .join(", ");
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
            (DecisionsSlot::NotificationSettingsSave, Reply::Command(q)) => {
                match ops::command_result(q) {
                    Ok(OperationResult::NotificationsSettings { value }) => {
                        if serde_json::to_value(&self.decisions.config_draft).ok()
                            == serde_json::to_value(&self.decisions.config_saving).ok()
                            && self.decisions.config_kinds_saving.as_deref()
                                == Some(self.decisions.config_kinds.as_str())
                        {
                            self.decisions.config_kinds = value
                                .config
                                .kinds
                                .0
                                .iter()
                                .map(|k| k.0.as_str())
                                .collect::<Vec<_>>()
                                .join(", ");
                            self.decisions.config_draft = Some(value.config.clone());
                        }
                        self.decisions.config_saving = None;
                        self.decisions.config_kinds_saving = None;
                        self.decisions.config =
                            Some((value.config, value.revision.as_str().to_owned()));
                    }
                    Ok(_) => {
                        self.decisions.notification_error =
                            Some("Unexpected notification settings reply".into())
                    }
                    Err(e) => self.decisions.notification_error = Some(e.describe()),
                }
            }
            (DecisionsSlot::OsNotificationPermission, Reply::OsNotificationPermission(value)) => {
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
                Action::WaitForDesktop => {
                    self.decisions.selected = None;
                    self.decisions.rationale.clear();
                }
                Action::ReloadNotifications => {
                    self.decisions.notifications.clear();
                    self.decisions.notifications_loaded = false;
                    self.load_notification_history();
                }
                Action::Read(id) => {
                    self.decisions.reading_notification = Some(id.clone());
                    let key = id.clone();
                    self.query(
                        DecisionsSlot::NotificationRead(key),
                        "Reading notification",
                        "notifications.read",
                        move |b| dops::query(b, "notifications.read", json!({"notificationId":id})),
                    );
                }
                Action::MarkRead(n, read) => self.change_notification(n, Some(read)),
                Action::Acknowledge(n) => self.change_notification(n, None),
                Action::Approval(id) => {
                    self.navigate(Route::Approvals);
                    self.cx.effects.push(Effect::InspectApproval(id));
                }
                Action::OpenArtifact(id) => {
                    self.routes.entry(Route::Artifacts).or_default().selection =
                        Some(id.as_str().into());
                    self.navigate(Route::Artifacts);
                }
                Action::OpenKnowledge(id) => {
                    self.navigate(Route::Knowledge);
                    self.cx.effects.push(Effect::SelectKnowledge(id));
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
                        let mut config = config.clone();
                        config.kinds.0 = self
                            .decisions
                            .config_kinds
                            .split(',')
                            .map(str::trim)
                            .filter(|x| !x.is_empty())
                            .map(|x| devworks_core::protocol::support::BoundedText(x.into()))
                            .collect();
                        let body = json!({"expectedRevision":revision,"config":config});
                        self.decisions.config_saving = Some(config.clone());
                        self.decisions.config_draft = Some(config);
                        self.decisions.config_kinds_saving =
                            Some(self.decisions.config_kinds.clone());
                        self.command(
                            DecisionsSlot::NotificationSettingsSave,
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
                    self.cx
                        .effects
                        .push(Effect::InspectApproval(id.as_str().into()));
                }
                "knowledge" => {
                    self.navigate(Route::Knowledge);
                    self.cx.effects.push(Effect::SelectKnowledge(id));
                }
                "artifact" => {
                    self.navigate(Route::Artifacts);
                }
                "feature" | "issue" | "task" | "work" | "root" => self.open_work(id.as_str()),
                "session" | "chat" => self.open_session(id.as_str().to_owned()),
                "project" => {
                    self.cx
                        .effects
                        .push(Effect::SelectProject(Some(id.as_str().into())));
                    self.navigate(Route::Overview);
                }
                "connection" => {
                    self.navigate(Route::Settings);
                    self.cx.effects.push(Effect::OpenConnections);
                }
                _ => {
                    *self.cx.status = Some(format!(
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
    fn completion_action(&mut self, ctx: &egui::Context, action: CardAction) {
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
                self.set_card_status(n.id.as_str(), Some("Confirming dismissal…".into()));
                self.change_notification(n, None);
            }
            CardAction::Open(_) => {
                if let Some(root) = n.root_id {
                    self.open_work(root.0.as_str());
                } else if let Some(link) = n.links.as_ref().and_then(|x| x.0.first()).cloned() {
                    self.open_notification_link(ctx, link);
                } else {
                    self.decisions.notifications_open = true;
                    self.decision_actions(ctx, vec![Action::Read(n.id.as_str().into())]);
                }
            }
        }
    }
    fn approvals_screen(&mut self, ui: &mut Ui) {
        let offline = self.online().err().map(str::to_owned);
        let online = offline.is_none();
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
                self.pending(&DecisionsSlot::ApprovalDecision(p.id.as_str().into()))
                    || self.unresolved(&DecisionsSlot::ApprovalDecision(p.id.as_str().into()))
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
                        if let Some(reason) = &offline {
                            widgets::state_block(
                                ui,
                                &ViewState::Offline {
                                    reason: reason.clone(),
                                },
                            );
                        }
                        if let Some(e) = &d.error {
                            ui.colored_label(theme::RED, e);
                        }
                        egui::ScrollArea::vertical()
                            .id_salt("approval-inbox")
                            .show(ui, |ui| {
                                if tab == 2 && d.project.is_none() {
                                    ui.label("Select a project to review Knowledge. This informational queue belongs to one project.");
                                } else if tab == 2 {
                                    if !d.reviews_loaded && online && d.error.is_none() {
                                        ui.label("Loading Knowledge review queue…");
                                    }
                                    if d.reviews_loaded && d.reviews.is_empty() {
                                        ui.label("No Knowledge entries need review.");
                                    }
                                    for r in &d.reviews {
                                        ui.push_id((format!("{:?}",r.kind),r.entry.id.as_str(),r.other.as_ref().map(|row|row.id.as_str())),|ui| {
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
                                        });
                                    }
                                } else {
                                    if !d.loaded && online && d.error.is_none() {
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
    fn notification_center(&mut self, ctx: &egui::Context) {
        if !self.decisions.notifications_open {
            return;
        }
        if !self.decisions.notifications_loaded && !self.pending(&DecisionsSlot::NotificationsList)
        {
            self.load_notification_history();
        }
        let mut open = true;
        let mut actions = Vec::new();
        let mut help_preferences = false;
        if let Some(b) = self.backend() {
            self.decisions.permission = Some(b.os_notification_permission());
        }
        let requesting_permission = self.pending(&DecisionsSlot::OsNotificationPermission);
        let preferences_busy = self.pending(&DecisionsSlot::NotificationSettingsSave)
            || self.unresolved(&DecisionsSlot::NotificationSettingsSave)
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
                self.pending(&DecisionsSlot::NotificationChange(n.id.as_str().into()))
                    || self.unresolved(&DecisionsSlot::NotificationChange(n.id.as_str().into()))
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
                if let Some(id)=&n.decision_id && ui.button("Open linked decision").clicked() {
                    actions.push(Action::Approval(id.as_str().into()));
                }
                if let Some(links)=&n.links {
                    for link in &links.0 {
                        if let Some(url)=&link.url {ui.hyperlink_to(&link.kind.0,&url.0);}
                        if (link.target_id.is_some()||link.artifact.is_some()) && ui.button(format!("Open {}",link.kind.0)).clicked() {
                            actions.push(Action::Link(link.clone()));
                        }
                    }
                }
            }
            ui.collapsing("Notification preferences",|ui|{
                if widgets::secondary_button(ui,"Help for Notification preferences").clicked() { help_preferences = true; }
                if let Some(config)=&mut d.config_draft {
                    ui.checkbox(&mut config.desktop_enabled,"Desktop notifications");
                    ui.label("Required-input desktop notifications are attempted only while the app is unfocused and permission allows them. In-app history remains available.");
                    if let Some(permission)=&d.permission { ui.label(permission_label(permission)); }
                    if ui.add_enabled(!requesting_permission,egui::Button::new("Request notification permission")).clicked() { actions.push(Action::RequestPermission); }
                    ui.label("Notification kinds (comma separated)");
                    let input=ui.text_edit_singleline(&mut d.config_kinds);
                    input.widget_info(||egui::WidgetInfo::labeled(egui::Role::TextInput,true,"Notification kinds"));
                    if ui.add_enabled(!preferences_busy,egui::Button::new("Save notification preferences")).clicked(){actions.push(Action::SaveConfig);}
                    if ui.button("Discard preference draft and reload").clicked(){actions.push(Action::ReloadConfig);}
                    if let Some((_,revision))=&d.config {ui.label(format!("Preferences revision {revision}"));}
                }
            });
        });
        self.decisions.notifications_open = open && !help_preferences;
        self.decision_actions(ctx, actions);
        if help_preferences {
            self.cx
                .effects
                .push(Effect::OpenHelp("settings-Notifications".into()));
        }
    }
}
