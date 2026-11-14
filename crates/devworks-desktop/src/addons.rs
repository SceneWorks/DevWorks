//! Settings → Add-ons (ADDON-02, EXT-05): one management screen for language packs and harness
//! packages over the same `addons.*` use cases agent tools reach.
//!
//! The list shows every package with its kind, version, effective availability (not installed,
//! disabled, missing dependency, incompatible, ready, failed) **separately** from its configured
//! enablement, and any pending version with what it waits for. The detail shows publisher,
//! signer and trust, targets and whether this platform is supported, languages, contributions,
//! prerequisites and whether each is satisfied, requested and effective access, the state
//! generation and rollback predecessor, the last failure and the package's logs; and offers
//! install/update from a bundle file, approval of a pending version (the person's install of
//! exactly that bundle), enable/disable, rollback, uninstall, publisher trust for updates, pack
//! configuration and per-project enablement. Indexing progress opens the Indexing screen.
//!
//! Nothing here decides a rule: every action is a framed command the gateway admits, and a
//! refusal is shown as the use case recorded it.
use crate::theme;
use crate::widgets::{self, Tone};
use devworks_core::protocol::provider::ArtifactRef;
use devworks_core::protocol::records::{
    Diagnostic, PackageAvailability, PackageRecord, PrerequisiteState,
};
use eframe::egui::{self, RichText, Ui};
use std::collections::HashMap;
use std::path::PathBuf;

/// What the person asked the screen to do.
#[derive(Clone, Debug)]
pub enum AddonsAction {
    Refresh,
    Select(String),
    /// Pick a bundle file and install it (a new package) or update `Some(package)` with it.
    InstallFromFile(Option<String>),
    /// Approve a pending version: install or update with exactly its bundle.
    Approve {
        package: String,
        bundle: ArtifactRef,
        update: bool,
    },
    Enable(String),
    Disable(String),
    Uninstall(String),
    Rollback {
        package: String,
        target: String,
        generation: i64,
    },
    TrustPublisher {
        package: String,
        revision: i64,
        trust: bool,
    },
    Configure {
        package: String,
        revision: i64,
        schema: serde_json::Value,
        value: serde_json::Value,
    },
    ProjectEnablement {
        package: String,
        revision: i64,
        disabled: Vec<String>,
    },
    Logs(String),
    OpenIndexing,
    /// Probe runtimes and refresh the signed package indexes.
    Discover,
    /// Download a version a verified index offers and install (or update with) it.
    InstallOffer {
        package: String,
        digest: String,
        update: bool,
    },
}

/// The screen's state between frames.
#[derive(Default)]
pub struct AddonsScreen {
    pub items: Vec<PackageRecord>,
    pub loaded: bool,
    pub error: Option<String>,
    /// The registry revision the list was read at (the lifecycle commands' expected revision).
    pub registry_revision: i64,
    pub selected: Option<String>,
    pub logs: HashMap<String, Vec<Diagnostic>>,
    /// The outcome of the last action, shown above the detail.
    pub notice: Option<String>,
    /// Configuration being edited (JSON), per package.
    pub config_text: HashMap<String, String>,
    /// Project IDs the package is disabled for, being edited (comma separated), per package.
    pub projects_text: HashMap<String, String>,
    project_bases: HashMap<String, (String, i64)>,
    config_bases: HashMap<String, (String, i64, serde_json::Value)>,
    pub filter: String,
    pub busy: bool,
    /// A configuration refusal whose field errors are being fetched: (package, diagnostic).
    pub awaiting: Option<(String, String)>,
    /// The field errors of the last refused configuration, per package.
    pub config_errors: HashMap<String, String>,
    /// Log pages read so far while a package's logs are followed to their end.
    pub logs_pending: HashMap<String, Vec<Diagnostic>>,
    /// Catalog pages read so far (and the registry revision of the first) while the catalog is
    /// followed to its end.
    pub catalog_pending: Option<(Vec<PackageRecord>, i64)>,
}

pub fn availability_label(a: Option<&PackageAvailability>) -> (&'static str, Tone) {
    match a {
        Some(PackageAvailability::Ready) => ("Ready", Tone::Good),
        Some(PackageAvailability::Disabled) => ("Disabled", Tone::Neutral),
        Some(PackageAvailability::NotInstalled) => ("Not installed", Tone::Neutral),
        Some(PackageAvailability::MissingDependency) => ("Missing dependency", Tone::Attention),
        Some(PackageAvailability::Incompatible) => ("Incompatible", Tone::Attention),
        Some(PackageAvailability::Failed) => ("Failed", Tone::Bad),
        None => ("Unknown", Tone::Neutral),
    }
}
fn kind_label(r: &PackageRecord) -> &'static str {
    match r.package_kind.as_ref().map(|k| k.0.as_str()) {
        Some("LanguagePack") => "Language pack",
        _ => "Harness package",
    }
}
fn list(
    v: &Option<
        devworks_core::protocol::support::BoundedList<
            devworks_core::protocol::support::BoundedText<1, 256>,
            0,
            128,
        >,
    >,
) -> String {
    v.as_ref()
        .map(|l| {
            l.0.iter()
                .map(|x| x.0.clone())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "none".into())
}

impl AddonsScreen {
    pub fn selected(&self) -> Option<&PackageRecord> {
        let s = self.selected.as_deref()?;
        self.items.iter().find(|r| r.id.as_str() == s)
    }

    pub fn ui(&mut self, ui: &mut Ui, online: Result<(), &str>, actions: &mut Vec<AddonsAction>) {
        let mut st = (&mut *self, &mut *actions);
        widgets::pane(
            ui,
            widgets::Header {
                title: "Add-ons",
                summary: "Language packs and harness packages: availability, access, approval and logs.",
                tabs: &[],
                tab: 0,
            },
            &mut st,
            |ui, st| {
                if widgets::secondary_button(ui, "Refresh").clicked() {
                    st.1.push(AddonsAction::Discover);
                    st.1.push(AddonsAction::Refresh);
                }
                if ui
                    .add_enabled(!st.0.busy, egui::Button::new("Install from file…"))
                    .on_hover_text("Install a package bundle (.dwpkg). Installing it approves exactly its contents.")
                    .clicked()
                {
                    st.1.push(AddonsAction::InstallFromFile(None));
                }
            },
            |ui, st, _| st.0.body(ui, online, st.1),
        );
    }

    fn body(&mut self, ui: &mut Ui, online: Result<(), &str>, actions: &mut Vec<AddonsAction>) {
        if let Err(why) = online {
            ui.label(RichText::new(why).color(theme::MUTED));
            return;
        }
        if let Some(e) = &self.error {
            widgets::chip(ui, format!("Could not load add-ons: {e}"), Tone::Bad);
        }
        if let Some(n) = &self.notice {
            ui.label(RichText::new(n).color(theme::MUTED));
        }
        widgets::caption(
            ui,
            "A package helper is a separate process for failure containment, not an OS sandbox: it runs with your user account's rights. Approval and publisher trust record who published which exact contents and that you agreed to run them.",
        );
        if !self.loaded {
            ui.label("Loading add-ons…");
            return;
        }
        if self.items.is_empty() {
            ui.label(RichText::new("No add-ons installed").strong());
            ui.label("Install a language pack or harness package from a bundle file.");
            return;
        }
        let r = ui.add(
            egui::TextEdit::singleline(&mut self.filter)
                .hint_text("Filter add-ons")
                .desired_width(280.0),
        );
        r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Filter add-ons"));
        ui.add_space(6.0);
        let filter = self.filter.to_lowercase();
        let rows: Vec<PackageRecord> = self
            .items
            .iter()
            .filter(|r| filter.is_empty() || r.id.as_str().to_lowercase().contains(&filter))
            .cloned()
            .collect();
        egui::ScrollArea::vertical()
            .id_salt("addons-list")
            .max_height(220.0)
            .show(ui, |ui| {
                for r in &rows {
                    let (avail, tone) = availability_label(r.availability.as_ref());
                    let selected = self.selected.as_deref() == Some(r.id.as_str());
                    ui.horizontal(|ui| {
                        let text = format!(
                            "{}  ·  {}  ·  {}  ·  {}",
                            r.id.as_str(),
                            kind_label(r),
                            r.version.0,
                            if r.enabled { "enabled" } else { "not enabled" }
                        );
                        if ui.selectable_label(selected, text).clicked() {
                            actions.push(AddonsAction::Select(r.id.as_str().to_owned()));
                        }
                        widgets::chip(ui, avail, tone);
                        if let Some(p) = &r.pending_version {
                            let waits = r
                                .pending_waits
                                .as_ref()
                                .map(|w| {
                                    w.0.iter()
                                        .map(|w| format!("{w:?}"))
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                })
                                .unwrap_or_default();
                            widgets::chip(ui, format!("{} pending: {waits}", p.0), Tone::Attention);
                        }
                    });
                }
            });
        widgets::divider(ui);
        if let Some(r) = self.selected().cloned() {
            // The detail scrolls within the remaining height, so every control in it (the
            // configuration form included) can be brought into view at any window size and
            // whatever banners sit above it.
            egui::ScrollArea::vertical()
                .id_salt("addons-detail")
                .auto_shrink([false, false])
                .show(ui, |ui| self.detail(ui, &r, actions));
        } else {
            ui.label(RichText::new("Select an add-on to see its details.").color(theme::MUTED));
        }
    }

    fn detail(&mut self, ui: &mut Ui, r: &PackageRecord, actions: &mut Vec<AddonsAction>) {
        let id = r.id.as_str().to_owned();
        let (avail, tone) = availability_label(r.availability.as_ref());
        ui.horizontal(|ui| {
            ui.heading(RichText::new(&id).color(theme::TEXT));
            widgets::chip(ui, avail, tone);
            widgets::chip(
                ui,
                if r.enabled { "Enabled" } else { "Not enabled" },
                Tone::Neutral,
            );
        });
        if let Some(why) = &r.availability_reason {
            ui.label(RichText::new(&why.0).color(theme::MUTED));
        }
        let row = |ui: &mut Ui, k: &str, v: String| {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(k).strong());
                ui.label(v);
            });
        };
        row(ui, "Kind", kind_label(r).into());
        row(
            ui,
            "Version",
            format!(
                "{} ({})",
                r.version.0,
                &String::from(r.digest.clone())[..12]
            ),
        );
        row(
            ui,
            "Publisher",
            r.publisher_id
                .as_ref()
                .map_or("unknown".into(), |p| p.as_str().to_owned()),
        );
        row(
            ui,
            "Trust",
            format!(
                "{}{}",
                r.trust_state.0,
                if r.publisher_trusted == Some(true) {
                    " · updates from this publisher are trusted"
                } else {
                    ""
                }
            ),
        );
        row(
            ui,
            "Signer",
            r.signer_key
                .as_ref()
                .map_or("unsigned".into(), |k| k.0.chars().take(16).collect()),
        );
        row(
            ui,
            "Platforms",
            format!(
                "{}{}",
                list(&r.targets),
                if r.host_target_supported == Some(false) {
                    " · not this platform"
                } else {
                    ""
                }
            ),
        );
        row(ui, "Languages", list(&r.languages));
        row(
            ui,
            "Contributions",
            r.contributions.as_ref().map_or("none".into(), |c| {
                c.0.iter()
                    .map(|x| x.0.clone())
                    .collect::<Vec<_>>()
                    .join(", ")
            }),
        );
        row(ui, "Requested access", list(&r.requested_access));
        row(ui, "Effective access", list(&r.effective_access));
        if let Some(p) = &r.prerequisites
            && !p.0.is_empty()
        {
            ui.label(RichText::new("Prerequisites").strong());
            for q in &p.0 {
                let state = match q.state.as_ref() {
                    Some(PrerequisiteState::Ready) => "ready",
                    Some(PrerequisiteState::Incompatible) => "incompatible",
                    _ => "missing",
                };
                let tone = match (q.satisfied, q.required) {
                    (true, _) => Tone::Good,
                    (false, true) => Tone::Bad,
                    (false, false) => Tone::Attention,
                };
                let found = q
                    .detected_version
                    .as_ref()
                    .map_or_else(String::new, |v| format!(" · found {}", v.0));
                ui.horizontal(|ui| {
                    ui.label(format!(
                        "{} {} {}{found}",
                        q.kind.0, q.id.0, q.requirement.0
                    ));
                    widgets::chip(
                        ui,
                        if q.required {
                            state.to_owned()
                        } else {
                            format!("optional, {state}")
                        },
                        tone,
                    );
                });
            }
        }
        row(
            ui,
            "State generation",
            r.state_generation
                .as_ref()
                .map_or("none".into(), |g| g.get().to_string()),
        );
        if let Some(code) = &r.failure_code {
            widgets::chip(ui, format!("Last failure: {}", code.0), Tone::Bad);
        }
        let revision = r.revision.get();
        // Versions signed publisher indexes offer (installing one approves exactly it).
        if let Some(offers) = &r.available
            && !offers.0.is_empty()
        {
            ui.label(RichText::new("Available").strong());
            let installed = r.installed == Some(true);
            for o in &offers.0 {
                // Wrapped: a long index location (a local folder path) must not push the
                // install button past the window's right edge.
                ui.horizontal_wrapped(|ui| {
                    ui.label(format!(
                        "{} from {} (publisher {}){}",
                        o.version.0,
                        o.source.0,
                        o.publisher_id.as_str(),
                        if o.host_target_supported {
                            ""
                        } else {
                            " · not for this platform"
                        }
                    ));
                    let digest = String::from(o.manifest_digest.clone());
                    let current = String::from(r.digest.clone()) == digest && installed;
                    if !current
                        && o.host_target_supported
                        && ui
                            .button(if installed {
                                format!("Update to {}", o.version.0)
                            } else {
                                format!("Install {}", o.version.0)
                            })
                            .clicked()
                    {
                        actions.push(AddonsAction::InstallOffer {
                            package: id.clone(),
                            digest,
                            update: installed,
                        });
                    }
                });
            }
        }
        // Pending version: approval is installing exactly that bundle.
        if let Some(p) = &r.pending_version {
            ui.add_space(6.0);
            let waits = r
                .pending_waits
                .as_ref()
                .map(|w| {
                    w.0.iter()
                        .map(|w| format!("{w:?}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            ui.label(
                RichText::new(format!("Version {} is staged and waits for: {waits}", p.0))
                    .color(theme::AMBER),
            );
            if let Some(pr) = &r.pending_proposal_id {
                ui.label(RichText::new(format!("Proposed by an agent (proposal {}); approval binds to these exact contents.", pr.as_str())).color(theme::MUTED));
            }
            if let Some(bundle) = &r.pending_bundle
                && ui.button(format!("Approve and activate {}", p.0)).clicked()
            {
                actions.push(AddonsAction::Approve {
                    package: id.clone(),
                    bundle: bundle.clone(),
                    // From the installation state, never from availability (an incompatible
                    // first install is not installed: it is installed, not updated).
                    update: r.installed == Some(true),
                });
            }
        }
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            // Lifecycle actions follow the installation state itself.
            let installed = r.installed == Some(true);
            if installed && r.enabled && ui.button("Disable").clicked() {
                actions.push(AddonsAction::Disable(id.clone()));
            }
            if installed
                && (!r.enabled || r.failure_code.is_some())
                && ui
                    .button(if r.enabled { "Retry" } else { "Enable" })
                    .clicked()
            {
                actions.push(AddonsAction::Enable(id.clone()));
            }
            if installed && ui.button("Update from file…").clicked() {
                actions.push(AddonsAction::InstallFromFile(Some(id.clone())));
            }
            if let (true, Some(v), Some(d), Some(g)) = (
                installed,
                &r.predecessor_version,
                &r.predecessor_digest,
                &r.state_generation,
            ) && ui.button(format!("Roll back to {}", v.0)).clicked()
            {
                actions.push(AddonsAction::Rollback {
                    package: id.clone(),
                    target: String::from(d.clone()),
                    generation: g.get(),
                });
            }
            if installed
                && ui
                    .button("Uninstall")
                    .on_hover_text(
                        "Removes the helper and its files; package state and history are kept.",
                    )
                    .clicked()
            {
                actions.push(AddonsAction::Uninstall(id.clone()));
            }
            if ui.button("Logs").clicked() {
                actions.push(AddonsAction::Logs(id.clone()));
            }
            if r.package_kind
                .as_ref()
                .is_some_and(|k| k.0 == "LanguagePack")
                && ui.button("Indexing progress").clicked()
            {
                actions.push(AddonsAction::OpenIndexing);
            }
        });
        // Publisher trust for automatic compatible updates (EXT-07).
        if r.signer_key.is_some() && r.installed == Some(true) {
            let mut trust = r.publisher_trusted == Some(true);
            if ui
                .checkbox(&mut trust, "Trust this publisher's compatible signed updates")
                .on_hover_text("Only newer stable versions of the same major, signed by the same key, needing no access beyond what is approved now, activate automatically. Contents an agent wrote always need your approval.")
                .changed()
            {
                actions.push(AddonsAction::TrustPublisher { package: id.clone(), revision, trust });
            }
        }
        // Pack configuration against its declared schema (an installed package's only).
        if let Some(schema) = r
            .configuration_schema
            .as_ref()
            .filter(|_| r.installed == Some(true))
        {
            ui.collapsing("Configuration", |ui| {
                ui.label(
                    RichText::new(format!(
                        "Schema {} revision {}",
                        schema.schema_id.as_str(),
                        schema.revision.get()
                    ))
                    .color(theme::MUTED),
                );
                if let Some(errors) = self.config_errors.get(&id) {
                    ui.label(RichText::new(errors).color(theme::RED));
                }
                let saved_value = r
                    .configuration
                    .as_ref()
                    .map(|c| c.value.0.clone())
                    .unwrap_or_else(|| serde_json::json!({}));
                let saved =
                    serde_json::to_string_pretty(&saved_value).unwrap_or_else(|_| "{}".into());
                let saved_schema = serde_json::to_value(schema).unwrap_or_default();
                let text = self
                    .config_text
                    .entry(id.clone())
                    .or_insert_with(|| saved.clone());
                let base = self
                    .config_bases
                    .entry(id.clone())
                    .or_insert_with(|| (saved.clone(), revision, saved_schema.clone()));
                let matches_saved = serde_json::from_str::<serde_json::Value>(text)
                    .ok()
                    .as_ref()
                    == Some(&saved_value);
                if revision != base.1 && (text == &base.0 || matches_saved) {
                    *text = saved.clone();
                    *base = (saved.clone(), revision, saved_schema.clone());
                }
                if ui.button("Reload saved configuration").clicked() {
                    *text = saved.clone();
                    *base = (saved, revision, saved_schema);
                }
                // Unsaved edits retain the schema and CAS revision they started against.
                let revision = base.1;
                let schema = base.2.clone();
                ui.add(
                    egui::TextEdit::multiline(text)
                        .code_editor()
                        .desired_rows(4),
                )
                .widget_info(|| {
                    egui::WidgetInfo::labeled(
                        egui::Role::MultilineTextInput,
                        true,
                        "Configuration (JSON)",
                    )
                });
                if ui.button("Save configuration").clicked() {
                    match serde_json::from_str::<serde_json::Value>(text) {
                        Ok(value) => actions.push(AddonsAction::Configure {
                            package: id.clone(),
                            revision,
                            schema,
                            value,
                        }),
                        Err(e) => self.notice = Some(format!("The configuration is not JSON: {e}")),
                    }
                }
            });
            ui.collapsing("Project enablement", |ui| {
                ui.label(
                    RichText::new(
                        "Projects this package is disabled for (comma-separated project IDs).",
                    )
                    .color(theme::MUTED),
                );
                let saved = r
                    .disabled_projects
                    .as_ref()
                    .map(|p| {
                        p.0.iter()
                            .map(|p| p.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();
                let text = self
                    .projects_text
                    .entry(id.clone())
                    .or_insert_with(|| saved.clone());
                let base = self
                    .project_bases
                    .entry(id.clone())
                    .or_insert_with(|| (saved.clone(), revision));
                if text.as_str() == base.0 || text.as_str() == saved {
                    *text = saved.clone();
                    *base = (saved, revision);
                }
                // An unsaved draft keeps its own CAS revision across catalog refreshes.
                let revision = base.1;
                ui.text_edit_singleline(text).widget_info(|| {
                    egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Disabled projects")
                });
                if ui.button("Save project enablement").clicked() {
                    let disabled = text
                        .split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned)
                        .collect();
                    actions.push(AddonsAction::ProjectEnablement {
                        package: id.clone(),
                        revision,
                        disabled,
                    });
                }
            });
        }
        if let Some(entries) = self.logs.get(&id) {
            ui.collapsing(format!("Logs ({})", entries.len()), |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("addon-logs")
                    .max_height(200.0)
                    .show(ui, |ui| {
                        // Newest first (the log is read oldest first, to its end).
                        for d in entries.iter().rev() {
                            ui.label(format!("{:?}  {}  {}", d.severity, d.code.0, d.message.0));
                        }
                    });
            });
        }
    }
}

/// A picked bundle path to install or update with.
#[derive(Clone, Debug)]
pub struct PickedBundle {
    pub path: PathBuf,
    pub update: Option<String>,
}

// ---------------------------------------------------------------------------------------------
// Frames and replies (the same use cases agent tools reach)

use crate::backend::{Backend, TaskOutput};
use crate::ops::{self, Failure, PROTOCOL};
use devworks_core::gateway::{CommandOutcome, QueryOutcome};
use devworks_core::protocol::results::OperationResult;
use devworks_core::services::packages::{
    PROJECT_ENABLEMENT_SCHEMA, UPDATE_POLICY_SCHEMA, host_schema_ref,
};
use serde_json::{Value, json};

/// The revision of `addons.*` records the screen reads (effective availability and the rest).
pub const ADDONS_REVISION: u64 = 3;
/// Media type of a package bundle artifact.
pub const BUNDLE_MEDIA_TYPE: &str = "application/vnd.devworks.package";
/// Largest bundle file the screen hashes and uploads.
pub const MAX_BUNDLE_FILE: u64 = 256 * 1024 * 1024;
/// The lifecycle commands the screen admits.
pub const COMMANDS: [&str; 7] = [
    "addons.install",
    "addons.update",
    "addons.enable",
    "addons.disable",
    "addons.rollback",
    "addons.uninstall",
    "addons.configure",
];

/// Most log entries kept per package (the newest; the log is read to its end).
pub const LOGS_KEPT: usize = 2000;
/// Most catalog pages followed in one load.
pub const CATALOG_PAGES: usize = 50;

fn page(cursor: Option<&str>) -> Value {
    match cursor {
        Some(c) => json!({"limit": ops::PAGE_LIMIT, "cursor": c}),
        None => json!({"limit": ops::PAGE_LIMIT}),
    }
}
/// One catalog page (`cursor`: the previous page's continuation).
pub fn catalog_frame(backend: &dyn Backend, cursor: Option<&str>) -> Vec<u8> {
    json!({"mode": "Query", "protocol": PROTOCOL, "queryId": backend.token("q"),
        "schemaRevision": ADDONS_REVISION,
        "body": {"kind": "addons.catalog", "page": page(cursor)}})
    .to_string()
    .into_bytes()
}
/// A catalog page, the registry revision it was read at and its continuation.
pub fn catalog(
    outcome: QueryOutcome,
) -> Result<(Vec<PackageRecord>, i64, Option<String>), Failure> {
    match ops::query_result(outcome)? {
        (OperationResult::AddonsCatalog { value }, registry) => {
            Ok((value.items.0, registry, value.next_cursor.map(|c| c.0)))
        }
        (other, _) => Err(Failure::Other(format!(
            "unexpected addons.catalog result {other:?}"
        ))),
    }
}
/// One page of a package's logs, oldest first (the contract's order), after `cursor`.
pub fn logs_frame(backend: &dyn Backend, package: &str, cursor: Option<&str>) -> Vec<u8> {
    json!({"mode": "Query", "protocol": PROTOCOL, "queryId": backend.token("q"),
        "body": {"kind": "addons.logs", "packageId": package, "afterSequence": "0",
            "page": page(cursor)}})
    .to_string()
    .into_bytes()
}
/// A page of log entries and its continuation.
pub fn logs(outcome: QueryOutcome) -> Result<(Vec<Diagnostic>, Option<String>), Failure> {
    match ops::query_result(outcome)? {
        (OperationResult::AddonsLogs { value }, _) => {
            Ok((value.items.0, value.next_cursor.map(|c| c.0)))
        }
        (other, _) => Err(Failure::Other(format!(
            "unexpected addons.logs result {other:?}"
        ))),
    }
}
/// A lifecycle command (`AddonCommandRequest`) at the record revision the screen reads.
pub fn lifecycle_frame(backend: &dyn Backend, command: Value) -> Vec<u8> {
    json!({"protocol": PROTOCOL, "requestKey": backend.token("rk"),
        "schemaRevision": ADDONS_REVISION, "command": command})
    .to_string()
    .into_bytes()
}
pub fn configure_frame(
    backend: &dyn Backend,
    package: &str,
    revision: i64,
    schema: Value,
    value: Value,
) -> Vec<u8> {
    json!({"mode": "Command", "protocol": PROTOCOL, "requestKey": backend.token("rk"),
        "schemaRevision": ADDONS_REVISION,
        "body": {"kind": "addons.configure", "packageId": package,
            "expectedRevision": revision.max(1).to_string(),
            "config": {"schema": schema, "value": value}}})
    .to_string()
    .into_bytes()
}
pub fn trust_frame(backend: &dyn Backend, package: &str, revision: i64, trust: bool) -> Vec<u8> {
    configure_frame(
        backend,
        package,
        revision,
        host_schema_ref(UPDATE_POLICY_SCHEMA).unwrap_or_default(),
        json!({"trustPublisherUpdates": trust, "releaseStream": "stable"}),
    )
}
pub fn project_frame(
    backend: &dyn Backend,
    package: &str,
    revision: i64,
    disabled: &[String],
) -> Vec<u8> {
    configure_frame(
        backend,
        package,
        revision,
        host_schema_ref(PROJECT_ENABLEMENT_SCHEMA).unwrap_or_default(),
        json!({"disabledProjects": disabled}),
    )
}
/// The package record a lifecycle command applied (or left unchanged).
pub fn package_result(outcome: CommandOutcome) -> Result<PackageRecord, Failure> {
    match ops::command_result(outcome)? {
        OperationResult::AddonsInstall { value }
        | OperationResult::AddonsUpdate { value }
        | OperationResult::AddonsEnable { value }
        | OperationResult::AddonsDisable { value }
        | OperationResult::AddonsRollback { value }
        | OperationResult::AddonsUninstall { value }
        | OperationResult::AddonsConfigure { value } => Ok(value),
        other => Err(Failure::Other(format!(
            "unexpected add-on result {other:?}"
        ))),
    }
}
/// The package and diagnostic of a configuration refused by the package's schema.
pub fn configuration_refusal(outcome: &CommandOutcome) -> Option<(String, String)> {
    let Ok(devworks_core::protocol::CommandReply::Recorded { receipt, .. }) = outcome else {
        return None;
    };
    let devworks_core::protocol::results::DomainDecision::Refused { fault } = &receipt.decision
    else {
        return None;
    };
    if fault.target_kind.as_ref().map(|t| t.0.as_str()) != Some("package_configuration") {
        return None;
    }
    Some((
        fault.target_id.as_ref()?.as_str().to_owned(),
        fault.diagnostic_id.as_ref()?.as_str().to_owned(),
    ))
}

/// The package an applied configuration changed (its form's field errors are stale).
pub fn configured_package(outcome: &CommandOutcome) -> Option<String> {
    let Ok(devworks_core::protocol::CommandReply::Recorded { receipt, .. }) = outcome else {
        return None;
    };
    match &receipt.decision {
        devworks_core::protocol::results::DomainDecision::Applied {
            result: OperationResult::AddonsConfigure { value },
        }
        | devworks_core::protocol::results::DomainDecision::NoChange {
            result: OperationResult::AddonsConfigure { value },
        } => Some(value.id.as_str().to_owned()),
        _ => None,
    }
}

/// What a lifecycle outcome tells the person.
pub fn describe(outcome: CommandOutcome) -> String {
    match package_result(outcome) {
        Ok(r) => match (&r.pending_version, &r.pending_waits) {
            (Some(v), Some(w)) if !w.0.is_empty() => format!(
                "{} {} is staged and waits for {}",
                r.id.as_str(),
                v.0,
                w.0.iter()
                    .map(|x| format!("{x:?}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            _ => format!(
                "{} {}: {}",
                r.id.as_str(),
                r.version.0,
                availability_label(r.availability.as_ref()).0
            ),
        },
        Err(e) => e.describe(),
    }
}

fn hash_file(path: &std::path::Path) -> Result<(String, u64), String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let f =
        std::fs::File::open(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    let mut f = f.take(MAX_BUNDLE_FILE + 1);
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut n = 0u64;
    loop {
        let k = f.read(&mut buf).map_err(|e| e.to_string())?;
        if k == 0 {
            break;
        }
        n += k as u64;
        h.update(&buf[..k]);
    }
    if n > MAX_BUNDLE_FILE {
        return Err("the bundle is larger than 256 MiB".into());
    }
    let digest = h.finalize();
    Ok((digest.iter().map(|b| format!("{b:02x}")).collect(), n))
}

/// Prepares the picked bundle's artifact frame without admitting effects. The shell retains
/// it through the command client before saving, then submits the exact install/update intent.
pub fn prepare_install_from_file(
    backend: &dyn Backend,
    path: &std::path::Path,
) -> Result<TaskOutput, String> {
    let (digest, bytes) = hash_file(path)?;
    let ticket = backend.mint_workspace_upload(path)?;
    let title = path.file_name().map_or("package bundle".into(), |n| {
        n.to_string_lossy().into_owned()
    });
    let save = ops::save_workspace_artifact(
        backend,
        ticket.as_str(),
        &title,
        BUNDLE_MEDIA_TYPE,
        &digest,
        bytes,
        &[],
    );
    Ok(TaskOutput::Prepared { frame: save })
}

/// Removes an unused download when preparation is refused or its UI reply is dropped.
/// Once submitted, the durable command slot owns its path until the save is confirmed.
pub struct PreparedDownload {
    path: Option<std::path::PathBuf>,
    root: std::path::PathBuf,
}
impl PreparedDownload {
    pub fn into_path(mut self) -> std::path::PathBuf {
        self.path.take().expect("owned download")
    }
}
impl Drop for PreparedDownload {
    fn drop(&mut self) {
        if let Some(path) = &self.path {
            crate::attachments::remove_owned(path, &self.root);
        }
    }
}

pub fn prepare_downloaded_install(
    backend: &dyn Backend,
    path: std::path::PathBuf,
) -> Result<TaskOutput, String> {
    let download = PreparedDownload {
        path: Some(path.clone()),
        root: backend.scratch_dir().join("packages"),
    };
    let TaskOutput::Prepared { frame } = prepare_install_from_file(backend, &path)? else {
        return Err("unexpected package preparation".into());
    };
    Ok(TaskOutput::PackagePrepared { frame, download })
}

/// Builds the install/update frame from the saved artifact; the durable client submits it.
pub fn install_saved(
    backend: &dyn Backend,
    artifact: &devworks_core::protocol::provider::ArtifactRef,
    update: Option<&str>,
    registry: i64,
) -> (&'static str, Vec<u8>) {
    let bundle = serde_json::to_value(artifact).expect("artifact reference");
    let expected = registry.max(1).to_string();
    let (op, command) = match update {
        Some(p) => (
            "addons.update",
            json!({"kind": "Update", "packageId": p, "bundle": bundle,
                "expectedRegistryRevision": expected}),
        ),
        None => (
            "addons.install",
            json!({"kind": "Install", "bundle": bundle, "expectedRegistryRevision": expected}),
        ),
    };
    let frame = lifecycle_frame(backend, command);
    (op, frame)
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::Harness;
    use egui_kittest::kittest::Queryable;
    use std::cell::RefCell;
    use std::rc::Rc;

    fn record(installed: bool) -> PackageRecord {
        serde_json::from_value(serde_json::json!({
            "id": "first-pack", "version": "1.0.0", "digest": "a".repeat(64), "revision": "1",
            "enabled": false, "trustState": "AwaitingApproval", "contributionIds": [],
            "availability": "Incompatible", "availabilityReason": "no helper for this platform",
            "installed": installed, "pendingVersion": "1.0.0", "pendingDigest": "a".repeat(64),
            "pendingWaits": ["ExactVersionApproval"],
            "pendingBundle": {"artifactId": "bundle-a", "versionId": "v1", "sha256": "b".repeat(64),
                "byteLength": "10", "mediaType": "application/vnd.devworks.package"},
            "configurationSchema": {"schemaId": "first-pack/config", "revision": "1", "sha256": "c".repeat(64)},
            "signerKey": "d".repeat(64)
        }))
        .unwrap()
    }

    fn render(r: PackageRecord) -> (Harness<'static>, Rc<RefCell<Vec<AddonsAction>>>) {
        let actions = Rc::new(RefCell::new(Vec::new()));
        let sink = actions.clone();
        let mut screen = AddonsScreen::default();
        let h = Harness::new_ui(move |ui| {
            let mut out = Vec::new();
            screen.detail(ui, &r, &mut out);
            sink.borrow_mut().extend(out);
        });
        (h, actions)
    }

    #[test]
    fn an_incompatible_first_install_is_approved_by_installing_and_offers_no_installed_actions() {
        let (mut h, actions) = render(record(false));
        h.run();
        // Lifecycle actions of an installed package are not offered.
        for label in ["Uninstall", "Update from file…", "Disable", "Enable"] {
            assert!(
                h.query_by_label(label).is_none(),
                "{label} offered for a package that is not installed"
            );
        }
        assert!(h.query_by_label("Configuration").is_none());
        h.get_by_label("Approve and activate 1.0.0").click();
        h.run();
        let a = actions.borrow();
        assert!(
            matches!(a.last(), Some(AddonsAction::Approve { update: false, .. })),
            "{a:?}"
        );
    }

    #[test]
    fn an_installed_package_is_approved_by_updating() {
        let (mut h, actions) = render(record(true));
        h.run();
        assert!(h.query_by_label("Uninstall").is_some());
        h.get_by_label("Approve and activate 1.0.0").click();
        h.run();
        assert!(matches!(
            actions.borrow().last(),
            Some(AddonsAction::Approve { update: true, .. })
        ));
    }
    #[test]
    fn saved_project_overrides_initialize_the_editor_and_refresh_preserves_unsaved_changes() {
        let mut r = record(true);
        r.disabled_projects = Some(devworks_core::protocol::support::BoundedList(vec![
            devworks_core::protocol::Id::new("project-a").unwrap(),
        ]));
        let state = Rc::new(RefCell::new((AddonsScreen::default(), r, Vec::new())));
        let ui_state = state.clone();
        let mut h = Harness::new_ui(move |ui| {
            let mut st = ui_state.borrow_mut();
            let (screen, record, actions) = &mut *st;
            screen.detail(ui, record, actions);
        });
        h.run();
        h.get_by_label("Project enablement").click();
        h.run();
        assert_eq!(state.borrow().0.projects_text["first-pack"], "project-a");
        // Clean editors track refreshed saved state.
        state
            .borrow_mut()
            .1
            .disabled_projects
            .as_mut()
            .unwrap()
            .0
            .push(devworks_core::protocol::Id::new("project-b").unwrap());
        state.borrow_mut().1.revision = devworks_core::protocol::Revision::new(2).unwrap();
        h.run();
        assert_eq!(
            state.borrow().0.projects_text["first-pack"],
            "project-a, project-b"
        );
        // An unsaved edit survives a subsequent catalog refresh and is what Save submits.
        state.borrow_mut().0.projects_text.insert(
            "first-pack".into(),
            "project-a, project-b, project-c".into(),
        );
        state
            .borrow_mut()
            .1
            .disabled_projects
            .as_mut()
            .unwrap()
            .0
            .clear();
        state.borrow_mut().1.revision = devworks_core::protocol::Revision::new(3).unwrap();
        h.run();
        h.get_by_label("Save project enablement").click();
        h.run();
        assert!(
            matches!(state.borrow().2.last(), Some(AddonsAction::ProjectEnablement { disabled, revision: 2, .. }) if disabled == &["project-a", "project-b", "project-c"])
        );
    }
    #[test]
    fn saved_configuration_initializes_refreshes_and_fences_unsaved_edits() {
        let mut r = record(true);
        r.configuration = Some(
            serde_json::from_value(
                json!({"schema": r.configuration_schema, "value": {"threads": 3, "mode": "fast"}}),
            )
            .unwrap(),
        );
        let state = Rc::new(RefCell::new((AddonsScreen::default(), r, Vec::new())));
        let ui_state = state.clone();
        let mut h = Harness::new_ui(move |ui| {
            let mut st = ui_state.borrow_mut();
            let (screen, record, actions) = &mut *st;
            screen.detail(ui, record, actions);
        });
        h.run();
        h.get_by_label("Configuration").click();
        h.run();
        let read =
            || serde_json::from_str::<Value>(&state.borrow().0.config_text["first-pack"]).unwrap();
        assert_eq!(read(), json!({"threads": 3, "mode": "fast"}));
        state.borrow_mut().1.configuration.as_mut().unwrap().value.0 =
            json!({"threads": 4, "mode": "fast"});
        state.borrow_mut().1.revision = devworks_core::protocol::Revision::new(2).unwrap();
        h.run();
        assert_eq!(read(), json!({"threads": 4, "mode": "fast"}));
        state
            .borrow_mut()
            .0
            .config_text
            .insert("first-pack".into(), r#"{"threads":5,"mode":"fast"}"#.into());
        state.borrow_mut().1.configuration.as_mut().unwrap().value.0 =
            json!({"threads": 6, "mode": "fast"});
        state.borrow_mut().1.revision = devworks_core::protocol::Revision::new(3).unwrap();
        h.run();
        h.get_by_label("Save configuration").click();
        h.run();
        assert!(
            matches!(state.borrow().2.last(), Some(AddonsAction::Configure { revision: 2, value, .. }) if value == &json!({"threads":5,"mode":"fast"}))
        );
        h.get_by_label("Reload saved configuration").click();
        h.run();
        assert_eq!(read(), json!({"threads": 6, "mode": "fast"}));
        h.get_by_label("Save configuration").click();
        h.run();
        assert!(matches!(
            state.borrow().2.last(),
            Some(AddonsAction::Configure { revision: 3, .. })
        ));
    }
}
