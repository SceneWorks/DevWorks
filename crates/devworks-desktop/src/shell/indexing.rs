//! Repository indexing in the shell (DW-23, CODE-07, GRAPH-02 baseline): a repository's index
//! facts on the Repositories screen (freshness against the observed `HEAD`, coverage and its
//! gaps, per-language structural/text coverage with grammar identity, semantic pack state,
//! overlays with their lifecycle state, and the latest run), the Indexing screen (runs with
//! stage, progress and failures; their diagnostics), and the section's source configuration
//! (languages, packs, exclusions, size bound) and discovery in Settings and on the Indexing
//! screen. Every change is the same `indexing.*` command an agent submits.
use super::{ListState, Shell};
use crate::backend::{Reply, Slot};
use crate::ops::{self, Failure};
use crate::route::Route;
use crate::widgets::{self, Header, Tone, ViewState, chip};
use devworks_core::protocol::OperationState;
use devworks_core::protocol::data::CoverageState;
use devworks_core::protocol::records::{
    IndexFreshnessState, IndexRunRecord, LanguageCoverageMode, OverlayState, RepositoryIndexStatus,
    RepositoryRecord, SemanticPackState, Severity,
};
use eframe::egui::{self, RichText, Ui};
use std::collections::BTreeMap;

/// A repository's index status as shown.
#[derive(Clone, Debug)]
pub enum StatusState {
    Loading,
    Ready(Box<RepositoryIndexStatus>),
    Failed(String),
}

/// The section configuration being edited, against the revision it was read at.
#[derive(Clone, Debug, Default)]
pub struct SourcesForm {
    pub project: Option<String>,
    pub revision: i64,
    pub languages: String,
    pub packages: String,
    pub excludes: String,
    pub max_bytes: String,
}

#[derive(Default)]
pub struct IndexingUi {
    pub status: BTreeMap<String, StatusState>,
    pub runs: ListState<IndexRunRecord>,
    pub sources: Option<SourcesForm>,
    pub sources_error: Option<String>,
    pub message: Option<(Tone, String)>,
    /// The last discovery's outcome.
    pub discovered: Option<String>,
}

/// What the indexing views ask the shell to do.
#[derive(Clone, Debug)]
pub enum IndexAction {
    Status(String),
    Index {
        repository: String,
        rebuild: bool,
    },
    Cancel(String),
    Refresh {
        repository: String,
        worktree: String,
    },
    Retire {
        overlay: String,
        revision: String,
    },
    Runs,
    LoadSources,
    SaveSources,
    Discover,
}

fn freshness_label(s: &IndexFreshnessState) -> (&'static str, Tone) {
    match s {
        IndexFreshnessState::Current => ("Current", Tone::Good),
        IndexFreshnessState::Stale => ("Stale", Tone::Attention),
        IndexFreshnessState::Indexing => ("Indexing", Tone::Accent),
        IndexFreshnessState::NotIndexed => ("Not indexed", Tone::Neutral),
        IndexFreshnessState::Failed => ("Index failed", Tone::Bad),
    }
}
fn overlay_label(s: &OverlayState) -> (&'static str, Tone) {
    match s {
        OverlayState::Queued => ("Queued", Tone::Neutral),
        OverlayState::Indexing => ("Indexing", Tone::Accent),
        OverlayState::Ready => ("Ready", Tone::Good),
        OverlayState::Failed => ("Failed", Tone::Bad),
        OverlayState::Stale => ("Stale", Tone::Attention),
        OverlayState::Expired => ("Expired", Tone::Neutral),
        OverlayState::Unavailable => ("Unavailable", Tone::Neutral),
    }
}
pub fn run_state_label(s: &OperationState) -> (&'static str, Tone) {
    match s {
        OperationState::IntentRecorded => ("Queued", Tone::Neutral),
        OperationState::RetryPending => ("Retry pending", Tone::Attention),
        OperationState::Dispatching => ("Running", Tone::Accent),
        OperationState::ObservedComplete => ("Succeeded", Tone::Good),
        OperationState::Failed => ("Failed", Tone::Bad),
        OperationState::CancelledBeforeDispatch => ("Cancelled", Tone::Neutral),
        _ => ("Unknown", Tone::Attention),
    }
}
fn active(s: &OperationState) -> bool {
    matches!(
        s,
        OperationState::IntentRecorded | OperationState::RetryPending | OperationState::Dispatching
    )
}
fn short_commit(c: &str) -> String {
    c.chars().take(10).collect()
}
fn lines(s: &str) -> Vec<String> {
    s.split(['\n', ','])
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect()
}

/// A run's progress and its non-informational diagnostics.
fn run_line(ui: &mut Ui, r: &IndexRunRecord, enabled: bool, actions: &mut Vec<IndexAction>) {
    ui.horizontal_wrapped(|ui| {
        let (label, tone) = run_state_label(&r.state);
        chip(ui, label, tone);
        let kind = r
            .diagnostics
            .0
            .iter()
            .find(|d| d.code.0 == "run_kind")
            .map_or("Index".to_owned(), |d| d.message.0.clone());
        ui.label(format!("{kind} · {}", r.stage.0));
        match &r.total_units {
            Some(t) => widgets::caption(
                ui,
                format!("{} / {} units", r.completed_units.get(), t.get()),
            ),
            None => widgets::caption(ui, format!("{} units", r.completed_units.get())),
        }
        if active(&r.state)
            && ui
                .add_enabled(enabled, egui::Button::new("Cancel run"))
                .clicked()
        {
            actions.push(IndexAction::Cancel(r.id.as_str().to_owned()));
        }
    });
    for d in r
        .diagnostics
        .0
        .iter()
        .filter(|d| !matches!(d.severity, Severity::Info))
        .take(6)
    {
        let tone = if matches!(d.severity, Severity::Error) {
            Tone::Bad
        } else {
            Tone::Attention
        };
        chip(ui, format!("{}: {}", d.code.0, d.message.0), tone);
    }
}

/// The index facts of one repository (its Overview facts on the Repositories screen).
pub fn index_block(
    ui: &mut Ui,
    indexing: &IndexingUi,
    r: &RepositoryRecord,
    enabled: bool,
    actions: &mut Vec<IndexAction>,
) {
    let id = r.id.as_str().to_owned();
    ui.add_space(8.0);
    ui.label(RichText::new("Index").strong());
    match indexing.status.get(&id) {
        None => {
            if widgets::secondary_button(ui, "Load index status").clicked() {
                actions.push(IndexAction::Status(id.clone()));
            }
        }
        Some(StatusState::Loading) => widgets::caption(ui, "Loading index status…"),
        Some(StatusState::Failed(e)) => {
            chip(ui, e.clone(), Tone::Bad);
            if widgets::secondary_button(ui, "Retry index status").clicked() {
                actions.push(IndexAction::Status(id.clone()));
            }
        }
        Some(StatusState::Ready(s)) => {
            if let Some(f) = &s.freshness {
                ui.horizontal_wrapped(|ui| {
                    let (label, tone) = freshness_label(&f.state);
                    chip(ui, label, tone);
                    let coverage = match s.coverage.state {
                        CoverageState::Complete => ("Complete coverage", Tone::Good),
                        CoverageState::Partial => ("Partial coverage", Tone::Attention),
                        CoverageState::Unavailable => ("No coverage", Tone::Neutral),
                    };
                    chip(ui, coverage.0, coverage.1);
                });
                widgets::caption(
                    ui,
                    format!(
                        "Indexed {} · HEAD {}{}",
                        f.indexed_commit
                            .as_ref()
                            .map_or("—".to_owned(), |c| short_commit(&c.0)),
                        f.head_commit
                            .as_ref()
                            .map_or("—".to_owned(), |c| short_commit(&c.0)),
                        f.head_ref
                            .as_ref()
                            .map_or(String::new(), |r| format!(" ({})", r.0)),
                    ),
                );
                if !f.reasons.0.is_empty() {
                    let reasons: Vec<&str> = f.reasons.0.iter().map(|r| r.0.as_str()).collect();
                    widgets::caption(ui, format!("Stale: {}", reasons.join(", ")));
                }
            }
            if !s.coverage.gaps.is_empty() {
                let gaps: Vec<&str> = s.coverage.gaps.iter().map(|g| g.as_str()).collect();
                widgets::caption(ui, format!("Gaps: {}", gaps.join(", ")));
            }
            if let Some(langs) = &s.languages {
                for l in langs.0.iter().filter(|l| l.files.get() > 0).take(24) {
                    let mode = match l.mode {
                        LanguageCoverageMode::Structural => "structural",
                        LanguageCoverageMode::TextFallback => "text only",
                        LanguageCoverageMode::NotSelected => "not selected",
                    };
                    widgets::caption(
                        ui,
                        format!(
                            "{} · {mode} · {} files ({} parsed, {} text, {} with errors){}",
                            l.name.0,
                            l.files.get(),
                            l.structural_files.get(),
                            l.text_files.get(),
                            l.parse_error_files.get(),
                            l.grammar
                                .as_ref()
                                .map_or(String::new(), |g| format!(" · {}", g.0)),
                        ),
                    );
                }
            }
            if let Some(packs) = &s.semantic_packs {
                for p in &packs.0 {
                    let state = match p.state {
                        SemanticPackState::NotInstalled => "not installed",
                        SemanticPackState::Unavailable => "unavailable",
                        SemanticPackState::Enabled => "enabled",
                        SemanticPackState::Applied => "applied",
                        SemanticPackState::Disabled => "disabled",
                    };
                    widgets::caption(ui, format!("Semantic pack {}: {state}", p.pack_id.as_str()));
                }
            }
            if let Some(overlays) = &s.overlays {
                for o in &overlays.0 {
                    ui.horizontal_wrapped(|ui| {
                        let (label, tone) = overlay_label(&o.state);
                        chip(ui, label, tone);
                        widgets::caption(
                            ui,
                            format!(
                                "Overlay {} · {}{}",
                                o.worktree_id.as_str(),
                                o.head_ref
                                    .as_ref()
                                    .map_or("detached".to_owned(), |r| r.0.clone()),
                                o.reason
                                    .as_ref()
                                    .map_or(String::new(), |r| format!(" · {}", r.0)),
                            ),
                        );
                        if !matches!(o.state, OverlayState::Unavailable)
                            && ui
                                .add_enabled(enabled, egui::Button::new("Refresh overlay"))
                                .clicked()
                        {
                            actions.push(IndexAction::Refresh {
                                repository: id.clone(),
                                worktree: o.worktree_id.as_str().to_owned(),
                            });
                        }
                        if !matches!(o.state, OverlayState::Unavailable)
                            && ui
                                .add_enabled(enabled, egui::Button::new("Retire overlay"))
                                .clicked()
                        {
                            actions.push(IndexAction::Retire {
                                overlay: o.overlay_id.as_str().to_owned(),
                                revision: o.revision.get().to_string(),
                            });
                        }
                    });
                }
            }
            if let Some(run) = &s.last_run {
                widgets::caption(ui, "Latest run");
                run_line(ui, run, enabled, actions);
            }
        }
    }
    ui.horizontal(|ui| {
        if ui
            .add_enabled(enabled, egui::Button::new("Index now"))
            .clicked()
        {
            actions.push(IndexAction::Index {
                repository: id.clone(),
                rebuild: false,
            });
        }
        if ui
            .add_enabled(enabled, egui::Button::new("Rebuild index"))
            .clicked()
        {
            actions.push(IndexAction::Index {
                repository: id.clone(),
                rebuild: true,
            });
        }
    });
}

/// The section's source configuration form and discovery.
pub fn sources_panel(
    ui: &mut Ui,
    indexing: &mut IndexingUi,
    enabled: bool,
    actions: &mut Vec<IndexAction>,
) {
    ui.label(RichText::new("Repository indexing sources").strong());
    widgets::caption(
        ui,
        "Repositories are discovered under the repository base directory (and each project's \
         override). Exclusions are globs over repository paths (`**/node_modules/**`), \
         `repository:<name>` or `group:<directory>`; an empty language list selects every \
         language with a grammar.",
    );
    if let Some(e) = &indexing.sources_error {
        chip(ui, e.clone(), Tone::Bad);
    }
    match &mut indexing.sources {
        None => {
            if widgets::secondary_button(ui, "Load indexing sources").clicked() {
                actions.push(IndexAction::LoadSources);
            }
        }
        Some(form) => {
            widgets::caption(ui, format!("Section revision {}", form.revision));
            for (label, buffer, multiline) in [
                ("Structural languages", &mut form.languages, false),
                ("Semantic packs", &mut form.packages, false),
                ("Exclusions", &mut form.excludes, true),
                ("Largest file (bytes)", &mut form.max_bytes, false),
            ] {
                widgets::caption(ui, label);
                let edit = if multiline {
                    egui::TextEdit::multiline(buffer).desired_rows(4)
                } else {
                    egui::TextEdit::singleline(buffer)
                };
                let r = ui.add(edit.desired_width(360.0));
                r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, label));
            }
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(enabled, egui::Button::new("Save indexing sources"))
                    .clicked()
                {
                    actions.push(IndexAction::SaveSources);
                }
                if widgets::secondary_button(ui, "Reload sources").clicked() {
                    actions.push(IndexAction::LoadSources);
                }
            });
        }
    }
    ui.horizontal(|ui| {
        if ui
            .add_enabled(enabled, egui::Button::new("Discover repositories"))
            .clicked()
        {
            actions.push(IndexAction::Discover);
        }
        if let Some(d) = &indexing.discovered {
            widgets::caption(ui, d.clone());
        }
    });
}

impl Shell {
    pub(super) fn index_act(&mut self, actions: Vec<IndexAction>) {
        for a in actions {
            match a {
                IndexAction::Status(r) => {
                    self.projects
                        .indexing
                        .status
                        .insert(r.clone(), StatusState::Loading);
                    self.query(
                        Slot::IndexStatus(r.clone()),
                        "Reading index status",
                        "indexing.status",
                        move |b| ops::index_status(b, &r),
                    );
                }
                IndexAction::Runs => {
                    let project = self.selected_project().map(str::to_owned);
                    self.query(
                        Slot::IndexRuns("*".into()),
                        "Loading index runs",
                        "indexing.runs",
                        move |b| ops::index_runs(b, project.as_deref(), None),
                    );
                }
                IndexAction::Index {
                    repository,
                    rebuild,
                } => {
                    let revision = self.section_revision();
                    let op = if rebuild {
                        "indexing.reindex"
                    } else {
                        "indexing.index"
                    };
                    let r = repository.clone();
                    self.command(
                        Slot::IndexCommand(repository),
                        if rebuild {
                            "Rebuilding an index"
                        } else {
                            "Indexing a repository"
                        },
                        op,
                        move |b| ops::index_repository(b, &r, rebuild, revision),
                    );
                }
                IndexAction::Refresh {
                    repository,
                    worktree,
                } => {
                    let revision = self.section_revision();
                    let r = repository.clone();
                    self.command(
                        Slot::IndexCommand(repository),
                        "Refreshing an overlay",
                        "indexing.refresh_overlay",
                        move |b| ops::refresh_overlay(b, &r, &worktree, revision),
                    );
                }
                IndexAction::Cancel(run) => {
                    let r = run.clone();
                    self.command(
                        Slot::IndexCancel(run),
                        "Cancelling an index run",
                        "indexing.cancel",
                        move |b| ops::cancel_index_run(b, &r),
                    );
                }
                IndexAction::Retire { overlay, revision } => {
                    let o = overlay.clone();
                    self.command(
                        Slot::IndexRetire(overlay),
                        "Retiring an overlay",
                        "indexing.retire_view",
                        move |b| ops::retire_view(b, &o, &revision),
                    );
                }
                IndexAction::LoadSources => self.load_index_sources(),
                IndexAction::SaveSources => {
                    let Some(form) = self.projects.indexing.sources.clone() else {
                        continue;
                    };
                    let Ok(max) = form.max_bytes.trim().parse::<i64>() else {
                        self.projects.indexing.sources_error =
                            Some("The largest file must be a number of bytes.".into());
                        continue;
                    };
                    self.projects.indexing.sources_error = None;
                    self.command(
                        Slot::IndexSourcesSave,
                        "Saving indexing sources",
                        "indexing.configure_sources",
                        move |b| {
                            ops::configure_sources(
                                b,
                                form.project.as_deref(),
                                form.revision,
                                &lines(&form.languages),
                                &lines(&form.packages),
                                &lines(&form.excludes),
                                max,
                            )
                        },
                    );
                }
                IndexAction::Discover => {
                    self.projects.indexing.discovered = Some("Scanning…".into());
                    self.query(
                        Slot::IndexDiscover,
                        "Discovering repositories",
                        "indexing.discover",
                        ops::discover,
                    );
                }
            }
        }
    }

    /// The workspace section's configuration revision (what `expectedSourceRevision` names).
    fn section_revision(&self) -> i64 {
        self.projects
            .indexing
            .sources
            .as_ref()
            .map_or(1, |f| f.revision)
    }

    pub(super) fn load_index_sources(&mut self) {
        let Some(b) = self.backend() else { return };
        match b.index_sources(None) {
            Ok(c) => {
                self.projects.indexing.sources = Some(SourcesForm {
                    project: None,
                    revision: c.revision,
                    languages: c.structural_languages.join(", "),
                    packages: c.semantic_packages.join(", "),
                    excludes: c.exclude_patterns.join("\n"),
                    max_bytes: c.max_file_bytes.to_string(),
                });
                self.projects.indexing.sources_error = None;
            }
            Err(e) => self.projects.indexing.sources_error = Some(e),
        }
    }

    /// Applies an indexing reply; returns it back when it is not one.
    pub(super) fn apply_indexing(&mut self, slot: Slot, reply: Reply) -> Option<(Slot, Reply)> {
        let ix = &mut self.projects.indexing;
        match (slot, reply) {
            (Slot::IndexStatus(r), Reply::Query(q)) => {
                let state = match ops::index_status_result(q) {
                    Ok(s) => StatusState::Ready(Box::new(s)),
                    Err(e) => StatusState::Failed(failure_text(e)),
                };
                ix.status.insert(r, state);
            }
            (Slot::IndexRuns(_), Reply::Query(q)) => match ops::index_runs_result(q) {
                Ok((items, more)) => {
                    ix.runs = ListState {
                        items,
                        loaded: true,
                        error: None,
                        more,
                    }
                }
                Err(e) => {
                    ix.runs.loaded = true;
                    ix.runs.error = Some(failure_text(e));
                }
            },
            (Slot::IndexCommand(r), Reply::Command(c)) => {
                match ops::index_run_result(c) {
                    Ok(run) => {
                        ix.message = Some((
                            Tone::Good,
                            format!("Run {} {}", run.id.as_str(), run_state_label(&run.state).0),
                        ));
                    }
                    Err(e) => ix.message = Some((Tone::Bad, failure_text(e))),
                }
                self.index_act(vec![IndexAction::Status(r), IndexAction::Runs]);
            }
            (Slot::IndexCancel(_), Reply::Command(c)) => {
                match ops::index_run_result(c) {
                    Ok(run) => {
                        ix.message = Some((
                            Tone::Neutral,
                            format!("Run {} {}", run.id.as_str(), run.stage.0),
                        ))
                    }
                    Err(e) => ix.message = Some((Tone::Bad, failure_text(e))),
                }
                let repos: Vec<String> = ix.status.keys().cloned().collect();
                let mut next: Vec<IndexAction> =
                    repos.into_iter().map(IndexAction::Status).collect();
                next.push(IndexAction::Runs);
                self.index_act(next);
            }
            (Slot::IndexRetire(_), Reply::Command(c)) => {
                match ops::index_mutation_result(c) {
                    Ok(_) => ix.message = Some((Tone::Neutral, "Overlay retired".into())),
                    Err(e) => ix.message = Some((Tone::Bad, failure_text(e))),
                }
                let repos: Vec<String> = ix.status.keys().cloned().collect();
                self.index_act(repos.into_iter().map(IndexAction::Status).collect());
            }
            (Slot::IndexSourcesSave, Reply::Command(c)) => {
                match ops::index_mutation_result(c) {
                    Ok(m) => {
                        ix.message = Some((
                            Tone::Good,
                            format!("Indexing sources saved (revision {})", m.revision.get()),
                        ));
                    }
                    Err(e) => ix.sources_error = Some(failure_text(e)),
                }
                self.load_index_sources();
            }
            (Slot::IndexDiscover, Reply::Query(q)) => {
                ix.discovered = Some(match ops::discover_result(q) {
                    Ok((items, more)) => format!(
                        "{} repositor{} under the configured base directories{}",
                        items.len(),
                        if items.len() == 1 { "y" } else { "ies" },
                        if more { " (more not shown)" } else { "" }
                    ),
                    Err(e) => failure_text(e),
                });
                self.load_repositories();
            }
            (slot, reply) => return Some((slot, reply)),
        }
        None
    }

    /// The Indexing screen: runs (stage, progress, failures) with the section's sources, and
    /// the runs' diagnostics.
    pub(super) fn indexing_screen(&mut self, ui: &mut Ui) {
        let screen = Route::Indexing.screen();
        let offline = self.online().err();
        let online = offline.is_none();
        let mut actions: Vec<IndexAction> = Vec::new();
        let mut mem = self.routes.remove(&Route::Indexing).unwrap_or_default();
        let tab = mem.tab;
        let ix = &mut self.projects.indexing;
        let (_, t) = widgets::pane(
            ui,
            Header {
                title: screen.title,
                summary: screen.summary,
                tabs: screen.subviews,
                tab,
            },
            &mut actions,
            |ui, actions| {
                if widgets::secondary_button(ui, "Refresh").clicked() {
                    actions.push(IndexAction::Runs);
                }
            },
            |ui, actions, tab| {
                if let Some(reason) = &offline {
                    widgets::state_block(
                        ui,
                        &ViewState::Offline {
                            reason: reason.clone(),
                        },
                    );
                    return;
                }
                if let Some((tone, text)) = &ix.message {
                    chip(ui, text.clone(), *tone);
                }
                egui::ScrollArea::vertical()
                    .id_salt("indexing-screen")
                    .show(ui, |ui| {
                        if tab == 0 {
                            sources_panel(ui, ix, online, actions);
                            ui.add_space(10.0);
                            ui.label(RichText::new("Runs").strong());
                            if !ix.runs.loaded {
                                widgets::state_block(
                                    ui,
                                    &ViewState::Loading {
                                        what: "index runs".into(),
                                    },
                                );
                            } else if let Some(e) = &ix.runs.error {
                                widgets::state_block(
                                    ui,
                                    &ViewState::Failed {
                                        what: "index runs".into(),
                                        error: e.clone(),
                                    },
                                );
                            } else if ix.runs.items.is_empty() {
                                widgets::state_block(
                                    ui,
                                    &ViewState::Empty {
                                        title: "No index runs".into(),
                                        body: "Index a repository from the Repositories screen."
                                            .into(),
                                    },
                                );
                            }
                            for r in &ix.runs.items {
                                ui.separator();
                                widgets::caption(
                                    ui,
                                    r.repository_ids
                                        .0
                                        .iter()
                                        .map(|i| i.as_str())
                                        .collect::<Vec<_>>()
                                        .join(", "),
                                );
                                run_line(ui, r, online, actions);
                            }
                        } else {
                            // Logs: every diagnostic the runs report (the full channel is in
                            // Diagnostics, component `indexing.repository`).
                            for r in &ix.runs.items {
                                for d in &r.diagnostics.0 {
                                    widgets::caption(
                                        ui,
                                        format!(
                                            "{} · {:?} · {}: {}",
                                            r.id.as_str(),
                                            d.severity,
                                            d.code.0,
                                            d.message.0
                                        ),
                                    );
                                }
                            }
                        }
                    });
            },
        );
        mem.tab = t;
        self.routes.insert(Route::Indexing, mem);
        self.index_act(actions);
    }
}

fn failure_text(e: Failure) -> String {
    match e {
        Failure::Refused(f) => match f.target_kind {
            Some(k) => format!("{:?} ({})", f.code, k.0),
            None => format!("{:?}", f.code),
        },
        other => format!("{other:?}"),
    }
}
