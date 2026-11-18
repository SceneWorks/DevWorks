//! The binding of a list pane to one paged query (DW-10): the request to send, the reply to
//! apply, and the body that shows every state the pane can be in, with a control that reads the
//! next page.
//!
//! A screen owns a [`Paged`] per list. It asks [`Paged::reload`] for the first page or
//! [`Paged::more_page`] for the next, sends the request the returned [`Fetch`] describes (the
//! generation and cursor go into the query), and hands the reply to [`Paged::apply`]. A reply
//! for a request that has since been replaced, or for a list that was [`reset`](Paged::reset),
//! is refused, so a late page never lands in the wrong list. [`list_body`] draws offline,
//! loading, failed, stale, empty, filtered and populated states and the load-more control.
use crate::theme;
use crate::virtual_list::{self, Scrolls};
use crate::widgets::{self, ViewState};
use eframe::egui::{self, RichText, Ui};

/// One page of a list reply: its rows and where the next page starts (`None`: the last page).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next: Option<String>,
}

/// A request to send: the generation to return with the reply and the cursor to read after
/// (`None`: the first page).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fetch {
    pub generation: u64,
    pub cursor: Option<String>,
}
impl Fetch {
    /// Whether the reply continues the shown list rather than replacing it.
    pub fn append(&self) -> bool {
        self.cursor.is_some()
    }
}

/// What applying a reply did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Applied {
    /// The reply answered a request that is no longer the current one; nothing changed.
    Stale,
    /// The first page replaced the list.
    Replaced,
    /// A further page was added to the list.
    Appended,
    /// The request failed; the list keeps what it had and shows the error.
    Failed,
    /// A gathering read holds a page and needs the next: send this request.
    Continue(Fetch),
}

#[derive(Clone, Copy, Debug)]
struct Inflight {
    generation: u64,
    append: bool,
    /// The read gathers pages, up to this many, before it replaces the list.
    gather: Option<usize>,
}

/// A list bound to one paged query.
#[derive(Debug)]
pub struct Paged<T> {
    pub items: Vec<T>,
    /// The first page has arrived.
    pub loaded: bool,
    pub error: Option<String>,
    next: Option<String>,
    generation: u64,
    inflight: Option<Inflight>,
    /// The last failure was a further page (retry reads that page, not the first).
    failed_append: bool,
    /// Pages of a gathering read so far; the list on screen stays until the last one arrives.
    staged: Vec<T>,
    staged_pages: usize,
    /// How many pages the rows shown came from.
    pages: usize,
}
impl<T> Default for Paged<T> {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            loaded: false,
            error: None,
            next: None,
            generation: 0,
            inflight: None,
            failed_append: false,
            staged: Vec::new(),
            staged_pages: 0,
            pages: 0,
        }
    }
}

impl<T> Paged<T> {
    /// Another page exists after the rows shown.
    pub fn more(&self) -> bool {
        self.next.is_some()
    }
    /// A request is in flight.
    pub fn loading(&self) -> bool {
        self.inflight.is_some()
    }
    /// The newest request's generation.
    pub fn generation(&self) -> u64 {
        self.generation
    }
    /// Starts reading the first page. The rows shown stay until it replies, so a refresh never
    /// blanks the list; it replaces every page read so far.
    pub fn reload(&mut self) -> Fetch {
        self.start(None, false, None)
    }
    /// How many pages the rows shown came from (a refresh reads as many again).
    pub fn pages(&self) -> usize {
        self.pages
    }
    /// Starts re-reading the rows shown: the first page and as many after it as are shown, or
    /// requested and still in flight. The rows stay until the last of them arrives, so a
    /// refresh keeps a list the user expanded.
    pub fn refresh(&mut self) -> Fetch {
        let requested = self.pages + usize::from(self.inflight.is_some_and(|f| f.append));
        self.staged.clear();
        self.staged_pages = 0;
        self.start(None, false, Some(requested.max(1)))
    }
    /// Starts reading every page, one after another ([`Applied::Continue`] names each next
    /// request). The rows shown stay until the last page arrives, so a list that must be
    /// complete to be correct (a board) never shows a part of itself.
    pub fn reload_all(&mut self) -> Fetch {
        self.staged.clear();
        self.staged_pages = 0;
        self.start(None, false, Some(usize::MAX))
    }
    /// Starts reading the next page; `None` when there is none or a request is in flight.
    pub fn more_page(&mut self) -> Option<Fetch> {
        if self.inflight.is_some() {
            return None;
        }
        let cursor = self.next.clone()?;
        Some(self.start(Some(cursor), true, None))
    }
    fn start(&mut self, cursor: Option<String>, append: bool, gather: Option<usize>) -> Fetch {
        self.generation += 1;
        self.inflight = Some(Inflight {
            generation: self.generation,
            append,
            gather,
        });
        Fetch {
            generation: self.generation,
            cursor,
        }
    }
    /// Applies the reply of the request `generation` named.
    pub fn apply(&mut self, generation: u64, reply: Result<Page<T>, String>) -> Applied {
        self.apply_with(generation, reply, None)
    }
    /// [`apply`](Self::apply) for rows that carry a key: a row already listed is not listed
    /// twice when pages overlap because the list changed between reads.
    pub fn apply_unique(
        &mut self,
        generation: u64,
        reply: Result<Page<T>, String>,
        key: impl Fn(&T) -> String,
    ) -> Applied {
        self.apply_with(generation, reply, Some(&key))
    }
    fn apply_with(
        &mut self,
        generation: u64,
        reply: Result<Page<T>, String>,
        key: Option<&dyn Fn(&T) -> String>,
    ) -> Applied {
        let Some(inflight) = self.inflight.filter(|f| f.generation == generation) else {
            return Applied::Stale;
        };
        self.inflight = None;
        match reply {
            Ok(page)
                if inflight
                    .gather
                    .is_some_and(|limit| page.next.is_some() && self.staged_pages + 1 < limit) =>
            {
                Self::join(&mut self.staged, page.items, key);
                self.staged_pages += 1;
                let limit = inflight.gather;
                let fetch = self.start(page.next, true, limit);
                Applied::Continue(fetch)
            }
            Ok(page) => {
                let appended = inflight.append && inflight.gather.is_none();
                if inflight.gather.is_some() {
                    Self::join(&mut self.staged, page.items, key);
                    self.items = std::mem::take(&mut self.staged);
                    self.pages = self.staged_pages + 1;
                    self.staged_pages = 0;
                } else if appended {
                    Self::join(&mut self.items, page.items, key);
                    self.pages += 1;
                } else {
                    self.items = page.items;
                    self.pages = 1;
                }
                self.next = page.next;
                self.loaded = true;
                self.error = None;
                self.failed_append = false;
                if appended {
                    Applied::Appended
                } else {
                    Applied::Replaced
                }
            }
            Err(error) => {
                self.staged.clear();
                self.staged_pages = 0;
                self.error = Some(error);
                self.failed_append = inflight.append && inflight.gather.is_none();
                Applied::Failed
            }
        }
    }
    fn join(into: &mut Vec<T>, more: Vec<T>, key: Option<&dyn Fn(&T) -> String>) {
        match key {
            None => into.extend(more),
            Some(key) => {
                let mut seen: std::collections::BTreeSet<String> = into.iter().map(key).collect();
                into.extend(more.into_iter().filter(|r| seen.insert(key(r))));
            }
        }
    }
    /// Applies a reply whose slot cannot carry a generation: the slot's newest request is the
    /// one in flight (an older reply was superseded before it got here), so a reply with none
    /// in flight is stale.
    pub fn apply_current(&mut self, reply: Result<Page<T>, String>) -> Applied {
        match self.inflight {
            Some(f) => self.apply(f.generation, reply),
            None => Applied::Stale,
        }
    }
    /// Gives up the request `generation` named without an outcome (its cursor expired, say),
    /// so the caller can start over. `false` when it is no longer the current request.
    pub fn abandon(&mut self, generation: u64) -> bool {
        let current = self.inflight.is_some_and(|f| f.generation == generation);
        if current {
            self.inflight = None;
        }
        current
    }
    /// Shows `items` as a complete list that arrived without a request (it was read elsewhere).
    pub fn replace(&mut self, items: Vec<T>) {
        self.items = items;
        self.loaded = true;
        self.error = None;
        self.next = None;
        self.failed_append = false;
        self.pages = 1;
        self.staged.clear();
        self.staged_pages = 0;
        self.inflight = None;
        self.generation += 1;
    }
    /// Forgets the rows and any request in flight: the list is bound to another query.
    pub fn reset(&mut self) {
        self.items.clear();
        self.loaded = false;
        self.error = None;
        self.next = None;
        self.inflight = None;
        self.failed_append = false;
        self.staged.clear();
        self.staged_pages = 0;
        self.pages = 0;
        self.generation += 1;
    }
    /// One list over several pages-in-parallel lists (one per scope): every row in order, loaded
    /// once all are, the first error, more while any has more, loading while any is.
    pub fn merged<'a>(parts: impl IntoIterator<Item = &'a Paged<T>>) -> Paged<T>
    where
        T: Clone + 'a,
    {
        let parts: Vec<&Paged<T>> = parts.into_iter().collect();
        Paged {
            items: parts.iter().flat_map(|p| p.items.iter().cloned()).collect(),
            loaded: !parts.is_empty() && parts.iter().all(|p| p.loaded),
            error: parts.iter().find_map(|p| p.error.clone()),
            next: parts.iter().any(|p| p.more()).then(String::new),
            generation: 0,
            inflight: parts.iter().any(|p| p.loading()).then_some(Inflight {
                generation: 0,
                append: true,
                gather: None,
            }),
            failed_append: parts.iter().any(|p| p.failed_append),
            staged: Vec::new(),
            staged_pages: 0,
            pages: parts.iter().map(|p| p.pages).max().unwrap_or(0),
        }
    }
    /// A copy holding the rows `keep` accepts and the same state, for a view that shows part of
    /// the list.
    pub fn view(&self, keep: impl Fn(&T) -> bool) -> Paged<T>
    where
        T: Clone,
    {
        Paged {
            items: self.items.iter().filter(|i| keep(i)).cloned().collect(),
            loaded: self.loaded,
            error: self.error.clone(),
            next: self.next.clone(),
            generation: self.generation,
            inflight: self.inflight,
            failed_append: self.failed_append,
            staged: Vec::new(),
            staged_pages: 0,
            pages: self.pages,
        }
    }
}

/// Texts of a bound list's states.
pub struct ListText<'a> {
    pub what: &'a str,
    pub empty_title: &'a str,
    pub empty_body: &'a str,
    pub id_salt: &'a str,
}

/// The label of the control that reads the next page.
pub const LOAD_MORE: &str = "Load more";

/// The states that come before any row: offline, a failed first read, a failed refresh or
/// further page (the rows stay, with a retry) and loading. `false` when there are no rows to
/// draw yet. `reload` or `more` is set when the user retries the read that failed.
pub fn gate<T>(
    ui: &mut Ui,
    online: &Result<(), String>,
    list: &Paged<T>,
    what: &str,
    reload: &mut bool,
    more: &mut bool,
) -> bool {
    if let Err(reason) = online {
        widgets::state_block(
            ui,
            &ViewState::Offline {
                reason: reason.clone(),
            },
        );
        return false;
    }
    if let Some(e) = &list.error {
        let state = if list.loaded {
            ViewState::Stale { what: what.into() }
        } else {
            ViewState::Failed {
                what: what.into(),
                error: e.clone(),
            }
        };
        let mut retry = widgets::state_block(ui, &state) == widgets::StateAction::Retry;
        if list.loaded {
            ui.label(RichText::new(e).color(theme::MUTED));
            retry = ui.button("Retry").clicked();
        }
        if retry {
            if list.failed_append {
                *more = true;
            } else {
                *reload = true;
            }
        }
        if !list.loaded {
            return false;
        }
    }
    if !list.loaded {
        widgets::state_block(ui, &ViewState::Loading { what: what.into() });
        return false;
    }
    true
}

/// The body of a list bound to one paged query: offline, failed, loading, empty and filtered
/// states, then the rows (virtualized) with the remembered selection and a load-more control.
/// `reload` is set when the user asks to read the list again, `more` when they ask for the next
/// page.
#[allow(clippy::too_many_arguments)]
pub fn list_body<T>(
    ui: &mut Ui,
    online: &Result<(), String>,
    list: &Paged<T>,
    text: ListText<'_>,
    reload: &mut bool,
    more: &mut bool,
    scrolls: &mut Scrolls,
    haystack: impl Fn(&T) -> String,
    key: impl Fn(&T) -> String,
    filter: &str,
    selection: &mut Option<String>,
    mut row: impl FnMut(&mut Ui, &T, bool) -> bool,
) {
    if !gate(ui, online, list, text.what, reload, more) {
        return;
    }
    let terms: Vec<String> = filter.split_whitespace().map(str::to_lowercase).collect();
    let rows: Vec<&T> = list
        .items
        .iter()
        .filter(|i| {
            let t = haystack(i).to_lowercase();
            terms.iter().all(|w| t.contains(w))
        })
        .collect();
    if rows.is_empty() {
        let none = list.items.is_empty() && !list.more();
        widgets::state_block(
            ui,
            &ViewState::Empty {
                title: if none {
                    text.empty_title.into()
                } else {
                    "Nothing matches the filter".into()
                },
                body: if none {
                    text.empty_body.into()
                } else if list.items.is_empty() {
                    "Read more to see further entries.".into()
                } else {
                    "Clear or change the filter, or read more entries.".into()
                },
            },
        );
        load_more_control(ui, list, more);
        return;
    }
    widgets::caption(
        ui,
        format!(
            "{} of {} shown{}",
            rows.len(),
            list.items.len(),
            if list.more() { " · more exist" } else { "" }
        ),
    );
    let selected = selection.clone();
    let mut picked = None;
    virtual_list::rows(ui, scrolls, text.id_salt, 28.0, rows.len(), |ui, i| {
        let item = rows[i];
        let k = key(item);
        if row(ui, item, selected.as_deref() == Some(k.as_str())) {
            picked = Some(k);
        }
    });
    if let Some(p) = picked {
        *selection = Some(p);
    }
    load_more_control(ui, list, more);
}

/// The control that reads the next page (nothing when the last page is shown).
pub fn load_more_control<T>(ui: &mut Ui, list: &Paged<T>, more: &mut bool) {
    if !list.more() {
        return;
    }
    let loading = list.loading();
    ui.horizontal(|ui| {
        let label = if loading {
            "Loading more…"
        } else {
            LOAD_MORE
        };
        if ui.add_enabled(!loading, egui::Button::new(label)).clicked() {
            *more = true;
        }
        if loading {
            ui.add(egui::Spinner::new().size(14.0));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(items: &[u32], next: Option<&str>) -> Page<u32> {
        Page {
            items: items.to_vec(),
            next: next.map(str::to_owned),
        }
    }

    #[test]
    fn a_first_page_replaces_the_rows_and_remembers_where_the_next_starts() {
        let mut list = Paged::<u32>::default();
        let first = list.reload();
        assert_eq!(first.cursor, None);
        assert!(list.loading() && !list.loaded);
        assert_eq!(
            list.apply(first.generation, Ok(page(&[1, 2], Some("c1")))),
            Applied::Replaced
        );
        assert!(list.loaded && list.more() && !list.loading());
        assert_eq!(list.items, [1, 2]);
    }

    #[test]
    fn the_next_page_is_read_after_the_cursor_and_appended() {
        let mut list = Paged::<u32>::default();
        let first = list.reload();
        list.apply(first.generation, Ok(page(&[1], Some("c1"))));
        let next = list.more_page().expect("a page follows");
        assert_eq!(next.cursor.as_deref(), Some("c1"));
        assert!(next.append());
        assert!(list.more_page().is_none(), "one request at a time");
        assert_eq!(
            list.apply(next.generation, Ok(page(&[2, 3], None))),
            Applied::Appended
        );
        assert_eq!(list.items, [1, 2, 3]);
        assert!(!list.more() && list.more_page().is_none());
    }

    #[test]
    fn a_reply_to_a_replaced_request_is_refused() {
        let mut list = Paged::<u32>::default();
        let old = list.reload();
        let new = list.reload();
        assert_eq!(
            list.apply(old.generation, Ok(page(&[9], None))),
            Applied::Stale
        );
        assert!(list.items.is_empty() && list.loading());
        assert_eq!(
            list.apply(new.generation, Ok(page(&[1], None))),
            Applied::Replaced
        );
    }

    #[test]
    fn a_reply_after_a_reset_is_refused() {
        let mut list = Paged::<u32>::default();
        let first = list.reload();
        list.reset();
        assert_eq!(
            list.apply(first.generation, Ok(page(&[1], None))),
            Applied::Stale
        );
        assert!(!list.loaded && list.items.is_empty());
    }

    #[test]
    fn a_failure_keeps_the_rows_and_a_retry_reads_the_page_that_failed() {
        let mut list = Paged::<u32>::default();
        let first = list.reload();
        list.apply(first.generation, Ok(page(&[1], Some("c1"))));
        let next = list.more_page().unwrap();
        assert_eq!(
            list.apply(next.generation, Err("busy".into())),
            Applied::Failed
        );
        assert_eq!(list.items, [1]);
        assert_eq!(list.error.as_deref(), Some("busy"));
        assert!(list.more(), "the cursor is kept for the retry");
        let retry = list.more_page().unwrap();
        assert_eq!(retry.cursor.as_deref(), Some("c1"));
        list.apply(retry.generation, Ok(page(&[2], None)));
        assert!(list.error.is_none());
        assert_eq!(list.items, [1, 2]);
    }

    #[test]
    fn a_refresh_replaces_every_page_read_so_far() {
        let mut list = Paged::<u32>::default();
        let first = list.reload();
        list.apply(first.generation, Ok(page(&[1], Some("c1"))));
        let next = list.more_page().unwrap();
        list.apply(next.generation, Ok(page(&[2], None)));
        let again = list.reload();
        assert_eq!(list.items, [1, 2], "the rows stay until the reply");
        list.apply(again.generation, Ok(page(&[7], Some("d1"))));
        assert_eq!(list.items, [7]);
        assert!(list.more());
    }

    #[test]
    fn a_gathering_read_shows_nothing_new_until_the_last_page() {
        let mut list = Paged::<u32>::default();
        let first = list.reload();
        list.apply(first.generation, Ok(page(&[1], None)));
        let all = list.reload_all();
        let Applied::Continue(second) = list.apply(all.generation, Ok(page(&[7], Some("c1"))))
        else {
            panic!("a page follows");
        };
        assert_eq!(second.cursor.as_deref(), Some("c1"));
        assert_eq!(list.items, [1], "the earlier rows stay while pages arrive");
        let Applied::Continue(third) = list.apply(second.generation, Ok(page(&[8], Some("c2"))))
        else {
            panic!("a page follows");
        };
        assert_eq!(
            list.apply(third.generation, Ok(page(&[9], None))),
            Applied::Replaced
        );
        assert_eq!(list.items, [7, 8, 9]);
        assert!(!list.more() && !list.loading());
    }

    #[test]
    fn a_gathering_read_that_fails_keeps_the_rows_and_retries_from_the_first_page() {
        let mut list = Paged::<u32>::default();
        let first = list.reload();
        list.apply(first.generation, Ok(page(&[1], None)));
        let all = list.reload_all();
        let Applied::Continue(second) = list.apply(all.generation, Ok(page(&[7], Some("c1"))))
        else {
            panic!("a page follows");
        };
        assert_eq!(
            list.apply(second.generation, Err("gone".into())),
            Applied::Failed
        );
        assert_eq!(list.items, [1]);
        let again = list.reload_all();
        assert!(!again.append());
        assert_eq!(
            list.apply(again.generation, Ok(page(&[2], None))),
            Applied::Replaced
        );
        assert_eq!(list.items, [2], "pages of the failed read are not kept");
    }

    #[test]
    fn a_refresh_reads_again_as_many_pages_as_are_shown_and_keeps_the_cursor() {
        let mut list = Paged::<u32>::default();
        let first = list.reload();
        list.apply(first.generation, Ok(page(&[1], Some("c1"))));
        let next = list.more_page().unwrap();
        list.apply(next.generation, Ok(page(&[2], Some("c2"))));
        assert_eq!(list.pages(), 2);
        let again = list.refresh();
        let Applied::Continue(second) = list.apply(again.generation, Ok(page(&[10], Some("d1"))))
        else {
            panic!("a second page is re-read");
        };
        assert_eq!(second.cursor.as_deref(), Some("d1"));
        assert_eq!(list.items, [1, 2], "the rows stay while it re-reads");
        assert_eq!(
            list.apply(second.generation, Ok(page(&[11], Some("d2")))),
            Applied::Replaced
        );
        assert_eq!(list.items, [10, 11]);
        assert_eq!(list.pages(), 2);
        let more = list.more_page().unwrap();
        assert_eq!(
            more.cursor.as_deref(),
            Some("d2"),
            "reading on continues after d2"
        );
    }

    #[test]
    fn a_refresh_during_a_further_page_keeps_that_page() {
        let mut list = Paged::<u32>::default();
        let first = list.reload();
        list.apply(first.generation, Ok(page(&[1], Some("c1"))));
        let pending = list.more_page().unwrap();
        let again = list.refresh();
        assert_eq!(
            list.apply(pending.generation, Ok(page(&[2], None))),
            Applied::Stale,
            "the replaced request's reply is dropped"
        );
        let Applied::Continue(second) = list.apply(again.generation, Ok(page(&[10], Some("d1"))))
        else {
            panic!("the requested page is re-read too");
        };
        list.apply(second.generation, Ok(page(&[11], None)));
        assert_eq!(list.items, [10, 11]);
        assert!(!list.more());
    }

    #[test]
    fn rows_listed_twice_by_overlapping_pages_are_kept_once() {
        let mut list = Paged::<u32>::default();
        let first = list.reload();
        list.apply(first.generation, Ok(page(&[1, 2], Some("c1"))));
        let next = list.more_page().unwrap();
        list.apply_unique(next.generation, Ok(page(&[2, 3], None)), |n| n.to_string());
        assert_eq!(list.items, [1, 2, 3]);
    }

    #[test]
    fn an_abandoned_request_can_be_started_again() {
        let mut list = Paged::<u32>::default();
        let first = list.reload();
        list.apply(first.generation, Ok(page(&[1], Some("c1"))));
        let next = list.more_page().unwrap();
        assert!(list.abandon(next.generation));
        assert!(!list.loading());
        assert!(!list.abandon(next.generation));
    }

    #[test]
    fn a_view_keeps_the_state_and_the_accepted_rows() {
        let mut list = Paged::<u32>::default();
        let first = list.reload();
        list.apply(first.generation, Ok(page(&[1, 2, 3], Some("c"))));
        let odd = list.view(|n| n % 2 == 1);
        assert_eq!(odd.items, [1, 3]);
        assert!(odd.loaded && odd.more());
    }
}
