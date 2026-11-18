//! Projects and work items for tests that need rows to hang other records on, written through
//! the repository ports (`insert_project`, `insert_work`) in one transaction, so the rows carry
//! everything a service-created one does (detail rows, search key) instead of a positional SQL
//! `INSERT` that must be rewritten whenever a column is added.
use devworks_core::ports::storage::{
    ApplicationStore, StorageError, StoredProject, StoredWork, WriteOptions, run_write,
};
use devworks_core::protocol::{Id, ProjectId};
use serde_json::json;

/// An active project.
#[derive(Clone)]
pub struct ProjectSeed {
    id: String,
    name: Option<String>,
    created_at_ms: i64,
}
impl ProjectSeed {
    pub fn new(id: &str) -> Self {
        Self {
            id: id.into(),
            name: None,
            created_at_ms: 1,
        }
    }
    /// The project's name (default: its id).
    pub fn name(mut self, name: &str) -> Self {
        self.name = Some(name.into());
        self
    }
    pub fn created_at(mut self, ms: i64) -> Self {
        self.created_at_ms = ms;
        self
    }
    fn stored(&self) -> StoredProject {
        StoredProject {
            id: ProjectId(id(&self.id)),
            name: self.name.clone().unwrap_or_else(|| self.id.clone()),
            description: String::new(),
            lifecycle: "Active".into(),
            revision: 1,
            scope_revision: 1,
            created_at_ms: self.created_at_ms,
            updated_at_ms: self.created_at_ms,
            archived_at_ms: None,
            deleted_at_ms: None,
            archive_epoch: 0,
            archive_stop_pending: false,
        }
    }
}

/// A work item: a feature, a task of a feature, or a standalone task (an issue).
#[derive(Clone)]
pub struct WorkSeed {
    id: String,
    project: String,
    kind: &'static str,
    parent: Option<String>,
    title: Option<String>,
    priority: i64,
    created_at_ms: i64,
}
impl WorkSeed {
    pub fn feature(id: &str, project: &str) -> Self {
        Self::of(id, project, "feature", None)
    }
    pub fn task(id: &str, project: &str, feature: &str) -> Self {
        Self::of(id, project, "task", Some(feature))
    }
    /// A standalone task (no feature), what the legacy `issue` kind reads as.
    pub fn issue(id: &str, project: &str) -> Self {
        Self::of(id, project, "task", None)
    }
    fn of(id: &str, project: &str, kind: &'static str, parent: Option<&str>) -> Self {
        Self {
            id: id.into(),
            project: project.into(),
            kind,
            parent: parent.map(str::to_owned),
            title: None,
            priority: 1,
            created_at_ms: 1,
        }
    }
    /// The title (default: the id).
    pub fn title(mut self, title: &str) -> Self {
        self.title = Some(title.into());
        self
    }
    pub fn priority(mut self, priority: i64) -> Self {
        self.priority = priority;
        self
    }
    pub fn created_at(mut self, ms: i64) -> Self {
        self.created_at_ms = ms;
        self
    }
    fn stored(&self) -> StoredWork {
        let title = self.title.clone().unwrap_or_else(|| self.id.clone());
        StoredWork {
            id: id(&self.id),
            project_id: ProjectId(id(&self.project)),
            kind: self.kind.into(),
            parent_feature_id: self.parent.as_deref().map(id),
            status_id: None,
            search_key: title.to_lowercase(),
            title,
            priority: self.priority,
            manual_order: 0,
            revision: 1,
            scope_revision: 1,
            created_at_ms: self.created_at_ms,
            updated_at_ms: self.created_at_ms,
            created_by: None,
            updated_by: None,
            caller_ref: None,
            completion: None,
            brief: json!({}),
        }
    }
}

fn id(s: &str) -> Id {
    Id::new(s).expect("a valid seed id")
}

/// Writes `projects` that do not exist yet, then `work` (parents before their tasks), in one
/// transaction. A work item that exists already is an error.
pub fn rows(
    store: &dyn ApplicationStore,
    projects: &[ProjectSeed],
    work: &[WorkSeed],
) -> Result<(), StorageError> {
    let projects: Vec<_> = projects.iter().map(ProjectSeed::stored).collect();
    let work: Vec<_> = work.iter().map(WorkSeed::stored).collect();
    run_write(store, WriteOptions::default(), move |tx| {
        for p in &projects {
            if tx.projects().project(&p.id)?.is_none() {
                tx.projects().insert_project(p)?;
            }
        }
        for w in &work {
            tx.work().insert_work(w)?;
        }
        Ok(())
    })
}

/// Projects only.
pub fn projects(
    store: &dyn ApplicationStore,
    projects: &[ProjectSeed],
) -> Result<(), StorageError> {
    rows(store, projects, &[])
}

/// Work items only (their projects exist already).
pub fn work(store: &dyn ApplicationStore, work: &[WorkSeed]) -> Result<(), StorageError> {
    rows(store, &[], work)
}
