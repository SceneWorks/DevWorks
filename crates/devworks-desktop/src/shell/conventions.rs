//! The native manager submits only the convention owner's scoped commands and reads.
use super::*;
use crate::{
    conventions_ops as cop,
    conventions_screen::{self as cv, Scope, Target},
};
use devworks_core::protocol::records::*;

impl Shell {
    fn convention_scope(&self, key: &str) -> Option<Scope> {
        self.conventions
            .states
            .keys()
            .find(|scope| scope.key() == key)
            .cloned()
    }
    fn load_conventions(&mut self, append: bool, search: bool) {
        let scope = self.conventions.scope.clone();
        let key = scope.key();
        let (generation, body, operation) = {
            let state = self.conventions.states.entry(scope.clone()).or_default();
            if state.listing || (append && state.next.is_none()) {
                return;
            }
            if search && state.search_recovery.is_some() {
                state.notice = Some((
                    Tone::Attention,
                    "Confirm the previous search outcome before starting a new search.".into(),
                ));
                return;
            }
            state.list_generation = state.list_generation.wrapping_add(1);
            state.listing = true;
            let lifecycle = match state.lifecycle {
                0 => vec!["Active"],
                1 => vec!["Retired"],
                2 => vec!["Active", "Retired"],
                _ => vec!["Deleted"],
            };
            let mut body = serde_json::json!({"scope":scope.wire(),"page":{"limit":cop::PAGE},"applicability":state.applicability,"lifecycles":lifecycle});
            if append {
                let Some(snapshot) = &state.query_snapshot else {
                    state.listing = false;
                    return;
                };
                body = snapshot.clone();
                body["page"]["cursor"] = serde_json::json!(state.next);
            }
            let operation = if search {
                body.as_object_mut().expect("query").remove("lifecycles");
                if !append {
                    body["text"] = serde_json::json!(state.text);
                    if !state.top_k.is_empty() {
                        body["topK"] = serde_json::json!(state.top_k);
                    }
                }
                "conventions.search"
            } else {
                if !append {
                    body["order"] = serde_json::json!(if state.order == 0 {
                        "Title"
                    } else {
                        "Organization"
                    });
                }
                "conventions.list"
            };
            if !append {
                state.pending_listing = Some((state.list_generation, body.clone()));
            }
            (state.list_generation, body, operation)
        };
        let submitted = if search {
            if let Some(b) = self.backend() {
                let frame = cop::query(&*b, operation, body);
                self.dispatch_convention_search(scope.clone(), generation, append, frame)
            } else {
                false
            }
        } else {
            self.query(
                Slot::ConventionsList(key, generation, append, false),
                "Reading conventions",
                operation,
                move |b| cop::query(b, operation, body),
            )
        };
        if !submitted {
            let state = self.conventions.states.get_mut(&scope).expect("scope");
            state.listing = false;
            state.list_error = Some(
                "Conventions cannot be read while offline or closing. Drafts are kept.".into(),
            );
        }
    }
    fn read_convention(&mut self, target: Target, remember: bool) {
        let scope = self.conventions.scope.clone();
        let state = self.conventions.states.entry(scope).or_default();
        if remember && let Some(previous) = state.reader.target.take() {
            state.history.push(previous);
        }
        state.reader.target = Some(target);
        state.reader.cursors = vec![None];
        state.reader.view = None;
        state.reader.error = None;
        state.detail_tab = 0;
        state.versions.clear();
        state.version_next = None;
        state.version_generation = state.version_generation.wrapping_add(1);
        state.version_loading = false;
        state.resources.clear();
        state.resource_next = None;
        state.resource_generation = state.resource_generation.wrapping_add(1);
        state.resource_loading = false;
        self.load_convention_passage();
    }
    fn load_convention_passage(&mut self) {
        let scope = self.conventions.scope.clone();
        let key = scope.key();
        let (generation, body) = {
            let state = self.conventions.states.entry(scope.clone()).or_default();
            let Some(target) = &state.reader.target else {
                return;
            };
            state.reader.generation = state.reader.generation.wrapping_add(1);
            state.reader.loading = true;
            state.reader.error = None;
            let mut body = serde_json::json!({"scope":scope.wire(),"format":(["Markdown","Raw","Manifest","SectionPath"][target.format]),"maxChars":cop::PASSAGE_CHARS.to_string()});
            if let Some(path) = &target.path {
                body["path"] = serde_json::json!(path);
            } else {
                body[if target.by_slug {
                    "slug"
                } else {
                    "conventionId"
                }] = serde_json::json!(target.id);
            }
            if let Some(revision) = target.revision {
                body["revision"] = serde_json::json!(revision.to_string());
            }
            if let Some(cursor) = state.reader.cursors.last().cloned().flatten() {
                body["cursor"] = serde_json::json!(cursor);
            }
            if let Some(section) = &target.section {
                body["sectionPath"] = serde_json::json!(section);
            }
            (state.reader.generation, body)
        };
        if !self.query(
            Slot::ConventionRead(key, generation),
            "Reading convention source",
            "conventions.read",
            move |b| cop::query(b, "conventions.read", body),
        ) {
            let state = self.conventions.states.get_mut(&scope).expect("scope");
            state.reader.loading = false;
            state.reader.error =
                Some("Convention source is unavailable while offline or closing.".into());
        }
    }
    fn load_convention_versions(&mut self, append: bool) {
        let scope = self.conventions.scope.clone();
        let key = scope.key();
        let (generation, body) = {
            let s = self.conventions.states.entry(scope.clone()).or_default();
            let Some(record) = s.record().cloned() else {
                return;
            };
            if s.version_loading || (append && s.version_next.is_none()) {
                return;
            }
            s.version_loading = true;
            s.version_generation = s.version_generation.wrapping_add(1);
            let mut body = serde_json::json!({"scope":scope.wire(),"conventionId":record.id,"page":{"limit":cop::PAGE}});
            if append {
                body["page"]["cursor"] = serde_json::json!(s.version_next);
            }
            (s.version_generation, body)
        };
        if !self.query(
            Slot::ConventionVersions(key, generation, append),
            "Reading convention versions",
            "conventions.version",
            move |b| cop::query(b, "conventions.version", body),
        ) {
            self.conventions
                .states
                .get_mut(&scope)
                .expect("scope")
                .version_loading = false;
        }
    }
    fn load_convention_resources(&mut self, append: bool) {
        let scope = self.conventions.scope.clone();
        let key = scope.key();
        let (generation, body) = {
            let s = self.conventions.states.entry(scope.clone()).or_default();
            let record = s.record().cloned();
            if s.resource_loading
                || (append && s.resource_query_snapshot.is_none())
                || (append && s.resource_mode != 2 && s.resource_next.is_none())
                || (s.resource_mode == 0 && record.is_none())
            {
                return;
            }
            if !append {
                s.resource_next = None;
            }
            if !append && s.resource_mode == 2 {
                s.resource_document_cursors = vec![None];
                s.resource_document = None;
            }
            s.resource_loading = true;
            s.resource_generation = s.resource_generation.wrapping_add(1);
            let mut body = serde_json::json!({"scope":scope.wire(),"page":{"limit":cop::PAGE},"applicability":if s.resource_mode==1 {s.applicability.clone()}else{Vec::new()}});
            body["selection"] = match s.resource_mode {
                1 => serde_json::json!({"kind":"Catalog"}),
                2 => {
                    let mut selection = serde_json::json!({"kind":"Document","rootSlug":s.resource_root_slug,"maxChars":cop::PASSAGE_CHARS.to_string()});
                    if append
                        && let Some(cursor) = s.resource_document_cursors.last().cloned().flatten()
                    {
                        selection["cursor"] = serde_json::json!(cursor);
                    }
                    selection
                }
                _ => {
                    let record = record.expect("attachment owner");
                    serde_json::json!({"kind":"Attachments","conventionId":record.id,"conventionRevision":record.revision,"lifecycles":["Active","Retired","Deleted"]})
                }
            };
            if append {
                body = s.resource_query_snapshot.clone().expect("captured query");
                if s.resource_mode == 2 {
                    if let Some(cursor) = s.resource_document_cursors.last().cloned().flatten() {
                        body["selection"]["cursor"] = serde_json::json!(cursor);
                    } else {
                        body["selection"]
                            .as_object_mut()
                            .expect("selection")
                            .remove("cursor");
                    }
                } else {
                    body["page"]["cursor"] = serde_json::json!(s.resource_next);
                }
            } else {
                s.resource_query_snapshot = Some(body.clone());
            }
            (s.resource_generation, body)
        };
        if !self.query(
            Slot::ConventionResources(key, generation, append),
            "Reading convention resources",
            "conventions.resources",
            move |b| cop::query(b, "conventions.resources", body),
        ) {
            self.conventions
                .states
                .get_mut(&scope)
                .expect("scope")
                .resource_loading = false;
        }
    }
    fn load_convention_parents(&mut self, append: bool) {
        let source = self.conventions.scope.clone();
        let key = source.key();
        let (target, generation, body) = {
            let s = self.conventions.states.entry(source).or_default();
            let Some(record) = s.record().cloned() else {
                return;
            };
            let Some(d) = s.move_drafts.get_mut(record.id.as_str()) else {
                return;
            };
            if append && d.next.is_none() {
                return;
            }
            d.generation = d.generation.wrapping_add(1);
            let mut body = serde_json::json!({"scope":d.scope.wire(),"page":{"limit":cop::PAGE},"applicability":[],"lifecycles":["Active","Retired"]});
            if append {
                body["page"]["cursor"] = serde_json::json!(d.next);
            }
            (record.id.as_str().to_owned(), d.generation, body)
        };
        self.query(
            Slot::ConventionParents(key, target, generation, append),
            "Reading destination conventions",
            "conventions.list",
            move |b| cop::query(b, "conventions.list", body),
        );
    }
    pub(super) fn apply_conventions(&mut self, slot: Slot, reply: Reply) -> Option<(Slot, Reply)> {
        let mut refresh_index_metadata = false;
        match (slot, reply) {
            (Slot::ConventionsList(key, generation, append, search), Reply::Query(outcome)) => {
                let scope = self.convention_scope(&key)?;
                let s = self.conventions.states.get_mut(&scope).expect("scope");
                let uncertain = search && cop::search_uncertain(&outcome);
                let result = if search {
                    cop::search(outcome).map(|p| {
                        (
                            p.scope_revision.get(),
                            p.next_cursor.map(|c| c.0),
                            p.items.0,
                            Vec::new(),
                            Some(p.retrieval),
                        )
                    })
                } else {
                    cop::page(outcome).map(|p| {
                        (
                            p.scope_revision.get(),
                            p.next_cursor.map(|c| c.0),
                            Vec::new(),
                            p.items.0,
                            None,
                        )
                    })
                };
                if search {
                    if s.active_search
                        .as_ref()
                        .is_some_and(|(g, _)| *g == generation)
                    {
                        s.active_search = None;
                    }
                    if s.search_recovery
                        .as_ref()
                        .is_some_and(|r| r.generation == generation)
                    {
                        let custody = result
                            .as_ref()
                            .ok()
                            .and_then(|(_, _, _, _, retrieval)| retrieval.as_ref())
                            .and_then(|r| {
                                r.embedding_request_id
                                    .as_ref()
                                    .zip(r.embedding_operation.as_ref())
                            })
                            .map(|(id, op)| (id.as_str().to_owned(), op.clone()));
                        let prior_unsettled = s
                            .search_preparation
                            .as_ref()
                            .is_some_and(|(_, op)| cop::preparation_unsettled(op));
                        let matching = s
                            .search_preparation
                            .as_ref()
                            .zip(custody.as_ref())
                            .is_some_and(|((_, old), (_, new))| {
                                old.operation_id == new.operation_id
                            });
                        if (!prior_unsettled || matching) && result.is_ok() {
                            s.search_preparation = custody;
                        }
                        let unsettled = s
                            .search_preparation
                            .as_ref()
                            .is_some_and(|(_, op)| cop::preparation_unsettled(op));
                        if result.is_ok() {
                            s.last_search = s.search_recovery.clone();
                        }
                        if !uncertain && !unsettled {
                            s.search_recovery = None;
                        }
                    }
                }
                if s.list_generation != generation {
                    return None;
                }
                s.listing = false;
                match result {
                    Ok((revision, next, hits, items, retrieval)) => {
                        if append && s.scope_revision.is_some_and(|old| old != revision) {
                            s.list_error=Some("The convention listing changed while paging. Refresh for a current listing; drafts are kept.".into());
                            return None;
                        }
                        if s.pending_listing
                            .as_ref()
                            .is_some_and(|(g, _)| *g == generation)
                        {
                            s.query_snapshot = s.pending_listing.take().map(|(_, body)| body);
                        }
                        if !append {
                            s.items.clear();
                            s.hits.clear();
                        }
                        s.items.extend(items);
                        s.hits.extend(hits);
                        s.retrieval = retrieval;
                        s.next = next;
                        s.scope_revision = Some(revision);
                        s.searched = search;
                        s.loaded = true;
                        s.available = true;
                        s.list_error = None;
                    }
                    Err(error) => {
                        if s.pending_listing
                            .as_ref()
                            .is_some_and(|(g, _)| *g == generation)
                        {
                            s.pending_listing = None;
                        }
                        s.list_error = Some(error.describe());
                        if matches!(error, Failure::NoUseCase(_)) {
                            s.available = false;
                        }
                    }
                }
            }
            (Slot::ConventionsList(key, generation, _, true), Reply::Task(Err(error))) => {
                let scope = self.convention_scope(&key)?;
                let state = self.conventions.states.get_mut(&scope).expect("scope");
                if state
                    .active_search
                    .as_ref()
                    .is_some_and(|(g, _)| *g == generation)
                {
                    state.active_search = None;
                }
                if state.list_generation == generation {
                    state.listing = false;
                    state.list_error = Some(format!(
                        "The search result is not confirmed: {error}. Confirm the original search before retrying."
                    ));
                }
            }
            (Slot::ConventionRead(key, generation), Reply::Query(outcome)) => {
                let scope = self.convention_scope(&key)?;
                let s = self.conventions.states.get_mut(&scope).expect("scope");
                if s.reader.generation != generation {
                    return None;
                }
                s.reader.loading = false;
                match cop::document(outcome) {
                    Ok(view) => {
                        let target = s.reader.target.as_mut()?;
                        if (!target.by_slug
                            && target.path.is_none()
                            && view.convention.id.as_str() != target.id)
                            || target
                                .revision
                                .is_some_and(|revision| revision != view.convention.revision.get())
                        {
                            s.reader.error=Some("The source revision changed while reading. Refresh explicitly before combining source passages.".into());
                            return None;
                        }
                        target.id = view.convention.id.as_str().into();
                        target.by_slug = false;
                        target.path = None;
                        target.revision = Some(view.convention.revision.get());
                        s.reader.view = Some(view);
                        s.reader.error = None;
                    }
                    Err(error) => s.reader.error = Some(error.describe()),
                }
            }
            (Slot::ConventionVersions(key, generation, append), Reply::Query(outcome)) => {
                let scope = self.convention_scope(&key)?;
                let s = self.conventions.states.get_mut(&scope).expect("scope");
                if generation != s.version_generation {
                    return None;
                }
                s.version_loading = false;
                match cop::page(outcome) {
                    Ok(page) => {
                        if append
                            && s.version_scope_revision
                                .is_some_and(|old| old != page.scope_revision.get())
                        {
                            s.notice=Some((Tone::Attention,"The version listing changed. Load versions again without discarding your draft.".into()));
                            return None;
                        }
                        if !append {
                            s.versions.clear();
                        }
                        s.versions.extend(page.items.0);
                        s.version_next = page.next_cursor.map(|c| c.0);
                        s.version_scope_revision = Some(page.scope_revision.get());
                    }
                    Err(error) => s.notice = Some((Tone::Attention, error.describe())),
                }
            }
            (Slot::ConventionResources(key, generation, append), Reply::Query(outcome)) => {
                let scope = self.convention_scope(&key)?;
                let s = self.conventions.states.get_mut(&scope).expect("scope");
                if generation != s.resource_generation {
                    return None;
                }
                s.resource_loading = false;
                match cop::resources(outcome) {
                    Ok(view) => {
                        let (revision, next) = match view {
                            ConventionResourcesView::Attachments { page } => {
                                if append
                                    && s.resource_scope_revision
                                        .is_some_and(|old| old != page.scope_revision.get())
                                {
                                    s.notice=Some((Tone::Attention,"The resource listing changed. Load it again; drafts are kept.".into()));
                                    return None;
                                }
                                if !append {
                                    s.resources.clear();
                                }
                                s.resources.extend(page.items.0);
                                (
                                    Some(page.scope_revision.get()),
                                    page.next_cursor.map(|c| c.0),
                                )
                            }
                            ConventionResourcesView::Catalog { page } => {
                                if append
                                    && s.resource_scope_revision
                                        .is_some_and(|old| old != page.scope_revision.get())
                                {
                                    s.notice=Some((Tone::Attention,"The document catalog changed. Load it again; drafts are kept.".into()));
                                    return None;
                                }
                                if !append {
                                    s.resource_catalog.clear();
                                }
                                s.resource_catalog.extend(page.items.0);
                                (
                                    Some(page.scope_revision.get()),
                                    page.next_cursor.map(|c| c.0),
                                )
                            }
                            ConventionResourcesView::Document { view } => {
                                s.resource_document = Some(view);
                                (None, None)
                            }
                        };
                        s.resource_next = next;
                        s.resource_scope_revision = revision;
                    }
                    Err(error) => s.notice = Some((Tone::Attention, error.describe())),
                }
            }
            (Slot::ConventionParents(key, id, generation, append), Reply::Query(outcome)) => {
                let scope = self.convention_scope(&key)?;
                let s = self.conventions.states.get_mut(&scope).expect("scope");
                let d = s.move_drafts.get_mut(&id)?;
                if generation != d.generation {
                    return None;
                }
                match cop::page(outcome) {
                    Ok(page) => {
                        if append
                            && d.scope_revision
                                .is_some_and(|old| old != page.scope_revision.get())
                        {
                            s.notice=Some((Tone::Attention,"The destination listing changed. Load parents again; this move draft is kept.".into()));
                            return None;
                        }
                        d.scope_revision = Some(page.scope_revision.get());
                        if !append {
                            d.choices.clear();
                        }
                        d.choices.extend(page.items.0);
                        d.next = page.next_cursor.map(|c| c.0);
                    }
                    Err(error) => s.notice = Some((Tone::Attention, error.describe())),
                }
            }
            (Slot::ConventionEditor(key, id, identity), Reply::Task(result)) => {
                let scope = self.convention_scope(&key)?;
                let s = self.conventions.states.get_mut(&scope).expect("scope");
                let draft = s.drafts.get_mut(&id).filter(|d| d.identity == identity)?;
                draft.loading = false;
                match result {
                    Ok(TaskOutput::Convention(cop::TaskResult::Editor {
                        id: loaded,
                        identity: token,
                        markdown,
                        raw,
                    })) if loaded == id && token == identity => {
                        draft.markdown = markdown;
                        draft.raw = raw;
                        draft.body_complete = true;
                        draft.error = None;
                    }
                    Err(error) => draft.error = Some(error),
                    _ => {
                        draft.error =
                            Some("The complete source reply did not match this draft.".into())
                    }
                }
            }
            (Slot::ConventionOperation(key, id), Reply::Query(outcome)) => {
                let scope = self.convention_scope(&key)?;
                let s = self.conventions.states.get_mut(&scope).expect("scope");
                match cop::operation(outcome) {
                    Ok(status) if status.operation.operation_id.0.as_str() == id => {
                        let text = format!(
                            "{:?} · {:?} · {} of {} items{}{}",
                            status.kind,
                            status.phase,
                            status.processed_items.get(),
                            status
                                .total_items
                                .as_ref()
                                .map_or_else(|| "unknown".into(), |v| v.get().to_string()),
                            if status.cancel_requested {
                                " · cancellation requested"
                            } else {
                                ""
                            },
                            status
                                .reason
                                .as_ref()
                                .map_or_else(String::new, |r| format!(" · {}", r.0))
                        );
                        s.operation_revisions
                            .insert(id.clone(), status.revision.get());
                        s.operation_text.insert(id.clone(), text);
                        if !s.operation_ids.contains(&id) {
                            s.operation_ids.push(id.clone());
                        }
                        if matches!(
                            status.phase,
                            ConventionOperationPhase::Completed
                                | ConventionOperationPhase::Failed
                                | ConventionOperationPhase::Cancelled
                        ) && let Some((identity, _)) = s.accepted_drafts.remove(&id)
                            && let Some(draft) = s.draft_by_identity_mut(identity)
                        {
                            draft.busy = false;
                            draft.error = Some(
                                if matches!(status.phase, ConventionOperationPhase::Completed) {
                                    "The import completed. Its published record is available in the refreshed listing; this draft is retained for comparison.".into()
                                } else {
                                    "The operation did not complete. This draft is retained.".into()
                                },
                            );
                        }
                        if matches!(status.kind, ConventionOperationKind::Index)
                            && matches!(
                                status.phase,
                                ConventionOperationPhase::Completed
                                    | ConventionOperationPhase::Failed
                                    | ConventionOperationPhase::Cancelled
                            )
                            && self.conventions.scope == scope
                            && s.reader.target.as_ref().is_some_and(|target| {
                                target.revision.is_some()
                                    && status
                                        .convention_id
                                        .as_ref()
                                        .is_none_or(|id| target.id == id.as_str())
                            })
                            && !s.reader.loading
                        {
                            refresh_index_metadata = true;
                        }
                        s.operation_statuses.insert(id, status);
                    }
                    Ok(_) => {
                        s.notice = Some((
                            Tone::Attention,
                            "The operation reply did not match the requested operation.".into(),
                        ));
                    }
                    Err(error) => {
                        s.operation_statuses.remove(&id);
                        s.operation_revisions.remove(&id);
                        s.operation_text.insert(id, error.describe());
                    }
                }
            }
            (Slot::ConventionPrepare(key, token), Reply::Task(result)) => match result {
                Ok(TaskOutput::Convention(cop::TaskResult::Pipeline(p))) => {
                    self.begin_convention_pipeline(token, p)
                }
                Ok(TaskOutput::Convention(cop::TaskResult::Prepared(frame))) => {
                    self.command(
                        Slot::ConventionArtifact(key.clone(), token.clone()),
                        "Storing convention source",
                        "artifacts.save",
                        move |_| frame,
                    );
                }
                Err(error) => self.fail_convention_pipeline(&key, &token, error),
                _ => self.fail_convention_pipeline(
                    &key,
                    &token,
                    "The prepared source did not match this transfer.".into(),
                ),
            },
            (Slot::ConventionArtifact(key, token), Reply::Command(outcome)) => {
                if is_unknown(&outcome) {
                    self.fail_convention_pipeline(&key,&token,"The source save outcome is unknown. Confirm outcomes reuses the exact original save request; the draft is kept.".into());
                    if let Some(scope) = self.convention_scope(&key) {
                        let identity = self
                            .conventions
                            .pipelines
                            .get(&token)
                            .and_then(|p| p.submission.draft)
                            .map(|(id, _)| id);
                        let s = self.conventions.states.get_mut(&scope).expect("scope");
                        s.busy = true;
                        if let Some(id) = identity
                            && let Some(d) = s.draft_by_identity_mut(id)
                        {
                            d.busy = true;
                        }
                    }
                    return None;
                }
                match ops::saved_artifact(outcome) {
                    Ok(record) => {
                        if let Some(p) = self.conventions.pipelines.get_mut(&token)
                            && let Err(error) =
                                crate::conventions_transfer::apply_artifact(p, record.artifact)
                        {
                            self.fail_convention_pipeline(&key, &token, error);
                            return None;
                        }
                        self.advance_convention_pipeline(token);
                    }
                    Err(error) => self.fail_convention_pipeline(&key, &token, error.describe()),
                }
            }
            (Slot::ConventionTransfer(key, _), Reply::Task(result)) => {
                if let Some(scope) = self.convention_scope(&key) {
                    let state = self.conventions.states.get_mut(&scope).expect("scope");
                    state.notice = Some(match result {
                        Ok(TaskOutput::Convention(cop::TaskResult::Transferred(text))) => {
                            (Tone::Good, text)
                        }
                        Err(error) => (Tone::Attention, error),
                        _ => (
                            Tone::Attention,
                            "The transfer reply did not match the requested transfer.".into(),
                        ),
                    });
                }
            }
            (Slot::ConventionCommand(key, token), Reply::Command(outcome)) => {
                self.finish_convention_change(&key, &token, outcome)
            }
            (slot, reply) => return Some((slot, reply)),
        }
        if refresh_index_metadata {
            self.load_convention_passage();
        }
        None
    }
}

impl Shell {
    fn submit_convention(
        &mut self,
        operation: &'static str,
        body: serde_json::Value,
        target: Option<String>,
        draft: Option<(u64, u64)>,
    ) {
        let scope = self.conventions.scope.clone();
        let key = scope.key();
        let Some(backend) = self.backend() else {
            return;
        };
        let token = backend.token("convention-submit");
        let state = self.conventions.states.entry(scope.clone()).or_default();
        if state.busy {
            return;
        }
        let submission = cop::Submission {
            scope: scope.clone(),
            operation: operation.into(),
            target,
            draft,
            reader_generation: state.reader.generation,
            resource: None,
        };
        if self.command(
            Slot::ConventionCommand(key, token.clone()),
            "Saving convention change",
            operation,
            move |b| cop::command(b, operation, body),
        ) {
            let state = self.conventions.states.get_mut(&scope).expect("scope");
            state.busy = true;
            state.notice = None;
            if let Some((identity, _)) = submission.draft
                && let Some(d) = state.draft_by_identity_mut(identity)
            {
                d.busy = true;
            }
            self.conventions.submissions.insert(token, submission);
        }
    }
    fn change_convention_applicability(&mut self) {
        let scope = self.conventions.scope.clone();
        let Some(state) = self.conventions.states.get(&scope) else {
            return;
        };
        let Some(draft) = state.draft() else {
            return;
        };
        let Some(base) = draft.target() else {
            return;
        };
        let body = serde_json::json!({"scope":scope.wire(),"conventionId":base.id,"expectedRevision":base.revision,"applicability":draft.applicability,"lifecycle":if draft.retired{"Retired"}else{"Active"}});
        self.submit_convention(
            "conventions.applicability",
            body,
            Some(base.id.as_str().into()),
            Some((draft.identity, draft.generation)),
        );
    }
    fn retire_convention(&mut self) {
        let scope = self.conventions.scope.clone();
        let Some(record) = self.conventions.state().and_then(|s| s.record()).cloned() else {
            return;
        };
        let mut body = serde_json::json!({"scope":scope.wire(),"conventionId":record.id,"expectedRevision":record.revision});
        let operation = if matches!(record.lifecycle, ConventionLifecycle::Retired) {
            body["applicability"] = serde_json::json!(record.applicability);
            body["lifecycle"] = serde_json::json!("Active");
            "conventions.applicability"
        } else {
            "conventions.retire"
        };
        self.submit_convention(operation, body, Some(record.id.as_str().into()), None);
    }
    fn delete_convention_target(&mut self) {
        let scope = self.conventions.scope.clone();
        let Some(delete) = self
            .conventions
            .state()
            .and_then(|s| s.confirm_delete.clone())
        else {
            return;
        };
        let (operation, body, target) = match delete {
            cv::Delete::Convention(record) => (
                "conventions.delete",
                serde_json::json!({"scope":scope.wire(),"conventionId":record.id,"expectedRevision":record.revision}),
                record.id.as_str().to_owned(),
            ),
            cv::Delete::Resource(resource) => (
                "conventions.delete_resource",
                serde_json::json!({"scope":scope.wire(),"conventionId":resource.convention_id,"expectedRevision":resource.convention_revision,"resourceId":resource.id}),
                resource.convention_id.as_str().to_owned(),
            ),
        };
        self.submit_convention(operation, body, Some(target), None);
    }
    fn move_convention(&mut self) {
        let scope = self.conventions.scope.clone();
        let Some(state) = self.conventions.states.get(&scope) else {
            return;
        };
        let Some(record) = state.record() else {
            return;
        };
        let Some(draft) = state.move_drafts.get(record.id.as_str()) else {
            return;
        };
        let Ok(order) = draft.order.parse::<i64>() else {
            self.conventions.state_mut().notice = Some((
                Tone::Attention,
                "Enter a signed whole number for sort order.".into(),
            ));
            return;
        };
        let parent = draft.parent.as_ref().map_or(
            serde_json::json!({"kind":"Root"}),
            |id| serde_json::json!({"kind":"Child","parentId":id}),
        );
        let base = draft.anchor.as_ref().unwrap_or(&draft.base);
        let body = serde_json::json!({"scope":scope.wire(),"conventionId":base.id,"expectedRevision":base.revision,"destination":draft.scope.wire(),"parent":parent,"sortOrder":order.to_string()});
        self.submit_convention(
            "conventions.move",
            body,
            Some(draft.base.id.as_str().into()),
            None,
        );
    }
    fn index_convention(&mut self, all: bool) {
        let scope = self.conventions.scope.clone();
        let Some(state) = self.conventions.state() else {
            return;
        };
        let (body, target) = if all {
            let Some(revision) = state.scope_revision else {
                return;
            };
            (
                serde_json::json!({"allScope":scope.wire(),"expectedRevision":revision.to_string()}),
                None,
            )
        } else {
            let Some(record) = state.record() else {
                return;
            };
            (
                serde_json::json!({"scope":scope.wire(),"conventionId":record.id,"expectedRevision":record.revision}),
                Some(record.id.as_str().into()),
            )
        };
        self.submit_convention("conventions.index", body, target, None);
    }
    fn finish_convention_change(
        &mut self,
        key: &str,
        token: &str,
        outcome: devworks_core::gateway::CommandOutcome,
    ) {
        let Some(scope) = self.convention_scope(key) else {
            return;
        };
        let result = cop::changed(outcome);
        let unknown = matches!(&result, Err(Failure::Unknown));
        if !unknown && let Some(p) = self.conventions.pipelines.remove(token) {
            for stage in p.files {
                if stage.owned
                    && let Some(b) = self.backend()
                {
                    crate::attachments::remove_owned(&stage.path, &b.scratch_dir());
                }
            }
        }
        let submission = if unknown {
            None
        } else {
            self.conventions.submissions.remove(token)
        };
        let state = self.conventions.states.get_mut(&scope).expect("scope");
        state.busy = unknown;
        if let Some(context) = &submission
            && let Some((identity, _)) = context.draft
            && let Some(draft) = state.draft_by_identity_mut(identity)
        {
            draft.busy = false;
        }
        match result {
            Ok(cop::Change::Accepted(operation)) => {
                if !state.operation_ids.contains(&operation) {
                    state.operation_ids.push(operation.clone());
                }
                if let Some(context) = &submission
                    && let Some((id, generation)) = context.draft
                {
                    state
                        .accepted_drafts
                        .insert(operation.clone(), (id, generation));
                    if let Some(d) = state.draft_by_identity_mut(id) {
                        d.busy = true;
                    }
                }
                state.operation_text.insert(
                    operation,
                    "Accepted; current operation status has not been read yet.".into(),
                );
                state.notice=Some((Tone::Good,"The convention operation was accepted. Its owning operation determines completion.".into()));
            }
            Ok(change) => {
                let record = match change {
                    cop::Change::Record(record) => Some(record),
                    cop::Change::Mutation(mutation) => mutation.convention,
                    cop::Change::Earlier => None,
                    cop::Change::Accepted(_) => unreachable!(),
                };
                if let (Some(record), Some(context)) = (&record, &submission)
                    && let Some((identity, generation)) = context.draft
                {
                    let same_scope = cv::Scope::of(&record.scope) == scope;
                    if same_scope
                        && let Some(draft) = state.drafts.get_mut(record.id.as_str())
                        && draft.identity == identity
                    {
                        let unchanged = draft.generation == generation;
                        let labels_match = draft
                            .applicability
                            .iter()
                            .map(String::as_str)
                            .eq(record.applicability.0.iter().map(|s| s.0.as_str()));
                        let lifecycle_match = draft.retired
                            == matches!(record.lifecycle, ConventionLifecycle::Retired);
                        let content_saved = context.operation == "conventions.edit"
                            || (!draft.markdown_changed
                                && !draft.raw_changed
                                && draft.title == record.title.0
                                && draft.slug == record.slug.0
                                && draft.raw_mode != 2);
                        draft.anchor = Some(record.clone());
                        if unchanged && labels_match && lifecycle_match && content_saved {
                            state.drafts.remove(record.id.as_str());
                            if matches!(&state.editor,Some(cv::Editor::Existing(id))if id==record.id.as_str())
                            {
                                state.editor = None;
                            }
                        }
                    }
                }
                if let (Some(record), Some(context)) = (&record, &submission)
                    && let Some((upload, id, generation)) = &context.resource
                {
                    if *upload {
                        if state.upload.as_ref().is_some_and(|d| {
                            d.base.id.as_str() == id && d.generation == *generation
                        }) {
                            state.upload = None;
                        }
                    } else if state
                        .resource_drafts
                        .get(id)
                        .is_some_and(|d| d.generation == *generation)
                    {
                        state.resource_drafts.remove(id);
                        if state.resource_editor.as_ref() == Some(id) {
                            state.resource_editor = None;
                        }
                    } else if let Some(d) = state.resource_drafts.get_mut(id) {
                        d.base = Some(record.clone());
                    }
                }
                let refresh_target = record
                    .as_ref()
                    .filter(|r| cv::Scope::of(&r.scope) == scope)
                    .filter(|r| {
                        submission
                            .as_ref()
                            .is_some_and(|c| c.reader_generation == state.reader.generation)
                            && state
                                .reader
                                .target
                                .as_ref()
                                .is_some_and(|t| !t.by_slug && t.id == r.id.as_str())
                    })
                    .map(|r| Target {
                        id: r.id.as_str().into(),
                        by_slug: false,
                        path: None,
                        revision: Some(r.revision.get()),
                        format: 0,
                        section: None,
                    });
                state.confirm_delete = None;
                state.list_generation = state.list_generation.wrapping_add(1);
                state.listing = false;
                state.notice = Some((
                    Tone::Good,
                    record.as_ref().map_or_else(
                        || {
                            if submission.as_ref().is_some_and(|c|c.operation=="conventions.retire"&&c.target.is_none()) {"Cancellation request recorded. Inspect the operation for its final state.".into()} else {"Convention change confirmed. Refresh to inspect the current records.".into()}
                        },
                        |r| {
                            format!(
                                "{} saved at revision {}. Independent drafts are kept.",
                                r.title.0,
                                r.revision.get()
                            )
                        },
                    ),
                ));
                if self.conventions.scope == scope {
                    self.load_conventions(false, false);
                    if let Some(target) = refresh_target {
                        self.read_convention(target, false);
                    }
                }
            }
            Err(error) => {
                let message = error.describe();
                state.notice = Some((Tone::Attention, message.clone()));
                if let Some(context) = submission
                    && let Some((identity, _)) = context.draft
                    && let Some(draft) = state.draft_by_identity_mut(identity)
                {
                    draft.error = Some(message);
                }
            }
        }
    }
}

impl Shell {
    fn load_convention_editor(&mut self) {
        let scope = self.conventions.scope.clone();
        let key = scope.key();
        let (record, identity) = {
            let state = self.conventions.states.entry(scope.clone()).or_default();
            let Some(draft) = state.draft_mut() else {
                return;
            };
            let Some(record) = draft.base.clone() else {
                return;
            };
            if draft.body_complete {
                return;
            }
            draft.loading = true;
            draft.error = None;
            (record, draft.identity)
        };
        let slot = Slot::ConventionEditor(key, record.id.as_str().into(), identity);
        if self.pending(&slot) {
            return;
        }
        if let Some(client) = &mut self.client {
            let selected_scope = scope.clone();
            client.task(slot, "Loading complete convention source", move |b| {
                cop::load_editor(b, &selected_scope, &record, identity).map(TaskOutput::Convention)
            });
        } else if let Some(draft) = self
            .conventions
            .states
            .get_mut(&scope)
            .and_then(|s| s.draft_by_identity_mut(identity))
        {
            draft.loading = false;
            draft.error = Some(
                "The complete source cannot be loaded while offline. The opening revision is kept."
                    .into(),
            );
        }
    }
    /// Returns false for actions handled by the artifact/operation mutation worker.
    fn convention_read_action(&mut self, action: &cv::Action) -> bool {
        match action {
            cv::Action::Knowledge(tab) => {
                self.routes.entry(Route::Knowledge).or_default().tab = *tab;
                self.knowledge.tab = *tab;
            }
            cv::Action::Scope(scope) => {
                self.conventions.scope = scope.clone();
                self.conventions.generation = self.conventions.generation.wrapping_add(1);
                if !self
                    .conventions
                    .state()
                    .is_some_and(|s| s.loaded || s.listing)
                {
                    self.load_conventions(false, false);
                }
            }
            cv::Action::Refresh => {
                self.load_conventions(false, false);
                if let Some(mut target) = self
                    .conventions
                    .state()
                    .and_then(|s| s.reader.target.clone())
                {
                    target.revision = None;
                    target.section = None;
                    if target.format == 3 {
                        target.format = 0;
                    }
                    self.read_convention(target, false);
                }
            }
            cv::Action::List(append) => {
                let search = *append && self.conventions.state().is_some_and(|s| s.searched);
                self.load_conventions(*append, search);
            }
            cv::Action::Search => self.load_conventions(false, true),
            cv::Action::ConfirmSearch => self.confirm_convention_search(),
            cv::Action::NewSearch => {
                self.archive_convention_search();
                self.load_conventions(false, true);
            }
            cv::Action::ConfirmEarlierSearch(index) => {
                if self
                    .conventions
                    .state()
                    .is_none_or(|s| s.active_search.is_some())
                {
                    return true;
                }
                let state = self.conventions.state_mut();
                if *index >= state.previous_searches.len() {
                    return true;
                }
                let context = state.previous_searches.remove(*index);
                self.archive_convention_search();
                let state = self.conventions.state_mut();
                state.search_recovery = Some(context.request);
                state.search_preparation = context.preparation;
                self.confirm_convention_search();
            }
            cv::Action::CancelSearch => {
                if let Some((_, token)) = self
                    .conventions
                    .state()
                    .and_then(|s| s.active_search.as_ref())
                {
                    token.cancel();
                }
            }
            cv::Action::Read(target) => self.read_convention(target.clone(), true),
            cv::Action::OpenPath => {
                if let Some(path) = self.conventions.state().map(|s| s.path_input.clone()) {
                    self.read_convention(
                        Target {
                            id: String::new(),
                            by_slug: false,
                            path: Some(path),
                            revision: None,
                            format: 0,
                            section: None,
                        },
                        true,
                    );
                }
            }
            cv::Action::OpenSlug => {
                if let Some(slug) = self.conventions.state().map(|s| s.slug_input.clone()) {
                    self.read_convention(
                        Target {
                            id: slug,
                            by_slug: true,
                            path: None,
                            revision: None,
                            format: 0,
                            section: None,
                        },
                        true,
                    );
                }
            }
            cv::Action::Page(next) => {
                let s = self.conventions.state_mut();
                if s.reader.loading {
                    return true;
                }
                if *next {
                    let Some(cursor) = s
                        .reader
                        .view
                        .as_ref()
                        .and_then(|v| v.passage.as_ref())
                        .and_then(|p| p.next_cursor.as_ref())
                        .map(|c| c.0.clone())
                    else {
                        return true;
                    };
                    s.reader.cursors.push(Some(cursor));
                } else if s.reader.cursors.len() > 1 {
                    s.reader.cursors.pop();
                } else {
                    return true;
                }
                self.load_convention_passage();
            }
            cv::Action::Back => {
                if let Some(target) = self.conventions.state_mut().history.pop() {
                    self.read_convention(target, false);
                }
            }
            cv::Action::New => {
                let s = self.conventions.state_mut();
                if s.new_draft.is_none() {
                    s.new_draft = Some(cv::Draft::default());
                }
                s.editor = Some(cv::Editor::New);
                s.detail_tab = 1;
            }
            cv::Action::Edit => {
                let s = self.conventions.state_mut();
                let Some(record) = s.record().cloned() else {
                    return true;
                };
                let id = record.id.as_str().to_owned();
                s.drafts
                    .entry(id.clone())
                    .or_insert_with(|| cv::Draft::of(&record));
                s.editor = Some(cv::Editor::Existing(id));
                s.detail_tab = 1;
                self.load_convention_editor();
            }
            cv::Action::LoadEditor => self.load_convention_editor(),
            cv::Action::Discard => {
                let s = self.conventions.state_mut();
                if s.draft().is_some_and(|d| d.busy) {
                    return true;
                }
                match s.editor.take() {
                    Some(cv::Editor::New) => s.new_draft = None,
                    Some(cv::Editor::Existing(id)) => {
                        s.drafts.remove(&id);
                    }
                    None => {}
                }
            }
            cv::Action::Rebase => {
                let s = self.conventions.state_mut();
                let Some(record) = s.record().cloned() else {
                    return true;
                };
                if let Some(draft) = s.draft_mut()
                    && !draft.busy
                    && draft.base.as_ref().is_some_and(|base| base.id == record.id)
                {
                    draft.anchor = Some(record);
                    draft.error = None;
                }
            }
            cv::Action::RebaseMove => {
                let s = self.conventions.state_mut();
                if let Some(record) = s.record().cloned()
                    && let Some(draft) = s.move_drafts.get_mut(record.id.as_str())
                {
                    draft.anchor = Some(record);
                }
            }
            cv::Action::Versions(append) => self.load_convention_versions(*append),
            cv::Action::Resources(append) => self.load_convention_resources(*append),
            cv::Action::MoveChoices(append) => self.load_convention_parents(*append),
            cv::Action::Applicability => self.change_convention_applicability(),
            cv::Action::Retire => self.retire_convention(),
            cv::Action::Delete(target) => {
                self.conventions.state_mut().confirm_delete = Some(target.clone())
            }
            cv::Action::ConfirmDelete => self.delete_convention_target(),
            cv::Action::Move => self.move_convention(),
            cv::Action::Index => self.index_convention(false),
            cv::Action::IndexAll => self.index_convention(true),
            cv::Action::ResumeTransfer(token) => {
                if !self.conventions.state().is_some_and(|s| s.busy) {
                    self.conventions.state_mut().busy = true;
                    self.advance_convention_pipeline(token.clone());
                }
            }
            cv::Action::InspectOperation(id) => self.inspect_convention_operation(id.clone()),
            cv::Action::CancelOperation(id) => {
                let scope = self.conventions.scope.clone();
                let revision = self
                    .conventions
                    .state()
                    .and_then(|s| s.operation_statuses.get(id))
                    .filter(|s| s.can_cancel && !s.cancel_requested)
                    .map(|s| s.revision.get());
                if let Some(revision) = revision {
                    self.submit_convention("conventions.retire",serde_json::json!({"scope":scope.wire(),"operationId":id,"expectedRevision":revision.to_string()}),None,None);
                }
            }
            cv::Action::ResourceDocumentPage(next) => {
                let s = self.conventions.state_mut();
                if *next {
                    if let Some(cursor) = s
                        .resource_document
                        .as_ref()
                        .and_then(|v| v.passage.as_ref())
                        .and_then(|p| p.next_cursor.as_ref())
                    {
                        s.resource_document_cursors.push(Some(cursor.0.clone()));
                    }
                } else if s.resource_document_cursors.len() > 1 {
                    s.resource_document_cursors.pop();
                }
                self.load_convention_resources(true);
            }
            _ => return false,
        }
        true
    }
}

impl Shell {
    fn inspect_convention_operation(&mut self, id: String) {
        let scope = self.conventions.scope.clone();
        let key = scope.key();
        self.query(
            Slot::ConventionOperation(key, id.clone()),
            "Reading convention operation",
            "conventions.read",
            move |b| {
                cop::query(
                    b,
                    "conventions.read",
                    serde_json::json!({"scope":scope.wire(),"operationId":id}),
                )
            },
        );
    }
}

impl Shell {
    fn convention_notice(&mut self, text: String) {
        self.conventions.state_mut().notice = Some((Tone::Attention, text));
    }
    fn convention_submission(
        &self,
        operation: &str,
        target: Option<String>,
        draft: Option<(u64, u64)>,
    ) -> cop::Submission {
        cop::Submission {
            scope: self.conventions.scope.clone(),
            operation: operation.into(),
            target,
            draft,
            reader_generation: self.conventions.state().map_or(0, |s| s.reader.generation),
            resource: None,
        }
    }
    fn prepare_convention_save(&mut self) {
        let scope = self.conventions.scope.clone();
        let Some(draft) = self
            .conventions
            .state()
            .and_then(|s| s.draft())
            .cloned()
            .filter(|d| !d.busy)
        else {
            return;
        };
        let Some(b) = self.backend() else {
            return;
        };
        let token = b.token("convention-transfer");
        let submission = self.convention_submission(
            if draft.base.is_some() {
                "conventions.edit"
            } else {
                "conventions.import"
            },
            draft.base.as_ref().map(|r| r.id.as_str().into()),
            Some((draft.identity, draft.generation)),
        );
        let slot = Slot::ConventionPrepare(scope.key(), token);
        let s = self.conventions.state_mut();
        s.busy = true;
        if let Some(d) = s.draft_mut() {
            d.busy = true;
        }
        self.conventions.submissions.insert(
            match &slot {
                Slot::ConventionPrepare(_, token) => token.clone(),
                _ => unreachable!(),
            },
            submission.clone(),
        );
        if let Some(c) = &mut self.client {
            c.task(slot, "Preparing convention source", move |b| {
                crate::conventions_transfer::prepare_edit(b, scope, draft, submission)
                    .map(cop::TaskResult::Pipeline)
                    .map(TaskOutput::Convention)
            });
        }
    }
    fn begin_convention_pipeline(
        &mut self,
        token: String,
        pipeline: crate::conventions_transfer::Pipeline,
    ) {
        self.conventions
            .states
            .entry(pipeline.scope.clone())
            .or_default()
            .busy = true;
        self.conventions.pipelines.insert(token.clone(), pipeline);
        self.advance_convention_pipeline(token);
    }
    fn advance_convention_pipeline(&mut self, token: String) {
        let Some(p) = self.conventions.pipelines.get(&token).cloned() else {
            return;
        };
        if p.at >= p.files.len() {
            let operation = match p.operation.as_str() {
                "conventions.import" => "conventions.import",
                _ => "conventions.edit",
            };
            let body = p.body.clone();
            let scopekey = p.scope.key();
            if self.command(
                Slot::ConventionCommand(scopekey, token.clone()),
                "Saving convention",
                operation,
                move |b| cop::command(b, operation, body),
            ) {
                self.conventions.submissions.insert(token, p.submission);
            }
        } else if let Some(c) = &mut self.client {
            c.task(
                Slot::ConventionPrepare(p.scope.key(), token),
                "Preparing source upload",
                move |b| {
                    crate::conventions_transfer::prepare_artifact(b, &p)
                        .map(cop::TaskResult::Prepared)
                        .map(TaskOutput::Convention)
                },
            );
        }
    }
    fn fail_convention_pipeline(&mut self, key: &str, token: &str, error: String) {
        let Some(scope) = self.convention_scope(key) else {
            return;
        };
        let draft = self
            .conventions
            .pipelines
            .get(token)
            .map(|p| &p.submission)
            .or_else(|| self.conventions.submissions.get(token))
            .and_then(|p| p.draft);
        let s = self.conventions.states.get_mut(&scope).expect("scope");
        s.busy = false;
        s.notice = Some((Tone::Attention, error.clone()));
        if let Some((id, _)) = draft
            && let Some(d) = s.draft_by_identity_mut(id)
        {
            d.busy = false;
            d.error = Some(error);
        }
    }
    pub(super) fn import_dropped_convention(&mut self, paths: Vec<std::path::PathBuf>) {
        if paths.len() != 1 {
            self.conventions.state_mut().notice = Some((
                Tone::Attention,
                "Drop one portable convention manifest at a time.".into(),
            ));
            return;
        }
        self.import_convention_path(paths.into_iter().next().expect("one manifest"));
    }
    fn import_convention_path(&mut self, path: std::path::PathBuf) {
        use crate::conventions_transfer::{Pipeline, StagedFile};
        if self.conventions.state().is_some_and(|s| s.busy) {
            self.conventions.state_mut().notice = Some((Tone::Attention,
                "The current convention change must be confirmed before importing another manifest.".into()));
            return;
        }
        let scope = self.conventions.scope.clone();
        let Some(b) = self.backend() else {
            return;
        };
        let token = b.token("convention-import");
        let submission = self.convention_submission("conventions.import", None, None);
        self.begin_convention_pipeline(token, Pipeline {
            scope: scope.clone(), operation: "conventions.import".into(),
            body: serde_json::json!({"scope":scope.wire(),"schema":devworks_core::services::conventions::model::manifest_schema()}),
            files: vec![StagedFile { path, name:"Convention portable manifest".into(), media_type:"application/json".into(), pointer:"/source".into(), owned:false }],
            at: 0, submission,
        });
    }
    fn convention_artifact_action(&mut self, action: cv::Action) {
        match action {
            cv::Action::Save => self.prepare_convention_save(),
            cv::Action::Import => {
                if let Some(path) = self.dialogs.pick_convention() {
                    self.import_convention_path(path);
                }
            }
            cv::Action::Export(format) => {
                let Some(record) = self.conventions.state().and_then(|s| s.record()).cloned()
                else {
                    return;
                };
                let Some(path) = self.dialogs.save_file(if format == 2 {
                    "convention.json"
                } else if format == 0 {
                    "convention.md"
                } else {
                    "convention.txt"
                }) else {
                    return;
                };
                let scope = self.conventions.scope.clone();
                let Some(b) = self.backend() else {
                    return;
                };
                let token = b.token("export");
                if let Some(c) = &mut self.client {
                    c.task(
                        Slot::ConventionTransfer(scope.key(), token),
                        "Exporting convention",
                        move |b| {
                            crate::conventions_transfer::export(b, &scope, &record, format, &path)
                                .map(|()| {
                                    TaskOutput::Convention(cop::TaskResult::Transferred(format!(
                                        "Exported {}",
                                        path.display()
                                    )))
                                })
                        },
                    );
                }
            }
            cv::Action::Download(resource) => {
                let Some(path) = self.dialogs.save_file("convention-attachment") else {
                    return;
                };
                let scope = self.conventions.scope.clone();
                let Some(b) = self.backend() else {
                    return;
                };
                let token = b.token("download");
                if let Some(c) = &mut self.client {
                    c.task(
                        Slot::ConventionTransfer(scope.key(), token),
                        "Downloading attachment",
                        move |b| {
                            crate::conventions_transfer::download(b, &scope, &resource, &path).map(
                                |()| {
                                    TaskOutput::Convention(cop::TaskResult::Transferred(format!(
                                        "Downloaded {}",
                                        path.display()
                                    )))
                                },
                            )
                        },
                    );
                }
            }
            cv::Action::Upload => {
                let Some(record) = self.conventions.state().and_then(|s| s.record()).cloned()
                else {
                    return;
                };
                if let Some(path) = self.dialogs.pick_files().into_iter().next() {
                    let name = path
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("Attachment")
                        .into();
                    self.conventions.state_mut().upload = Some(cv::UploadDraft {
                        base: record,
                        media_type: crate::attachments::media_type(&path).into(),
                        path,
                        name,
                        generation: 0,
                    });
                }
            }
            cv::Action::UpdateResource(resource) => {
                let s = self.conventions.state_mut();
                let base = s.record().cloned();
                let id = resource.id.as_str().to_owned();
                s.resource_drafts.entry(id.clone()).or_insert_with(|| {
                    let mut d = cv::ResourceDraft::of(&resource);
                    d.base = base;
                    d
                });
                s.resource_editor = Some(id);
            }
            cv::Action::DiscardResource => {
                let s = self.conventions.state_mut();
                if let Some(id) = s.resource_editor.take() {
                    s.resource_drafts.remove(&id);
                }
            }
            cv::Action::RebaseResource => {
                let s = self.conventions.state_mut();
                let base = s.record().cloned();
                if let Some(id) = &s.resource_editor
                    && let Some(current) = s.resources.iter().find(|r| r.id.as_str() == id)
                    && let Some(d) = s.resource_drafts.get_mut(id)
                {
                    d.resource = current.clone();
                    d.base = base;
                }
            }
            cv::Action::ReplaceResource => {
                if let Some(path) = self.dialogs.pick_files().into_iter().next() {
                    let s = self.conventions.state_mut();
                    if let Some(id) = &s.resource_editor
                        && let Some(d) = s.resource_drafts.get_mut(id)
                    {
                        d.replacement = Some(path);
                        d.generation = d.generation.wrapping_add(1);
                    }
                }
            }
            cv::Action::SaveResource | cv::Action::CommitUpload => {
                self.save_convention_resource(matches!(action, cv::Action::CommitUpload))
            }
            cv::Action::EmbeddingSetup => self.open_embedding_setup(),
            _ => {}
        }
    }
    fn save_convention_resource(&mut self, upload: bool) {
        use crate::conventions_transfer::{Pipeline, StagedFile};
        let scope = self.conventions.scope.clone();
        let Some(s) = self.conventions.state() else {
            return;
        };
        if s.busy {
            return;
        }
        let resource_snapshot = if upload {
            s.upload
                .as_ref()
                .map(|d| (true, d.base.id.as_str().to_owned(), d.generation))
        } else {
            s.resource_editor.as_ref().and_then(|id| {
                s.resource_drafts
                    .get(id)
                    .map(|d| (false, id.clone(), d.generation))
            })
        };
        let (base, change, stage) = if upload {
            let Some(d) = &s.upload else {
                return;
            };
            let placeholder = d.base.content.clone();
            (
                d.base.clone(),
                serde_json::json!({"kind":"Attach","name":d.name,"mediaType":d.media_type,"content":placeholder}),
                Some((d.path.clone(), d.name.clone())),
            )
        } else {
            let Some(d) = s
                .resource_editor
                .as_ref()
                .and_then(|id| s.resource_drafts.get(id))
            else {
                return;
            };
            let Some(base) = &d.base else {
                return;
            };
            (
                base.clone(),
                serde_json::json!({"kind":"Update","resourceId":d.resource.id,"expectedRevision":d.resource.revision,"name":d.name,"mediaType":d.media_type,"content":d.resource.content,"lifecycle":if d.retired{"Retired"}else{"Active"}}),
                d.replacement.as_ref().map(|p| (p.clone(), d.name.clone())),
            )
        };
        let Some(content) = &base.content else {
            self.convention_notice(
                "The opening document source is unavailable; nothing was changed.".into(),
            );
            return;
        };
        let body = serde_json::json!({"scope":scope.wire(),"conventionId":base.id,"expectedRevision":base.revision,"content":content,"resources":[change]});
        let files = stage
            .map(|(path, name)| StagedFile {
                media_type: crate::attachments::media_type(&path).into(),
                path,
                name,
                pointer: "/resources/0/content".into(),
                owned: false,
            })
            .into_iter()
            .collect();
        let Some(b) = self.backend() else {
            return;
        };
        let token = b.token("convention-resource");
        let mut submission =
            self.convention_submission("conventions.edit", Some(base.id.as_str().into()), None);
        submission.resource = resource_snapshot;
        self.begin_convention_pipeline(
            token,
            Pipeline {
                scope,
                operation: "conventions.edit".into(),
                body,
                files,
                at: 0,
                submission,
            },
        );
    }
    pub(super) fn conventions_screen(&mut self, ui: &mut Ui) {
        if !self.conventions.initialized {
            self.conventions.initialized = true;
            if let Some(project) = self.selected_project() {
                self.conventions.scope = Scope::Project(project.to_owned());
            }
            self.load_conventions(false, false);
        }
        if self
            .conventions
            .state()
            .is_none_or(|s| !s.loaded && !s.listing && s.list_error.is_none())
        {
            self.load_conventions(false, false);
        }
        self.conventions.projects = self
            .projects
            .list
            .items
            .iter()
            .map(|p| (p.id.0.as_str().into(), p.name.0.clone()))
            .collect();
        if self
            .conventions
            .last_operation_poll
            .is_none_or(|last| last.elapsed() >= Duration::from_secs(2))
        {
            self.conventions.last_operation_poll = Some(Instant::now());
            let ids: Vec<_> = self
                .conventions
                .state()
                .map(|s| {
                    s.operation_ids
                        .iter()
                        .filter(|id| {
                            s.operation_statuses.get(*id).is_none_or(|status| {
                                !matches!(
                                    status.phase,
                                    ConventionOperationPhase::Completed
                                        | ConventionOperationPhase::Failed
                                        | ConventionOperationPhase::Cancelled
                                )
                            })
                        })
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            for id in ids {
                if !self.pending(&Slot::ConventionOperation(
                    self.conventions.scope.key(),
                    id.clone(),
                )) {
                    self.inspect_convention_operation(id);
                }
            }
        }
        if self.conventions.state().is_some_and(|s| {
            s.operation_ids.iter().any(|id| {
                s.operation_statuses.get(id).is_none_or(|status| {
                    !matches!(
                        status.phase,
                        ConventionOperationPhase::Completed
                            | ConventionOperationPhase::Failed
                            | ConventionOperationPhase::Cancelled
                    )
                })
            })
        }) {
            ui.ctx().request_repaint_after(Duration::from_secs(2));
        }
        self.conventions.supports_search_cancel = self
            .backend()
            .is_some_and(|b| b.supports_query_cancellation());
        let mut actions = Vec::new();
        self.conventions.ui(ui, self.online().is_ok(), &mut actions);
        for action in actions {
            if !self.convention_read_action(&action) {
                self.convention_artifact_action(action);
            }
        }
    }
}

impl Shell {
    pub(super) fn conventions_pending_memory(&self) -> serde_json::Value {
        let operations: Vec<_> = self
            .conventions
            .states
            .iter()
            .filter(|(_, s)| !s.operation_ids.is_empty())
            .map(|(scope, s)| serde_json::json!({"scope":scope,"ids":s.operation_ids}))
            .collect();
        let searches:Vec<_>=self.conventions.states.iter().filter(|(_,s)|s.search_recovery.is_some()||s.last_search.is_some()).map(|(scope,s)|serde_json::json!({"scope":scope,"recovery":s.search_recovery,"last":s.last_search,"preparation":s.search_preparation,"earlier":s.previous_searches})).collect();
        if self.conventions.pipelines.is_empty()
            && self.conventions.submissions.is_empty()
            && operations.is_empty()
            && searches.is_empty()
        {
            return serde_json::Value::Null;
        }
        serde_json::json!({"pipelines":self.conventions.pipelines,"submissions":self.conventions.submissions,"operations":operations,"searches":searches,"scope":self.conventions.scope})
    }
    pub(super) fn restore_conventions_pending(&mut self, value: &serde_json::Value) {
        if let Ok(scope) = serde_json::from_value(value["scope"].clone()) {
            self.conventions.scope = scope;
            self.conventions.initialized = true;
        }
        self.conventions.pipelines =
            serde_json::from_value(value["pipelines"].clone()).unwrap_or_default();
        self.conventions.submissions =
            serde_json::from_value(value["submissions"].clone()).unwrap_or_default();
        for p in self.conventions.pipelines.values_mut() {
            p.submission.draft = None;
            p.submission.resource = None;
            self.conventions.states.entry(p.scope.clone()).or_default();
        }
        for p in self.conventions.submissions.values_mut() {
            p.draft = None;
            p.resource = None;
            self.conventions.states.entry(p.scope.clone()).or_default();
        }
        if let Some(searches) = value["searches"].as_array() {
            for search in searches {
                if let Ok(scope) = serde_json::from_value::<Scope>(search["scope"].clone()) {
                    let state = self.conventions.states.entry(scope).or_default();
                    state.search_recovery =
                        serde_json::from_value(search["recovery"].clone()).unwrap_or_default();
                    state.last_search =
                        serde_json::from_value(search["last"].clone()).unwrap_or_default();
                    state.search_preparation =
                        serde_json::from_value(search["preparation"].clone()).unwrap_or_default();
                    state.previous_searches =
                        serde_json::from_value(search["earlier"].clone()).unwrap_or_default();
                }
            }
        }
        if let Some(operations) = value["operations"].as_array() {
            for operation in operations {
                if let (Ok(scope), Ok(ids)) = (
                    serde_json::from_value::<Scope>(operation["scope"].clone()),
                    serde_json::from_value::<Vec<String>>(operation["ids"].clone()),
                ) {
                    self.conventions
                        .states
                        .entry(scope)
                        .or_default()
                        .operation_ids = ids;
                }
            }
        }
    }
}

impl Shell {
    fn dispatch_convention_search(
        &mut self,
        scope: Scope,
        generation: u64,
        append: bool,
        frame: Vec<u8>,
    ) -> bool {
        if self.online().is_err() {
            return false;
        }
        let Some(backend) = self.backend() else {
            return false;
        };
        let token = devworks_core::ports::storage::Cancellation::new();
        let state = self.conventions.states.entry(scope.clone()).or_default();
        state.search_recovery = Some(cop::SearchRecovery {
            frame: frame.clone(),
            append,
            generation,
        });
        state.active_search = Some((generation, token.clone()));
        let slot = Slot::ConventionsList(scope.key(), generation, append, true);
        let submitted = self.client.as_mut().is_some_and(|c| {
            if backend.supports_query_cancellation() {
                c.query_cancellable(
                    slot,
                    "Searching active conventions",
                    "conventions.search",
                    frame,
                    devworks_core::ports::storage::ReadOptions {
                        cancellation: Some(token),
                        ..Default::default()
                    },
                )
            } else {
                c.query(
                    slot,
                    "Searching active conventions",
                    "conventions.search",
                    frame,
                )
            }
        });
        if !submitted {
            let s = self.conventions.states.get_mut(&scope).expect("scope");
            s.search_recovery = None;
            s.active_search = None;
        }
        submitted
    }
    fn confirm_convention_search(&mut self) {
        let scope = self.conventions.scope.clone();
        let state = self.conventions.states.entry(scope.clone()).or_default();
        if state.active_search.is_some() {
            return;
        }
        let Some(recovery) = state
            .search_recovery
            .as_ref()
            .or(state.last_search.as_ref())
            .cloned()
        else {
            return;
        };
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&recovery.frame) else {
            state.list_error =
                Some("The saved search frame is unreadable; no new search was sent.".into());
            return;
        };
        let mut body = value["body"].clone();
        let Some(object) = body.as_object_mut() else {
            return;
        };
        object.remove("kind");
        let mut old = state.query_snapshot.clone().unwrap_or_default();
        if let Some(page) = old
            .get_mut("page")
            .and_then(serde_json::Value::as_object_mut)
        {
            page.remove("cursor");
        }
        let mut filter = body.clone();
        if let Some(page) = filter
            .get_mut("page")
            .and_then(serde_json::Value::as_object_mut)
        {
            page.remove("cursor");
        }
        let append = recovery.append && state.searched && old == filter;
        state.list_generation = state.list_generation.wrapping_add(1);
        let generation = state.list_generation;
        state.pending_listing = Some((generation, body));
        state.listing = true;
        state.list_error = None;
        if !self.dispatch_convention_search(scope.clone(), generation, append, recovery.frame) {
            let s = self.conventions.states.get_mut(&scope).expect("scope");
            s.listing = false;
            s.list_error =
                Some("The original search cannot be confirmed while offline or closing.".into());
        }
    }
    pub(super) fn cancel_convention_queries(&self) {
        for state in self.conventions.states.values() {
            if let Some((_, token)) = &state.active_search {
                token.cancel();
            }
        }
    }
}

impl Shell {
    fn archive_convention_search(&mut self) {
        let state = self.conventions.state_mut();
        if let Some(request) = state.search_recovery.take() {
            state.previous_searches.push(cop::SearchContext {
                request,
                preparation: state.search_preparation.take(),
            });
        } else {
            state.search_preparation = None;
        }
    }
}
