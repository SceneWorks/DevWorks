//! Repository indexing requests and replies: index status, runs, the section's sources and
//! discovery. Every change is the same `indexing.*` command an agent submits.
use super::ListState;
use crate::backend::Reply;
use crate::indexing_ops::IndexingSlot;
pub use crate::indexing_screen::{
    IndexAction, IndexingScreen, SourcesForm, StatusState, index_block, run_state_label,
    sources_panel,
};
use crate::memory::RouteMemory;
use crate::ops::{self, Failure};
use crate::project_purge::Live;
use crate::route::Route;
use crate::screen::{Cx, Screen, ScreenContext, ScreenId, controller};
use crate::widgets::Tone;
use devworks_core::ports::storage::ProjectPurgeClientScope;
use eframe::egui::Ui;
use serde_json::json;

controller!(IndexingCtl, indexing: IndexingScreen);

impl Screen for IndexingScreen {
    type Slot = IndexingSlot;
    const ID: ScreenId = ScreenId::Indexing;
    fn load(&mut self, cx: &mut Cx, _route: Route) {
        let mut ctl = IndexingCtl { indexing: self, cx };
        if ctl.indexing.sources.is_none() {
            ctl.load_index_sources();
        }
        ctl.act(vec![IndexAction::Runs]);
    }
    fn refresh(&mut self, _cx: &mut Cx, _route: Route) {}
    fn apply(&mut self, cx: &mut Cx, slot: IndexingSlot, reply: Reply) {
        if let Some((slot, reply)) =
            (IndexingCtl { indexing: self, cx }).apply_indexing(slot, reply)
        {
            cx.unhandled(&slot, reply);
        }
    }
    fn ui(&mut self, cx: &mut Cx, _route: Route, ui: &mut Ui) {
        IndexingCtl { indexing: self, cx }.indexing_screen(ui);
    }
    fn purge_live(&self, out: &mut Live) {
        if let Some(f) = &self.sources
            && let Some(p) = &f.project
        {
            out.projects.entry(p.clone()).or_insert_with(|| json!({}))["indexSources"] = json!({"languages":f.languages,"packages":f.packages,"excludes":f.excludes,"max":f.max_bytes});
        }
    }
    fn remove_purged(&mut self, scope: &ProjectPurgeClientScope) {
        let p = scope.project_id.0.as_str();
        if self
            .sources
            .as_ref()
            .is_some_and(|f| f.project.as_deref() == Some(p))
        {
            self.sources = None;
            self.message = None;
        }
        self.runs = Default::default();
    }
    fn context(&self, _route: Route, mem: &RouteMemory) -> ScreenContext {
        ScreenContext {
            tab: mem.tab,
            selection: mem.selection.clone(),
        }
    }
}

impl IndexingScreen {
    /// Runs indexing actions another screen (Repositories, Settings) asked for.
    pub(crate) fn act(&mut self, cx: &mut Cx, actions: Vec<IndexAction>) {
        IndexingCtl { indexing: self, cx }.act(actions);
    }
}

impl IndexingCtl<'_, '_> {
    fn act(&mut self, actions: Vec<IndexAction>) {
        for a in actions {
            match a {
                IndexAction::Status(r) => {
                    self.indexing.status.insert(r.clone(), StatusState::Loading);
                    self.query(
                        IndexingSlot::IndexStatus(r.clone()),
                        "Reading index status",
                        "indexing.status",
                        move |b| ops::index_status(b, &r),
                    );
                }
                IndexAction::Runs => {
                    let project = self.selected_project().map(str::to_owned);
                    self.query(
                        IndexingSlot::IndexRuns("*".into()),
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
                        IndexingSlot::IndexCommand(repository),
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
                        IndexingSlot::IndexCommand(repository),
                        "Refreshing an overlay",
                        "indexing.refresh_overlay",
                        move |b| ops::refresh_overlay(b, &r, &worktree, revision),
                    );
                }
                IndexAction::Cancel(run) => {
                    let r = run.clone();
                    self.command(
                        IndexingSlot::IndexCancel(run),
                        "Cancelling an index run",
                        "indexing.cancel",
                        move |b| ops::cancel_index_run(b, &r),
                    );
                }
                IndexAction::Retire { overlay, revision } => {
                    let o = overlay.clone();
                    self.command(
                        IndexingSlot::IndexRetire(overlay),
                        "Retiring an overlay",
                        "indexing.retire_view",
                        move |b| ops::retire_view(b, &o, &revision),
                    );
                }
                IndexAction::LoadSources => self.load_index_sources(),
                IndexAction::SaveSources => {
                    let Some(form) = self.indexing.sources.clone() else {
                        continue;
                    };
                    let Ok(max) = form.max_bytes.trim().parse::<i64>() else {
                        self.indexing.sources_error =
                            Some("The largest file must be a number of bytes.".into());
                        continue;
                    };
                    self.indexing.sources_error = None;
                    self.command(
                        IndexingSlot::IndexSourcesSave,
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
                    self.indexing.discovered = Some("Scanning…".into());
                    self.query(
                        IndexingSlot::IndexDiscover,
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
        self.indexing.sources.as_ref().map_or(1, |f| f.revision)
    }

    fn load_index_sources(&mut self) {
        let Some(b) = self.backend() else { return };
        match b.index_sources(None) {
            Ok(c) => {
                self.indexing.sources = Some(SourcesForm {
                    project: None,
                    revision: c.revision,
                    languages: c.structural_languages.join(", "),
                    packages: c.semantic_packages.join(", "),
                    excludes: c.exclude_patterns.join("\n"),
                    max_bytes: c.max_file_bytes.to_string(),
                });
                self.indexing.sources_error = None;
            }
            Err(e) => self.indexing.sources_error = Some(e),
        }
    }

    /// Applies an indexing reply; returns it back when it is not one.
    fn apply_indexing(
        &mut self,
        slot: IndexingSlot,
        reply: Reply,
    ) -> Option<(IndexingSlot, Reply)> {
        let ix = &mut *self.indexing;
        match (slot, reply) {
            (IndexingSlot::IndexStatus(r), Reply::Query(q)) => {
                let state = match ops::index_status_result(q) {
                    Ok(s) => StatusState::Ready(Box::new(s)),
                    Err(e) => StatusState::Failed(failure_text(e)),
                };
                ix.status.insert(r, state);
            }
            (IndexingSlot::IndexRuns(_), Reply::Query(q)) => match ops::index_runs_result(q) {
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
            (IndexingSlot::IndexCommand(r), Reply::Command(c)) => {
                match ops::index_run_result(c) {
                    Ok(run) => {
                        ix.message = Some((
                            Tone::Good,
                            format!("Run {} {}", run.id.as_str(), run_state_label(&run.state).0),
                        ));
                    }
                    Err(e) => ix.message = Some((Tone::Bad, failure_text(e))),
                }
                self.act(vec![IndexAction::Status(r), IndexAction::Runs]);
            }
            (IndexingSlot::IndexCancel(_), Reply::Command(c)) => {
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
                self.act(next);
            }
            (IndexingSlot::IndexRetire(_), Reply::Command(c)) => {
                match ops::index_mutation_result(c) {
                    Ok(_) => ix.message = Some((Tone::Neutral, "Overlay retired".into())),
                    Err(e) => ix.message = Some((Tone::Bad, failure_text(e))),
                }
                let repos: Vec<String> = ix.status.keys().cloned().collect();
                self.act(repos.into_iter().map(IndexAction::Status).collect());
            }
            (IndexingSlot::IndexSourcesSave, Reply::Command(c)) => {
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
            (IndexingSlot::IndexDiscover, Reply::Query(q)) => {
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

    fn indexing_screen(&mut self, ui: &mut Ui) {
        let offline = self.online().err().map(str::to_owned);
        let mut actions: Vec<IndexAction> = Vec::new();
        let mut mem = self.routes.remove(&Route::Indexing).unwrap_or_default();
        mem.tab = self.indexing.ui(ui, offline, mem.tab, &mut actions);
        self.routes.insert(Route::Indexing, mem);
        self.act(actions);
    }
}

fn lines(s: &str) -> Vec<String> {
    s.split(['\n', ','])
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect()
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
