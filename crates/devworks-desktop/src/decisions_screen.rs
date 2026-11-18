//! Approval and notification view state. All eligibility, promotion, and wakeups are server-owned.
use crate::paged::Paged;
use crate::theme;
use devworks_core::protocol::records::{
    ApprovalContent, ApprovalReviewedContent, ApprovalState, ApprovalViewRecord,
    KnowledgeReviewItem, NotificationConfig, NotificationLink, NotificationRecord,
};
use eframe::egui::{self, RichText, Ui};

#[derive(Default)]
pub struct DecisionsUi {
    pub project: Option<String>,
    pub tab: usize,
    pub history_state: Option<String>,
    /// The approvals of the shown tab, every page read before they show.
    pub approvals: Paged<ApprovalViewRecord>,
    /// The inspected proposal is the exact displayed snapshot; a refresh never replaces a draft.
    pub selected: Option<ApprovalViewRecord>,
    pub(crate) inspecting: Option<String>,
    pub(crate) current: Option<ApprovalStatus>,
    pub rationale: String,
    pub text: Option<ApprovalText>,
    pub decision_error: Option<String>,
    /// The Knowledge review queue of the project, every page read before it shows.
    pub reviews: Paged<KnowledgeReviewItem>,
    /// The notification center is open.
    pub(crate) notifications_open: bool,
    pub notifications: Paged<NotificationRecord>,
    /// A refused notification read or change (the list's own failure is `notifications.error`).
    pub notification_error: Option<String>,
    pub notification_project: Option<String>,
    pub unread_only: bool,
    pub unacknowledged_only: bool,
    pub category: String,
    pub notification: Option<NotificationRecord>,
    pub(crate) reading_notification: Option<String>,
    /// Complete authoritative unacknowledged verified-completion feed, independent of filters.
    pub completions: Paged<NotificationRecord>,
    pub permission: Option<devworks_core::ports::os_notifications::OsNotificationPermission>,
    pub config: Option<(NotificationConfig, String)>,
    pub config_draft: Option<NotificationConfig>,
    pub(crate) config_saving: Option<NotificationConfig>,
    pub(crate) config_kinds: String,
    pub(crate) config_kinds_saving: Option<String>,
}

impl DecisionsUi {
    pub(crate) fn purge_owned(
        &mut self,
        scope: &devworks_core::ports::storage::ProjectPurgeClientScope,
    ) {
        use devworks_core::protocol::data::DataScope;
        let owned=self.selected.as_ref().is_some_and(|r| matches!(&r.scope,Some(DataScope::Project{project_id}) if project_id==&scope.project_id));
        if owned {
            self.selected = None;
            self.inspecting = None;
            self.current = None;
            self.rationale.clear();
            self.text = None;
            self.decision_error = None;
        }
        self.approvals.reset();
        self.reviews.reset();
        self.notifications.reset();
        self.notification = None;
        self.reading_notification = None;
        self.completions.reset();
        if self.project.as_deref() == Some(scope.project_id.0.as_str()) {
            self.project = None;
        }
        if self.notification_project.as_deref() == Some(scope.project_id.0.as_str()) {
            self.notification_project = None;
        }
        // Notification configuration drafts are independent workspace authoring state.
    }
}

pub(crate) struct ApprovalStatus {
    pub(crate) revision: String,
    pub(crate) state: ApprovalState,
    pub(crate) stale: Option<bool>,
    pub(crate) content_digest: String,
    pub(crate) target_revision: String,
}
impl From<&ApprovalViewRecord> for ApprovalStatus {
    fn from(p: &ApprovalViewRecord) -> Self {
        Self {
            revision: p.revision.as_str().into(),
            state: p.state.clone(),
            stale: p.stale,
            content_digest: p.content_digest.as_str().into(),
            target_revision: p.target_revision.as_str().into(),
        }
    }
}

pub struct ApprovalText {
    pub(crate) artifact: devworks_core::protocol::provider::ArtifactRef,
    pub(crate) pages: Vec<String>,
    pub(crate) page: usize,
    pub(crate) next: Option<String>,
    pub(crate) error: Option<String>,
}

/// A small owner-derived package review carries its source bundle as an attachment. Large
/// reviews instead carry mandatory JSON evidence; plans always require their exact text.
pub(crate) fn package_source_attachment(p: &ApprovalViewRecord) -> bool {
    let Some(artifact) = &p.artifact else {
        return false;
    };
    if artifact.media_type != "application/vnd.devworks.package"
        || !matches!(p.payload, Some(ApprovalContent::SavedOperation { .. }))
        || !matches!(
            p.operation_kind.0.as_str(),
            "addons.install"
                | "addons.update"
                | "addons.enable"
                | "addons.configure"
                | "addons.rollback"
        )
    {
        return false;
    }
    let Some(reviewed) = &p.reviewed_content else {
        return false;
    };
    reviewed.0["operation"] == p.operation_kind.0
        && reviewed.0["inspection"].is_object()
        && reviewed.0.pointer("/source/bundle")
            == Some(&serde_json::to_value(artifact).expect("typed package source"))
}
/// The readable review of an agent's `containers.create`: the image and command, each mount
/// source with its host path and read-only flag, and each published port with its bind address.
/// `None` when the proposal is not one or its review is held in an evidence artifact instead.
pub(crate) fn container_create_review(p: &ApprovalViewRecord) -> Option<Vec<String>> {
    if p.operation_kind.0 != "containers.create" {
        return None;
    }
    let reviewed = &p.reviewed_content.as_ref()?.0;
    if reviewed["operation"] != "containers.create" {
        return None;
    }
    let text = |v: &serde_json::Value| v.as_str().unwrap_or("?").to_owned();
    let mut lines = vec![format!("Image: {}", text(&reviewed["image"]))];
    let command: Vec<String> = reviewed["command"].as_array()?.iter().map(text).collect();
    lines.push(if command.is_empty() {
        "Command: image default".into()
    } else {
        format!("Command: {}", command.join(" "))
    });
    let mounts = reviewed["mounts"].as_array()?;
    if mounts.is_empty() {
        lines.push("Mounts: none".into());
    }
    for mount in mounts {
        lines.push(format!(
            "Mount {} ({} / {}) at {}, {}",
            text(&mount["hostPath"]),
            text(&mount["source"]["rootId"]),
            text(&mount["source"]["relative"]),
            text(&mount["destination"]),
            if mount["readOnly"].as_bool()? {
                "read-only"
            } else {
                "writable"
            }
        ));
    }
    let ports = reviewed["ports"].as_array()?;
    if ports.is_empty() {
        lines.push("Published ports: none".into());
    }
    for port in ports {
        lines.push(format!(
            "Port {}/{} on {} to container port {}{}",
            port["hostPort"].as_u64()?,
            text(&port["protocol"]),
            text(&port["bindAddress"]),
            port["containerPort"].as_u64()?,
            if port["exposeBeyondLoopback"].as_bool()? {
                ", reachable from other machines"
            } else {
                ""
            }
        ));
    }
    for entry in reviewed["environment"].as_array().into_iter().flatten() {
        lines.push(match entry["credentialRef"].as_str() {
            Some(credential) => format!(
                "Environment {} from credential {credential}",
                text(&entry["name"])
            ),
            None => format!(
                "Environment {}={}",
                text(&entry["name"]),
                text(&entry["value"])
            ),
        });
    }
    Some(lines)
}
/// The readable review of an agent's `containers.build`, `pull`, `start` or `exec`, with its
/// heading: the build context and Dockerfile, the image and registry, or the container with its
/// owner, and for exec the command and environment (secret values arrive already redacted).
/// `None` when the proposal is not one or its review is held in an evidence artifact instead.
pub(crate) fn container_operation_review(
    p: &ApprovalViewRecord,
) -> Option<(&'static str, Vec<String>)> {
    let reviewed = &p.reviewed_content.as_ref()?.0;
    if reviewed["operation"] != p.operation_kind.0 {
        return None;
    }
    let text = |v: &serde_json::Value| v.as_str().unwrap_or("?").to_owned();
    let container = |lines: &mut Vec<String>| {
        let c = &reviewed["container"];
        lines.push(format!(
            "Container {} from image {}, {}",
            text(&c["containerId"]),
            text(&c["image"]),
            text(&c["phase"])
        ));
        lines.push(format!(
            "Owner {} ({}), session {}",
            text(&c["owner"]["actor"]),
            text(&c["owner"]["ownerKind"]),
            c["owner"]["sessionId"].as_str().unwrap_or("none")
        ));
    };
    let mut lines = Vec::new();
    let heading = match p.operation_kind.0.as_str() {
        "containers.build" => {
            let context = &reviewed["context"];
            let definition = &reviewed["definition"];
            lines.push(format!("Tag: {}", text(&reviewed["tag"])));
            lines.push(format!(
                "Context {} ({} / {})",
                text(&context["hostPath"]),
                text(&context["source"]["rootId"]),
                text(&context["source"]["relative"])
            ));
            lines.push(format!(
                "Dockerfile {} bytes, sha256 {}",
                text(&definition["byteLength"]),
                text(&definition["sha256"])
            ));
            lines.extend(text(&definition["text"]).lines().map(|l| format!("  {l}")));
            "Image to build"
        }
        "containers.pull" => {
            lines.push(format!("Image: {}", text(&reviewed["image"])));
            lines.push(format!("Registry: {}", text(&reviewed["registry"])));
            if let Some(credential) = reviewed["credentialRef"].as_str() {
                lines.push(format!("Registry credential {credential}"));
            }
            "Image to pull"
        }
        "containers.start" => {
            container(&mut lines);
            "Container to start"
        }
        "containers.exec" => {
            container(&mut lines);
            let command: Vec<String> = reviewed["command"].as_array()?.iter().map(text).collect();
            lines.push(format!("Command: {}", command.join(" ")));
            lines.push(format!(
                "Working directory: {}",
                text(&reviewed["workingDirectory"])
            ));
            for entry in reviewed["environment"].as_array().into_iter().flatten() {
                lines.push(match entry["credentialRef"].as_str() {
                    Some(credential) => format!(
                        "Environment {} from credential {credential}",
                        text(&entry["name"])
                    ),
                    None => format!(
                        "Environment {}={}",
                        text(&entry["name"]),
                        text(&entry["value"])
                    ),
                });
            }
            "Command to run in a container"
        }
        _ => return None,
    };
    Some((heading, lines))
}
pub(crate) fn required_approval_text(
    p: &ApprovalViewRecord,
) -> Option<devworks_core::protocol::provider::ArtifactRef> {
    match &p.reviewed_source {
        Some(ApprovalReviewedContent::FeaturePlan { plan, .. }) => Some(plan.content.clone()),
        _ if package_source_attachment(p) => None,
        _ => p.artifact.clone(),
    }
}
#[derive(Clone)]
pub(crate) enum Action {
    Reload,
    Inspect(String),
    Decide(bool),
    WaitForDesktop,
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
    OpenArtifact(devworks_core::protocol::Id),
    ReloadConfig,
}

pub(crate) fn project_filter(
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
pub(crate) fn json_view(ui: &mut Ui, label: &str, value: &impl serde::Serialize) {
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
pub(crate) fn approval_detail(
    ui: &mut Ui,
    d: &mut DecisionsUi,
    actions: &mut Vec<Action>,
    enabled: bool,
) {
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
        ui.label(format!(
            "Requested {}",
            crate::widgets::local_time(at.get())
        ));
    }
    if let Some(scope) = &p.scope {
        json_view(ui, "Scope", scope);
    }
    if let Some(artifact) = &p.artifact {
        if package_source_attachment(p) {
            ui.label(format!(
                "Package source attachment · {} bytes",
                artifact.byte_length.as_str()
            ));
            json_view(ui, "Package source attachment identity", artifact);
            if ui.button("Open package source artifact").clicked() {
                actions.push(Action::OpenArtifact(artifact.artifact_id.clone()));
            }
            ui.label("The binary bundle is attached separately from the exact owner-derived review below.");
        } else {
            json_view(ui, "Reviewed artifact identity", artifact);
        }
    }
    if let Some(digest) = &p.scope_digest {
        ui.label(format!("Scope digest: {}", digest.as_str()));
    }
    let changed = d.current.as_ref().is_some_and(|current| {
        current.revision != p.revision.as_str()
            || current.content_digest != p.content_digest.as_str()
            || current.target_revision != p.target_revision.as_str()
    });
    if changed {
        ui.colored_label(theme::AMBER,"A newer proposal revision is available. This draft remains bound to the inspected revision.");
    }
    let stale = p.stale == Some(true)
        || d.current.as_ref().is_some_and(|current| {
            current.stale == Some(true) || matches!(current.state, ApprovalState::Superseded)
        });
    if let Some(current) = &d.current
        && (changed || !matches!(current.state, ApprovalState::Pending))
    {
        ui.label(format!(
            "Current proposal: {:?} @ {}",
            current.state, current.revision
        ));
    }
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
            ApprovalReviewedContent::FeaturePlan { plan, budget } => {
                if let Some(budget) = budget {
                    crate::usage_screen::budget_choice(ui, budget);
                }
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
    if let Some(lines) = container_create_review(p) {
        ui.label(RichText::new("Container to create").strong());
        for line in lines {
            ui.add(egui::Label::new(line).wrap().selectable(true));
        }
    }
    if let Some((heading, lines)) = container_operation_review(p) {
        ui.label(RichText::new(heading).strong());
        for line in lines {
            ui.add(egui::Label::new(line).wrap().selectable(true));
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
    let desktop_takeover = p.operation_kind.0 == "desktop.request_control";
    if desktop_takeover {
        ui.label("Desktop control is waiting for your OK. Wait keeps this request pending and sends no input.");
    } else {
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
    }
    let can = enabled
        && matches!(p.state, ApprovalState::Pending)
        && d.current
            .as_ref()
            .is_none_or(|current| matches!(current.state, ApprovalState::Pending))
        && !changed
        && (desktop_takeover || !d.rationale.trim().is_empty());
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
            egui::Button::new(if desktop_takeover {
                "OK"
            } else if matches!(p.payload, Some(ApprovalContent::FindingRisk { .. })) {
                "Accept this finding's risk"
            } else {
                "Approve exact proposal"
            }),
        )
        .clicked()
    {
        actions.push(Action::Decide(true));
    }
    if desktop_takeover {
        if ui.add_enabled(can, egui::Button::new("Wait")).clicked() {
            actions.push(Action::WaitForDesktop);
        }
    } else if ui
        .add_enabled(can, egui::Button::new("Reject proposal"))
        .clicked()
    {
        actions.push(Action::Decide(false));
    }
    if !enabled {
        ui.label("Decisions require an online host and confirmation of any earlier outcome.");
    }
}

pub(crate) fn permission_label(
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

#[cfg(test)]
mod artifact_review_tests {
    use super::*;
    use serde_json::json;
    fn proposal(operation: &str) -> ApprovalViewRecord {
        let artifact = json!({
            "artifactId": "source",
            "versionId": "version",
            "sha256": "a".repeat(64),
            "byteLength": "123",
            "mediaType": "application/vnd.devworks.package"
        });
        serde_json::from_value(json!({
            "id": "review",
            "revision": "1",
            "contentDigest": "b".repeat(64),
            "state": "Pending",
            "operationKind": operation,
            "targetId": "pack",
            "targetRevision": "1",
            "summary": "Review exact package",
            "artifact": artifact,
            "payload": {
                "kind": "SavedOperation",
                "schemaRevision": "1",
                "input": {}
            },
            "reviewedContent": {
                "operation": operation,
                "inspection": {},
                "source": {"bundle": artifact}
            }
        }))
        .unwrap()
    }
    #[test]
    fn container_create_review_lists_mounts_ports_and_image() {
        let mut p: ApprovalViewRecord = serde_json::from_value(json!({
            "id": "review",
            "revision": "1",
            "contentDigest": "b".repeat(64),
            "state": "Pending",
            "operationKind": "containers.create",
            "targetId": "project-a",
            "targetRevision": "1",
            "summary": "Review container",
            "payload": {
                "kind": "SavedOperation",
                "schemaRevision": "8",
                "input": {}
            },
            "reviewedContent": {
                "operation": "containers.create",
                "engineId": "docker",
                "projectId": "project-a",
                "image": "alpine:3.20",
                "command": ["sh", "-c", "serve"],
                "mounts": [
                    {
                        "source": {"rootId": "repo", "relative": "src"},
                        "hostPath": "/work/repo/src",
                        "destination": "/src",
                        "readOnly": true
                    },
                    {
                        "source": {"rootId": "repo", "relative": "data"},
                        "hostPath": "/work/repo/data",
                        "destination": "/data",
                        "readOnly": false
                    }
                ],
                "ports": [
                    {
                        "hostPort": 8080,
                        "containerPort": 80,
                        "protocol": "tcp",
                        "exposeBeyondLoopback": false,
                        "bindAddress": "127.0.0.1"
                    },
                    {
                        "hostPort": 5353,
                        "containerPort": 53,
                        "protocol": "udp",
                        "exposeBeyondLoopback": true,
                        "bindAddress": "all host interfaces"
                    }
                ],
                "environment": [
                    {"name": "MODE", "value": "dev"},
                    {"name": "TOKEN", "credentialRef": "cred-1"}
                ],
                "resourceIds": []
            }
        }))
        .unwrap();
        assert_eq!(
            container_create_review(&p).unwrap(),
            [
                "Image: alpine:3.20",
                "Command: sh -c serve",
                "Mount /work/repo/src (repo / src) at /src, read-only",
                "Mount /work/repo/data (repo / data) at /data, writable",
                "Port 8080/tcp on 127.0.0.1 to container port 80",
                "Port 5353/udp on all host interfaces to container port 53, reachable from other machines",
                "Environment MODE=dev",
                "Environment TOKEN from credential cred-1",
            ]
        );
        p.operation_kind.0 = "git.commit".into();
        assert!(container_create_review(&p).is_none());
    }
    #[test]
    fn container_build_pull_start_and_exec_reviews_list_their_key_inputs() {
        let review = |operation: &str, reviewed: serde_json::Value| {
            let p: ApprovalViewRecord = serde_json::from_value(json!({
                "id": "review",
                "revision": "1",
                "contentDigest": "b".repeat(64),
                "state": "Pending",
                "operationKind": operation,
                "targetId": "project-a",
                "targetRevision": "1",
                "summary": "Review container",
                "payload": {
                    "kind": "SavedOperation",
                    "schemaRevision": "1",
                    "input": {}
                },
                "reviewedContent": reviewed
            }))
            .unwrap();
            container_operation_review(&p)
        };
        let container = json!({"containerId":"c-1","revision":"1","engine":"docker","image":"alpine:3.20","phase":"Running",
            "owner":{"actor":"agent-a","ownerKind":"Agent","sessionId":"s-a","projectId":"project-a","rootId":null,"assignmentId":null}});
        assert_eq!(
            review(
                "containers.build",
                json!({"operation":"containers.build","tag":"app:1",
                "context":{"source":{"rootId":"repo","relative":"ctx"},"hostPath":"/work/repo/ctx"},
                "definition":{"sha256":"c".repeat(64),"byteLength":"32","text":"FROM alpine:3.20\nRUN make"}})
            ),
            Some((
                "Image to build",
                vec![
                    "Tag: app:1".into(),
                    "Context /work/repo/ctx (repo / ctx)".into(),
                    format!("Dockerfile 32 bytes, sha256 {}", "c".repeat(64)),
                    "  FROM alpine:3.20".into(),
                    "  RUN make".into(),
                ]
            ))
        );
        assert_eq!(
            review(
                "containers.pull",
                json!({"operation":"containers.pull","image":"ghcr.io/acme/tool:1","registry":"ghcr.io","credentialRef":"cred-1"})
            ),
            Some((
                "Image to pull",
                vec![
                    "Image: ghcr.io/acme/tool:1".to_owned(),
                    "Registry: ghcr.io".into(),
                    "Registry credential cred-1".into(),
                ]
            ))
        );
        assert_eq!(
            review(
                "containers.start",
                json!({"operation":"containers.start","container":container})
            ),
            Some((
                "Container to start",
                vec![
                    "Container c-1 from image alpine:3.20, Running".to_owned(),
                    "Owner agent-a (Agent), session s-a".into(),
                ]
            ))
        );
        assert_eq!(
            review(
                "containers.exec",
                json!({"operation":"containers.exec","container":container,
                "command":["sh","-c","make test"],"workingDirectory":"container default",
                "environment":[{"name":"API_TOKEN","value":"[REDACTED]"},{"name":"KEY","credentialRef":"cred-1"}]})
            ),
            Some((
                "Command to run in a container",
                vec![
                    "Container c-1 from image alpine:3.20, Running".to_owned(),
                    "Owner agent-a (Agent), session s-a".into(),
                    "Command: sh -c make test".into(),
                    "Working directory: container default".into(),
                    "Environment API_TOKEN=[REDACTED]".into(),
                    "Environment KEY from credential cred-1".into(),
                ]
            ))
        );
        // Another operation, or a review held in evidence, gets no readable lines.
        assert!(
            review(
                "containers.create",
                json!({"operation":"containers.create"})
            )
            .is_none()
        );
        assert!(review("containers.pull", json!({"operation":"containers.build"})).is_none());
    }
    #[test]
    fn only_exact_owner_package_sources_are_optional_binary_attachments() {
        for operation in [
            "addons.install",
            "addons.update",
            "addons.enable",
            "addons.configure",
            "addons.rollback",
        ] {
            let mut p = proposal(operation);
            assert!(package_source_attachment(&p));
            assert!(required_approval_text(&p).is_none());
            p.artifact.as_mut().unwrap().media_type = "application/json".into();
            assert!(!package_source_attachment(&p));
            assert!(
                required_approval_text(&p).is_some(),
                "required JSON evidence is never exempted"
            );
            p = proposal(operation);
            p.reviewed_content = None;
            assert!(
                required_approval_text(&p).is_some(),
                "missing owner review cannot establish source-attachment role"
            );
            p = proposal(operation);
            p.artifact.as_mut().unwrap().version_id =
                devworks_core::protocol::Id::new("different").unwrap();
            assert!(
                required_approval_text(&p).is_some(),
                "a different artifact cannot bypass required evidence loading"
            );
        }
        assert!(required_approval_text(&proposal("git.commit")).is_some());
    }
}
