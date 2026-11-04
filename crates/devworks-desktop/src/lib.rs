//! DevWorks native desktop shell (egui/eframe) and its composition root.
//!
//! # Boundary rationale (ARCH-02/03/06/07/08)
//!
//! * **One composition root.** [`host::Engine::start`] opens the single SQLite store, composes
//!   the core services over it and boots only through `devworks_engine::boot`, so recovery and
//!   every required consumer run before the first UI request is admitted (A3). Window close
//!   drives [`shell::Lifecycle`] to `Runtime::shutdown`; nothing keeps running in a tray.
//! * **The same operations as agent tools.** The shell reaches the application only through
//!   [`backend::Backend`]: framed queries and commands for host-selected operations, admitted by
//!   the runtime's gateway with the host-bound human context, and host-ingress upload tickets.
//!   There is no store, SQL or business rule here; views hold view state and drafts only.
//! * **Never blocking the frame loop.** [`backend::Client`] runs every request on worker
//!   threads and returns replies by slot; runtime "queue state may have changed" hints wake the
//!   loop and visible data is re-queried incrementally (a transcript fetches only entries after
//!   its tail cursor).
//! * **Testable without a window.** [`shell::Shell::ui`] renders into any `egui::Ui`; tests drive
//!   it headlessly with `egui_kittest` over the real runtime in a temporary data root and assert
//!   on the AccessKit tree. Native dialogs sit behind [`shell::Dialogs`].
//! * **Honest states.** Screens whose content belongs to later items keep their route, pane
//!   binding, integrated header, subviews and inner sidebars, and show designed unavailable,
//!   offline, loading, failed, stale and partial states ([`widgets::ViewState`]) — never
//!   fabricated values (UI-06). The coordinator (DW-16) and router (DW-17) are not part of this
//!   build, and the composer says so instead of pretending to send to a model.
//!
//! The approved UI reference is `docs/planning/ui-reference.md`; the HTML prototype is a design
//! reference only (ARCH-06: the shipped UI is egui, with no WebView).

// Replies and faults are core's wire DTOs carried by value, as in core itself; boxing them would
// change only ergonomics, and none sits on a hot path.
#![allow(clippy::large_enum_variant, clippy::result_large_err)]

pub mod addons;
pub mod attachments;
pub mod backend;
pub mod chat;
pub mod files;
pub mod host;
pub mod memory;
pub mod notifications;
pub mod ops;
pub mod platform;
pub mod route;
pub mod settings_screen;
pub mod shell;
pub mod theme;
pub mod virtual_list;
pub mod widgets;

/// The application version shown in the sidebar footer.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
