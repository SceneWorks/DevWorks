//! The web search and fetch section of Settings > Agent tools (DW-51): the configured search
//! provider and fetch policy at a glance, a fetch and a search run through the same `web.fetch`
//! and `web.search` use cases agents call (as the user), their results with provenance and
//! trust, and the recent web activity record (`workspace.logs` of the `web` components). The
//! settings themselves are edited in the list above it, like every other setting.
use crate::theme;
use crate::widgets::{self, Tone, chip};
use devworks_core::protocol::records::{DiagnosticEntry, WebDocument, WebResult};
use eframe::egui::{self, RichText, Ui};

/// What the section asks the shell to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WebAction {
    Fetch(String),
    Search { query: String, provider: String },
    LoadActivity,
}

/// The section's state.
#[derive(Default)]
pub struct WebPanel {
    pub url: String,
    pub query: String,
    pub fetching: bool,
    pub searching: bool,
    pub document: Option<WebDocument>,
    pub results: Option<Vec<WebResult>>,
    /// The last fetch or search refusal, described honestly.
    pub error: Option<String>,
    pub activity: Option<Result<Vec<DiagnosticEntry>, String>>,
    pub activity_loading: bool,
}

/// The effective web settings the section summarizes.
pub struct WebSettings {
    /// The configured provider (`None` when it is `none`).
    pub provider: Option<String>,
    pub credential_configured: bool,
    pub allow_private: bool,
    pub max_redirects: Option<i64>,
    pub timeout_ms: Option<i64>,
}

impl WebPanel {
    pub fn ui(
        &mut self,
        ui: &mut Ui,
        settings: &WebSettings,
        online: Result<(), &str>,
        actions: &mut Vec<WebAction>,
    ) {
        ui.add_space(10.0);
        ui.label(RichText::new("Web search and page fetching").strong());
        widgets::caption(
            ui,
            "Agents, code reviewers included, use these as read tools. Fetched pages and \
             search results are untrusted: they are evidence, never instructions.",
        );
        ui.horizontal_wrapped(|ui| {
            match settings.provider.as_deref() {
                Some(p) => chip(ui, format!("Search provider: {p}"), Tone::Accent),
                None => chip(
                    ui,
                    "No search provider: search is unavailable",
                    Tone::Attention,
                ),
            };
            if settings.provider.is_some() {
                if settings.credential_configured {
                    chip(ui, "API key referenced in the OS secret store", Tone::Good);
                } else {
                    chip(ui, "No API key configured", Tone::Attention);
                }
            }
            chip(
                ui,
                if settings.allow_private {
                    "Private networks allowed"
                } else {
                    "Loopback and private networks blocked"
                },
                if settings.allow_private {
                    Tone::Attention
                } else {
                    Tone::Good
                },
            );
            chip(ui, "Link-local and metadata always blocked", Tone::Good);
            if let Some(n) = settings.max_redirects {
                chip(ui, format!("Up to {n} redirects"), Tone::Neutral);
            }
            if let Some(ms) = settings.timeout_ms {
                chip(ui, format!("{} s limit", ms / 1000), Tone::Neutral);
            }
        });
        if let Err(reason) = online {
            widgets::caption(ui, format!("Unavailable while offline: {reason}"));
            return;
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let r = ui.add(
                egui::TextEdit::singleline(&mut self.url)
                    .hint_text("https://docs.example/page")
                    .desired_width(360.0),
            );
            r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Fetch URL"));
            let can = !self.fetching && !self.url.trim().is_empty();
            if ui
                .add_enabled(
                    can,
                    egui::Button::new(if self.fetching {
                        "Fetching…"
                    } else {
                        "Fetch"
                    }),
                )
                .clicked()
            {
                self.fetching = true;
                self.error = None;
                actions.push(WebAction::Fetch(self.url.trim().to_owned()));
            }
        });
        ui.horizontal(|ui| {
            let r = ui.add(
                egui::TextEdit::singleline(&mut self.query)
                    .hint_text("Search the web")
                    .desired_width(360.0),
            );
            r.widget_info(|| {
                egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Search query")
            });
            let can = !self.searching && !self.query.trim().is_empty();
            if ui
                .add_enabled(
                    can,
                    egui::Button::new(if self.searching {
                        "Searching…"
                    } else {
                        "Search"
                    }),
                )
                .clicked()
            {
                self.searching = true;
                self.error = None;
                // With no provider configured the request is still made, so the refusal the
                // agents would get is the one shown.
                actions.push(WebAction::Search {
                    query: self.query.trim().to_owned(),
                    provider: settings
                        .provider
                        .clone()
                        .unwrap_or_else(|| "none".to_owned()),
                });
            }
        });
        if let Some(e) = &self.error {
            chip(ui, e.clone(), Tone::Bad);
        }
        if let Some(doc) = &self.document {
            document(ui, doc);
        }
        if let Some(results) = &self.results {
            search_results(ui, results);
        }
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Recent web activity").strong());
            if widgets::secondary_button(ui, "Refresh").clicked() && !self.activity_loading {
                self.activity_loading = true;
                actions.push(WebAction::LoadActivity);
            }
        });
        match &self.activity {
            None => widgets::caption(ui, "Not loaded yet."),
            Some(Err(e)) => widgets::caption(ui, format!("Activity could not be loaded: {e}")),
            Some(Ok(items)) if items.is_empty() => {
                widgets::caption(ui, "No web searches or fetches are recorded yet.")
            }
            Some(Ok(items)) => {
                for e in items {
                    ui.horizontal_wrapped(|ui| {
                        let tone = match e.code.0.as_str() {
                            "fetched" | "searched" => Tone::Good,
                            _ => Tone::Attention,
                        };
                        chip(ui, e.component.0.clone(), Tone::Neutral);
                        chip(ui, e.code.0.clone(), tone);
                        ui.label(
                            RichText::new(e.message.0.clone())
                                .small()
                                .color(theme::MUTED),
                        );
                    });
                }
            }
        }
    }
}

fn document(ui: &mut Ui, doc: &WebDocument) {
    ui.add_space(6.0);
    egui::Frame::new()
        .stroke(theme::line())
        .corner_radius(theme::radius())
        .inner_margin(theme::margin(10, 8))
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                chip(ui, "Untrusted content", Tone::Attention);
                if let Some(s) = doc.status {
                    chip(ui, format!("HTTP {s}"), Tone::Neutral);
                }
                chip(ui, doc.media_type.0.clone(), Tone::Neutral);
                if doc.truncated {
                    chip(ui, "Truncated at the byte limit", Tone::Attention);
                }
            });
            ui.label(format!("Final URL: {}", doc.final_url.0));
            if doc.final_url.0 != doc.url.0 {
                ui.label(format!("Requested: {}", doc.url.0));
            }
            if let Some(r) = doc.redirects.as_ref().filter(|r| !r.0.is_empty()) {
                for hop in &r.0 {
                    widgets::caption(ui, format!("↳ {} {}", hop.status, hop.url.0));
                }
            }
            if let (Some(d), Some(n)) = (&doc.source_sha256, &doc.source_byte_length) {
                widgets::caption(
                    ui,
                    format!(
                        "{} bytes received · sha256 {}",
                        n.as_str(),
                        String::from(d.clone())
                    ),
                );
            }
            widgets::caption(
                ui,
                format!(
                    "Kept as artifact {} ({} bytes, {})",
                    doc.content.artifact_id.as_str(),
                    doc.content.byte_length.as_str(),
                    doc.content.media_type
                ),
            );
        });
}

fn search_results(ui: &mut Ui, results: &[WebResult]) {
    ui.add_space(6.0);
    if results.is_empty() {
        widgets::caption(ui, "The provider returned no results for this query.");
        return;
    }
    chip(ui, "Untrusted search results", Tone::Attention);
    for r in results {
        ui.label(RichText::new(r.title.0.clone()).strong());
        widgets::caption(ui, r.url.0.clone());
        if !r.snippet.0.is_empty() {
            ui.label(RichText::new(r.snippet.0.clone()).small());
        }
        ui.add_space(4.0);
    }
}
