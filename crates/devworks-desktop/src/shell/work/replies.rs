//! Applies backend replies to the work screens.
use super::*;

impl WorkCtl<'_, '_> {
    /// Applies a reply for one of the work slots; returns it back when it is not one.
    pub(super) fn apply_work(&mut self, slot: WorkSlot, reply: Reply) -> Option<(WorkSlot, Reply)> {
        let (slot, reply) = self.apply_workflow(slot, reply)?;
        let (slot, reply) = self.apply_code_review(slot, reply)?;
        match (slot, reply) {
            (WorkSlot::KeepWorking(project), Reply::Task(result)) => {
                if self.selected_project.as_deref() == Some(project.as_str()) {
                    self.work.message = result.err().map(|error| (Tone::Attention, error));
                }
            }
            (WorkSlot::WorkMessagesHistory(project), Reply::Query(reply)) => {
                if self.work.messages.project.as_deref() != Some(project.as_str()) {
                    return None;
                }
                let page = wops::message_history_page(reply)
                    .map(|page| {
                        let next = page.next_before.map(|before| before.get().to_string());
                        Page::new(page.items.0, next)
                    })
                    .map_err(|error| error.describe());
                let board = &mut self.work.messages;
                // A message listed on two pages (one was posted between reads) shows once.
                match board
                    .history
                    .apply_current_unique(page, |m| m.id.as_str().to_owned())
                {
                    Applied::Replaced | Applied::Appended => {
                        board.error = None;
                        if std::mem::take(&mut board.history_again) {
                            self.load_messages(true);
                        }
                    }
                    Applied::Failed => board.error = board.history.error.clone(),
                    Applied::Continue(fetch) => self.send_history(project, fetch),
                    _ => {}
                }
            }
            (WorkSlot::WorkMessagesInbox(project), Reply::Command(reply)) => {
                if self.work.messages.project.as_deref() != Some(project.as_str()) {
                    return None;
                }
                if self.work.messages.inbox_refresh_pending {
                    self.work.messages.inbox_refresh_pending = false;
                    self.load_messages(false);
                    return None;
                }
                match wops::message_inbox_page(reply) {
                    Ok(page) => {
                        let board = &mut self.work.messages;
                        // The first page after a refresh replaces what was kept on screen.
                        if !board.inbox_loaded {
                            board.inbox.clear();
                        }
                        board.inbox_has_more = page.items.0.len() == 100;
                        for message in page.items.0 {
                            if let Some(known) =
                                board.inbox.iter_mut().find(|known| known.id == message.id)
                            {
                                *known = message;
                            } else {
                                board.inbox.push(message);
                            }
                        }
                        board.inbox_cursor = Some(page.next_cursor.0);
                        board.inbox_loaded = true;
                        board.error = None;
                    }
                    Err(error) => self.work.messages.error = Some(error.describe()),
                }
            }
            (WorkSlot::WorkMessagesEvents(project), Reply::Query(reply)) => {
                if self.work.messages.project.as_deref() != Some(project.as_str()) {
                    return None;
                }
                match wops::message_events_page(reply) {
                    Ok(page) => {
                        let board = &mut self.work.messages;
                        let changed = !page.items.0.is_empty();
                        for event in page.items.0 {
                            if !board
                                .events
                                .iter()
                                .any(|known| known.sequence == event.sequence)
                            {
                                board.events.push(event);
                            }
                        }
                        board.event_after = Some(page.next_sequence.get());
                        board.event_poll_at =
                            Some(std::time::Instant::now() + std::time::Duration::from_secs(1));
                        board.error = None;
                        if changed {
                            self.load_messages(true);
                        } else if !board.history.loaded || !board.inbox_loaded {
                            self.load_messages(false);
                        }
                    }
                    Err(error) => {
                        self.work.messages.error = Some(error.describe());
                        self.work.messages.event_poll_at =
                            Some(std::time::Instant::now() + std::time::Duration::from_secs(1));
                    }
                }
            }
            (WorkSlot::WorkMessagePost(project), Reply::Command(reply)) => {
                if self.work.messages.project.as_deref() != Some(project.as_str()) {
                    return None;
                }
                match wops::message_posted(reply) {
                    Ok(result) => {
                        if result.item_versions.is_some() {
                            self.load_work();
                        }
                        if self.work.messages.sending.as_ref() == Some(&self.work.messages.draft) {
                            self.work.messages.draft = MessageDraft::default();
                        }
                        self.work.messages.sending = None;
                        self.load_messages(true);
                    }
                    Err(error) => {
                        self.work.messages.sending = None;
                        self.work.messages.error = Some(error.describe());
                    }
                }
            }
            (WorkSlot::WorkMessageAck(project, id), Reply::Command(reply)) => {
                if self.work.messages.project.as_deref() != Some(project.as_str()) {
                    return None;
                }
                match wops::message_acked(reply) {
                    Ok(result) => {
                        if result.item_versions.is_some() {
                            self.load_work();
                        }
                        self.work
                            .messages
                            .inbox
                            .retain(|message| message.id.as_str() != id);
                        self.load_messages(true);
                    }
                    Err(error) => self.work.messages.error = Some(error.describe()),
                }
            }
            (WorkSlot::WorkFeatures(p) | WorkSlot::WorkCards(p), Reply::Query(_))
                if self.work.of.as_deref() != Some(p.as_str()) => {}
            (WorkSlot::WorkFeatures(_) | WorkSlot::WorkCards(_), Reply::Query(_))
                if self.work.list_at != self.work.mutations =>
            {
                self.load_work();
            }
            (WorkSlot::WorkFeatures(p), Reply::Query(q)) => match wops::work_page(q) {
                Ok((items, next)) => {
                    let page = Page::new(items, next);
                    if let Applied::Continue(fetch) = self.work.features.apply_current(Ok(page)) {
                        self.send_features(&p, fetch);
                    }
                }
                Err(Failure::Refused(f)) if f.code == DomainErrorCode::CursorExpired => {
                    self.load_work()
                }
                Err(e) => {
                    self.work.features.apply_current(Err(e.describe()));
                }
            },
            (WorkSlot::WorkCards(p), Reply::Query(q)) => match wops::work_page(q) {
                Ok((items, next)) => {
                    let page = Page::new(items, next);
                    match self.work.cards.apply_current(Ok(page)) {
                        Applied::Continue(fetch) => self.send_cards(&p, fetch),
                        Applied::Replaced => {
                            let known: BTreeSet<String> = self
                                .work
                                .cards
                                .items
                                .iter()
                                .map(|c| c.id.as_str().to_owned())
                                .collect();
                            // A card that left the project's lists is no longer selected.
                            if self.work.card.as_ref().is_some_and(|c| !known.contains(c)) {
                                self.work.card = None;
                            }
                        }
                        _ => {}
                    }
                }
                Err(Failure::Refused(f)) if f.code == DomainErrorCode::CursorExpired => {
                    self.load_work()
                }
                Err(e) => {
                    self.work.cards.apply_current(Err(e.describe()));
                }
            },
            (WorkSlot::WorkActivity(project, root, generation), Reply::Query(reply)) => {
                if self.work.of.as_deref() != Some(project.as_str()) {
                    return None;
                }
                let page = wops::events_page(reply)
                    .map(|(items, resume, _)| Page::new(items, resume))
                    .map_err(|error| error.describe());
                // An event listed on two pages (the feed moved between reads) shows once.
                let applied =
                    self.history_feed(root.as_deref())
                        .feed
                        .apply_unique(generation, page, |e| e.event_id.0.as_str().to_owned());
                if let Applied::Continue(fetch) = applied {
                    self.request_history(root, fetch);
                }
            }
            (WorkSlot::WorkGitInspect(_, tree), Reply::Query(q)) => {
                self.work
                    .git_inspections
                    .insert(tree, ops::git_inspected(q).map_err(|e| e.describe()));
            }
            (WorkSlot::ReadinessHistory(id, generation), Reply::Query(q)) => {
                let page = wops::readiness_history_result(q)
                    .map(|page| {
                        Page::new(
                            page.items
                                .0
                                .into_iter()
                                .filter(|row| row.kind.0 == "readiness_finding")
                                .collect(),
                            page.next_cursor.map(|c| c.0),
                        )
                    })
                    .map_err(|e| e.describe());
                if let Some(list) = self.work.readiness_history.get_mut(&id)
                    && let Applied::Continue(fetch) =
                        list.apply_unique(generation, page, |row| row.id.as_str().to_owned())
                {
                    self.request_readiness_history(&id, fetch);
                }
            }
            (WorkSlot::WorkGit(id), Reply::Query(q)) => {
                let page = wops::git_links_result(q)
                    .map(|page| Page::new(page.items.0, page.next_cursor.map(|c| c.0)))
                    .map_err(|e| e.describe());
                if let Some(list) = self.work.git.get_mut(&id)
                    && let Applied::Continue(fetch) = list.apply_current(page)
                {
                    self.send_git_links(&id, fetch);
                }
            }
            (WorkSlot::WorkDetail(id), Reply::Query(q)) => match wops::work_record(q) {
                Ok(w) => {
                    // A command can publish a newer approval while this read is in
                    // flight. Slot fencing orders reads, not reads against commands.
                    if self
                        .work
                        .detail
                        .get(&id)
                        .is_none_or(|known| w.revision.get() >= known.revision.get())
                    {
                        self.work.detail.insert(id, w);
                    }
                }
                Err(e) => self.work.message = Some((Tone::Bad, e.describe())),
            },
            (WorkSlot::WorkOpen(id, feature), Reply::Query(q)) => match wops::work_record(q) {
                Ok(w) => {
                    self.select_project(Some(w.project_id.0.as_str().to_owned()));
                    let key = w.id.as_str().to_owned();
                    if feature {
                        let view = self.routes.entry(Route::Features).or_default();
                        view.tab = 0;
                        view.selection = Some(key);
                        self.navigate(Route::Features);
                    } else if let Some(f) = &w.parent_id {
                        self.work.board = Some(f.as_str().to_owned());
                        self.work.card = Some(key);
                        self.routes.entry(Route::Features).or_default().tab = 1;
                        self.navigate(Route::Features);
                    } else {
                        self.routes.entry(Route::Issues).or_default().selection = Some(key);
                        self.navigate(Route::Issues);
                    }
                }
                Err(Failure::Refused(f))
                    if !feature && f.code == DomainErrorCode::UnsupportedTarget =>
                {
                    self.query(
                        WorkSlot::WorkOpen(id.clone(), true),
                        "Opening work",
                        "features.get",
                        move |be| wops::work_get(be, wops::Family::Features, &id),
                    );
                }
                Err(e) => self.announce(format!("The work could not be opened: {}", e.describe())),
            },
            (WorkSlot::WorkInspect(id), Reply::Query(q)) => match wops::run_snapshot(q) {
                Ok(s) => {
                    let held = s.blockers.items.iter().any(|b| wops::manual_hold(&b.cause));
                    self.work.runs.insert(id.clone(), s.clone());
                    // Implementation is gated by every condition (pause, plan, readiness,
                    // prerequisites, questions, capacity), so it names all that hold the root.
                    let purpose = devworks_core::protocol::Purpose::Implementation;
                    let root = id.clone();
                    self.query(
                        WorkSlot::WorkEligibility(id.clone()),
                        "Reading why work is held",
                        "runs.eligibility",
                        move |be| wops::run_eligibility(be, &root, purpose),
                    );
                    if self.work.continue_all.remove(&id)
                        && held
                        && let Some(w) = self.record(&id)
                    {
                        self.command(
                            WorkSlot::WorkRun(id),
                            "Continuing held work",
                            "runs.continue",
                            move |be| wops::run_control(be, "runs.continue", &w, &s),
                        );
                    }
                }
                Err(_) => {
                    self.work.continue_all.remove(&id);
                }
            },
            (WorkSlot::WorkEligibility(id), Reply::Query(q)) => {
                if let Ok(e) = wops::run_eligible(q) {
                    self.work.eligibility.insert(id, e);
                }
            }
            (WorkSlot::WorkRedirect(id), Reply::Command(c)) => {
                let sent = self.work.redirect_sent.remove(&id);
                match wops::run_redirected(c) {
                    Ok(_) => {
                        // A draft edited while the send ran is kept.
                        if sent.is_some_and(|s| {
                            self.work.redirect_drafts.get(&id).map(|d| d.trim()) == Some(s.as_str())
                        }) {
                            self.work.redirect_drafts.remove(&id);
                        }
                        self.announce("Redirect sent.");
                    }
                    Err(e) => self.work_failure("Redirect", e),
                }
                self.inspect(&id);
            }
            (WorkSlot::WorkStop(id), Reply::Command(c)) => {
                match wops::run_stopped(c) {
                    Ok(wops::Stopped::Applied) => self.announce("The work is stopped."),
                    Ok(wops::Stopped::Accepted) => {
                        self.announce("Stop accepted: the work stops at its next safe boundary.")
                    }
                    Err(e) => self.work_failure("Stop", e),
                }
                self.inspect(&id);
            }
            (WorkSlot::ArchiveCount(p, features), Reply::Query(q)) => {
                let key = (p.clone(), features);
                match wops::work_page(q) {
                    Ok((items, next)) => {
                        let entry = self.work.counting.entry(key.clone()).or_insert((0, 0));
                        entry.0 += items.len();
                        entry.1 += items.iter().filter(|w| !w.attempts.0.is_empty()).count();
                        match next {
                            Some(c) => {
                                let (op, family) = if features {
                                    ("features.list", wops::Family::Features)
                                } else {
                                    ("issues.list", wops::Family::Issues)
                                };
                                self.query(
                                    WorkSlot::ArchiveCount(p.clone(), features),
                                    "Counting running work",
                                    op,
                                    move |be| wops::work_list(be, family, &p, Some(&c)),
                                );
                            }
                            None => {
                                self.work.counted.insert(key.clone());
                                let other = (key.0.clone(), !features);
                                if self.work.counted.contains(&other) {
                                    let a =
                                        self.work.counting.get(&key).copied().unwrap_or_default();
                                    let b =
                                        self.work.counting.get(&other).copied().unwrap_or_default();
                                    self.work
                                        .archive_counts
                                        .insert(key.0.clone(), (a.0 + b.0, a.1 + b.1));
                                }
                            }
                        }
                    }
                    Err(_) => {
                        // Not counted: the confirmation says so rather than showing zero.
                        self.work.counting.remove(&key);
                    }
                }
            }
            (WorkSlot::WorkCreate(project), Reply::Command(c)) => match wops::work_changed(c) {
                Ok(w) => {
                    self.work.mutations += 1;
                    if let Some(saved) = self.work.create_saving.remove(&project)
                        && self.work.of.as_ref() == Some(&project)
                        && self.work.create.as_ref() == Some(&saved)
                    {
                        self.work.create = None;
                    }
                    let text = format!("Created {} {}.", kind_label(&w).to_lowercase(), w.title.0);
                    self.work.message = Some((Tone::Good, text.clone()));
                    self.announce(text);
                    self.load_work();
                }
                Err(e) => self.work_failure("Creating work", e),
            },
            (WorkSlot::WorkCommand(id), Reply::Command(c)) => {
                let unknown = is_unknown(&c);
                let outcome = match wops::approval_requested(c.clone()) {
                    Ok(p) => Ok(format!(
                        "Plan approval was requested ({}). The active plan and its work are unchanged until you decide.",
                        p.id.as_str()
                    )),
                    Err(_) => wops::work_changed(c).map(|w| {
                        self.work.detail.insert(id.clone(), w.clone());
                        format!("{} is saved.", w.title.0)
                    }),
                };
                if !unknown {
                    self.work.moving.remove(&id);
                }
                match outcome {
                    Ok(text) => {
                        self.work.mutations += 1;
                        // Only this item's saved edit is settled: an editor holding later edits
                        // stays open on the saved revision; a draft of other work is kept.
                        if let Some(saved) = self.work.edit_saving.remove(&id) {
                            let current = self.work.detail.get(&id).cloned();
                            self.work.edit =
                                edit_after_save(self.work.edit.take(), &saved, current);
                        }
                        if self
                            .work
                            .unlink
                            .as_ref()
                            .is_some_and(|u| u.id.as_str() == id)
                        {
                            self.work.unlink = None;
                        }
                        self.work.message = Some((Tone::Good, text.clone()));
                        self.announce(text);
                        self.load_work();
                        if let Some(w) = self.record(&id) {
                            self.load_detail(&w);
                        }
                    }
                    Err(e) => {
                        // The editor keeps its draft (an unknown outcome stays pending).
                        if !unknown {
                            self.work.edit_saving.remove(&id);
                        }
                        self.work_failure("The change", e)
                    }
                }
            }
            (WorkSlot::WorkOrder(_), Reply::Command(c)) => match wops::work_ordered(c) {
                Ok(_) => {
                    self.work.mutations += 1;
                    self.announce("The order was saved.");
                    self.load_work();
                }
                Err(e) => self.work_failure("Reordering", e),
            },
            (WorkSlot::WorkPlanDrop(id), Reply::Task(t)) => {
                let stored = match t {
                    Ok(TaskOutput::Stored { outcome, .. }) => ops::saved_artifact(outcome),
                    Ok(_) => Err(Failure::Other("unexpected reply".into())),
                    Err(e) => Err(Failure::Other(e)),
                };
                match (stored, self.work.plan_saving.get_mut(&id)) {
                    (Ok(record), Some(form)) => {
                        for drop in form.drops.values_mut().filter(|d| d.artifact.is_none()) {
                            drop.artifact = Some(record.artifact.clone());
                        }
                        let form = form.clone();
                        self.save_plan(form);
                    }
                    (Ok(_), None) => {}
                    (Err(e), _) => {
                        self.unsaved_plan(&id);
                        self.work_failure("Storing requirement decisions", e);
                    }
                }
            }
            (WorkSlot::WorkPlanText(id), Reply::Task(t)) => {
                let stored = match t {
                    Ok(TaskOutput::Stored { outcome, .. }) => ops::saved_artifact(outcome),
                    Ok(_) => Err(Failure::Other("unexpected reply".into())),
                    Err(e) => Err(Failure::Other(e)),
                };
                // The draft submitted is the form this feature's save started from, never the
                // editor nor another feature's save.
                match (stored, self.work.plan_saving.get_mut(&id)) {
                    (Ok(record), Some(form)) => {
                        form.content = Some(record.artifact.clone());
                        form.original = form.text.clone();
                        let form = form.clone();
                        self.submit_plan(&form, &record.artifact);
                    }
                    (Ok(_), None) => {}
                    (Err(e), _) => {
                        self.unsaved_plan(&id);
                        self.work_failure("Storing the plan text", e)
                    }
                }
            }
            (WorkSlot::WorkPlan(id), Reply::Command(c)) => {
                if is_unknown(&c) {
                    // Still pending: confirmed later with its exact frame.
                    self.work_failure("Saving the plan draft", Failure::Unknown);
                    return None;
                }
                let saved = self.work.plan_saving.remove(&id);
                match wops::work_changed(c) {
                    Ok(w) => {
                        self.work.mutations += 1;
                        self.work.detail.insert(id.clone(), w.clone());
                        if let Some(saved) = &saved {
                            self.work.plan = plan_after_save(self.work.plan.take(), saved, &w);
                        }
                        let text = format!("The plan draft of {} is saved.", w.title.0);
                        self.work.message = Some((Tone::Good, text.clone()));
                        self.announce(text);
                        self.load_work();
                        self.load_detail(&w);
                    }
                    Err(e) => {
                        if let Some(saved) = saved
                            && self.work.plan.is_none()
                        {
                            self.work.plan = Some(saved);
                        }
                        self.work_failure("Saving the plan draft", e)
                    }
                }
            }
            (WorkSlot::WorkRun(id), Reply::Command(c)) => {
                match wops::run_changed(c) {
                    Ok(s) => {
                        self.work.runs.insert(id.clone(), s);
                        self.announce("Run controls changed.");
                    }
                    Err(e) => self.work_failure("Run control", e),
                }
                self.inspect(&id);
                self.load_work();
            }
            (WorkSlot::GithubIssuePublish(issue_id), Reply::Command(c)) => {
                use devworks_core::protocol::{
                    CommandReply,
                    records::GithubIssueLinkResult,
                    results::{DomainDecision, OperationResult},
                };
                if let Ok(CommandReply::Recorded { receipt, .. }) = &c
                    && matches!(receipt.decision, DomainDecision::Accepted { .. })
                {
                    self.work.message =
                        Some((Tone::Accent, "GitHub issue publication queued.".into()));
                    self.inspect(&issue_id);
                } else {
                    match ops::command_result(c) {
                        Ok(OperationResult::IssuesPublish {
                            value: GithubIssueLinkResult::Current(link),
                        }) => {
                            self.work.github_links.insert(issue_id.clone(), link);
                            self.work.message =
                                Some((Tone::Good, "Published issue to GitHub.".into()));
                            self.inspect(&issue_id);
                            self.load_work();
                        }
                        Err(e) => self.work_failure("Publishing issue to GitHub", e),
                        _ => {}
                    }
                }
            }
            (WorkSlot::GithubIssueLink(issue_id), Reply::Command(c)) => {
                use devworks_core::protocol::{
                    records::GithubIssueLinkResult, results::OperationResult,
                };
                match ops::command_result(c) {
                    Ok(OperationResult::IssuesLink {
                        value: GithubIssueLinkResult::Current(link),
                    }) => {
                        self.work.github_link_number_drafts.remove(&issue_id);
                        self.work.github_links.insert(issue_id.clone(), link);
                        self.work.message = Some((Tone::Good, "Linked GitHub issue.".into()));
                        self.inspect(&issue_id);
                        self.load_work();
                    }
                    Err(e) => self.work_failure("Linking GitHub issue", e),
                    _ => {}
                }
            }
            (WorkSlot::GithubPullChecks(pr_url), Reply::Query(q)) => {
                use devworks_core::protocol::results::OperationResult;
                match ops::query_result(q).map(|(r, _)| r) {
                    Ok(OperationResult::GithubChecks { value }) => {
                        self.work.github_pr_checks.insert(pr_url, value.items.0);
                    }
                    Err(e) => self.work.message = Some((Tone::Bad, e.describe())),
                    _ => {}
                }
            }
            (WorkSlot::GithubPullReviews(pr_url), Reply::Query(q)) => {
                use devworks_core::protocol::results::OperationResult;
                match ops::query_result(q).map(|(r, _)| r) {
                    Ok(OperationResult::GithubReviews { value }) => {
                        self.work.github_pr_reviews.insert(pr_url, value.items.0);
                    }
                    Err(e) => self.work.message = Some((Tone::Bad, e.describe())),
                    _ => {}
                }
            }
            (slot, reply) => return Some((slot, reply)),
        }
        None
    }

    pub(super) fn work_failure(&mut self, what: &str, e: Failure) {
        let text = match &e {
            Failure::Unknown => format!(
                "{what}: not yet confirmed whether it was applied. Use Confirm outcomes to check \
                 again; it is never applied twice."
            ),
            Failure::Refused(f) if f.code == DomainErrorCode::RevisionConflict => format!(
                "{what} was not applied: it changed meanwhile. Your draft is kept; review the \
                 current version and try again."
            ),
            Failure::Refused(f) => {
                let kind = f.target_kind.as_ref().map(|k| k.0.as_str()).unwrap_or("");
                let why = match kind {
                    "dependency_cycle" => "that link would create a dependency cycle.".to_owned(),
                    "plan_scope" => {
                        "it is outside the feature's approved scope; change the plan instead."
                            .to_owned()
                    }
                    "project_archived" => "the project is archived.".to_owned(),
                    "work_completed" => {
                        "completed work keeps its prerequisites; reopen it first.".to_owned()
                    }
                    "requirement_missing" => {
                        "a requirement of the approved plan is missing from the draft.".to_owned()
                    }
                    "plan_changed" => "the plan changed meanwhile.".to_owned(),
                    "feature_closed" => "that feature is finished.".to_owned(),
                    _ => e.describe(),
                };
                format!("{what} was not applied: {why}")
            }
            other => format!("{what} was not applied: {}", other.describe()),
        };
        // A refused or failed change keeps its form (the user's draft); only a confirmed
        // change clears it.
        self.work.message = Some((Tone::Bad, text.clone()));
        self.announce(text);
        if matches!(&e, Failure::Refused(_)) {
            self.load_work();
        }
    }
}
