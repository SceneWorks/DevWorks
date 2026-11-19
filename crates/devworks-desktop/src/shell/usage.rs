//! Usage/price request lifecycle. Exact command frames use the shell's durable retry path.
use super::*;
use crate::{
    backend::Unresolved,
    paged::{Applied, Fetch, Page},
    screen::{Cx, Screen, ScreenContext, ScreenId, controller},
    usage_ops::{self as uops, UsageSlot},
    usage_screen::{Action, PriceDraft, TariffForm, UsageScreen},
};
use devworks_core::ports::storage::ProjectPurgeClientScope;
use devworks_core::protocol::{OperationState, records::*, results::OperationResult};

fn values(input: &str) -> Vec<&str> {
    input
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect()
}
/// The comma separated values of a filter, or nothing when it is blank.
fn given(text: &str) -> Option<Vec<&str>> {
    (!text.trim().is_empty()).then(|| values(text))
}
controller!(UsageCtl, usage: UsageScreen);

impl Screen for UsageScreen {
    type Slot = UsageSlot;
    const ID: ScreenId = ScreenId::Usage;
    fn load(&mut self, cx: &mut Cx, _route: Route) {
        self.tab = cx.routes.entry(Route::Usage).or_default().tab;
        // Entering the route again re-reads what was applied, so the pages shown stay.
        UsageCtl { usage: self, cx }.load_usage(false);
        cx.load_router_catalog();
        if !cx.projects.list.loaded {
            cx.load_projects();
        }
    }
    fn refresh(&mut self, cx: &mut Cx, _route: Route) {
        if self.tab == 2 && !self.prices.loading() {
            UsageCtl { usage: self, cx }.refresh_visible_prices();
        }
    }
    fn apply(&mut self, cx: &mut Cx, slot: UsageSlot, reply: Reply) {
        if let Some((slot, reply)) = (UsageCtl { usage: self, cx }).apply_usage(slot, reply) {
            cx.unhandled(&slot, reply);
        }
    }
    fn ui(&mut self, cx: &mut Cx, _route: Route, ui: &mut Ui) {
        UsageCtl { usage: self, cx }.usage_screen(ui);
    }
    fn restore_in_flight(&mut self, slot: &UsageSlot, u: &Unresolved) {
        if matches!(slot, UsageSlot::PriceOverride | UsageSlot::PriceRefresh) {
            self.restore_price_command(slot, u.frame());
        }
    }
    fn remove_purged(&mut self, scope: &ProjectPurgeClientScope) {
        let p = scope.project_id.0.as_str();
        self.data.reset();
        self.options.reset();
        self.applied = None;
        self.applied_kind = None;
        if self.draft.project.as_deref() == Some(p) {
            self.draft.project = None;
        }
    }
    fn context(&self, _route: Route, mem: &RouteMemory) -> ScreenContext {
        ScreenContext {
            tab: self.tab,
            selection: mem.selection.clone(),
        }
    }
}

impl UsageScreen {
    fn restore_price_command(&mut self, slot: &UsageSlot, frame: &[u8]) {
        self.price_busy = true;
        self.message = Some(
            "A submitted price change is awaiting confirmation. Its original request is retained."
                .into(),
        );
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(frame) else {
            return;
        };
        let body = &v["body"];
        match slot {
            UsageSlot::PriceOverride => {
                let (Some(model), Some(revision)) =
                    (body["modelId"].as_str(), body["expectedRevision"].as_str())
                else {
                    return;
                };
                let tariffs = if body["change"]["kind"] == "Set" {
                    let Ok(rates) = serde_json::from_value::<Vec<PriceTariff>>(
                        body["change"]["tariffs"].clone(),
                    ) else {
                        return;
                    };
                    rates.iter().map(TariffForm::from_tariff).collect()
                } else {
                    Vec::new()
                };
                let draft = PriceDraft {
                    model: model.into(),
                    revision: revision.into(),
                    tariffs,
                };
                self.price_saving = Some(draft.clone());
                self.price_draft = Some(draft);
            }
            UsageSlot::PriceRefresh => {
                if let Some(providers) = body["providerIds"].as_array() {
                    self.price_providers = providers
                        .iter()
                        .filter_map(serde_json::Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ");
                }
            }
            _ => {}
        }
    }
}

impl UsageCtl<'_, '_> {
    /// Reads the report (or the price catalog) for the filters in the form. `restart` is the
    /// Apply gesture, which always starts from the first page; without it, filters that are
    /// those already applied re-read every page shown.
    fn load_usage(&mut self, restart: bool) {
        if self.usage.tab == 2 {
            if restart {
                self.load_prices(false);
            } else {
                self.reread_prices();
            }
            return;
        }
        let kind = self.usage.kind();
        if let Err(e) = self.usage.draft.body(kind, None) {
            self.usage.data.error = Some(e);
            return;
        }
        let same = !restart
            && self.usage.data.loaded
            && self.usage.applied.as_ref() == Some(&self.usage.draft)
            && self.usage.applied_kind == Some(kind);
        self.usage.applied = Some(self.usage.draft.clone());
        self.usage.applied_kind = Some(kind);
        self.usage.data.error = None;
        // A re-read of the applied report joins every page shown into one result again.
        let fetch = if same {
            self.usage.data.refresh()
        } else {
            self.usage.data.reload()
        };
        self.send_usage(fetch);
        if kind != "Filters" {
            // The filter values of the applied range are read with the report.
            self.usage.options.reset();
            let fetch = self.usage.options.reload_all();
            self.load_usage_options(fetch);
        } else {
            self.usage.options.reset();
        }
    }
    /// Submits the report request `fetch` names, for the filters and kind applied.
    fn send_usage(&mut self, fetch: Fetch) {
        let (Some(filters), Some(kind)) = (&self.usage.applied, self.usage.applied_kind) else {
            self.usage.data.abandon(fetch.generation);
            return;
        };
        let filters = filters.clone();
        let slot = UsageSlot::Usage(fetch.generation, fetch.append());
        self.cx.paged(
            &mut self.usage.data,
            fetch,
            slot,
            "Loading usage report",
            "router.usage",
            move |b, cursor| match filters.body(kind, cursor) {
                Ok(body) => uops::query(b, body),
                Err(_) => Vec::new(),
            },
        );
    }
    fn load_usage_options(&mut self, fetch: Fetch) {
        let Some(filters) = &self.usage.applied else {
            return;
        };
        let filters = filters.clone();
        let slot = UsageSlot::UsageOptions(fetch.generation);
        self.cx.paged(
            &mut self.usage.options,
            fetch,
            slot,
            "Loading available usage filter values",
            "router.usage",
            move |b, cursor| match filters.body("Filters", cursor) {
                Ok(body) => uops::query(b, body),
                Err(_) => Vec::new(),
            },
        );
    }
    fn more_usage(&mut self) {
        let Some(filters) = self.usage.applied.clone() else {
            return;
        };
        let kind = self.usage.kind();
        let Some(fetch) = self.usage.data.more_page() else {
            return;
        };
        let slot = UsageSlot::Usage(fetch.generation, true);
        self.cx.paged(
            &mut self.usage.data,
            fetch,
            slot,
            "Loading more usage rows",
            "router.usage",
            move |b, cursor| match filters.body(kind, cursor) {
                Ok(body) => uops::query(b, body),
                Err(_) => Vec::new(),
            },
        );
    }
    /// Keep a user's loaded page set intact when a background refresh publishes a new catalog.
    /// A partial first page already owns a cursor: probing also preserves a More gesture
    /// before its append reply arrives. Completed single pages can be reloaded normally. A
    /// paged catalog is deliberately not re-read here: a changed catalog revision is flagged
    /// for the user's explicit Reload (a restart), which keeps the draft.
    fn refresh_visible_prices(&mut self) {
        if self.usage.prices.pages() <= 1
            && self
                .usage
                .prices
                .items
                .first()
                .is_none_or(|page| !page.has_more)
        {
            self.reload_applied_prices();
        } else if !self.pending(&UsageSlot::PriceProbe(
            self.usage.prices.reload_generation(),
        )) {
            let generation = self.usage.prices.reload_generation();
            self.query(
                UsageSlot::PriceProbe(generation),
                "Checking for updated price catalog",
                "router.price_status",
                |b| uops::query(b, uops::price_probe()),
            );
        }
    }
    /// Re-reads the applied catalog query after a change the user made (a saved override, a
    /// finished price refresh): as many pages as are shown are read again and joined, so the
    /// rows the user loaded stay. A page of another catalog revision than the first ends the
    /// read with the reload prompt, as when paging.
    fn reload_applied_prices(&mut self) {
        let Some(body) = self.usage.price_query.clone() else {
            self.load_prices(false);
            return;
        };
        let fetch = self.usage.prices.reread();
        self.send_prices(fetch, body, "Refreshing applied price catalog");
    }
    /// Entering the route again: the applied catalog query is re-read when the form still holds
    /// it, else the form's filters are applied from the first page.
    fn reread_prices(&mut self) {
        let same = match (self.price_body(), &self.usage.price_query) {
            (Some(form), Some(applied)) => {
                serde_json::to_value(&form).ok() == serde_json::to_value(applied).ok()
            }
            _ => false,
        };
        if same && self.usage.prices.loaded {
            self.reload_applied_prices();
        } else {
            self.load_prices(false);
        }
    }
    fn price_body(&self) -> Option<devworks_core::operations::router::PriceStatusInput> {
        uops::price_status(
            &values(&self.usage.price_models),
            given(&self.usage.price_providers).as_deref(),
            given(&self.usage.price_snapshots).as_deref(),
            self.usage.history,
        )
    }
    fn load_prices(&mut self, append: bool) {
        let (body, fetch) = if append {
            let Some(body) = self.usage.price_query.clone() else {
                return;
            };
            let Some(fetch) = self.usage.prices.more_page() else {
                return;
            };
            (body, fetch)
        } else {
            let Some(body) = self.price_body() else {
                self.usage.price_error = Some("Check the price filters.".into());
                return;
            };
            self.usage.price_query = Some(body.clone());
            // The filters were applied (or the user asked for the catalog again): start over.
            (body, self.usage.prices.reload())
        };
        self.send_prices(fetch, body, "Loading price catalog");
    }
    fn send_prices(
        &mut self,
        fetch: Fetch,
        body: devworks_core::operations::router::PriceStatusInput,
        label: &str,
    ) {
        let slot = UsageSlot::Prices(fetch.generation, fetch.append());
        self.cx.paged(
            &mut self.usage.prices,
            fetch,
            slot,
            label,
            "router.price_status",
            move |b, cursor| uops::query(b, uops::price_page(body, cursor)),
        );
    }
    fn edit_price(&mut self, model: String) {
        if self.usage.price_busy || self.usage.price_update_available {
            return;
        }
        let Some(prices) = self.usage.prices.items.first() else {
            return;
        };
        if prices.has_more {
            return;
        }
        let Some(record) = prices
            .models
            .0
            .iter()
            .find(|m| m.model_id.as_str() == model)
        else {
            return;
        };
        // Current price subjects include non-conversational embedding identities. Older
        // servers may omit this metadata, so retain the existing conversational fallback.
        let identity = record
            .upstream_model_id
            .as_ref()
            .zip(record.endpoint_profile.as_ref())
            .map(|(u, p)| (u.0.as_str(), p))
            .or_else(|| {
                self.catalog
                    .iter()
                    .find(|r| r.id.as_str() == model)
                    .map(|r| (r.upstream_model_id.0.as_str(), &r.endpoint_profile))
            });
        let configured = prices
            .items
            .0
            .iter()
            .filter(|entry| {
                entry
                    .model_id
                    .as_ref()
                    .is_some_and(|id| id.as_str() == model)
                    && matches!(entry.origin, PriceOrigin::UserConfigured)
                    && record.override_revision.as_ref().is_some_and(|current| {
                        entry
                            .override_revision
                            .as_ref()
                            .is_some_and(|revision| revision.as_str() == current.as_str())
                    })
            })
            .map(|e| TariffForm::from_tariff(&e.tariff))
            .collect::<Vec<_>>();
        let mut tariffs = if configured.is_empty() {
            // Official rows belong to an upstream model in an immutable provider snapshot,
            // rather than to a local catalog model ID. Resolve that exact identity before
            // seeding an override; history can contain other snapshots and upstream models.
            let upstream = if record.snapshot_id.is_some() {
                let Some((wire, profile)) = identity else {
                    self.usage.price_error = Some(
                        "Reload the price subject before editing this official tariff.".into(),
                    );
                    return;
                };
                Some(if matches!(profile, EndpointKind::OpenRouterChat) {
                    wire.split_once('/').map_or(wire, |(_, upstream)| upstream)
                } else {
                    wire
                })
            } else {
                None
            };
            prices
                .items
                .0
                .iter()
                .filter(|entry| {
                    matches!(entry.origin, PriceOrigin::OfficialSnapshot)
                        && upstream.is_some_and(|upstream| entry.upstream_model_id.0 == upstream)
                        && record.snapshot_id.as_ref().is_some_and(|current| {
                            entry
                                .snapshot_id
                                .as_ref()
                                .is_some_and(|id| id.as_str() == current.as_str())
                        })
                })
                .map(|e| {
                    let mut form = TariffForm::from_tariff(&e.tariff);
                    if let Some((wire, profile)) = identity {
                        // Provider snapshots use canonical model identities. An override belongs
                        // to the exact configured model and serving transport selected here.
                        form.upstream = wire.into();
                        if !form.profile.is_empty() {
                            form.profile = serde_json::to_value(profile)
                                .expect("typed endpoint profile")
                                .as_str()
                                .expect("endpoint profile is a wire name")
                                .to_owned();
                        }
                    }
                    form
                })
                .collect::<Vec<_>>()
        } else {
            configured
        };
        if tariffs.is_empty() {
            let mut form = TariffForm::default();
            if let Some((wire, profile)) = identity {
                form.upstream = wire.into();
                if matches!(profile, EndpointKind::Embedding) {
                    form.profile = "Embedding".into();
                }
            }
            tariffs.push(form);
        }
        self.usage.price_draft = Some(PriceDraft {
            model,
            revision: record.revision.as_str().into(),
            tariffs,
        });
    }
    fn save_price(&mut self, clear: bool) {
        if self.usage.price_busy {
            return;
        }
        let Some(draft) = self.usage.price_draft.clone() else {
            return;
        };
        let tariffs = if clear {
            None
        } else {
            let tariffs = match draft
                .tariffs
                .iter()
                .map(TariffForm::tariff)
                .collect::<Result<Vec<_>, _>>()
            {
                Ok(tariffs) => tariffs,
                Err(e) => {
                    self.usage.price_error = Some(e);
                    return;
                }
            };
            Some(tariffs)
        };
        let input = uops::price_override(&draft.model, &draft.revision, tariffs);
        self.usage.price_saving = Some(draft);
        self.usage.price_busy = true;
        if !self.command(
            UsageSlot::PriceOverride,
            "Saving tariff override",
            "router.price_override",
            move |b| uops::command(b, input),
        ) {
            self.usage.price_busy = false;
        }
    }
    fn refresh_prices(&mut self) {
        let Some(catalog) = self.usage.prices.items.first() else {
            return;
        };
        let providers = if self.usage.price_providers.trim().is_empty() {
            catalog
                .providers
                .0
                .iter()
                .filter(|p| matches!(p.provider_id.as_str(), "openai" | "anthropic"))
                .map(|p| p.provider_id.as_str().to_owned())
                .collect::<Vec<_>>()
        } else {
            values(&self.usage.price_providers)
                .into_iter()
                .map(str::to_owned)
                .collect()
        };
        if providers.is_empty() {
            self.usage.price_error =
                Some("Select an official price source to refresh (openai or anthropic).".into());
            return;
        }
        let input = uops::price_refresh(&providers, &catalog.catalog_revision);
        self.usage.price_busy = true;
        // Refresh retains its original revision 1 contract; status and override negotiate 3.
        if !self.command(
            UsageSlot::PriceRefresh,
            "Requesting official price refresh",
            "router.price_refresh",
            move |b| uops::command(b, input),
        ) {
            self.usage.price_busy = false;
        }
    }
    fn poll_price_refresh(&mut self, ctx: &egui::Context) {
        let Some(operation) = self.usage.refresh.as_ref() else {
            return;
        };
        if self
            .usage
            .last_refresh
            .is_none_or(|at| at.elapsed() >= Duration::from_secs(3))
            && !self.pending(&UsageSlot::PriceOperation)
        {
            let id = operation.operation_id.clone();
            self.usage.last_refresh = Some(Instant::now());
            self.query(
                UsageSlot::PriceOperation,
                "Checking price refresh completion",
                "history.operation_status",
                move |b| uops::operation_status(b, &id),
            );
        }
        ctx.request_repaint_after(Duration::from_secs(3));
    }
    fn apply_usage(&mut self, slot: UsageSlot, reply: Reply) -> Option<(UsageSlot, Reply)> {
        match (slot, reply) {
            (UsageSlot::Usage(generation, _), Reply::Query(q)) => {
                match uops::usage(q) {
                    Ok(data) => {
                        let next = UsageScreen::next_of(&data);
                        let page = Page::new(vec![data], next);
                        // A further page joins the result shown (refused when it belongs to
                        // another report); a first page replaces it.
                        let applied =
                            self.usage
                                .data
                                .apply_joined(generation, Ok(page), UsageScreen::join);
                        match applied {
                            Applied::Stale => return None,
                            Applied::Continue(fetch) => {
                                self.send_usage(fetch);
                                return None;
                            }
                            _ => {}
                        }
                    }
                    Err(e) => {
                        if self.usage.data.apply(generation, Err(e.describe())) == Applied::Stale {
                            return None;
                        }
                    }
                }
            }
            (UsageSlot::UsageOptions(generation), Reply::Query(q)) => match uops::usage(q) {
                Ok(UsageQueryResult::Filters { filters }) => {
                    let next = filters.next_cursor.map(|c| c.0);
                    if let Applied::Continue(fetch) = self
                        .usage
                        .options
                        .apply(generation, Ok(Page::new(filters.items.0, next)))
                    {
                        self.load_usage_options(fetch);
                    }
                }
                Ok(_) => {
                    if self
                        .usage
                        .options
                        .apply(generation, Err("Unexpected usage filter response".into()))
                        != Applied::Stale
                    {
                        self.usage.data.error = Some("Unexpected usage filter response".into());
                    }
                }
                Err(e) => {
                    if self.usage.options.apply(generation, Err(e.describe())) != Applied::Stale {
                        self.usage.data.error = Some(e.describe());
                    }
                }
            },
            // A probe belongs to the catalog request that was current when it was asked.
            (UsageSlot::PriceProbe(generation), _)
                if generation != self.usage.prices.reload_generation() => {}
            (UsageSlot::PriceProbe(_), Reply::Query(q)) => match uops::prices(q) {
                Ok(current) => {
                    self.usage.price_update_available =
                        self.usage.prices.items.first().is_some_and(|previous| {
                            previous.catalog_revision.as_str() != current.catalog_revision.as_str()
                        })
                }
                Err(e) => self.usage.price_error = Some(e.describe()),
            },
            (UsageSlot::Prices(generation, _), Reply::Query(q)) => match uops::prices(q) {
                Ok(result) => {
                    let next = result.next_cursor.as_ref().map(|c| c.0.clone());
                    let page = Page::new(vec![result], next);
                    match self
                        .usage
                        .prices
                        .apply_joined(generation, Ok(page), join_prices)
                    {
                        Applied::Stale => return None,
                        Applied::Fenced => {
                            self.usage.price_error = self.usage.prices.error.clone();
                            return None;
                        }
                        Applied::Continue(fetch) => {
                            if let Some(body) = self.usage.price_query.clone() {
                                self.send_prices(fetch, body, "Refreshing applied price catalog");
                            }
                            return None;
                        }
                        _ => {}
                    }
                    self.usage.price_update_available = false;
                    self.usage.price_error = None;
                }
                Err(e) => {
                    if self.usage.prices.apply(generation, Err(e.describe())) != Applied::Stale {
                        self.usage.price_error = Some(e.describe());
                    }
                }
            },
            (UsageSlot::PriceOverride, Reply::Command(q)) => {
                self.usage.price_busy = is_unknown(&q);
                match uops::overridden(q) {
                    Ok(_) => {
                        if self.usage.price_draft == self.usage.price_saving {
                            self.usage.price_draft = None;
                        }
                        self.usage.price_saving = None;
                        self.usage.message=Some("Tariff override recorded. Earlier requests retain their captured price provenance.".into());
                        self.reload_applied_prices();
                    }
                    Err(e) => self.usage.price_error = Some(e.describe()),
                }
            }
            (UsageSlot::PriceRefresh, Reply::Command(q)) => {
                self.usage.price_busy = is_unknown(&q);
                match uops::refreshed(q) {
                    Ok(operation) => {
                        self.usage.refresh = Some(operation);
                        self.usage.last_refresh = None;
                        self.usage.message = Some(
                            "Price refresh accepted; awaiting the recorded operation outcome."
                                .into(),
                        );
                    }
                    Err(e) => self.usage.price_error = Some(e.describe()),
                }
            }
            (UsageSlot::PriceOperation, Reply::Query(q)) => match ops::query_result(q) {
                Ok((OperationResult::HistoryOperationStatus { value }, _)) => {
                    let complete = matches!(value.state, OperationState::ObservedComplete);
                    let terminal = complete
                        || matches!(
                            value.state,
                            OperationState::Failed
                                | OperationState::CancelledBeforeDispatch
                                | OperationState::NeedsInput
                        );
                    if terminal {
                        self.usage.refresh = None;
                        self.usage.message = Some(format!("Price refresh {:?}.", value.state));
                        self.reload_applied_prices();
                    } else {
                        self.usage.refresh = Some(value);
                    }
                }
                Ok(_) => {
                    self.usage.price_error = Some("Unexpected price refresh status response".into())
                }
                Err(e) => self.usage.price_error = Some(e.describe()),
            },
            (s, r) => return Some((s, r)),
        }
        None
    }
    fn usage_screen(&mut self, ui: &mut Ui) {
        self.usage.tab = self.routes.entry(Route::Usage).or_default().tab;
        let projects = self
            .projects
            .list
            .items
            .iter()
            .map(|p| (p.id.0.as_str().into(), p.name.0.clone()))
            .collect::<Vec<_>>();
        let mut actions = Vec::new();
        self.usage.ui(ui, self.cx.online(), &projects, &mut actions);
        self.routes.entry(Route::Usage).or_default().tab = self.usage.tab;
        for action in actions {
            match action {
                Action::Apply => self.load_usage(true),
                Action::More => self.more_usage(),
                Action::LoadPrices => self.load_prices(false),
                Action::MorePrices => self.load_prices(true),
                Action::RefreshPrices => self.refresh_prices(),
                Action::EditPrice(model) => self.edit_price(model),
                Action::SavePrice => self.save_price(false),
                Action::ClearPrice => self.save_price(true),
                Action::RebasePrice => {
                    if let (Some(draft), Some(prices)) =
                        (&mut self.usage.price_draft, self.usage.prices.items.first())
                        && let Some(current) = prices
                            .models
                            .0
                            .iter()
                            .find(|m| m.model_id.as_str() == draft.model)
                    {
                        draft.revision = current.revision.as_str().into();
                    }
                }
                Action::OpenWork(id) => self.open_work(&id),
                Action::OpenSession(id) => self.open_session(id),
            }
        }
        self.poll_price_refresh(ui.ctx());
    }
}

/// Joins a further page of the price catalog onto the view shown: the page must come from the
/// same catalog revision, and the lists the view carries are merged without repeats.
fn join_prices(
    previous: &mut PriceCatalogView,
    mut result: PriceCatalogView,
) -> Result<(), String> {
    if previous.catalog_revision.as_str() != result.catalog_revision.as_str() {
        return Err(
            "The price catalog changed while paging. Reload the catalog; your draft is preserved."
                .into(),
        );
    }
    previous.items.0.append(&mut result.items.0);
    if let Some(mut history) = result.override_history.take() {
        if let Some(previous_history) = &mut previous.override_history {
            previous_history.0.append(&mut history.0);
        } else {
            previous.override_history = Some(history);
        }
    }
    previous.models.0.append(&mut result.models.0);
    previous
        .models
        .0
        .sort_by(|a, b| a.model_id.as_str().cmp(b.model_id.as_str()));
    previous
        .models
        .0
        .dedup_by(|a, b| a.model_id.as_str() == b.model_id.as_str());
    previous.snapshots.0.append(&mut result.snapshots.0);
    previous
        .snapshots
        .0
        .sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
    previous
        .snapshots
        .0
        .dedup_by(|a, b| a.id.as_str() == b.id.as_str());
    previous.providers.0.append(&mut result.providers.0);
    previous
        .providers
        .0
        .sort_by(|a, b| a.provider_id.as_str().cmp(b.provider_id.as_str()));
    previous
        .providers
        .0
        .dedup_by(|a, b| a.provider_id.as_str() == b.provider_id.as_str());
    previous.next_cursor = result.next_cursor;
    previous.has_more = result.has_more;
    previous.coverage = result.coverage;
    Ok(())
}
