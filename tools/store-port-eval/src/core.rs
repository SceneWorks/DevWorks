//! Driver-free candidate persistence ports; fixture-specific subset, not the production core.
#[derive(Debug, PartialEq, Eq)]
pub enum StoreError {
    Unavailable,
    RevisionConflict,
    ScopeDenied,
    ViewUnavailable,
    ReceiptConflict,
    Interrupted,
}
pub type Result<T> = std::result::Result<T, StoreError>;
#[derive(Clone)]
pub struct Scope {
    pub project_id: String,
    pub expected_scope_revision: i64,
}
pub struct Pin {
    pub id: String,
    pub view_id: String,
    pub scope: Scope,
}
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub chunk_id: String,
    pub distance: f64,
}
pub trait ProjectRepository {
    fn revision(&self, id: &str) -> Result<i64>;
    fn rename(&mut self, id: &str, expected: i64, title: &str) -> Result<()>;
}
pub trait JournalRepository {
    fn receipt(&self, key: &str) -> Result<Option<String>>;
    fn record(&mut self, key: &str, project: &str, payload: &str) -> Result<()>;
}
/// Repository borrows cannot escape the owning transaction.
/// ```compile_fail
/// use devworks_store_port_probe::core::{ApplicationStore,ProjectRepository};
/// fn escape(store:&mut dyn ApplicationStore)->&mut dyn ProjectRepository {
///     let mut tx=store.begin_write().unwrap();
///     tx.projects()
/// }
/// ```
pub trait WriteTransaction {
    fn projects(&mut self) -> &mut dyn ProjectRepository;
    fn journal(&mut self) -> &mut dyn JournalRepository;
    fn commit(self: Box<Self>) -> Result<()>;
}
pub trait ApplicationStore {
    fn begin_write(&mut self) -> Result<Box<dyn WriteTransaction + '_>>;
    fn pin(&mut self, scope: Scope, view_id: &str, now: i64, expires: i64) -> Result<Pin>;
    fn nodes(&self, pin: &Pin, now: i64) -> Result<Vec<String>>;
    fn edges(&self, pin: &Pin, now: i64) -> Result<Vec<(String, String)>>;
    fn vector_candidates(
        &self,
        pin: &Pin,
        query: &[f32],
        space: &str,
        limit: u16,
        now: i64,
    ) -> Result<Vec<Candidate>>;
    fn lexical_candidates(&self, pin: &Pin, text: &str, now: i64) -> Result<Vec<String>>;
    fn materialize(&self, pin: &Pin, candidates: &[Candidate], now: i64) -> Result<Vec<String>>;
    fn publish(
        &mut self,
        source: &str,
        expected_source_revision: i64,
        expected_pointer_revision: i64,
        view: &str,
    ) -> Result<()>;
}
pub fn rename_project(
    store: &mut dyn ApplicationStore,
    key: &str,
    project: &str,
    expected: i64,
    title: &str,
    interrupt: bool,
) -> Result<bool> {
    let mut tx = store.begin_write()?;
    let payload = format!("{project}:{expected}:{title}");
    if let Some(old) = tx.journal().receipt(key)? {
        return if old == payload {
            Ok(false)
        } else {
            Err(StoreError::ReceiptConflict)
        };
    }
    if tx.projects().revision(project)? != expected {
        return Err(StoreError::RevisionConflict);
    }
    tx.projects().rename(project, expected, title)?;
    tx.journal().record(key, project, &payload)?;
    if interrupt {
        return Err(StoreError::Interrupted);
    }
    tx.commit()?;
    Ok(true)
}
