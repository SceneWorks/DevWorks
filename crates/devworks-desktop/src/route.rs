//! Typed navigation targets and the screen registry: each screen's stable ID, title, Help topic,
//! navigation group, subviews, inner sidebar and the item that delivers its content. The shell,
//! later coordinator screen context (COORD-09, DW-15/16) and Help bindings (DW-52) read the same
//! registry, so titles and topics cannot drift between hard-coded catalogs.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Route {
    Chat,
    Approvals,
    Portfolio,
    Overview,
    Features,
    Issues,
    Repositories,
    Agents,
    Artifacts,
    Knowledge,
    Dreaming,
    Databases,
    Jobs,
    Usage,
    Help,
    Settings,
    /// Resource reservations (reached from Agents).
    Resources,
    /// Indexing runs and logs (reached from Repositories).
    Indexing,
    /// Installed and available add-ons (reached from Settings).
    AddOns,
}

/// Where a route sits in the left navigation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NavGroup {
    /// Chat, Approvals (directly below Chat, UI-14) and All projects.
    Global,
    /// The selected project's screens.
    Project,
    /// The collapsible Intelligence group (UI-14).
    Intelligence,
    /// Jobs, usage, Help and Settings at the bottom.
    Bottom,
    /// Reached from another screen's header action, not the navigation.
    Secondary,
}

/// One registered screen.
#[derive(Debug)]
pub struct Screen {
    pub route: Route,
    /// Stable screen ID (screen context, Help bindings, persisted navigation).
    pub id: &'static str,
    pub title: &'static str,
    /// The header's one-line description.
    pub summary: &'static str,
    pub group: NavGroup,
    /// Local Help topic.
    pub help_topic: &'static str,
    /// Subviews, shown as tabs inside the integrated header.
    pub subviews: &'static [&'static str],
    /// Title of the right inner sidebar, where the approved design places one (UI-15).
    pub inner_sidebar: Option<&'static str>,
    /// Labels of the inline summary strip directly beneath the header (Overview, Features).
    pub summary_strip: &'static [&'static str],
    /// What the screen shows and the CodeGraph item that delivers it, when that content is not
    /// bound in this build. `None`: bound to an implemented service now.
    pub pending: Option<Pending>,
    /// A header action that opens a secondary route.
    pub link: Option<(&'static str, Route)>,
}

/// Content owned by a later item.
#[derive(Debug, Clone, Copy)]
pub struct Pending {
    pub what: &'static str,
    pub owner: &'static str,
}

const fn pending(what: &'static str, owner: &'static str) -> Option<Pending> {
    Some(Pending { what, owner })
}

pub static SCREENS: [Screen; 19] = [
    Screen {
        route: Route::Chat,
        id: "chat",
        title: "Workspace coordinator",
        summary: "One conversation across all your projects.",
        group: NavGroup::Global,
        help_topic: "chat",
        subviews: &[],
        inner_sidebar: Some("Current / Chat History"),
        summary_strip: &[],
        pending: None,
        link: None,
    },
    Screen {
        route: Route::Approvals,
        id: "approvals",
        title: "Approvals",
        summary: "Decisions waiting for you across every project.",
        group: NavGroup::Global,
        help_topic: "approvals",
        subviews: &["Inbox", "History"],
        inner_sidebar: Some("Decision"),
        summary_strip: &[],
        pending: pending("the all-project approval inbox and decisions", "DW-14"),
        link: None,
    },
    Screen {
        route: Route::Portfolio,
        id: "portfolio",
        title: "All projects",
        summary: "Every project in this workspace.",
        group: NavGroup::Global,
        help_topic: "projects",
        subviews: &["Active", "Archived", "Deleted"],
        inner_sidebar: Some("Project"),
        summary_strip: &[],
        pending: None,
        link: None,
    },
    Screen {
        route: Route::Overview,
        id: "overview",
        title: "Overview",
        summary: "The selected project at a glance, with its activity history.",
        group: NavGroup::Project,
        help_topic: "overview",
        subviews: &["Summary", "Activity"],
        inner_sidebar: None,
        summary_strip: &["Features", "Issues", "Working", "Needs you"],
        pending: pending(
            "project summary and Activity with filters and full history",
            "DW-12",
        ),
        link: None,
    },
    Screen {
        route: Route::Features,
        id: "features",
        title: "Features",
        summary: "Planned features and their delivery.",
        group: NavGroup::Project,
        help_topic: "features",
        subviews: &["List", "Board"],
        inner_sidebar: None,
        summary_strip: &["Planned", "In progress", "In review", "Complete"],
        pending: pending("features, task cards and the board", "DW-12"),
        link: None,
    },
    Screen {
        route: Route::Issues,
        id: "issues",
        title: "Issues",
        summary: "Standalone issues without a feature.",
        group: NavGroup::Project,
        help_topic: "issues",
        subviews: &["List", "History"],
        inner_sidebar: None,
        summary_strip: &[],
        pending: pending("standalone issues and their workspaces", "DW-12"),
        link: None,
    },
    Screen {
        route: Route::Repositories,
        id: "repositories",
        title: "Repositories",
        summary: "Source, structure, health and review history.",
        group: NavGroup::Project,
        help_topic: "repositories",
        subviews: &["Overview", "Nodes", "Graph", "Health", "Reviews"],
        inner_sidebar: Some("Repository"),
        summary_strip: &[],
        // Registrations, texts and index removal are bound (DW-11); the Nodes, Graph, Health and
        // Reviews subviews show their owners (DW-28, DW-29).
        pending: None,
        link: Some(("Indexing logs", Route::Indexing)),
    },
    Screen {
        route: Route::Agents,
        id: "agents",
        title: "Agents & sessions",
        summary: "Coordinators and workers in the selected project.",
        group: NavGroup::Project,
        help_topic: "agents",
        subviews: &["Active", "Finished"],
        inner_sidebar: None,
        summary_strip: &[],
        pending: pending(
            "assignments, agent transcripts and run controls",
            "DW-15, DW-9",
        ),
        link: Some(("Resource reservations", Route::Resources)),
    },
    Screen {
        route: Route::Artifacts,
        id: "artifacts",
        title: "Artifacts",
        summary: "Files and outputs kept by DevWorks.",
        group: NavGroup::Project,
        help_topic: "artifacts",
        subviews: &["Workspace", "Project"],
        inner_sidebar: None,
        summary_strip: &[],
        pending: None,
        link: None,
    },
    Screen {
        route: Route::Knowledge,
        id: "knowledge",
        title: "Knowledge",
        summary: "Claims, entities, evidence and documents.",
        group: NavGroup::Intelligence,
        help_topic: "knowledge",
        subviews: &["Entries", "Documents"],
        inner_sidebar: Some("Entry"),
        summary_strip: &[],
        pending: pending("Knowledge entries and retained documents", "DW-30, DW-35"),
        link: None,
    },
    Screen {
        route: Route::Dreaming,
        id: "dreaming",
        title: "Dreaming",
        summary: "Knowledge consolidation runs and their proposals.",
        group: NavGroup::Intelligence,
        help_topic: "dreaming",
        subviews: &["Proposals", "Coordinator"],
        inner_sidebar: Some("Run history"),
        summary_strip: &[],
        pending: pending("dreaming runs, proposals and revert", "DW-32"),
        link: None,
    },
    Screen {
        route: Route::Databases,
        id: "databases",
        title: "Databases",
        summary: "Indexed database structure and separately permitted access.",
        group: NavGroup::Intelligence,
        help_topic: "databases",
        subviews: &["Schema", "Relationships", "Access"],
        inner_sidebar: Some("Database sources"),
        summary_strip: &[],
        pending: pending("database schema sources and access", "DW-36, DW-37"),
        link: None,
    },
    Screen {
        route: Route::Jobs,
        id: "jobs",
        title: "Scheduled jobs",
        summary: "Durable schedules and manual jobs.",
        group: NavGroup::Bottom,
        help_topic: "jobs",
        subviews: &["Schedules", "Runs"],
        inner_sidebar: None,
        summary_strip: &[],
        pending: pending("schedules, manual jobs and their runs", "DW-50"),
        link: None,
    },
    Screen {
        route: Route::Usage,
        id: "usage",
        title: "Usage & costs",
        summary: "Attributed requests, prices, budgets and cache.",
        group: NavGroup::Bottom,
        help_topic: "usage",
        subviews: &["Spending", "Requests", "Cache"],
        inner_sidebar: None,
        summary_strip: &[],
        pending: pending("request attribution, prices and budgets", "DW-21"),
        link: None,
    },
    Screen {
        route: Route::Help,
        id: "help",
        title: "Help",
        summary: "Local documentation for people and agents.",
        group: NavGroup::Bottom,
        help_topic: "help",
        subviews: &[],
        inner_sidebar: None,
        summary_strip: &[],
        pending: pending("version-matched offline Help", "DW-52"),
        link: None,
    },
    Screen {
        route: Route::Settings,
        id: "settings",
        title: "Settings",
        summary: "Workspace configuration, scope and effective values.",
        group: NavGroup::Bottom,
        help_topic: "settings",
        subviews: &[],
        inner_sidebar: None,
        summary_strip: &[],
        pending: None,
        link: Some(("Add-ons", Route::AddOns)),
    },
    Screen {
        route: Route::Resources,
        id: "resources",
        title: "Resource reservations",
        summary: "Shared machine resources, holders and waiters.",
        group: NavGroup::Secondary,
        help_topic: "resources",
        subviews: &[],
        inner_sidebar: None,
        summary_strip: &[],
        pending: None,
        link: None,
    },
    Screen {
        route: Route::Indexing,
        id: "indexing",
        title: "Indexing",
        summary: "Structural and semantic index runs and logs.",
        group: NavGroup::Secondary,
        help_topic: "indexing",
        subviews: &["Runs", "Logs"],
        inner_sidebar: None,
        summary_strip: &[],
        pending: pending("repository discovery and index publication", "DW-23"),
        link: None,
    },
    Screen {
        route: Route::AddOns,
        id: "addons",
        title: "Add-ons",
        summary: "Managed executable packages and their access.",
        group: NavGroup::Secondary,
        help_topic: "addons",
        subviews: &[],
        inner_sidebar: None,
        summary_strip: &[],
        pending: None,
        link: None,
    },
];

/// The navigation order of the left sidebar (UI-14).
pub const NAV_ORDER: [Route; 16] = [
    Route::Chat,
    Route::Approvals,
    Route::Portfolio,
    Route::Overview,
    Route::Features,
    Route::Issues,
    Route::Repositories,
    Route::Agents,
    Route::Artifacts,
    Route::Knowledge,
    Route::Dreaming,
    Route::Databases,
    Route::Jobs,
    Route::Usage,
    Route::Help,
    Route::Settings,
];

impl Route {
    pub fn screen(self) -> &'static Screen {
        SCREENS
            .iter()
            .find(|s| s.route == self)
            .expect("every route is registered")
    }
    /// The navigation label (the Home chat screen is "Chat").
    pub fn nav_label(self) -> &'static str {
        match self {
            Self::Chat => "Chat",
            Self::Agents => "Agents",
            other => other.screen().title,
        }
    }
    pub fn all() -> impl Iterator<Item = Route> {
        SCREENS.iter().map(|s| s.route)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_route_is_registered_once_with_a_stable_id() {
        let mut ids: Vec<&str> = SCREENS.iter().map(|s| s.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), SCREENS.len());
        for r in Route::all() {
            assert_eq!(r.screen().route, r);
        }
    }

    #[test]
    fn approvals_sit_directly_below_chat_and_intelligence_is_grouped() {
        assert_eq!(&NAV_ORDER[..2], &[Route::Chat, Route::Approvals]);
        let intelligence: Vec<Route> = NAV_ORDER
            .iter()
            .copied()
            .filter(|r| r.screen().group == NavGroup::Intelligence)
            .collect();
        assert_eq!(
            intelligence,
            [Route::Knowledge, Route::Dreaming, Route::Databases]
        );
        // Activity lives inside Overview, not in the navigation.
        assert!(Route::Overview.screen().subviews.contains(&"Activity"));
    }

    #[test]
    fn inner_sidebars_sit_where_the_approved_design_places_them() {
        for r in [
            Route::Approvals,
            Route::Knowledge,
            Route::Dreaming,
            Route::Databases,
        ] {
            assert!(r.screen().inner_sidebar.is_some(), "{r:?}");
        }
        for r in [Route::Overview, Route::Features] {
            assert!(!r.screen().summary_strip.is_empty(), "{r:?}");
        }
    }
}
