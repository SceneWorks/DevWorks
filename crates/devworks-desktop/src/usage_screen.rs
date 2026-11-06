//! Native usage/price views. Every total, classification, coverage flag and tariff is supplied
//! by the shared services; drafts retain their inspected revision across refreshes.
use crate::{
    theme,
    widgets::{self, Header, Tone, ViewState, chip},
};
use devworks_core::protocol::{
    data::{Coverage, CoverageState},
    records::*,
};
use eframe::egui::{self, Ui};
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub const REPORTS: [(&str, &str); 6] = [
    ("AssistantUsage", "Assistant usage"),
    ("AssistantActivity", "Assistant activity"),
    ("McpUsage", "MCP usage"),
    ("CodeReviewUsage", "Code review usage"),
    ("RepositoryAnalysisUsage", "Repository analysis usage"),
    ("Filters", "Filter discovery"),
];
pub const TABS: [&str; 3] = ["Reports", "Attempts", "Prices"];
#[derive(Clone, Debug, Default)]
pub struct Filters {
    pub project: Option<String>,
    pub start: String,
    pub end: String,
    pub interval: String,
    pub user: String,
    pub provider: String,
    pub model: String,
    pub tool: String,
    pub purpose: String,
    pub root: String,
    pub session: String,
    pub job_run: String,
}
impl Filters {
    pub fn body(&self, kind: &str, cursor: Option<&str>) -> Result<Value, String> {
        let mut page = json!({"limit":100});
        if let Some(c) = cursor {
            page["cursor"] = json!(c);
        }
        let mut body = json!({"reportKind":kind,"scope":{"kind":"Workspace"},"page":page});
        if let Some(p) = &self.project {
            body["projectId"] = json!(p);
        }
        for (key, value) in [("startMs", &self.start), ("endMs", &self.end)] {
            if !value.trim().is_empty() {
                let timestamp = value.trim().parse::<jiff::Timestamp>().map_err(|_| {
                    format!("Enter {key} as a UTC date and time, such as 2026-11-05T00:00:00Z.")
                })?;
                body[key] = json!(timestamp.as_millisecond().to_string());
            }
        }
        for (key, value) in [
            ("interval", &self.interval),
            ("user", &self.user),
            ("provider", &self.provider),
            ("model", &self.model),
            ("tool", &self.tool),
            ("purpose", &self.purpose),
            ("rootId", &self.root),
            ("sessionId", &self.session),
            ("jobRunId", &self.job_run),
        ] {
            if !value.trim().is_empty() {
                body[key] = json!(value.trim());
            }
        }
        Ok(body)
    }
}
#[derive(Clone, Debug, PartialEq)]
pub struct TariffForm {
    pub upstream: String,
    pub service: String,
    pub lower: String,
    pub upper: String,
    pub modality: String,
    pub charge: String,
    pub ttl: String,
    pub unit: String,
    pub units: String,
    pub amount: String,
    pub region: String,
    pub profile: String,
}
impl Default for TariffForm {
    fn default() -> Self {
        Self {
            upstream: String::new(),
            service: "Standard".into(),
            lower: String::new(),
            upper: String::new(),
            modality: "Text".into(),
            charge: "Input".into(),
            ttl: String::new(),
            unit: "Token".into(),
            units: "1000000".into(),
            amount: String::new(),
            region: String::new(),
            profile: String::new(),
        }
    }
}
impl TariffForm {
    pub fn from_tariff(t: &PriceTariff) -> Self {
        let v = serde_json::to_value(t).expect("typed tariff");
        let field = |key: &str| v[key].as_str().unwrap_or_default().to_owned();
        Self {
            upstream: field("upstreamModelId"),
            service: field("serviceTier"),
            lower: field("contextLowerExclusive"),
            upper: field("contextUpperInclusive"),
            modality: field("modality"),
            charge: field("chargeKind"),
            ttl: field("cacheTtlSeconds"),
            unit: field("unit"),
            units: field("units"),
            amount: field("amountUsd"),
            region: field("region"),
            profile: field("endpointProfile"),
        }
    }
    pub fn tariff(&self) -> Result<PriceTariff, String> {
        let mut v = json!({"upstreamModelId":self.upstream,"serviceTier":self.service,"modality":self.modality,"chargeKind":self.charge,"unit":self.unit,"units":self.units.trim(),"amountUsd":self.amount.trim()});
        for (key, value) in [
            ("contextLowerExclusive", &self.lower),
            ("contextUpperInclusive", &self.upper),
            ("cacheTtlSeconds", &self.ttl),
            ("region", &self.region),
            ("endpointProfile", &self.profile),
        ] {
            if !value.trim().is_empty() {
                v[key] = json!(if matches!(key, "region" | "endpointProfile") {
                    value.as_str()
                } else {
                    value.trim()
                });
            }
        }
        let tariff: PriceTariff =
            serde_json::from_value(v).map_err(|e| format!("Check the tariff fields: {e}"))?;
        devworks_core::protocol::support::Check::check(&tariff)?;
        Ok(tariff)
    }
}
#[derive(Clone, Debug, PartialEq)]
pub struct PriceDraft {
    pub model: String,
    pub revision: String,
    pub tariffs: Vec<TariffForm>,
}
#[derive(Default)]
pub struct UsageScreen {
    pub tab: usize,
    pub report: usize,
    pub draft: Filters,
    pub applied: Option<Filters>,
    pub generation: u64,
    pub data: Option<UsageQueryResult>,
    pub loading: bool,
    pub error: Option<String>,
    pub options: Vec<UsageFilterOption>,
    pub prices: Option<PriceCatalogView>,
    pub prices_loading: bool,
    pub price_generation: u64,
    pub price_pages: usize,
    pub price_update_available: bool,
    pub price_error: Option<String>,
    pub history: bool,
    pub price_models: String,
    pub price_providers: String,
    pub price_snapshots: String,
    pub price_query: Option<Value>,
    pub price_draft: Option<PriceDraft>,
    pub price_saving: Option<PriceDraft>,
    pub price_busy: bool,
    pub refresh: Option<OperationRef>,
    pub last_refresh: Option<std::time::Instant>,
    pub message: Option<String>,
}
#[derive(Clone, Debug)]
pub enum Action {
    Apply,
    More,
    MorePrices,
    LoadPrices,
    RefreshPrices,
    EditPrice(String),
    SavePrice,
    ClearPrice,
    RebasePrice,
    OpenWork(String),
    OpenSession(String),
}
impl UsageScreen {
    pub fn kind(&self) -> &'static str {
        if self.tab == 1 {
            "Attempts"
        } else {
            REPORTS[self.report.min(REPORTS.len() - 1)].0
        }
    }
    pub fn more(&self) -> Option<&str> {
        match self.data.as_ref()? {
            UsageQueryResult::Attempts { page } => page.next_cursor.as_ref().map(|x| x.0.as_str()),
            UsageQueryResult::Report { report } => {
                report.next_cursor.as_ref().map(|x| x.0.as_str())
            }
            UsageQueryResult::Filters { filters } => {
                filters.next_cursor.as_ref().map(|x| x.0.as_str())
            }
        }
    }
    pub fn append(&mut self, value: UsageQueryResult) -> Result<(), String> {
        let context = |v: &UsageQueryResult| match v {
            UsageQueryResult::Attempts { page } => serde_json::to_value(&page.context),
            UsageQueryResult::Report { report } => serde_json::to_value(&report.context),
            UsageQueryResult::Filters { filters } => serde_json::to_value(&filters.context),
        };
        let Some(previous) = &mut self.data else {
            self.data = Some(value);
            return Ok(());
        };
        if context(previous).ok() != context(&value).ok() {
            return Err(
                "The report changed while paging. Refresh to load one consistent result.".into(),
            );
        }
        match (previous, value) {
            (UsageQueryResult::Attempts { page: a }, UsageQueryResult::Attempts { page: b }) => {
                a.items.0.extend(b.items.0);
                a.next_cursor = b.next_cursor;
                a.has_more = b.has_more;
                a.coverage = b.coverage;
            }
            (UsageQueryResult::Report { report: a }, UsageQueryResult::Report { report: b }) => {
                a.items.0.extend(b.items.0);
                a.next_cursor = b.next_cursor;
                a.has_more = b.has_more;
                a.coverage = b.coverage;
            }
            (
                UsageQueryResult::Filters { filters: a },
                UsageQueryResult::Filters { filters: b },
            ) => {
                a.items.0.extend(b.items.0);
                a.next_cursor = b.next_cursor;
                a.has_more = b.has_more;
                a.coverage = b.coverage;
            }
            _ => return Err("The report kind changed while paging. Refresh the report.".into()),
        }
        Ok(())
    }
    pub fn ui(
        &mut self,
        ui: &mut Ui,
        online: Result<(), &str>,
        projects: &[(String, String)],
        actions: &mut Vec<Action>,
    ) {
        let tab = self.tab;
        let (_, selected) = widgets::pane(
            ui,
            Header {
                title: "Usage & costs",
                summary: "Attributed usage, retained reports and price sources.",
                tabs: &TABS,
                tab,
            },
            &mut (&mut *self, &mut *actions),
            |ui, state| {
                if ui
                    .add_enabled(
                        online.is_ok() && !state.0.loading && !state.0.prices_loading,
                        egui::Button::new(if tab == 2 {
                            "Reload price catalog"
                        } else {
                            "Refresh usage report"
                        }),
                    )
                    .clicked()
                {
                    state.1.push(if tab == 2 {
                        Action::LoadPrices
                    } else {
                        Action::Apply
                    });
                }
            },
            |ui, state, tab| {
                let (s, actions) = state;
                if let Err(reason) = online {
                    widgets::state_block(
                        ui,
                        &ViewState::Offline {
                            reason: reason.into(),
                        },
                    );
                    return;
                }
                egui::ScrollArea::vertical()
                    .id_salt("usage-screen-body")
                    .show(ui, |ui| {
                        if tab == 2 {
                            s.price_ui(ui, actions);
                        } else {
                            if tab == 0 {
                                let old = s.report;
                                egui::ComboBox::from_label("Report")
                                    .selected_text(REPORTS[s.report].1)
                                    .show_ui(ui, |ui| {
                                        for (i, (_, label)) in REPORTS.iter().enumerate() {
                                            ui.selectable_value(&mut s.report, i, *label);
                                        }
                                    });
                                if old != s.report {
                                    actions.push(Action::Apply);
                                }
                            }
                            s.filters_ui(ui, projects, actions);
                            if s.loading {
                                ui.spinner();
                                ui.label(if s.data.is_some() {
                                    "Refreshing; previous applied result remains below."
                                } else {
                                    "Loading usage…"
                                });
                            }
                            if let Some(e) = &s.error {
                                ui.colored_label(theme::RED, e);
                            }
                            if let Some(data) = &s.data {
                                s.result_ui(ui, data, actions);
                            } else if !s.loading && s.error.is_none() {
                                ui.label("Apply filters to read authoritative usage.");
                            }
                            if s.more().is_some()
                                && ui
                                    .add_enabled(
                                        !s.loading,
                                        egui::Button::new("Load more usage rows"),
                                    )
                                    .clicked()
                            {
                                actions.push(Action::More);
                            }
                        }
                        if let Some(message) = &s.message {
                            ui.label(message);
                        }
                    });
            },
        );
        self.tab = selected;
        if selected != tab {
            actions.push(if selected == 2 {
                Action::LoadPrices
            } else {
                Action::Apply
            });
        }
    }
    fn filters_ui(
        &mut self,
        ui: &mut Ui,
        projects: &[(String, String)],
        actions: &mut Vec<Action>,
    ) {
        ui.collapsing("Usage filters",|ui|{
            let current=self.draft.project.as_ref().and_then(|p|projects.iter().find(|(id,_)|id==p)).map_or("All authorized projects",|(_,name)|name.as_str());
            egui::ComboBox::from_label("Usage project").selected_text(current).show_ui(ui,|ui|{ui.selectable_value(&mut self.draft.project,None,"All authorized projects");for(id,name)in projects{ui.selectable_value(&mut self.draft.project,Some(id.clone()),name);}});
            ui.horizontal_wrapped(|ui|{field(ui,"Start UTC",&mut self.draft.start,"2026-11-01T00:00:00Z");field(ui,"End UTC (exclusive)",&mut self.draft.end,"2026-12-01T00:00:00Z");});
            ui.label("Blank dates use the service's last-30-days range. Start is inclusive; end is exclusive.");
            egui::ComboBox::from_label("Usage interval").selected_text(if self.draft.interval.is_empty(){"Auto"}else{&self.draft.interval}).show_ui(ui,|ui|{for(value,label)in[("","Auto"),("day","Day"),("week","Week (Monday)"),("month","Month")]{ui.selectable_value(&mut self.draft.interval,value.into(),label);}});
            for(dimension,value)in[("user",&mut self.draft.user),("provider",&mut self.draft.provider),("model",&mut self.draft.model),("tool",&mut self.draft.tool)]{ui.push_id(dimension,|ui|{field(ui,&format!("Usage {dimension}"),value,"All");let choices=self.options.iter().filter(|o|o.dimension.0==dimension).map(|o|o.value.0.as_str()).collect::<Vec<_>>();if !choices.is_empty(){egui::ComboBox::from_label(format!("Available {dimension} values")).selected_text("Choose a recorded value").show_ui(ui,|ui|{for choice in choices{ui.selectable_value(value,choice.into(),choice);}});}});}
            field(ui,"Usage purpose",&mut self.draft.purpose,"All purposes");
            ui.collapsing("Work and session filters",|ui|{field(ui,"Work root ID",&mut self.draft.root,"");field(ui,"Session ID",&mut self.draft.session,"");field(ui,"Job run ID",&mut self.draft.job_run,"");});
            if let Err(e)=self.draft.body(self.kind(),None){ui.colored_label(theme::RED,e);}
            if ui.add_enabled(!self.loading&&self.draft.body(self.kind(),None).is_ok(),egui::Button::new("Apply usage filters")).clicked(){actions.push(Action::Apply);}
        });
    }
    fn result_ui(&self, ui: &mut Ui, data: &UsageQueryResult, actions: &mut Vec<Action>) {
        let (context, coverage) = match data {
            UsageQueryResult::Attempts { page } => (&page.context, &page.coverage),
            UsageQueryResult::Report { report } => (&report.context, &report.coverage),
            UsageQueryResult::Filters { filters } => (&filters.context, &filters.coverage),
        };
        coverage_ui(ui, coverage);
        widgets::caption(
            ui,
            format!(
                "Applied {} to {} UTC · {} buckets · read revision {}",
                time(
                    context
                        .range
                        .start_ms
                        .0
                        .parse::<i64>()
                        .expect("validated range timestamp")
                ),
                time(
                    context
                        .range
                        .end_ms
                        .0
                        .parse::<i64>()
                        .expect("validated range timestamp")
                ),
                context.interval.0,
                context.read_revision.as_str()
            ),
        );
        ui.collapsing("Applied filters", |ui| {
            let value = serde_json::to_value(&context.applied_filters).expect("typed filters");
            for (key, value) in value.as_object().into_iter().flatten() {
                ui.label(format!(
                    "{}: {}",
                    human(key),
                    if let Some(s) = value.as_str() {
                        s.to_owned()
                    } else {
                        value.to_string()
                    }
                ));
            }
        });
        match data {
            UsageQueryResult::Attempts { page } => {
                totals_ui(ui, &page.totals);
                if page.items.0.is_empty() {
                    ui.label("No attempt records in this result. Coverage above determines whether history is complete.");
                }
                for record in &page.items.0 {
                    ui.push_id(record.id.as_str(), |ui| {
                        ui.collapsing(
                            format!(
                                "{} · {} · {}",
                                record
                                    .resolved_model_id
                                    .as_ref()
                                    .map_or("Model unknown", |m| m.0.as_str()),
                                record.state.0,
                                cost_label(&record.cost)
                            ),
                            |ui| attempt_ui(ui, record, actions),
                        );
                    });
                }
            }
            UsageQueryResult::Report { report } => {
                let totals = report
                    .items
                    .0
                    .iter()
                    .filter_map(|row| {
                        if let UsageReportRow::Total { label, value, .. } = row {
                            Some((label.0.as_str(), value.as_str().to_owned()))
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>();
                if !totals.is_empty() {
                    widgets::summary_strip(
                        ui,
                        &totals
                            .iter()
                            .map(|(label, value)| (*label, Some(value.clone())))
                            .collect::<Vec<_>>(),
                        "Returned report metrics",
                    );
                }
                let mut series: BTreeMap<(&str, &str), Vec<(i64, &str)>> = BTreeMap::new();
                let mut breakdowns: BTreeMap<&str, Vec<(&str, &str)>> = BTreeMap::new();
                for row in &report.items.0 {
                    match row {
                        UsageReportRow::SeriesPoint {
                            key,
                            label,
                            bucket_start_ms,
                            value,
                            ..
                        } => {
                            series.entry((&key.0, &label.0)).or_default().push((
                                bucket_start_ms
                                    .0
                                    .parse::<i64>()
                                    .expect("validated bucket timestamp"),
                                value.as_str(),
                            ));
                        }
                        UsageReportRow::Breakdown {
                            dimension,
                            label,
                            value,
                            ..
                        } => {
                            breakdowns
                                .entry(&dimension.0)
                                .or_default()
                                .push((&label.0, value.as_str()));
                        }
                        _ => {}
                    }
                }
                for ((key, label), points) in series {
                    ui.push_id(("series", key), |ui| {
                        let title = if let Some((dimension, _)) = key
                            .strip_prefix("breakdown:")
                            .and_then(|key| key.split_once(':'))
                        {
                            format!("{} · {label} over time", human(dimension))
                        } else {
                            format!("{label} over time")
                        };
                        ui.collapsing(title, |ui| {
                            egui::Grid::new("series-values")
                                .striped(true)
                                .show(ui, |ui| {
                                    ui.strong("UTC bucket");
                                    ui.strong(label);
                                    ui.end_row();
                                    for (at, value) in points {
                                        ui.label(time(at));
                                        ui.label(value);
                                        ui.end_row();
                                    }
                                });
                        });
                    });
                }
                for (dimension, rows) in breakdowns {
                    ui.push_id(("breakdown", dimension), |ui| {
                        ui.heading(format!("By {}", human(dimension)));
                        egui::Grid::new("breakdown-values")
                            .striped(true)
                            .show(ui, |ui| {
                                for (label, value) in rows {
                                    ui.label(label);
                                    ui.label(value);
                                    ui.end_row();
                                }
                            });
                    });
                }
                if report.has_more {
                    ui.label("More report rows are available; loaded series and breakdowns are partial until all pages are read.");
                }
                if report.items.0.is_empty() {
                    ui.label("No report rows were returned. No monetary or cache observations are inferred.");
                }
            }
            UsageQueryResult::Filters { filters } => {
                ui.label("Available values for the applied range. Categorical filters do not narrow discovery.");
                for option in &filters.items.0 {
                    ui.label(format!(
                        "{}: {}",
                        human(&option.dimension.0),
                        option.value.0
                    ));
                }
                if filters.has_more {
                    ui.label("More filter values are available.");
                }
            }
        }
    }
    fn price_ui(&mut self, ui: &mut Ui, actions: &mut Vec<Action>) {
        ui.horizontal_wrapped(|ui| {
            ui.checkbox(&mut self.history, "Include historical price sources");
            if ui
                .add_enabled(
                    !self.prices_loading,
                    egui::Button::new("Apply price catalog filters"),
                )
                .clicked()
            {
                actions.push(Action::LoadPrices);
            }
        });
        ui.collapsing("Price catalog filters", |ui| {
            field(
                ui,
                "Price model IDs",
                &mut self.price_models,
                "Comma separated; all when blank",
            );
            field(
                ui,
                "Price source IDs",
                &mut self.price_providers,
                "Comma separated; all when blank",
            );
            field(
                ui,
                "Price snapshot IDs",
                &mut self.price_snapshots,
                "Comma separated; current when blank",
            );
        });
        if self.prices_loading && self.prices.is_none() {
            ui.spinner();
            ui.label("Loading price sources…");
        }
        if let Some(e) = &self.price_error {
            ui.colored_label(theme::RED, e);
        }
        let Some(prices) = &self.prices else {
            ui.label("Load the catalog to inspect current prices, unpriced models and sources.");
            return;
        };
        coverage_ui(ui, &prices.coverage);
        if self.price_update_available {
            ui.colored_label(theme::AMBER, "The price catalog changed. Reload to inspect the current revision; loaded pages and your tariff draft are preserved until then.");
        }
        ui.label(format!(
            "Price catalog revision {}",
            prices.catalog_revision.as_str()
        ));
        for provider in &prices.providers.0 {
            ui.push_id(provider.provider_id.as_str(), |ui| {
                ui.group(|ui| {
                    ui.strong(provider.provider_id.as_str());
                    chip(
                        ui,
                        if provider.stale {
                            "Stale source"
                        } else {
                            "Current source"
                        },
                        if provider.stale {
                            Tone::Attention
                        } else {
                            Tone::Good
                        },
                    );
                    if let Some(at) = &provider.last_checked_at_ms {
                        ui.label(format!("Checked {}", time(at.get())));
                    }
                    if let Some(at) = &provider.next_check_at_ms {
                        ui.label(format!("Next check {}", time(at.get())));
                    }
                    if let Some(e) = &provider.error {
                        ui.colored_label(theme::RED, &e.0);
                    }
                });
            });
        }
        if ui
            .add_enabled(
                !self.price_busy
                    && !self.prices_loading
                    && self.refresh.is_none()
                    && !self.price_update_available
                    && (!self.price_providers.trim().is_empty() || !prices.providers.0.is_empty()),
                egui::Button::new("Refresh official price sources"),
            )
            .clicked()
        {
            actions.push(Action::RefreshPrices);
        }
        ui.label("Official sources: openai, anthropic. Public price refresh does not enable a connection or require provider credentials.");
        if let Some(operation) = &self.refresh {
            ui.label(format!(
                "Price refresh {:?} · {}",
                operation.state,
                operation.operation_id.0.as_str()
            ));
            ui.label("The previous verified source remains in use until refresh completes.");
        }
        ui.collapsing("Price source history", |ui| {
            for snapshot in &prices.snapshots.0 {
                ui.push_id(snapshot.id.as_str(), |ui| {
                    ui.group(|ui| {
                        ui.label(format!(
                            "{} · {}",
                            snapshot.provider_id.as_str(),
                            snapshot.id.as_str()
                        ));
                        ui.hyperlink_to("Official source", &snapshot.source_url.0);
                        ui.label(format!(
                            "Verified {} · parser {}",
                            time(snapshot.verified_at_ms.get()),
                            snapshot.parser_version.0
                        ));
                        ui.label(format!("Source digest {}", snapshot.source_digest.as_str()));
                        if let Some(date) = &snapshot.effective_date {
                            ui.label(format!("Effective {}", date.0));
                        }
                        if ui.button("Inspect this source snapshot").clicked() {
                            self.price_snapshots = snapshot.id.as_str().into();
                            self.history = true;
                            actions.push(Action::LoadPrices);
                        }
                    });
                });
            }
        });
        ui.heading("Models and tariffs");
        let can_edit = !prices.has_more && !self.price_update_available && !self.price_busy;
        if prices.has_more {
            widgets::caption(
                ui,
                "Load the remaining price rows before editing so every current rate is retained. Filter to one model to narrow this view.",
            );
        }
        for model in &prices.models.0 {
            let row_id = ui.make_persistent_id(("usage-price-model-row", model.model_id.as_str()));
            ui.scope_builder(egui::UiBuilder::new().scope_id(row_id), |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.strong(model.model_id.as_str());
                    ui.label(format!(
                        "{:?} · revision {}",
                        model.price_kind,
                        model.revision.as_str()
                    ));
                    let edit = ui.add_enabled(can_edit, egui::Button::new("Edit tariff override"));
                    edit.widget_info(|| {
                        egui::WidgetInfo::labeled(
                            egui::Role::Button,
                            can_edit,
                            format!("Edit tariff override for {}", model.model_id.as_str()),
                        )
                    });
                    if edit.clicked() {
                        actions.push(Action::EditPrice(model.model_id.as_str().into()));
                    }
                });
                if let Some(reason) = &model.untracked_reason {
                    ui.label(&reason.0);
                }
            });
        }
        if let Some(history) = &prices.override_history {
            ui.collapsing("Configured price history", |ui| {
                for (index, change) in history.0.iter().enumerate() {
                    ui.push_id(("configured-price-history", index), |ui| {
                        let (model, revision, author, updated) = match change {
                            PriceOverrideOutcome::Set {
                                model_id,
                                revision,
                                updated_by,
                                updated_at_ms,
                                ..
                            }
                            | PriceOverrideOutcome::Cleared {
                                model_id,
                                revision,
                                updated_by,
                                updated_at_ms,
                            } => (model_id, revision, updated_by, updated_at_ms),
                        };
                        ui.group(|ui| {
                            ui.strong(format!(
                                "{} · revision {} · {}",
                                model.as_str(),
                                revision.as_str(),
                                if matches!(change, PriceOverrideOutcome::Set { .. }) {
                                    "Override set"
                                } else {
                                    "Override cleared"
                                }
                            ));
                            ui.label(format!(
                                "Recorded by {} · {}",
                                author
                                    .as_ref()
                                    .map_or("Not recorded", |actor| actor.0.as_str()),
                                updated.as_ref().map_or_else(
                                    || "Time not recorded".into(),
                                    |at| time(at.get())
                                )
                            ));
                            if let PriceOverrideOutcome::Set { tariffs, .. } = change {
                                for (rate, tariff) in tariffs.0.iter().enumerate() {
                                    ui.push_id(rate, |ui| {
                                        ui.collapsing(
                                            format!("Historical configured rate {}", rate + 1),
                                            |ui| tariff_ui(ui, tariff),
                                        );
                                    });
                                }
                            } else {
                                ui.label("This change removed the configured override.");
                            }
                        });
                    });
                }
            });
        }
        for (index, entry) in prices.items.0.iter().enumerate() {
            ui.push_id(("tariff", index), |ui| {
                ui.collapsing(
                    format!(
                        "{} · {} · {:?} · {:?}",
                        entry.upstream_model_id.0,
                        entry.tariff.service_tier.0,
                        entry.tariff.charge_kind,
                        entry.origin
                    ),
                    |ui| tariff_ui(ui, &entry.tariff),
                );
            });
        }
        if prices.has_more
            && ui
                .add_enabled(
                    !self.prices_loading,
                    egui::Button::new("Load more price rows"),
                )
                .clicked()
        {
            actions.push(Action::MorePrices);
        }
        if let Some(draft) = &mut self.price_draft {
            ui.separator();
            ui.heading(format!("Tariff override: {}", draft.model));
            ui.label(format!("Editing inspected revision {}", draft.revision));
            let current = prices
                .models
                .0
                .iter()
                .find(|m| m.model_id.as_str() == draft.model);
            let stale = self.price_update_available
                || current.is_none_or(|model| model.revision.as_str() != draft.revision);
            if stale {
                ui.colored_label(theme::AMBER,if current.is_some(){"This model's price revision changed. Your tariff draft is preserved."}else{"The current model revision is not in these loaded rows. Load its catalog entry before submitting; your draft is preserved."});
                if current.is_some()
                    && !self.price_update_available
                    && !prices.has_more
                    && ui.button("Review and use current price revision").clicked()
                {
                    actions.push(Action::RebasePrice);
                }
            }
            let mut remove = None;
            for (i, tariff) in draft.tariffs.iter_mut().enumerate() {
                ui.push_id(("tariff-editor", i), |ui| {
                    ui.group(|ui| {
                        ui.heading(format!("Rate {}", i + 1));
                        tariff_form(ui, tariff);
                        if ui.button("Remove rate").clicked() {
                            remove = Some(i);
                        }
                    });
                });
            }
            if let Some(i) = remove {
                draft.tariffs.remove(i);
            }
            if ui.button("Add tariff rate").clicked() {
                let mut next = TariffForm::default();
                if let Some(first) = draft.tariffs.first() {
                    next.upstream = first.upstream.clone();
                }
                draft.tariffs.push(next);
            }
            let valid =
                !draft.tariffs.is_empty() && draft.tariffs.iter().all(|t| t.tariff().is_ok());
            if ui
                .add_enabled(
                    !self.price_busy && !stale && valid,
                    egui::Button::new("Save tariff override"),
                )
                .clicked()
            {
                actions.push(Action::SavePrice);
            }
            if ui
                .add_enabled(
                    !self.price_busy && !stale,
                    egui::Button::new("Clear tariff override"),
                )
                .clicked()
            {
                actions.push(Action::ClearPrice);
            }
            if !valid {
                ui.label("Complete each rate's upstream model, service tier, amount and units before saving.");
            }
        }
    }
}

pub fn field(ui: &mut Ui, label: &str, value: &mut String, hint: &str) {
    ui.vertical(|ui| {
        ui.label(label);
        let response = ui.add(
            egui::TextEdit::singleline(value)
                .id_salt(label)
                .desired_width(ui.available_width().min(240.0))
                .hint_text(hint),
        );
        response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, label));
    });
}
pub fn human(key: &str) -> String {
    let mut out = String::new();
    for (i, c) in key.chars().enumerate() {
        if c == '_' {
            out.push(' ');
        } else if c.is_uppercase() && i > 0 {
            out.push(' ');
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    let mut chars = out.chars();
    chars.next().map_or(String::new(), |first| {
        first.to_uppercase().collect::<String>() + chars.as_str()
    })
}
pub fn time(ms: i64) -> String {
    jiff::Timestamp::from_millisecond(ms)
        .map(|t| {
            t.to_zoned(jiff::tz::TimeZone::UTC)
                .strftime("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|_| "Time unavailable".into())
}
pub fn usd(micro: i64) -> String {
    format!("${}.{:06}", micro / 1_000_000, micro % 1_000_000)
}
fn observed(value: &Option<devworks_core::protocol::Counter>) -> String {
    value.as_ref().map_or_else(
        || "Unknown / not recorded".into(),
        |v| v.as_str().to_owned(),
    )
}
fn coverage_ui(ui: &mut Ui, coverage: &Coverage) {
    let (tone, text) = match coverage.state {
        CoverageState::Complete => (Tone::Good, "Complete recorded coverage"),
        CoverageState::Partial => (
            Tone::Attention,
            "Partial coverage — totals may omit unknown history",
        ),
        CoverageState::Unavailable => (
            Tone::Bad,
            "Coverage unavailable — complete usage is unknown",
        ),
    };
    chip(ui, text, tone);
    for gap in &coverage.gaps {
        ui.label(format!("Coverage gap: {}", human(gap.as_str())));
    }
    if coverage.more_gaps {
        ui.label("Additional coverage gaps exist.");
    }
}
fn totals_ui(ui: &mut Ui, t: &UsageTotals) {
    widgets::summary_strip(
        ui,
        &[
            ("Attempts", Some(t.attempts.as_str().into())),
            ("Tracked spend", Some(usd(t.tracked_micro_usd.get()))),
            ("Reserved", Some(usd(t.reserved_micro_usd.get()))),
        ],
        "Authoritative totals for the applied result",
    );
    for (label, value) in [
        ("Reported spend", usd(t.reported_micro_usd.get())),
        ("Estimated spend", usd(t.estimated_micro_usd.get())),
        ("Local/free attempts", t.local_free_attempts.as_str().into()),
        ("Untracked attempts", t.untracked_attempts.as_str().into()),
        (
            "Unknown-tail attempts",
            t.unknown_tail_attempts.as_str().into(),
        ),
        ("Incomplete attempts", t.incomplete_attempts.as_str().into()),
        (
            "Monetarily incomplete attempts",
            t.monetary_incomplete_attempts.as_str().into(),
        ),
        (
            "Historical usage facts",
            t.legacy_usage_facts.as_str().into(),
        ),
        (
            "Historical untracked facts",
            t.legacy_untracked_facts.as_str().into(),
        ),
        (
            "Historical raw cost observations (separate from spend)",
            usd(t.legacy_observation_micro_usd.get()),
        ),
    ] {
        ui.label(format!("{label}: {value}"));
    }
}
fn cost_label(cost: &UsageCostSnapshot) -> String {
    format!(
        "{:?} · {}{}",
        cost.price_kind,
        cost.amount_micro_usd
            .as_ref()
            .map_or_else(|| "Amount unknown".into(), |n| usd(n.get())),
        if cost.monetary_complete {
            ""
        } else {
            " · incomplete"
        }
    )
}
fn attempt_ui(ui: &mut Ui, r: &UsageAttemptRecord, actions: &mut Vec<Action>) {
    ui.label(cost_label(&r.cost));
    ui.label(format!(
        "Request identity: {} · attempt identity: {}",
        r.request_id
            .as_ref()
            .map_or("Not recorded", |id| id.as_str()),
        r.attempt_id
            .as_ref()
            .map_or("Not recorded", |id| id.as_str())
    ));
    ui.label(format!(
        "Billing transport: {} · recorded actor: {}",
        r.provider_id
            .as_ref()
            .map_or("Not recorded", |id| id.as_str()),
        r.actor_id
            .as_ref()
            .map_or("Not recorded", |actor| actor.0.as_str())
    ));
    ui.label(format!(
        "Price origin {:?} · {}",
        r.cost.price_origin,
        if r.cost.subscription_equivalent {
            "Subscription equivalent estimate"
        } else {
            "Recorded request cost"
        }
    ));
    if let Some(reason) = &r.cost.untracked_reason {
        ui.colored_label(theme::AMBER, &reason.0);
    }
    if r.unknown_tail {
        ui.colored_label(
            theme::AMBER,
            "This request has an unknown tail; full usage and cost are not established.",
        );
    }
    for (label, value) in [
        ("Input tokens", &r.input_tokens),
        ("Uncached input tokens", &r.uncached_input_tokens),
        ("Output tokens", &r.output_tokens),
        ("Reasoning tokens", &r.reasoning_tokens),
        ("Cache read tokens", &r.cache_read_tokens),
        ("Cache write tokens", &r.cache_write_tokens),
    ] {
        ui.label(format!("{label}: {}", observed(value)));
    }
    for (label, value) in [
        ("Cache read cost estimate", &r.cache_read_micro_usd),
        ("Cache write cost estimate", &r.cache_write_micro_usd),
    ] {
        ui.label(format!(
            "{label}: {}",
            value
                .as_ref()
                .map_or_else(|| "Unknown / not recorded".into(), |v| usd(v.get()))
        ));
    }
    if let Some(semantics) = &r.input_semantics {
        ui.label(format!("Input semantics: {}", semantics.0));
    }
    for quantity in &r.unit_quantities.0 {
        ui.label(format!(
            "{} {}: {} {}{}",
            quantity.modality.0,
            quantity.charge_kind.0,
            quantity.quantity.as_str(),
            quantity.unit.0,
            quantity
                .cache_write_ttl_ms
                .as_ref()
                .map_or(String::new(), |ttl| format!(
                    " · cache TTL {} ms",
                    ttl.as_str()
                ))
        ));
    }
    for diagnostic in &r.cache_diagnostics.0 {
        ui.label(format!(
            "{}{}{}",
            diagnostic.kind.0,
            if diagnostic.inferred {
                " (inferred)"
            } else {
                " (observed)"
            },
            diagnostic
                .detail
                .as_ref()
                .map_or(String::new(), |d| format!(": {}", d.0))
        ));
    }
    if let Some(work) = &r.work_id
        && ui.button("Open attributed work").clicked()
    {
        actions.push(Action::OpenWork(work.as_str().into()));
    }
    if let Some(session) = &r.session_id
        && ui.button("Open attributed chat").clicked()
    {
        actions.push(Action::OpenSession(session.as_str().into()));
    }
    ui.collapsing("Recorded attribution and price provenance", |ui| {
        record_fields(ui, &serde_json::to_value(r).expect("typed attempt"));
    });
    for (i, tariff) in r.cost.tariffs.0.iter().enumerate() {
        ui.push_id(("recorded-tariff", i), |ui| {
            ui.collapsing(format!("Recorded tariff {}", i + 1), |ui| {
                tariff_ui(ui, tariff)
            });
        });
    }
}
fn record_fields(ui: &mut Ui, value: &Value) {
    if let Some(object) = value.as_object() {
        for (key, value) in object {
            ui.push_id(key, |ui| {
                if value.is_object() || value.is_array() {
                    ui.collapsing(human(key), |ui| {
                        if let Some(values) = value.as_array() {
                            for (i, v) in values.iter().enumerate() {
                                ui.push_id(i, |ui| record_fields(ui, v));
                            }
                        } else {
                            record_fields(ui, value);
                        }
                    });
                } else {
                    ui.label(format!(
                        "{}: {}",
                        human(key),
                        value
                            .as_str()
                            .map_or_else(|| value.to_string(), str::to_owned)
                    ));
                }
            });
        }
    } else {
        ui.label(
            value
                .as_str()
                .map_or_else(|| value.to_string(), str::to_owned),
        );
    }
}
fn tariff_ui(ui: &mut Ui, t: &PriceTariff) {
    record_fields(ui, &serde_json::to_value(t).expect("typed tariff"));
}
fn choice(ui: &mut Ui, label: &str, value: &mut String, options: &[&str]) {
    egui::ComboBox::from_label(label)
        .selected_text(value.as_str())
        .show_ui(ui, |ui| {
            for option in options {
                ui.selectable_value(value, (*option).into(), *option);
            }
        });
}
fn tariff_form(ui: &mut Ui, t: &mut TariffForm) {
    field(ui, "Tariff upstream model", &mut t.upstream, "");
    field(
        ui,
        "Service tier",
        &mut t.service,
        "Standard or an exact custom tier",
    );
    ui.horizontal_wrapped(|ui| {
        field(
            ui,
            "Context above (exclusive)",
            &mut t.lower,
            "Unbounded when blank",
        );
        field(
            ui,
            "Context up to (inclusive)",
            &mut t.upper,
            "Unbounded when blank",
        );
    });
    choice(
        ui,
        "Tariff modality",
        &mut t.modality,
        &["Text", "Image", "Audio", "Video", "Any"],
    );
    choice(
        ui,
        "Charge kind",
        &mut t.charge,
        &[
            "Input",
            "Output",
            "CacheRead",
            "CacheWrite",
            "Request",
            "Runtime",
        ],
    );
    field(ui, "Cache TTL seconds", &mut t.ttl, "Not specified");
    choice(
        ui,
        "Charge unit",
        &mut t.unit,
        &["Token", "Second", "Image", "Request", "Character"],
    );
    ui.horizontal_wrapped(|ui| {
        field(ui, "Units per rate", &mut t.units, "");
        field(ui, "Amount USD per rate", &mut t.amount, "Exact decimal");
    });
    field(ui, "Tariff region", &mut t.region, "Any region");
    field(ui, "Endpoint profile", &mut t.profile, "Any profile");
    if !t.amount.is_empty()
        && let Err(error) = t.tariff()
    {
        ui.colored_label(theme::RED, error);
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BudgetDraft {
    pub enabled: bool,
    pub dollars: String,
}
impl BudgetDraft {
    pub fn from_view(view: Option<&FeatureBudgetView>) -> Self {
        view.and_then(|v| v.limit_micro_usd.as_ref())
            .map_or_else(Self::default, |n| Self {
                enabled: true,
                dollars: format!("{}.{:06}", n.get() / 1_000_000, n.get() % 1_000_000),
            })
    }
    pub fn from_input(input: &FeatureBudgetInput) -> Self {
        match input {
            FeatureBudgetInput::Off {} => Self::default(),
            FeatureBudgetInput::Limit { micro_usd } => Self {
                enabled: true,
                dollars: format!(
                    "{}.{:06}",
                    micro_usd.get() / 1_000_000,
                    micro_usd.get() % 1_000_000
                ),
            },
        }
    }
    pub fn input(&self) -> Result<Value, String> {
        if !self.enabled {
            return Ok(json!({"kind":"Off"}));
        }
        let pieces = self.dollars.trim().split('.').collect::<Vec<_>>();
        if pieces.is_empty()
            || pieces.len() > 2
            || pieces[0].is_empty()
            || !pieces[0].bytes().all(|c| c.is_ascii_digit())
            || pieces
                .get(1)
                .is_some_and(|n| n.len() > 6 || !n.bytes().all(|c| c.is_ascii_digit()))
        {
            return Err("Enter a USD limit with up to six decimal places.".into());
        }
        let whole = pieces[0]
            .parse::<i64>()
            .ok()
            .and_then(|n| n.checked_mul(1_000_000));
        let fraction = pieces
            .get(1)
            .map_or(Ok(0), |n| format!("{n:0<6}").parse::<i64>());
        let micro = whole
            .and_then(|whole| fraction.ok().and_then(|part| whole.checked_add(part)))
            .ok_or("The USD limit is outside the supported range.")?;
        Ok(json!({"kind":"Limit","microUsd":micro.to_string()}))
    }
    pub fn ui(&mut self, ui: &mut Ui) -> bool {
        let mut changed = ui
            .checkbox(&mut self.enabled, "Limit feature spending")
            .changed();
        if self.enabled {
            ui.label("Feature spending limit (USD)");
            let response = ui.add(egui::TextEdit::singleline(&mut self.dollars).hint_text("10.00"));
            response.widget_info(|| {
                egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Feature spending limit USD")
            });
            changed |= response.changed();
        } else {
            widgets::caption(ui, "Spending cap off");
        }
        if let Err(error) = self.input() {
            ui.colored_label(theme::RED, error);
        }
        changed
    }
}
pub fn budget_choice(ui: &mut Ui, budget: &FeatureBudgetInput) {
    match budget {
        FeatureBudgetInput::Off {} => {
            ui.label("Proposed feature spending cap: Off");
        }
        FeatureBudgetInput::Limit { micro_usd } => {
            ui.label(format!(
                "Proposed feature spending cap: {}",
                usd(micro_usd.get())
            ));
        }
    }
}
pub fn budget_status(ui: &mut Ui, budget: &FeatureBudgetView) {
    ui.label(format!(
        "Feature spending cap: {}",
        budget
            .limit_micro_usd
            .as_ref()
            .map_or_else(|| "Off".into(), |limit| usd(limit.get()))
    ));
    ui.label(format!(
        "Tracked spend {} · reserved {}",
        usd(budget.spent_micro_usd.get()),
        usd(budget.reserved_micro_usd.get())
    ));
    if budget.warning {
        chip(ui, "75% spending warning reached", Tone::Attention);
    }
    if budget.continue_required {
        chip(
            ui,
            "Budget hold · explicit Continue required",
            Tone::Attention,
        );
        ui.label("Raising or turning off the cap does not clear this hold. Continue rechecks all remaining holds.");
    }
    if !budget.accounting_ready
        || budget.untracked_attempts.get() > 0
        || budget.unknown_tail_attempts.get() > 0
        || budget.monetary_incomplete_attempts.get() > 0
    {
        ui.colored_label(
            theme::AMBER,
            "Accounting coverage is incomplete; total monetary exposure is unknown.",
        );
    }
    widgets::caption(
        ui,
        format!(
            "Untracked {} · unknown tail {} · monetary incomplete {} · historical facts {} · budget revision {}",
            budget.untracked_attempts.as_str(),
            budget.unknown_tail_attempts.as_str(),
            budget.monetary_incomplete_attempts.as_str(),
            budget.legacy_usage_facts.as_str(),
            budget.revision.as_str()
        ),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tariff_editor_round_trips_every_dimension_and_exact_decimal_rate() {
        let raw = json!({"upstreamModelId":"publisher/model-v7","serviceTier":"priority","contextLowerExclusive":"200000","contextUpperInclusive":"1000000","modality":"Video","chargeKind":"CacheWrite","cacheTtlSeconds":"3600","unit":"Second","units":"17","amountUsd":"0.000000000123","region":"region-42","endpointProfile":"compatible"});
        let tariff: PriceTariff = serde_json::from_value(raw.clone()).unwrap();
        let edited = TariffForm::from_tariff(&tariff).tariff().unwrap();
        assert_eq!(serde_json::to_value(edited).unwrap(), raw);
    }
    #[test]
    fn feature_cap_editor_uses_exact_micro_usd_and_distinguishes_zero_from_off() {
        let mut draft = BudgetDraft {
            enabled: true,
            dollars: "0.000001".into(),
        };
        assert_eq!(
            draft.input().unwrap(),
            json!({"kind":"Limit","microUsd":"1"})
        );
        draft.dollars = "0".into();
        assert_eq!(
            draft.input().unwrap(),
            json!({"kind":"Limit","microUsd":"0"})
        );
        draft.enabled = false;
        assert_eq!(draft.input().unwrap(), json!({"kind":"Off"}));
        draft.enabled = true;
        draft.dollars = "0.0000001".into();
        assert!(draft.input().is_err());
        draft.dollars = "9223372036855".into();
        assert!(draft.input().is_err());
    }
    #[test]
    fn report_paging_refuses_a_different_read_snapshot_without_replacing_prior_rows() {
        let page = |revision: &str| {
            serde_json::from_value::<UsageQueryResult>(json!({"kind":"Report","report":{"context":{"reportKind":"AssistantUsage","range":{"startMs":"0","endMs":"1000"},"interval":"day","appliedFilters":{},"readRevision":revision},"items":[{"kind":"Total","key":"totalTokens","label":"Total tokens","value":"8"}],"hasMore":false,"coverage":{"state":"Partial","gaps":["history-missing"],"moreGaps":false}}})).unwrap()
        };
        let mut screen = UsageScreen {
            data: Some(page("1")),
            ..Default::default()
        };
        assert!(screen.append(page("2")).is_err());
        assert_eq!(
            serde_json::to_value(screen.data.as_ref().unwrap()).unwrap(),
            serde_json::to_value(page("1")).unwrap()
        );
    }
}
