//! Reads: loading features, cards, history, messages and one work item's detail.
use super::*;

impl WorkCtl<'_, '_> {
    /// Reads the selected project's features, cards and Activity (every page).
    pub(super) fn load_work(&mut self) {
        // Saves still running belong to their items, not to the project shown: a project
        // switch keeps them, so each reply still finishes its own save.
        let Some(p) = self.selected_project.clone() else {
            self.reset_for(None);
            return;
        };
        if self.work.of.as_deref() != Some(p.as_str()) {
            self.reset_for(Some(p.clone()));
        }
        self.work.allocation_repositories = self
            .selected_project_record()
            .map(|project| {
                project
                    .repository_ids
                    .0
                    .iter()
                    .map(|id| {
                        let label = self
                            .cx
                            .projects
                            .repositories
                            .items
                            .iter()
                            .find(|r| r.id == *id)
                            .map(|r| r.name.0.clone())
                            .unwrap_or_else(|| id.as_str().to_owned());
                        (id.as_str().to_owned(), label)
                    })
                    .collect()
            })
            .unwrap_or_default();
        self.load_workflow(&p, false);
        self.load_allocation_queue(&p, None, None);
        if let Some(feature) = self.work.board.clone() {
            self.load_allocation_queue(&p, Some(feature), None);
        }
        self.refresh_allocation_claims();
        self.work.list_at = self.work.mutations;
        let fetch = self.work.features.reload_all();
        self.send_features(&p, fetch);
        let fetch = self.work.cards.reload_all();
        self.send_cards(&p, fetch);
        // Derived reverse links have no independent revision on their prerequisite. Reload the
        // visible detail too; the client fences any older reply for the same detail slot.
        let mem = self.route_memory(self.route());
        let selected = match self.route() {
            Route::Features if mem.tab.min(1) == 1 => {
                self.work.card.clone().or_else(|| self.work.board.clone())
            }
            Route::Features | Route::Issues => mem.selection,
            _ => None,
        };
        if let Some(w) = selected.and_then(|id| self.record(&id)) {
            self.load_detail(&w);
        }
        // Initial activity loads one newest page.
        if !self.work.activity.feed.loaded && !self.work.activity.feed.loading() {
            self.load_activity();
        }
    }

    /// Drops what the lists of another project showed, keeping the saves still running and
    /// what survives a project switch.
    pub(super) fn reset_for(&mut self, project: Option<String>) {
        *self.work = WorkUi {
            // The project's feed keeps its request counter, so a late reply of the old project
            // stays refused.
            activity: {
                let mut feed = std::mem::take(&mut self.work.activity.feed);
                feed.reset();
                Activity {
                    feed,
                    ..Activity::default()
                }
            },
            of: project,
            workflows: std::mem::take(&mut self.work.workflows),
            allocation_claims: std::mem::take(&mut self.work.allocation_claims),
            allocation_recovery: std::mem::take(&mut self.work.allocation_recovery),
            allocation_reused: std::mem::take(&mut self.work.allocation_reused),
            allocation_inspect_errors: std::mem::take(&mut self.work.allocation_inspect_errors),
            allocation_queues: std::mem::take(&mut self.work.allocation_queues),
            allocation_busy: std::mem::take(&mut self.work.allocation_busy),
            allocation_filters: std::mem::take(&mut self.work.allocation_filters),
            allocation_requests: std::mem::take(&mut self.work.allocation_requests),
            claim_lease_ms: std::mem::take(&mut self.work.claim_lease_ms),
            inspect_claim_id: std::mem::take(&mut self.work.inspect_claim_id),
            archive_counts: std::mem::take(&mut self.work.archive_counts),
            plan_saving: std::mem::take(&mut self.work.plan_saving),
            edit_saving: std::mem::take(&mut self.work.edit_saving),
            create_saving: std::mem::take(&mut self.work.create_saving),
            ..WorkUi::default()
        };
    }

    /// Whether a work list read is in flight.
    pub(super) fn work_loading(&self) -> bool {
        self.work.of.as_ref().is_some_and(|p| {
            self.pending(&WorkSlot::WorkFeatures(p.clone()))
                || self.pending(&WorkSlot::WorkCards(p.clone()))
        })
    }

    pub(super) fn history_feed(&mut self, root: Option<&str>) -> &mut Activity {
        match root {
            Some(root) => self.work.histories.entry(root.to_owned()).or_default(),
            None => &mut self.work.activity,
        }
    }

    pub(super) fn request_history(&mut self, root: Option<String>, fetch: Fetch) {
        let Some(project) = self.work.of.clone() else {
            return;
        };
        let requested = root.clone();
        let slot = WorkSlot::WorkActivity(project.clone(), root.clone(), fetch.generation);
        let list = match &root {
            Some(root) => &mut self.work.histories.entry(root.clone()).or_default().feed,
            None => &mut self.work.activity.feed,
        };
        let sent = self.cx.paged(
            list,
            fetch,
            slot,
            "Loading history",
            "history.events",
            move |backend, cursor| {
                wops::history_events(backend, &project, requested.as_deref(), cursor)
            },
        );
        if !sent {
            self.history_feed(root.as_deref()).feed.error =
                Some("History is unavailable while closing or offline.".into());
        }
    }

    pub(super) fn refresh_history(&mut self, root: Option<String>) {
        if self.work.of.is_none() {
            return;
        }
        self.history_feed(root.as_deref()).feed.error = None;
        let fetch = self.history_feed(root.as_deref()).feed.refresh();
        self.request_history(root, fetch);
    }

    pub(super) fn older_history(&mut self, root: Option<String>) {
        let Some(fetch) = self.history_feed(root.as_deref()).feed.more_page() else {
            return;
        };
        self.request_history(root, fetch);
    }

    pub(super) fn load_activity(&mut self) {
        self.refresh_history(None);
    }

    pub(super) fn load_messages(&mut self, reset: bool) {
        let Some(project) = self.selected_project.clone() else {
            return;
        };
        let explicit = reset || self.work.messages.project.as_deref() != Some(project.as_str());
        // A re-read under way is not superseded by every message event: one more follows when
        // it is done, so a busy stream cannot keep a long history from ever finishing.
        if reset
            && self.work.messages.project.as_deref() == Some(project.as_str())
            && self.work.messages.history.gathering()
        {
            self.work.messages.history_again = true;
            return;
        }
        if explicit {
            let same_project = self.work.messages.project.as_deref() == Some(project.as_str());
            let inbox_refresh_pending =
                same_project && self.pending(&WorkSlot::WorkMessagesInbox(project.clone()));
            let (draft, events, event_after, event_poll_at) = if same_project {
                (
                    self.work.messages.draft.clone(),
                    std::mem::take(&mut self.work.messages.events),
                    self.work.messages.event_after,
                    self.work.messages.event_poll_at,
                )
            } else {
                (MessageDraft::default(), Vec::new(), None, None)
            };
            // A refresh keeps the messages on screen until its replies replace them: emptying
            // the board meanwhile would remove the controls a click in that frame was aimed at.
            let (history, inbox) = if same_project {
                (
                    std::mem::take(&mut self.work.messages.history),
                    std::mem::take(&mut self.work.messages.inbox),
                )
            } else {
                // Another project's list starts empty; its request counter carries on, so a
                // late reply of the old project stays refused.
                let mut history = std::mem::take(&mut self.work.messages.history);
                history.reset();
                (history, Vec::new())
            };
            self.work.messages = MessageBoard {
                project: Some(project.clone()),
                history,
                inbox,
                draft,
                events,
                event_after,
                event_poll_at,
                inbox_refresh_pending,
                ..MessageBoard::default()
            };
        }
        let event_after = self.work.messages.event_after;
        // Subscribe from the current event head before reading the initial inbox/history.
        // Otherwise a post between those reads and the subscription can be missed forever.
        if event_after.is_none() {
            if !self.pending(&WorkSlot::WorkMessagesEvents(project.clone())) {
                let p = project.clone();
                self.query(
                    WorkSlot::WorkMessagesEvents(project),
                    "Reading message changes",
                    "agents.events",
                    move |be| wops::message_events(be, &p, None),
                );
            }
            return;
        }
        // An explicit refresh asks again, superseding a read in flight. The events poll and the
        // inbox reply only start the first read: asking again while it is under way would
        // supersede it every second on a slow backend, and it would never finish.
        let history = &mut self.work.messages.history;
        if explicit || (!history.loaded && !history.loading()) {
            // Another project's history was reset above, so this starts it over; the same
            // project's is re-read page for page, keeping the older messages the user loaded.
            let fetch = history.reread();
            self.send_history(project.clone(), fetch);
        }
        if !self.pending(&WorkSlot::WorkMessagesInbox(project.clone())) {
            let p = project.clone();
            self.command(
                WorkSlot::WorkMessagesInbox(project.clone()),
                "Reading inbox",
                "agents.receive",
                move |be| wops::message_inbox(be, &p, None),
            );
        }
    }

    pub(super) fn older_messages(&mut self) {
        let Some(project) = self.selected_project.clone() else {
            return;
        };
        let Some(fetch) = self.work.messages.history.more_page() else {
            return;
        };
        self.send_history(project, fetch);
    }

    pub(super) fn send_history(&mut self, project: String, fetch: Fetch) {
        let p = project.clone();
        self.cx.paged(
            &mut self.work.messages.history,
            fetch,
            WorkSlot::WorkMessagesHistory(project),
            "Reading messages",
            "agents.history",
            move |be, cursor| {
                wops::message_history(be, &p, cursor.and_then(|before| before.parse().ok()))
            },
        );
    }

    pub(super) fn load_detail(&mut self, w: &WorkRecord) {
        if !self
            .work
            .histories
            .get(w.id.as_str())
            .is_some_and(|feed| feed.feed.loading())
        {
            self.refresh_history(Some(w.id.as_str().to_owned()));
        }
        self.load_git_links(w.id.as_str());
        let (op, family) = if matches!(w.kind, WorkKind::Feature) {
            ("features.get", wops::Family::Features)
        } else {
            ("tasks.get", wops::Family::Tasks)
        };
        let id = w.id.as_str().to_owned();
        self.query(
            WorkSlot::WorkDetail(id.clone()),
            "Reading work",
            op,
            move |be| wops::work_get(be, family, &id),
        );
    }

    pub(super) fn load_readiness_history(&mut self, root: &str, more: bool) {
        let list = self
            .work
            .readiness_history
            .entry(root.to_owned())
            .or_default();
        let fetch = if more {
            list.more_page()
        } else {
            Some(list.reread())
        };
        if let Some(fetch) = fetch {
            self.request_readiness_history(root, fetch);
        }
    }

    pub(super) fn request_readiness_history(&mut self, root: &str, fetch: Fetch) {
        let Some(list) = self.work.readiness_history.get_mut(root) else {
            return;
        };
        let root = root.to_owned();
        let slot = WorkSlot::ReadinessHistory(root.clone(), fetch.generation);
        self.cx.paged(
            list,
            fetch,
            slot,
            "Reading settled finding history",
            "history.work_lineage",
            move |be, cursor| wops::readiness_history(be, &root, cursor),
        );
    }

    /// Reads the root's source-control links: a first read shows its spinner, a re-read keeps
    /// the shown list until every page is read.
    pub(super) fn load_git_links(&mut self, work: &str) {
        let list = self.work.git.entry(work.to_owned()).or_default();
        let fetch = list.reload_all();
        self.send_git_links(work, fetch);
    }

    pub(super) fn send_git_links(&mut self, work: &str, fetch: Fetch) {
        let root = work.to_owned();
        if let Some(list) = self.work.git.get_mut(work) {
            self.cx.paged(
                list,
                fetch,
                WorkSlot::WorkGit(root.clone()),
                "Reading source control",
                "history.git_links",
                move |be, cursor| wops::git_links(be, &root, cursor),
            );
        }
    }

    pub(super) fn send_features(&mut self, project: &str, fetch: Fetch) {
        let p = project.to_owned();
        self.cx.paged(
            &mut self.work.features,
            fetch,
            WorkSlot::WorkFeatures(project.to_owned()),
            "Loading features",
            "features.list",
            move |be, cursor| wops::work_list(be, wops::Family::Features, &p, cursor),
        );
    }

    pub(super) fn send_cards(&mut self, project: &str, fetch: Fetch) {
        let p = project.to_owned();
        self.cx.paged(
            &mut self.work.cards,
            fetch,
            WorkSlot::WorkCards(project.to_owned()),
            "Loading tasks and issues",
            "tasks.list",
            move |be, cursor| wops::work_list(be, wops::Family::Tasks, &p, cursor),
        );
    }

    /// Reads every inspected root's run snapshot again (one at a time per root).
    pub(super) fn reinspect_runs(&mut self) {
        let roots: Vec<String> = self.work.runs.keys().cloned().collect();
        for root in roots {
            if !self.pending(&WorkSlot::WorkInspect(root.clone())) {
                self.inspect(&root);
            }
        }
    }
    pub(super) fn inspect(&mut self, root: &str) {
        let r = root.to_owned();
        self.query(
            WorkSlot::WorkInspect(root.to_owned()),
            "Reading run controls",
            "runs.inspect",
            move |be| wops::run_inspect(be, &r),
        );
    }

    /// Opens a work item by ID (a completion card's link): its project is selected and its
    /// screen shown with it selected.
    pub(super) fn open_work(&mut self, id: &str) {
        let w = id.to_owned();
        self.query(
            WorkSlot::WorkOpen(id.to_owned(), false),
            "Opening work",
            "tasks.get",
            move |be| wops::work_get(be, wops::Family::Tasks, &w),
        );
    }

    /// Counts the roots (features and standalone issues) and running roots of `project` for
    /// an archive confirmation.
    pub(super) fn count_archive_work(&mut self, project: &str) {
        for features in [true, false] {
            let key = (project.to_owned(), features);
            if self.work.counted.contains(&key)
                || self.pending(&WorkSlot::ArchiveCount(key.0.clone(), features))
            {
                continue;
            }
            self.work.counting.insert(key, (0, 0));
            let p = project.to_owned();
            let (op, family) = if features {
                ("features.list", wops::Family::Features)
            } else {
                ("issues.list", wops::Family::Issues)
            };
            self.query(
                WorkSlot::ArchiveCount(project.to_owned(), features),
                "Counting running work",
                op,
                move |be| wops::work_list(be, family, &p, None),
            );
        }
    }

    pub(super) fn record(&self, id: &str) -> Option<WorkRecord> {
        self.work.freshest(id)
    }

    pub(crate) fn project_open(&self) -> bool {
        self.selected_project_record()
            .is_some_and(|p| matches!(p.lifecycle, LifeState::Active))
    }
}
