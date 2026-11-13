//! One desktop screen behind one trait. A screen owns its view state, its slot type (the
//! requests it waits for) and the reply, refresh, restore and purge handling of those slots;
//! the shell reaches it through [`DynScreen`] by the [`ScreenId`] a route or slot names.
//! Screens not migrated yet are [`ScreenId::Legacy`] and keep their code in the shell.
use crate::backend::{Backend, Client, Reply, Slot, Unresolved};
use crate::memory::RouteMemory;
use crate::project_purge::Live;
use crate::route::Route;
use devworks_core::ports::storage::ProjectPurgeClientScope;
use eframe::egui::Ui;
use serde_json::Value;
use std::fmt::Debug;

/// The screen that owns a route or a slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScreenId {
    /// Still handled inside the shell.
    Legacy,
    Help,
}
impl ScreenId {
    /// The screen's key in persisted pending state (`PendingMemory::screens`).
    pub fn key(self) -> &'static str {
        match self {
            Self::Legacy => "legacy",
            Self::Help => "help",
        }
    }
}

/// Work a screen asks of the shell. The shell runs it right after the screen call returns.
#[derive(Debug)]
pub enum Effect {
    Navigate(Route),
}

/// What the coordinator is told about the visible screen.
#[derive(Debug, Default)]
pub struct ScreenContext {
    pub tab: usize,
    pub selection: Option<String>,
}

/// The shell state a screen may use during one call.
pub struct Cx<'a> {
    client: Option<&'a mut Client>,
    online: Result<(), String>,
    status: &'a mut Option<String>,
    route: Route,
    effects: Vec<Effect>,
}
impl<'a> Cx<'a> {
    pub fn new(
        client: Option<&'a mut Client>,
        online: Result<(), String>,
        status: &'a mut Option<String>,
        route: Route,
    ) -> Self {
        Self {
            client,
            online,
            status,
            route,
            effects: Vec::new(),
        }
    }
    /// The application, or why requests cannot be made now.
    pub fn online(&self) -> Result<(), &str> {
        self.online.as_ref().map(|_| ()).map_err(String::as_str)
    }
    /// The route shown when the call started.
    pub fn route(&self) -> Route {
        self.route
    }
    pub fn navigate(&mut self, route: Route) {
        self.effects.push(Effect::Navigate(route));
    }
    pub fn into_effects(self) -> Vec<Effect> {
        self.effects
    }
    /// Submits a query; `false` when nothing was submitted.
    pub fn query(
        &mut self,
        slot: impl Into<Slot>,
        label: &str,
        op: &'static str,
        frame: impl FnOnce(&dyn Backend) -> Vec<u8>,
    ) -> bool {
        let online = self.online.is_ok();
        submit_query(
            self.client.as_deref_mut(),
            online,
            slot.into(),
            label,
            op,
            frame,
        )
    }
    /// Submits a command; `false` when it was not submitted now.
    pub fn command(
        &mut self,
        slot: impl Into<Slot>,
        label: &str,
        op: &'static str,
        frame: impl FnOnce(&dyn Backend) -> Vec<u8>,
    ) -> bool {
        let online = self.online.is_ok();
        submit_command(
            self.client.as_deref_mut(),
            online,
            self.status,
            slot.into(),
            label,
            op,
            frame,
        )
    }
    /// Reports a reply its screen has no handling for (`slot` is the screen's own slot).
    pub fn unhandled(&mut self, slot: &dyn Debug, reply: Reply) {
        *self.status = Some(match reply {
            Reply::Task(Err(e)) => format!("A request for {slot:?} failed: {e}"),
            _ => format!("Unexpected reply for {slot:?}"),
        });
    }
}

/// Submits a query; `false` when nothing was submitted (offline, or closing: the shell admits
/// nothing new once the window is closing).
pub(crate) fn submit_query(
    client: Option<&mut Client>,
    online: bool,
    slot: Slot,
    label: &str,
    op: &'static str,
    frame: impl FnOnce(&dyn Backend) -> Vec<u8>,
) -> bool {
    if !online {
        return false;
    }
    let Some(client) = client else {
        return false;
    };
    let backend = client.backend().clone();
    let frame = frame(&*backend);
    client.query(slot, label, op, frame)
}

/// Submits a command; `false` when it was not submitted now (offline or closing; a closed
/// client keeps it unresolved). A caller that marked state for it restores that state.
pub(crate) fn submit_command(
    client: Option<&mut Client>,
    online: bool,
    status: &mut Option<String>,
    slot: Slot,
    label: &str,
    op: &'static str,
    frame: impl FnOnce(&dyn Backend) -> Vec<u8>,
) -> bool {
    if !online {
        return false;
    }
    let Some(client) = client else {
        return false;
    };
    // A slot whose earlier command has no definite outcome keeps it until Confirm outcomes
    // settles that exact frame: a new request there would replace it (and could commit a
    // second time under a fresh request key).
    if client.unresolved().iter().any(|u| u.slot == slot) {
        *status = Some(format!(
            "{label}: an earlier request here is not confirmed yet. Use Confirm outcomes \
             first; nothing is applied twice."
        ));
        return false;
    }
    let backend = client.backend().clone();
    let frame = frame(&*backend);
    client.command(slot, label, op, frame)
}

/// One screen. `Slot` variant names are persisted in `ui-state.json`: they stay unique across
/// every screen's slot type and never change.
pub trait Screen {
    type Slot: Into<Slot> + TryFrom<Slot, Error = Slot> + Debug;
    const ID: ScreenId;
    /// The screen was opened (`route` is one of the routes it owns).
    fn load(&mut self, cx: &mut Cx, route: Route);
    /// A hint or the periodic repair re-reads what the open screen shows.
    fn refresh(&mut self, cx: &mut Cx, route: Route);
    fn apply(&mut self, cx: &mut Cx, slot: Self::Slot, reply: Reply);
    fn ui(&mut self, cx: &mut Cx, route: Route, ui: &mut Ui);
    /// What the screen keeps for its unconfirmed commands (`unresolved`) across a restart.
    fn pending_memory(&self, _unresolved: &[&Unresolved]) -> Option<Value> {
        None
    }
    fn restore_pending(&mut self, _saved: &Value) {}
    /// A command restored unconfirmed at launch: mark the state its reply settles.
    fn restore_in_flight(&mut self, _slot: &Self::Slot, _u: &Unresolved) {}
    /// Unsaved editor state a project purge must see.
    fn purge_live(&self, _out: &mut Live) {}
    /// Drops state of a purged project.
    fn remove_purged(&mut self, _scope: &ProjectPurgeClientScope) {}
    fn context(&self, route: Route, mem: &RouteMemory) -> ScreenContext;
}

/// [`Screen`] without its slot type, for the shell's registry.
pub trait DynScreen {
    fn id(&self) -> ScreenId;
    fn load(&mut self, cx: &mut Cx, route: Route);
    fn refresh(&mut self, cx: &mut Cx, route: Route);
    /// Applies a reply for one of this screen's slots.
    fn apply(&mut self, cx: &mut Cx, slot: Slot, reply: Reply);
    fn ui(&mut self, cx: &mut Cx, route: Route, ui: &mut Ui);
    fn pending_memory(&self, unresolved: &[&Unresolved]) -> Option<Value>;
    fn restore_pending(&mut self, saved: &Value);
    fn restore_in_flight(&mut self, u: &Unresolved);
    fn purge_live(&self, out: &mut Live);
    fn remove_purged(&mut self, scope: &ProjectPurgeClientScope);
    fn context(&self, route: Route, mem: &RouteMemory) -> ScreenContext;
}
impl<S: Screen> DynScreen for S {
    fn id(&self) -> ScreenId {
        S::ID
    }
    fn load(&mut self, cx: &mut Cx, route: Route) {
        Screen::load(self, cx, route);
    }
    fn refresh(&mut self, cx: &mut Cx, route: Route) {
        Screen::refresh(self, cx, route);
    }
    fn apply(&mut self, cx: &mut Cx, slot: Slot, reply: Reply) {
        match S::Slot::try_from(slot) {
            Ok(slot) => Screen::apply(self, cx, slot, reply),
            Err(slot) => cx.unhandled(&slot, reply),
        }
    }
    fn ui(&mut self, cx: &mut Cx, route: Route, ui: &mut Ui) {
        Screen::ui(self, cx, route, ui);
    }
    fn pending_memory(&self, unresolved: &[&Unresolved]) -> Option<Value> {
        Screen::pending_memory(self, unresolved)
    }
    fn restore_pending(&mut self, saved: &Value) {
        Screen::restore_pending(self, saved);
    }
    fn restore_in_flight(&mut self, u: &Unresolved) {
        if let Ok(slot) = S::Slot::try_from(u.slot.clone()) {
            Screen::restore_in_flight(self, &slot, u);
        }
    }
    fn purge_live(&self, out: &mut Live) {
        Screen::purge_live(self, out);
    }
    fn remove_purged(&mut self, scope: &ProjectPurgeClientScope) {
        Screen::remove_purged(self, scope);
    }
    fn context(&self, route: Route, mem: &RouteMemory) -> ScreenContext {
        Screen::context(self, route, mem)
    }
}
