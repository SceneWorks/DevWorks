//! Settings requests and replies: categories and their drafts (`settings.*`), Agent tools (web
//! and MCP servers), Provider connections and embedding spaces (`router.*`) and credential
//! intake. Every change is the same command an agent tool submits; a save whose outcome is not
//! known keeps its exact frame (Confirm outcomes, and the next launch, resubmit it). Roles and
//! skills, and the repository indexing sources, are drawn inside it by their own screens.
use super::CONNECTION_POLL;
use crate::backend::{Reply, TaskOutput, Unresolved, is_unknown};
use crate::indexing_screen::{self as indexing, IndexAction};
use crate::mcp_panel::McpAction;
use crate::memory::RouteMemory;
use crate::ops::{self, Failure};
use crate::paged::Applied;
use crate::projects_ops::ProjectsSlot;
use crate::route::Route;
use crate::screen::{Cx, Effect, Screen, ScreenContext, ScreenId, controller};
use crate::settings_ops::SettingsSlot;
use crate::settings_screen::{HostSection, KEEP_AWAKE, SettingsAction, SettingsScreen};
use crate::web_panel::WebAction;
use crate::widgets::{self, Tone};
use devworks_core::operations::{OperationInput as In, mcp};
use devworks_core::protocol::records::{
    ConnectionLifecycle, ItemState, McpTransport, RouterConfigAction,
};
use devworks_core::protocol::{DomainErrorCode, Id};
use devworks_core::settings::validate::codes;
use devworks_core::settings::view::Health;
use eframe::egui::{self, Ui};
use std::time::{Duration, Instant};

controller!(SettingsCtl, settings: SettingsScreen);

impl Screen for SettingsScreen {
    type Slot = SettingsSlot;
    const ID: ScreenId = ScreenId::Settings;
    fn load(&mut self, cx: &mut Cx, _route: Route) {
        if self.view.is_none() && !self.loading {
            SettingsCtl { settings: self, cx }.load_settings();
        }
    }
    /// Background discovery commits independently of the manual refresh command: the completed
    /// inventory is read on normal hints and periodic repair. Loading keeps unsaved model edits
    /// and leaves unknown configure outcomes untouched.
    fn refresh(&mut self, cx: &mut Cx, _route: Route) {
        if self.show_connections && !self.connections.connections.loading() {
            SettingsCtl { settings: self, cx }.load_connections();
        }
    }
    fn apply(&mut self, cx: &mut Cx, slot: SettingsSlot, reply: Reply) {
        if let Some((slot, reply)) =
            (SettingsCtl { settings: self, cx }).apply_settings(slot, reply)
        {
            cx.unhandled(&slot, reply);
        }
    }
    fn ui(&mut self, cx: &mut Cx, route: Route, ui: &mut Ui) {
        let online = cx.online().map_err(str::to_owned);
        self.mcp.projects = cx.projects.list.items.clone();
        let mut actions = Vec::new();
        // Repository indexing sources sit with the repository paths (General).
        let mut index_actions = Vec::new();
        let up = online.is_ok();
        let mut indexing_screen = cx.indexing.take();
        let load_sources = indexing_screen
            .as_ref()
            .is_some_and(|i| i.sources.is_none() && i.sources_error.is_none());
        {
            let mut host = |ui: &mut Ui, section: HostSection| match section {
                HostSection::Indexing => {
                    ui.add_space(8.0);
                    if let Some(i) = indexing_screen.as_deref_mut() {
                        indexing::sources_panel(ui, i, up, &mut index_actions);
                    }
                    if widgets::secondary_button(ui, "Open indexing runs").clicked() {
                        index_actions.push(IndexAction::Runs);
                    }
                }
                // The panel is a screen of its own: it draws itself and runs its own actions.
                HostSection::Instructions(panel) => Screen::ui(panel, cx, route, ui),
            };
            SettingsScreen::ui(
                self,
                ui,
                online.as_ref().map(|_| ()).map_err(String::as_str),
                &mut actions,
                &mut host,
            );
        }
        let mut ctl = SettingsCtl { settings: self, cx };
        ctl.settings_actions(actions);
        if index_actions.iter().any(|a| matches!(a, IndexAction::Runs)) {
            ctl.effects.push(Effect::SetRoute(Route::Indexing));
        }
        if load_sources && up {
            index_actions.insert(0, IndexAction::LoadSources);
        }
        for action in index_actions {
            ctl.index(action);
        }
        let ctx = ui.ctx().clone();
        ctl.poll_connections(&ctx);
        ctl.poll_mcp(&ctx);
    }
    fn restore_in_flight(&mut self, slot: &SettingsSlot, u: &Unresolved) {
        match slot {
            SettingsSlot::Configure(n) => {
                let p = &mut self.connections;
                p.sequence = p.sequence.max(*n);
                p.restore_edit(*n, u.frame());
                p.busy.push((*n, u.label.clone()));
            }
            SettingsSlot::SettingsSave => {
                self.saving = true;
                self.unconfirmed = true;
            }
            _ => {}
        }
    }
    fn help_topic(&self, _route: Route, _mem: &RouteMemory) -> Option<&'static str> {
        Some(SettingsScreen::help_topic(self))
    }
    fn context(&self, _route: Route, mem: &RouteMemory) -> ScreenContext {
        ScreenContext {
            tab: mem.tab,
            selection: mem.selection.clone(),
        }
    }
}

/// What the shell and other screens ask of the Settings controller.
impl SettingsScreen {
    /// Reads the settings again, with the container runtimes, projects and MCP servers.
    pub(crate) fn reload(&mut self, cx: &mut Cx) {
        SettingsCtl { settings: self, cx }.load_settings();
    }
    /// Shows Provider connections and reads them.
    pub(crate) fn show_connections_topic(&mut self, cx: &mut Cx) {
        self.show_connections = true;
        SettingsCtl { settings: self, cx }.load_connections();
    }
    /// Shows the embedding setup of Provider connections and reads it.
    pub(crate) fn show_embedding_setup(&mut self, cx: &mut Cx) {
        self.show_connections = true;
        self.connections.embedding_spaces.open = true;
        SettingsCtl { settings: self, cx }.load_connections();
    }
    /// Shows the Instructions category.
    pub(crate) fn show_instructions(&mut self) {
        self.category = Some(devworks_core::settings::catalog::Category::Instructions);
        if let Some(v) = &mut self.view {
            v.set_category(self.category);
        }
    }
    /// Confirm outcomes resubmits a save: it runs again (and says so) until its outcome arrives.
    pub(crate) fn outcomes_confirmed(&mut self) {
        self.unconfirmed = false;
    }
}

impl SettingsCtl<'_, '_> {
    fn load_settings(&mut self) {
        if let Some(backend) = self.backend() {
            self.settings.connections.secret_store = Some(backend.secret_store());
        }
        self.settings.loading = true;
        self.query(
            SettingsSlot::SettingsRead,
            "Loading settings",
            "settings.read_effective",
            ops::settings_read,
        );
        self.refresh_container_capabilities();
        if !self.projects.list.loaded && !self.pending(&ProjectsSlot::ProjectsList) {
            self.load_projects();
        }
        self.load_mcp_servers(None);
    }
    fn refresh_container_capabilities(&mut self) {
        self.query(
            SettingsSlot::ContainerCapabilities,
            "Checking container runtimes",
            "containers.capabilities",
            ops::container_capabilities,
        );
    }

    fn settings_revision(&self) -> Option<i64> {
        self.settings.view.as_ref().map(|v| v.revision)
    }

    fn explain(&mut self, key: String) {
        self.settings
            .explained_at
            .insert(key.clone(), self.settings_revision());
        let k = key.clone();
        self.query(
            SettingsSlot::SettingsExplain(key),
            "Explaining a setting",
            "settings.explain_source",
            move |b| ops::settings_explain(b, &k),
        );
    }

    fn check_settings_health(&mut self, revision: i64) {
        if let Some(b) = self.backend() {
            self.settings.set_health(KEEP_AWAKE, b.keep_awake_health());
        }
        let dirs = self.settings.directories();
        if dirs.is_empty() {
            return;
        }
        self.query(
            SettingsSlot::SettingsHealth,
            "Checking directories",
            "settings.validate",
            move |b| {
                let entries: Vec<(&str, serde_json::Value)> =
                    dirs.iter().map(|(k, v)| (*k, v.clone())).collect();
                ops::settings_validate_values(b, revision, &entries)
            },
        );
    }

    fn settings_actions(&mut self, actions: Vec<SettingsAction>) {
        for a in actions {
            match a {
                SettingsAction::Reload => self.load_settings(),
                SettingsAction::RefreshContainers => self.refresh_container_capabilities(),
                SettingsAction::Validate => {
                    let Some(b) = self.backend() else { continue };
                    let Some(frame) = self
                        .settings
                        .view
                        .as_ref()
                        .and_then(|v| ops::settings_submit(&*b, v, true))
                    else {
                        continue;
                    };
                    self.settings.validating = true;
                    self.settings.validated_drafts = Some(self.settings.drafts());
                    if let Some(c) = self.client.as_deref_mut() {
                        c.query(
                            SettingsSlot::SettingsValidate.into(),
                            "Validating settings",
                            "settings.validate",
                            frame,
                        );
                    }
                }
                SettingsAction::Save => {
                    let Some(b) = self.backend() else { continue };
                    let Some(frame) = self
                        .settings
                        .view
                        .as_ref()
                        .and_then(|v| ops::settings_submit(&*b, v, false))
                    else {
                        continue;
                    };
                    self.settings.saving = true;
                    self.settings.submitted = self
                        .settings
                        .view
                        .as_ref()
                        .map(|v| {
                            v.dirty_keys()
                                .into_iter()
                                .filter_map(|k| Some((k.to_owned(), v.item(k)?.draft.clone()?)))
                                .collect()
                        })
                        .unwrap_or_default();
                    if let Some(c) = self.client.as_deref_mut() {
                        c.command(
                            SettingsSlot::SettingsSave.into(),
                            "Saving settings",
                            "settings.save_revision",
                            frame,
                        );
                    }
                }
                SettingsAction::StoreSecret { key, value } => {
                    self.store_setting_secret(&key, value)
                }
                SettingsAction::Explain(key) => self.explain(key),
                SettingsAction::ChooseDirectory(key) => {
                    let start = self
                        .settings
                        .view
                        .as_ref()
                        .and_then(|v| v.item(&key))
                        .and_then(|i| i.shown().as_str().map(str::to_owned));
                    if let Some(p) = self.dialogs.pick_folder(start.as_deref()) {
                        self.settings.choose(&key, p.display().to_string());
                    }
                }
                SettingsAction::OpenAddOns => self.navigate(Route::AddOns),
                SettingsAction::OpenJobs => self.navigate(Route::Jobs),
                SettingsAction::OpenNotifications => {
                    self.effects.push(Effect::OpenNotifications);
                }
                SettingsAction::OpenConnections => self.load_connections(),
                SettingsAction::OpenHelp(topic) => self.effects.push(Effect::OpenHelp(topic)),
                SettingsAction::Connection(a) => self.connection_action(a),
                SettingsAction::Web(action) => self.web_action(action),
                SettingsAction::Mcp(action) => self.mcp_action(action),
            }
        }
    }

    fn mcp_scope_key(&self) -> String {
        self.settings.mcp.project.as_ref().map_or_else(
            || "workspace".to_owned(),
            |project| format!("project:{}", project.0.as_str()),
        )
    }

    fn load_mcp_servers(&mut self, cursor: Option<String>) {
        if cursor.is_none() {
            self.settings.mcp.loading = true;
        }
        let next = cursor.clone();
        let scope = self.settings.mcp.project.clone();
        let scope_key = self.mcp_scope_key();
        if !self.query(
            SettingsSlot::McpServers(scope_key, cursor),
            "Loading MCP servers",
            "mcp.servers",
            move |backend| ops::mcp_servers(backend, scope.as_ref(), next.as_deref()),
        ) {
            self.settings.mcp.loading = false;
        }
    }

    fn load_mcp_capabilities(&mut self, server_id: Id, cursor: Option<String>) {
        let Some(server) = self
            .settings
            .mcp
            .servers
            .iter()
            .find(|server| server.id == server_id)
        else {
            return;
        };
        let revision = server.catalog_revision.get();
        let scope = self.settings.mcp.project.clone();
        let slot = SettingsSlot::McpCapabilities(
            self.mcp_scope_key(),
            server_id.as_str().to_owned(),
            cursor.clone(),
        );
        self.query(
            slot,
            "Loading MCP capabilities",
            "mcp.capabilities",
            move |backend| {
                ops::mcp_capabilities(
                    backend,
                    scope.as_ref(),
                    &server_id,
                    revision,
                    cursor.as_deref(),
                )
            },
        );
    }

    /// A removed or replaced bearer token leaves its stored secret unreferenced once the new
    /// configuration is saved; delete it then.
    fn release_dropped_mcp_credentials(
        &mut self,
        saved: &devworks_core::protocol::records::McpServer,
    ) {
        let Some(old) = self
            .settings
            .mcp
            .servers
            .iter()
            .find(|server| server.id == saved.id)
        else {
            return;
        };
        let kept = crate::mcp_panel::credential_handles(saved);
        let dropped: Vec<Id> = crate::mcp_panel::credential_handles(old)
            .into_iter()
            .filter(|handle| !kept.contains(handle))
            .collect();
        self.release_mcp_credentials(dropped);
    }

    /// Delete the stored secrets behind MCP handles no saved server references any more.
    fn release_mcp_credentials(&mut self, dropped: Vec<Id>) {
        if dropped.is_empty() {
            return;
        }
        if let Some(client) = self.client.as_deref_mut() {
            client.task(
                SettingsSlot::McpRelease.into(),
                "Removing MCP credential",
                move |backend| {
                    backend.release_mcp_credentials(dropped)?;
                    // Nothing to submit: the release reply only reports a failure.
                    Ok(TaskOutput::Prepared { frame: Vec::new() })
                },
            );
        }
    }

    fn mcp_action(&mut self, action: McpAction) {
        if let McpAction::Sampling(id, ..) = &action {
            self.settings.mcp.reconnect = Some(id.clone());
        }
        match action {
            McpAction::Reload => self.load_mcp_servers(None),
            McpAction::Catalog(id, _revision) => self.load_mcp_capabilities(id, None),
            McpAction::Open(id, catalog_revision, request) => {
                let server = id.clone();
                let scope = self.settings.mcp.project.clone();
                let slot = SettingsSlot::McpContent(self.mcp_scope_key(), id.as_str().to_owned());
                let submitted = self.online().is_ok()
                    && self.client.as_deref_mut().is_some_and(|client| {
                        client.network_task(slot.into(), "Reading MCP content", move |backend| {
                            ops::mcp_content(
                                backend,
                                scope.as_ref(),
                                &id,
                                catalog_revision,
                                &request,
                            )
                            .map(TaskOutput::McpContent)
                        })
                    });
                if !submitted {
                    self.settings
                        .mcp
                        .opened(&server, Err("the app is offline or closing".into()));
                }
            }
            mut other => {
                let pending_server = match &other {
                    McpAction::Connect(id, _)
                    | McpAction::Disable(id, _)
                    | McpAction::Discover(id, _) => Some(id.clone()),
                    McpAction::Remove(id, _, handles) => {
                        self.settings.mcp.removing = Some(handles.clone());
                        Some(id.clone())
                    }
                    _ => None,
                };
                let secret = match &mut other {
                    McpAction::Configure { secret, .. } => secret.take(),
                    _ => None,
                };
                let submitted = match (other.input(self.settings.mcp.project.as_ref()), secret) {
                    (None, _) => false,
                    (Some(mut input), Some(secret)) => {
                        self.online().is_ok()
                            && self.client.as_deref_mut().is_some_and(|client| {
                                client.task(
                                    SettingsSlot::McpCommand.into(),
                                    "Storing MCP credential",
                                    move |backend| {
                                        let handle = backend.stage_mcp_api_key(secret)?;
                                        // The key goes to the OS secret store first; only its
                                        // handle travels.
                                        if let In::Mcp(mcp::Input::Configure(configure)) =
                                            &mut input
                                            && let McpTransport::Http { credential_ref, .. } =
                                                &mut configure.transport
                                        {
                                            *credential_ref = Some(handle);
                                        }
                                        Ok(TaskOutput::Prepared {
                                            frame: ops::mcp_command(backend, input),
                                        })
                                    },
                                )
                            })
                    }
                    (Some(input), None) => self.command(
                        SettingsSlot::McpCommand,
                        "Updating MCP",
                        input.operation(),
                        move |backend| ops::mcp_command(backend, input),
                    ),
                };
                if !submitted {
                    self.settings.mcp.busy = false;
                    self.settings.mcp.reconnect = None;
                    self.settings.mcp.removing = None;
                    self.settings.mcp.error = Some("MCP command could not be submitted.".into());
                } else {
                    self.settings.mcp.pending_server = pending_server;
                    self.settings.mcp.pending_operation = None;
                    self.settings.mcp.last_operation_poll = None;
                }
            }
        }
    }

    /// Runs a web fetch, search or activity load through the same use cases agents call.
    fn web_action(&mut self, action: WebAction) {
        match action {
            WebAction::Fetch(url) => {
                if !self.query(
                    SettingsSlot::WebFetch,
                    "Fetching a page",
                    "web.fetch",
                    |b| ops::web_fetch(b, &url),
                ) {
                    self.settings.web.fetching = false;
                }
            }
            WebAction::Search { query, provider } => {
                if !self.query(
                    SettingsSlot::WebSearch,
                    "Searching the web",
                    "web.search",
                    |b| ops::web_search(b, &query, &provider),
                ) {
                    self.settings.web.searching = false;
                }
            }
            WebAction::LoadActivity => {
                if !self.query(
                    SettingsSlot::WebActivity,
                    "Loading web activity",
                    "workspace.logs",
                    ops::web_activity,
                ) {
                    self.settings.web.activity_loading = false;
                }
            }
        }
    }

    // -----------------------------------------------------------------------------------------
    // Settings → Provider connections (DW-18)

    /// Loads every connection and the whole catalog, page by page, as new loads.
    fn load_connections(&mut self) {
        if let Some(b) = self.backend() {
            self.settings.connections.secret_store = Some(b.secret_store());
        }
        // Each list reads every page before it replaces what is shown.
        let p = &mut self.settings.connections;
        p.last_load = Some(Instant::now());
        let fetch = p.connections.reload_all();
        self.send_connections(fetch);
        let fetch = self.settings.connections.catalog.reload_all();
        self.send_connection_catalog(fetch);
        self.load_embedding_spaces(None);
    }

    fn send_connections(&mut self, fetch: crate::paged::Fetch) {
        let slot = SettingsSlot::Connections(fetch.generation);
        self.cx.paged(
            &mut self.settings.connections.connections,
            fetch,
            slot,
            "Loading provider connections",
            "router.connections",
            ops::router_connections,
        );
    }
    fn send_connection_catalog(&mut self, fetch: crate::paged::Fetch) {
        let slot = SettingsSlot::ConnectionCatalog(fetch.generation);
        self.cx.paged(
            &mut self.settings.connections.catalog,
            fetch,
            slot,
            "Loading models",
            "router.catalog",
            ops::router_catalog_all,
        );
    }

    fn load_embedding_spaces(&mut self, cursor: Option<String>) {
        let panel = &mut self.settings.connections.embedding_spaces;
        panel.generation += 1;
        panel.loading = true;
        panel.last_load = Some(Instant::now());
        let generation = panel.generation;
        if !self.query(
            SettingsSlot::EmbeddingSpaces(generation),
            "Loading embedding spaces",
            "router.embedding_spaces",
            move |b| ops::router_embedding_spaces(b, cursor.as_deref()),
        ) {
            self.settings.connections.embedding_spaces.loading = false;
        }
    }

    fn connection_action(&mut self, action: crate::connections_screen::ConnectionAction) {
        use crate::connections_screen::ConnectionAction as A;
        let saving_editor = matches!(&action, A::ConfigureEdit { .. });
        match action {
            A::Reload => self.load_connections(),
            A::EmbeddingSetup => self.open_embedding_setup(),
            A::EmbeddingPrice(model) => {
                self.effects.push(Effect::OpenPrices(model));
            }
            A::EmbeddingSpaces { cursor } => self.load_embedding_spaces(cursor),
            A::UseSessionOnly => {
                if let Some(b) = self.backend() {
                    match b.use_session_only_secrets() {
                        Ok(()) => {
                            self.settings.connections.message = Some((
                                Tone::Attention,
                                "Credentials are kept in memory for this session only; sign in \
                                 again after a restart."
                                    .into(),
                            ));
                            self.settings.connections.secret_store = Some(b.secret_store());
                        }
                        Err(e) => self.settings.connections.message = Some((Tone::Bad, e)),
                    }
                }
            }
            A::Configure {
                label,
                action,
                key,
                code,
            }
            | A::ConfigureEdit {
                label,
                action,
                key,
                code,
            } => {
                if self.online().is_err() {
                    self.settings.connections.message =
                        Some((Tone::Bad, "DevWorks is offline or closing.".into()));
                    return;
                }
                let p = &mut self.settings.connections;
                p.sequence += 1;
                let n = p.sequence;
                if saving_editor {
                    p.preparing_edit(n, &action);
                }
                let submitted = self.client.as_deref_mut().is_some_and(|c| {
                    c.task(SettingsSlot::Configure(n).into(), &label, move |b| {
                        let mut action = action;
                        // The key goes to the OS secret store first; only its handle travels.
                        if let Some(key) = key {
                            let handle = b
                                .stage_api_key(key)
                                .map_err(|e| format!("the secret store refused the key: {e}"))?;
                            if let RouterConfigAction::Connection { credential_ref, .. } =
                                &mut action
                            {
                                *credential_ref = Some(handle);
                            }
                        }
                        if let Some((flow, pasted)) = code {
                            b.submit_sign_in_code(&flow, pasted)?;
                        }
                        Ok(TaskOutput::Prepared {
                            frame: ops::router_configure(b, action),
                        })
                    })
                });
                if submitted {
                    self.settings.connections.busy.push((n, label));
                } else {
                    self.settings.connections.edit_finished(n, false, false);
                    self.settings.connections.message =
                        Some((Tone::Bad, "The change was not submitted.".into()));
                }
            }
        }
    }

    /// Reloads the connections panel while a sign-in is open (a device code finishes in the
    /// background), at most every few seconds.
    fn poll_connections(&mut self, ctx: &egui::Context) {
        let p = &self.settings.connections;
        let signing_in = p.connections.items.iter().any(|r| {
            r.lifecycle.as_ref().is_some_and(|l| {
                matches!(
                    l,
                    ConnectionLifecycle::Authorizing | ConnectionLifecycle::Refreshing
                )
            })
        });
        let qualifying = p.embedding_spaces.qualifying();
        if !self.settings.show_connections || (!signing_in && !qualifying) {
            return;
        }
        let due =
            !p.connections.loading() && p.last_load.is_none_or(|t| t.elapsed() >= CONNECTION_POLL);
        if due {
            self.load_connections();
        }
        ctx.request_repaint_after(CONNECTION_POLL);
    }

    fn poll_mcp(&mut self, ctx: &egui::Context) {
        let Some(operation) = self.settings.mcp.pending_operation.clone() else {
            return;
        };
        let interval = Duration::from_secs(2);
        let slot = SettingsSlot::McpOperation(self.mcp_scope_key());
        if !self.pending(&slot)
            && self
                .settings
                .mcp
                .last_operation_poll
                .is_none_or(|last| last.elapsed() >= interval)
        {
            self.settings.mcp.last_operation_poll = Some(Instant::now());
            self.query(
                slot,
                "Checking MCP operation",
                "history.operation_status",
                move |backend| crate::usage_ops::operation_status(backend, &operation),
            );
        }
        ctx.request_repaint_after(interval);
    }

    fn apply_settings(
        &mut self,
        slot: SettingsSlot,
        reply: Reply,
    ) -> Option<(SettingsSlot, Reply)> {
        match (slot, reply) {
            // Started before a session mutation was applied: it may lack that change.
            (SettingsSlot::ContainerCapabilities, Reply::Query(q)) => {
                match ops::container_capability_entries(q) {
                    Ok(entries) => {
                        self.settings.container_capabilities = Some(entries);
                        self.settings.container_error = None;
                    }
                    Err(error) => self.settings.container_error = Some(error.describe()),
                }
            }
            (SettingsSlot::SettingsRead, Reply::Query(q)) => match ops::settings_entries(q) {
                Ok((revision, entries)) => {
                    self.settings.loaded(revision, &entries);
                    self.check_settings_health(revision);
                }
                Err(e) => {
                    self.settings.loading = false;
                    // The save committed but its result could not be read back: keep every
                    // draft (a later save of an already-saved value is a no-op change).
                    self.settings.after_save = false;
                    self.settings.saving = false;
                    self.settings.submitted.clear();
                    self.settings.error = Some(e.describe());
                }
            },
            (SettingsSlot::SettingsValidate, Reply::Query(_))
                if self.settings.validated_drafts.take() != Some(self.settings.drafts()) =>
            {
                // The drafts changed while it ran: its findings describe other values.
                self.settings.validating = false;
                self.settings.message = Some((
                    Tone::Neutral,
                    "Values changed while validating; validate again.".into(),
                ));
            }
            (SettingsSlot::SettingsValidate, Reply::Query(q)) => {
                self.settings.validating = false;
                match ops::settings_report(q) {
                    Ok(report) => {
                        let valid = report.valid;
                        if let Some(v) = &mut self.settings.view {
                            v.apply_report(&report);
                        }
                        self.settings.message = Some(if valid {
                            (Tone::Good, "The changes are valid.".into())
                        } else {
                            (Tone::Bad, "Some values need attention.".into())
                        });
                    }
                    Err(e) => self.settings.message = Some((Tone::Bad, e.describe())),
                }
            }
            (SettingsSlot::SettingsHealth, Reply::Query(q)) => {
                if let Ok(report) = ops::settings_report(q) {
                    for (key, _) in self.settings.directories() {
                        let unavailable = report
                            .issues
                            .0
                            .iter()
                            .find(|i| i.path.0 == key && i.code.0 == codes::PATH_UNAVAILABLE);
                        let health = match unavailable {
                            Some(i) => Health::Unavailable(i.message.0.clone()),
                            None => Health::Available,
                        };
                        self.settings.set_health(key, health);
                    }
                }
            }
            (SettingsSlot::SettingsSave, Reply::Command(c)) => {
                let outcome = ops::settings_saved(c);
                self.settings.unconfirmed = matches!(outcome, Err(Failure::Unknown));
                match outcome {
                    Err(Failure::Unknown) => {
                        // Still "saving": the identical request confirms it, never a new one.
                        self.settings.message = Some((
                            Tone::Attention,
                            "Not yet confirmed whether the settings were saved. Use Confirm \
                             outcomes to check again."
                                .into(),
                        ));
                        self.announce("Settings: outcome not confirmed.");
                    }
                    Ok(()) => {
                        // Tier membership and other routing settings may have changed: the
                        // selectors' catalog and every availability are read again.
                        self.cx.invalidate_routing();
                        // Still saving until the follow-up read settles the drafts.
                        self.settings.after_save = true;
                        self.settings.message = Some((Tone::Good, "Settings saved.".into()));
                        self.announce("Settings saved.");
                        self.load_settings();
                    }
                    Err(e) => {
                        self.settings.saving = false;
                        self.settings.submitted.clear();
                        let conflict = matches!(&e, Failure::Refused(f) if f.code == DomainErrorCode::RevisionConflict);
                        self.settings.message = Some((
                            Tone::Bad,
                            if conflict {
                                "Settings changed elsewhere; your changes are kept for review."
                                    .into()
                            } else {
                                e.describe()
                            },
                        ));
                        self.announce("Settings were not saved.");
                        if conflict {
                            self.load_settings();
                        }
                    }
                }
            }
            (SettingsSlot::SettingsExplain(key), Reply::Query(_))
                if self.settings.explained_at.get(&key).copied().flatten()
                    != self.settings_revision() =>
            {
                // The view moved to another revision meanwhile: explain it at that one.
                self.explain(key);
            }
            (SettingsSlot::WebFetch, Reply::Query(q)) => {
                let web = &mut self.settings.web;
                web.fetching = false;
                match ops::web_document(q) {
                    Ok(doc) => {
                        web.document = Some(doc);
                        web.error = None;
                    }
                    Err(e) => web.error = Some(e.describe()),
                }
                self.web_action(WebAction::LoadActivity);
            }
            (SettingsSlot::WebSearch, Reply::Query(q)) => {
                let web = &mut self.settings.web;
                web.searching = false;
                match ops::web_results(q) {
                    Ok(results) => {
                        web.results = Some(results);
                        web.error = None;
                    }
                    Err(e) => web.error = Some(e.describe()),
                }
                self.web_action(WebAction::LoadActivity);
            }
            (SettingsSlot::WebActivity, Reply::Query(q)) => {
                let web = &mut self.settings.web;
                web.activity_loading = false;
                web.activity = Some(ops::web_entries(q).map_err(|e| e.describe()));
            }
            (SettingsSlot::McpServers(scope, _), Reply::Query(_))
                if scope != self.mcp_scope_key() => {}
            (SettingsSlot::McpServers(_, cursor), Reply::Query(q)) => match ops::mcp_server_page(q)
            {
                Ok((servers, next)) => {
                    self.settings
                        .mcp
                        .loaded_servers(servers, cursor.is_some(), next.is_none());
                    if let Some(next) = next {
                        self.load_mcp_servers(Some(next));
                    } else {
                        self.settings.mcp.loading = false;
                        self.settings.mcp.last_refresh = Some(Instant::now());
                        if let Some(server) = self.settings.mcp.selected.as_ref().and_then(|id| {
                            self.settings
                                .mcp
                                .servers
                                .iter()
                                .find(|server| &server.id == id)
                        }) && matches!(server.state, ItemState::Failed)
                        {
                            self.settings.mcp.error = Some(server.last_failure.as_ref().map_or_else(
                                || "The MCP server operation failed. Check its configuration and retry.".into(),
                                |failure| failure.0.clone(),
                            ));
                        }
                        if let Some(server) = self.settings.mcp.selected.as_ref().and_then(|id| {
                            self.settings
                                .mcp
                                .servers
                                .iter()
                                .find(|server| &server.id == id)
                        }) {
                            self.load_mcp_capabilities(server.id.clone(), None);
                        }
                    }
                }
                Err(error) => {
                    self.settings.mcp.loading = false;
                    self.settings.mcp.error = Some(error.describe());
                }
            },
            (SettingsSlot::McpContent(scope, _), Reply::Task(_))
                if scope != self.mcp_scope_key() => {}
            (SettingsSlot::McpContent(_, server), Reply::Task(t)) => {
                if let Ok(server) = Id::new(server) {
                    self.settings.mcp.opened(
                        &server,
                        match t {
                            Ok(TaskOutput::McpContent(text)) => Ok(text),
                            Ok(_) => Err("unexpected reply".into()),
                            Err(error) => Err(error),
                        },
                    );
                }
            }
            (SettingsSlot::McpCapabilities(scope, _, _), Reply::Query(_))
                if scope != self.mcp_scope_key() => {}
            (SettingsSlot::McpCapabilities(_, server, cursor), Reply::Query(q)) => {
                match ops::mcp_capability_page(q) {
                    Ok((capabilities, next)) => {
                        if let Ok(id) = Id::new(server) {
                            self.settings
                                .mcp
                                .loaded_catalog(&id, capabilities, cursor.is_some());
                            if let Some(next) = next {
                                self.load_mcp_capabilities(id, Some(next));
                            }
                        }
                    }
                    Err(error) => self.settings.mcp.error = Some(error.describe()),
                }
            }
            (SettingsSlot::McpCommand, Reply::Task(Ok(TaskOutput::Prepared { frame }))) => {
                if !self.client.as_deref_mut().is_some_and(|client| {
                    client.command(
                        SettingsSlot::McpCommand.into(),
                        "Updating MCP",
                        "mcp.configure",
                        frame,
                    )
                }) {
                    self.settings.mcp.busy = false;
                    self.settings.mcp.error =
                        Some("MCP configuration could not be submitted.".into());
                }
            }
            (SettingsSlot::McpCommand, Reply::Task(Err(error))) => {
                self.settings.mcp.busy = false;
                self.settings.mcp.pending_server = None;
                self.settings.mcp.error = Some(error);
            }
            (SettingsSlot::McpRelease, Reply::Task(Err(error))) => {
                self.settings.mcp.error = Some(format!(
                    "The removed MCP token could not be deleted: {error}"
                ));
            }
            (SettingsSlot::McpRelease, Reply::Task(Ok(_))) => {}
            (SettingsSlot::McpCommand, Reply::Command(c)) => {
                self.settings.mcp.busy = false;
                match ops::mcp_command_status(c) {
                    Ok((message, operation, configured)) => {
                        self.settings.mcp.message = Some(message);
                        self.settings.mcp.error = None;
                        let reconnect = self.settings.mcp.reconnect.take();
                        // A removed server's tokens are no longer referenced once the removal
                        // is saved, whether or not its session has closed yet.
                        if let Some(handles) = self.settings.mcp.removing.take() {
                            if self.settings.mcp.pending_server.as_ref()
                                == self.settings.mcp.selected.as_ref()
                            {
                                self.settings.mcp.selected = None;
                                self.settings.mcp.capabilities.clear();
                            }
                            self.release_mcp_credentials(handles);
                        }
                        if let Some(server) = &configured {
                            self.release_dropped_mcp_credentials(server);
                            self.settings.mcp.saved(server);
                        }
                        if let Some(server) = &configured {
                            self.settings.mcp.selected = Some(server.id.clone());
                        }
                        self.settings.mcp.pending_operation = operation;
                        if self.settings.mcp.pending_operation.is_none() {
                            self.settings.mcp.pending_server = None;
                        }
                        self.load_mcp_servers(None);
                        if let Some(server) =
                            configured.filter(|s| reconnect.as_ref() == Some(&s.id))
                        {
                            self.mcp_action(McpAction::Connect(server.id, server.revision.get()));
                        }
                    }
                    Err(error) => {
                        self.settings.mcp.reconnect = None;
                        self.settings.mcp.removing = None;
                        self.settings.mcp.keep_selection = false;
                        self.settings.mcp.pending_server = None;
                        self.settings.mcp.pending_operation = None;
                        self.settings.mcp.error = Some(error.describe());
                    }
                }
            }
            (SettingsSlot::McpOperation(scope), Reply::Query(_))
                if scope != self.mcp_scope_key() => {}
            (SettingsSlot::McpOperation(_), Reply::Query(q)) => match ops::query_result(q) {
                Ok((
                    devworks_core::protocol::results::OperationResult::HistoryOperationStatus {
                        value,
                    },
                    _,
                )) => {
                    if self
                        .settings
                        .mcp
                        .observed_operation(&value.operation_id, value.state)
                    {
                        self.load_mcp_servers(None);
                    }
                }
                Ok(_) => {
                    self.settings.mcp.error =
                        Some("Unexpected MCP operation status response".into())
                }
                Err(error) => self.settings.mcp.error = Some(error.describe()),
            },
            (SettingsSlot::SettingsExplain(key), Reply::Query(q)) => match ops::settings_chain(q) {
                Ok(chain) => {
                    if let Some(v) = &mut self.settings.view {
                        v.apply_explanation(&key, &chain);
                    }
                    self.settings.explained.insert(key);
                }
                Err(e) => self.settings.message = Some((Tone::Bad, e.describe())),
            },
            (SettingsSlot::EmbeddingSpaces(g), Reply::Query(q)) => {
                let panel = &mut self.settings.connections.embedding_spaces;
                if g != panel.generation {
                    return None;
                }
                panel.loading = false;
                match ops::embedding_space_page(q) {
                    Ok(page) => panel.loaded(page),
                    Err(error) => panel.error = Some(error.describe()),
                }
            }
            (SettingsSlot::Connections(g), Reply::Query(q)) => {
                let page = ops::connection_records(q)
                    .map(|(items, next)| crate::paged::Page { items, next })
                    .map_err(|e| e.describe());
                let panel = &mut self.settings.connections;
                match panel.connections.apply(g, page) {
                    Applied::Continue(fetch) => self.send_connections(fetch),
                    Applied::Replaced => panel.adopt_connections(),
                    Applied::Stale => return None,
                    Applied::Failed | Applied::Appended => {}
                }
            }
            (SettingsSlot::ConnectionCatalog(g), Reply::Query(q)) => {
                let page = ops::route_models(q)
                    .map(|(items, next)| crate::paged::Page { items, next })
                    .map_err(|e| e.describe());
                let panel = &mut self.settings.connections;
                match panel.catalog.apply(g, page) {
                    Applied::Continue(fetch) => self.send_connection_catalog(fetch),
                    Applied::Replaced => panel.adopt_catalog(),
                    Applied::Stale => return None,
                    Applied::Failed | Applied::Appended => {}
                }
            }
            (SettingsSlot::Configure(n), Reply::Task(Ok(TaskOutput::Prepared { frame }))) => {
                self.settings.connections.prepared_edit(n, &frame);
                let label = self
                    .settings
                    .connections
                    .busy
                    .iter()
                    .find(|(k, _)| *k == n)
                    .map(|(_, label)| label.clone())
                    .unwrap_or_else(|| "Configuring provider".into());
                // Preparation has no router effects. The client owns the exact frame before
                // admission, including when a close has already stopped new requests.
                if let Some(client) = self.client.as_deref_mut() {
                    client.command(
                        SettingsSlot::Configure(n).into(),
                        &label,
                        "router.configure",
                        frame,
                    );
                }
            }
            (SettingsSlot::Configure(n), Reply::Command(outcome)) => {
                let unknown = is_unknown(&outcome);
                let result = ops::configured(outcome);
                let p = &mut self.settings.connections;
                let label = p
                    .busy
                    .iter()
                    .find(|(k, _)| *k == n)
                    .map(|(_, label)| label.clone())
                    .unwrap_or_default();
                if !unknown {
                    p.busy.retain(|(k, _)| *k != n);
                }
                p.edit_finished(n, result.is_ok(), unknown);
                p.message = Some(match result {
                    Ok(true) => (Tone::Good, format!("{label}: done.")),
                    Ok(false) => (Tone::Neutral, format!("{label}: nothing changed.")),
                    Err(e) => (Tone::Bad, format!("{label}: {}", e.describe())),
                });
                self.load_connections();
                self.cx.invalidate_routing();
            }
            (SettingsSlot::Configure(n), Reply::Task(t)) => {
                // A task failure before preparation has no router effects. A failed command
                // worker, however, leaves its exact frame unresolved in the client.
                let unconfirmed = self.client.as_ref().is_some_and(|c| {
                    c.unconfirmed()
                        .iter()
                        .any(|u| u.slot == crate::backend::Slot::from(SettingsSlot::Configure(n)))
                });
                let p = &mut self.settings.connections;
                p.edit_finished(n, false, unconfirmed);
                let label = p
                    .busy
                    .iter()
                    .find(|(k, _)| *k == n)
                    .map(|(_, label)| label.clone())
                    .unwrap_or_default();
                if !unconfirmed {
                    p.busy.retain(|(k, _)| *k != n);
                }
                p.message = Some((
                    if unconfirmed {
                        Tone::Attention
                    } else {
                        Tone::Bad
                    },
                    match t {
                        Err(e) => format!(
                            "{label}: {e}{}",
                            if unconfirmed {
                                ". The original change is awaiting confirmation."
                            } else {
                                ""
                            }
                        ),
                        Ok(_) => format!("{label}: unexpected reply."),
                    },
                ));
            }
            (SettingsSlot::SettingsSecret(key), Reply::Task(result)) => {
                self.setting_secret_prepared(&key, result)
            }
            other => return Some(other),
        }
        None
    }
}
