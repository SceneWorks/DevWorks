//! Add-ons requests and replies: the `addons.*` use cases agent tools reach, the bundle
//! download or file save that precedes an install, and the runtime and index discovery behind
//! the screen. Every lifecycle change is one framed command with its exact frame kept.
use crate::addons::{self as a, AddonsAction, AddonsScreen};
use crate::addons_ops::AddonsSlot;
use crate::attachments;
use crate::backend::{Reply, TaskOutput, Unresolved, is_unknown};
use crate::memory::RouteMemory;
use crate::ops;
use crate::route::Route;
use crate::screen::{Cx, Screen, ScreenContext, ScreenId, controller};
use eframe::egui::Ui;

controller!(AddonsCtl, addons: AddonsScreen);

impl Screen for AddonsScreen {
    type Slot = AddonsSlot;
    const ID: ScreenId = ScreenId::AddOns;
    fn load(&mut self, cx: &mut Cx, _route: Route) {
        let mut ctl = AddonsCtl { addons: self, cx };
        ctl.load_addons();
        ctl.discover();
    }
    fn refresh(&mut self, _cx: &mut Cx, _route: Route) {}
    fn apply(&mut self, cx: &mut Cx, slot: AddonsSlot, reply: Reply) {
        if let Some((slot, reply)) = (AddonsCtl { addons: self, cx }).apply_addons(slot, reply) {
            cx.unhandled(&slot, reply);
        }
    }
    fn ui(&mut self, cx: &mut Cx, _route: Route, ui: &mut Ui) {
        let online = cx.online().map_err(str::to_owned);
        let mut actions = Vec::new();
        AddonsScreen::ui(
            self,
            ui,
            online.as_ref().map(|_| ()).map_err(String::as_str),
            &mut actions,
        );
        AddonsCtl { addons: self, cx }.addons_actions(actions);
    }
    fn restore_in_flight(&mut self, _slot: &AddonsSlot, _u: &Unresolved) {
        self.busy = true;
    }
    fn context(&self, _route: Route, mem: &RouteMemory) -> ScreenContext {
        ScreenContext {
            tab: mem.tab,
            selection: self.selected.clone(),
        }
    }
}

impl AddonsCtl<'_, '_> {
    /// Loads the whole catalog, following its pages (see the `AddonsList` reply).
    fn load_addons(&mut self) {
        self.addons.catalog_pending = None;
        self.query(
            AddonsSlot::AddonsList,
            "Loading add-ons",
            "addons.catalog",
            |b| a::catalog_frame(b, None),
        );
    }
    /// Checks the runtimes and refreshes the package indexes.
    fn discover(&mut self) {
        if let Some(c) = self.client.as_deref_mut() {
            c.network_task(
                AddonsSlot::AddonsDiscovery.into(),
                "Checking runtimes and package indexes",
                |b| b.refresh_packages().map(|()| TaskOutput::Refreshed),
            );
        }
    }
    /// Loads a package's logs from the start, following their pages to the end.
    fn load_addon_logs(&mut self, package: String) {
        self.addons.logs_pending.insert(package.clone(), Vec::new());
        let p = package.clone();
        self.query(
            AddonsSlot::AddonsLogs(package),
            "Loading add-on logs",
            "addons.logs",
            move |b| a::logs_frame(b, &p, None),
        );
    }
    fn addons_actions(&mut self, actions: Vec<AddonsAction>) {
        for action in actions {
            let registry = self.addons.registry_revision;
            let command = |op: &'static str,
                           cmd: serde_json::Value|
             -> (&'static str, serde_json::Value) { (op, cmd) };
            let lifecycle = match action {
                AddonsAction::Refresh => {
                    self.load_addons();
                    None
                }
                AddonsAction::Discover => {
                    self.discover();
                    None
                }
                AddonsAction::InstallOffer {
                    package,
                    digest,
                    update,
                } => {
                    if let Some(c) = self.client.as_deref_mut() {
                        self.addons.busy = c.network_task(
                            AddonsSlot::AddonsSave {
                                update: update.then_some(package.clone()),
                                registry,
                                download: None,
                            }
                            .into(),
                            "Downloading the add-on",
                            move |b| {
                                let path = b.fetch_index_bundle(&package, &digest)?;
                                a::prepare_downloaded_install(b, path)
                            },
                        );
                    }
                    None
                }
                AddonsAction::Select(id) => {
                    self.addons.selected = Some(id);
                    None
                }
                AddonsAction::Logs(id) => {
                    self.load_addon_logs(id);
                    None
                }
                AddonsAction::OpenIndexing => {
                    self.navigate(Route::Indexing);
                    None
                }
                AddonsAction::InstallFromFile(update) => {
                    if let Some(path) = self.cx.dialogs.pick_package()
                        && let Some(c) = self.cx.client.as_deref_mut()
                    {
                        self.addons.busy = c.task(
                            AddonsSlot::AddonsSave {
                                update,
                                registry,
                                download: None,
                            }
                            .into(),
                            "Preparing the add-on",
                            move |b| a::prepare_install_from_file(b, &path),
                        );
                    }
                    None
                }
                AddonsAction::Approve {
                    package,
                    bundle,
                    update,
                } => {
                    let bundle = serde_json::to_value(&bundle).unwrap_or_default();
                    let expected = registry.max(1).to_string();
                    Some(if update {
                        command(
                            "addons.update",
                            serde_json::json!({"kind": "Update", "packageId": package, "bundle": bundle, "expectedRegistryRevision": expected}),
                        )
                    } else {
                        command(
                            "addons.install",
                            serde_json::json!({"kind": "Install", "bundle": bundle, "expectedRegistryRevision": expected}),
                        )
                    })
                }
                AddonsAction::Enable(p) => Some(command(
                    "addons.enable",
                    serde_json::json!({"kind": "Enable", "packageId": p, "expectedRegistryRevision": registry.max(1).to_string()}),
                )),
                AddonsAction::Disable(p) => Some(command(
                    "addons.disable",
                    serde_json::json!({"kind": "Disable", "packageId": p, "expectedRegistryRevision": registry.max(1).to_string()}),
                )),
                AddonsAction::Uninstall(p) => Some(command(
                    "addons.uninstall",
                    serde_json::json!({"kind": "Uninstall", "packageId": p, "expectedRegistryRevision": registry.max(1).to_string()}),
                )),
                AddonsAction::Rollback {
                    package,
                    target,
                    generation,
                } => Some(command(
                    "addons.rollback",
                    serde_json::json!({"kind": "Rollback", "packageId": package, "targetManifestDigest": target, "expectedRegistryRevision": registry.max(1).to_string(), "expectedStateGeneration": generation.max(1).to_string()}),
                )),
                AddonsAction::TrustPublisher {
                    package,
                    revision,
                    trust,
                } => {
                    self.addons.busy = true;
                    self.command(
                        AddonsSlot::AddonsCommand,
                        "Saving publisher trust",
                        "addons.configure",
                        move |b| a::trust_frame(b, &package, revision, trust),
                    );
                    None
                }
                AddonsAction::Configure {
                    package,
                    revision,
                    schema,
                    value,
                } => {
                    self.addons.busy = true;
                    self.command(
                        AddonsSlot::AddonsCommand,
                        "Saving the add-on configuration",
                        "addons.configure",
                        move |b| a::configure_frame(b, &package, revision, schema, value),
                    );
                    None
                }
                AddonsAction::ProjectEnablement {
                    package,
                    revision,
                    disabled,
                } => {
                    self.addons.busy = true;
                    self.command(
                        AddonsSlot::AddonsCommand,
                        "Saving project enablement",
                        "addons.configure",
                        move |b| a::project_frame(b, &package, revision, &disabled),
                    );
                    None
                }
            };
            if let Some((op, cmd)) = lifecycle {
                self.addons.busy = true;
                self.command(
                    AddonsSlot::AddonsCommand,
                    "Updating the add-on",
                    op,
                    move |b| a::lifecycle_frame(b, cmd),
                );
            }
        }
    }

    fn apply_addons(&mut self, slot: AddonsSlot, reply: Reply) -> Option<(AddonsSlot, Reply)> {
        match (slot, reply) {
            (AddonsSlot::AddonsList, Reply::Query(q)) => match a::catalog(q) {
                Ok((items, registry, next)) => {
                    let (mut all, first) = self
                        .addons
                        .catalog_pending
                        .take()
                        .unwrap_or_else(|| (Vec::new(), registry));
                    let pages = all.len() / usize::from(ops::PAGE_LIMIT) + 1;
                    if registry != first {
                        // The registry changed between pages: read it again from the start.
                        self.load_addons();
                    } else if let Some(cursor) = next.filter(|_| pages < a::CATALOG_PAGES) {
                        all.extend(items);
                        self.addons.catalog_pending = Some((all, first));
                        self.query(
                            AddonsSlot::AddonsList,
                            "Loading add-ons",
                            "addons.catalog",
                            move |b| a::catalog_frame(b, Some(&cursor)),
                        );
                    } else {
                        all.extend(items);
                        self.addons.items = all;
                        self.addons.registry_revision = registry;
                        self.addons.loaded = true;
                        self.addons.error = None;
                    }
                }
                Err(e) => {
                    self.addons.catalog_pending = None;
                    self.addons.error = Some(e.describe());
                }
            },
            (AddonsSlot::AddonsLogs(id), Reply::Query(q)) => match a::logs(q) {
                Ok((entries, next)) => {
                    let mut all = self.addons.logs_pending.remove(&id).unwrap_or_default();
                    all.extend(entries);
                    if let Some((p, d)) = self.addons.awaiting.clone()
                        && p == id
                        && let Some(e) = all.iter().find(|e| e.id.as_str() == d)
                    {
                        self.addons.config_errors.insert(p, e.message.0.clone());
                        self.addons.awaiting = None;
                    }
                    // Only the newest entries are kept while the log is followed to its end.
                    if all.len() > a::LOGS_KEPT {
                        all.drain(..all.len() - a::LOGS_KEPT);
                    }
                    match next {
                        Some(cursor) => {
                            self.addons.logs_pending.insert(id.clone(), all);
                            self.query(
                                AddonsSlot::AddonsLogs(id.clone()),
                                "Loading add-on logs",
                                "addons.logs",
                                move |b| a::logs_frame(b, &id, Some(&cursor)),
                            );
                        }
                        None => {
                            self.addons.logs.insert(id, all);
                        }
                    }
                }
                Err(e) => {
                    self.addons.logs_pending.remove(&id);
                    self.addons.notice = Some(e.describe());
                }
            },
            (AddonsSlot::AddonsCommand, Reply::Command(outcome)) => {
                self.addons.busy = is_unknown(&outcome);
                // A configuration refused by the package's schema names the diagnostic that
                // carries its field errors: fetch it for the form.
                if let Some(package) = a::configured_package(&outcome) {
                    self.addons.config_errors.remove(&package);
                }
                if let Some((package, diagnostic)) = a::configuration_refusal(&outcome) {
                    self.addons.awaiting = Some((package.clone(), diagnostic));
                    self.load_addon_logs(package);
                }
                self.addons.notice = Some(a::describe(outcome));
                self.load_addons();
            }
            (AddonsSlot::AddonsDiscovery, Reply::Task(t)) => {
                if let Err(e) = t {
                    self.addons.notice = Some(e);
                }
                self.load_addons();
            }
            (
                AddonsSlot::AddonsSave {
                    update, registry, ..
                },
                Reply::Task(Ok(TaskOutput::PackagePrepared { frame, download })),
            ) => {
                if let Some(client) = self.client.as_deref_mut() {
                    client.command(
                        AddonsSlot::AddonsSave {
                            update,
                            registry,
                            download: Some(download.into_path()),
                        }
                        .into(),
                        "Saving the add-on bundle",
                        "artifacts.save",
                        frame,
                    );
                }
            }
            (
                AddonsSlot::AddonsSave {
                    update,
                    registry,
                    download,
                },
                Reply::Task(Ok(TaskOutput::Prepared { frame })),
            ) => {
                if let Some(client) = self.client.as_deref_mut() {
                    client.command(
                        AddonsSlot::AddonsSave {
                            update,
                            registry,
                            download,
                        }
                        .into(),
                        "Saving the add-on bundle",
                        "artifacts.save",
                        frame,
                    );
                }
            }
            (
                AddonsSlot::AddonsSave {
                    update,
                    registry,
                    download,
                },
                Reply::Command(outcome),
            ) => {
                let unknown = is_unknown(&outcome);
                if !unknown && let (Some(path), Some(backend)) = (&download, self.backend()) {
                    attachments::remove_owned(path, &backend.scratch_dir().join("packages"));
                }
                match ops::saved_artifact(outcome) {
                    Ok(saved) => {
                        if let Some(backend) = self.backend() {
                            let (op, frame) = a::install_saved(
                                &*backend,
                                &saved.artifact,
                                update.as_deref(),
                                registry,
                            );
                            if let Some(client) = self.client.as_deref_mut() {
                                client.command(
                                    AddonsSlot::AddonsCommand.into(),
                                    "Installing the add-on",
                                    op,
                                    frame,
                                );
                            }
                        }
                    }
                    Err(e) => {
                        self.addons.busy = unknown;
                        self.addons.notice = Some(e.describe());
                    }
                }
            }
            (AddonsSlot::AddonsSave { .. }, Reply::Task(t)) => {
                self.addons.busy = false;
                self.addons.notice = Some(match t {
                    Err(e) => e,
                    Ok(_) => "unexpected reply".into(),
                });
            }
            other => return Some(other),
        }
        None
    }
}
