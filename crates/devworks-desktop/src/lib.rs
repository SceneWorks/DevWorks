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
//!   fabricated values (UI-06). The coordinator (DW-16) is not part of this build, and the composer
//!   says so instead of pretending to send to a model. The composer's route and effort selectors
//!   are bound to the router ([`routing`]), which reads the provider connections configured in
//!   Settings → Provider connections ([`connections_screen`]); with none connected they report
//!   that no model is eligible rather than pretending one is.
//!
//! The approved UI reference is `docs/planning/ui-reference.md`; the HTML prototype is a design
//! reference only (ARCH-06: the shipped UI is egui, with no WebView).

// Replies and faults are core's wire DTOs carried by value, as in core itself; boxing them would
// change only ergonomics, and none sits on a hot path.
#![allow(clippy::large_enum_variant, clippy::result_large_err)]

pub mod addons;
pub mod artifacts_screen;
pub mod attachments;
pub mod backend;
pub mod chat;
pub mod connections_screen;
pub mod conventions_ops;
pub mod conventions_screen;
pub mod conventions_transfer;
pub mod decision_ops;
pub mod embedding_spaces_screen;
pub mod files;
pub mod graph_ops;
pub mod graph_screen;
pub mod help_ops;
pub mod help_screen;
pub mod host;
pub mod instructions_ops;
pub mod instructions_screen;
pub mod knowledge_screen;
pub mod mcp_panel;
pub mod memory;
pub mod native_notifications;
pub mod notifications;
pub mod ops;
pub mod platform;
pub mod project_purge;
pub mod route;
pub mod routing;
pub mod screen;
pub mod settings_screen;
pub mod shell;
pub mod theme;
pub mod usage_ops;
pub mod usage_screen;
pub mod virtual_list;
pub mod watch_ops;
pub mod watchers_screen;
pub mod web_panel;
pub mod widgets;
pub mod work_ops;

/// The application version shown in the sidebar footer.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod databases_ops;
pub mod databases_screen;
