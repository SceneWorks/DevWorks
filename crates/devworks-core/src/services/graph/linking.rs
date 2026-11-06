//! The existing complete-view linker with query-wide work and retention accounting.
use super::budget::Budget;
use crate::ports::storage::*;
use crate::protocol::Id;
use std::cell::RefCell;

struct BoundedIndex<'a> {
    read: &'a dyn IndexRead,
    budget: RefCell<&'a mut Budget>,
}

impl BoundedIndex<'_> {
    fn charge(&self, bytes: usize) -> Result<(), StorageError> {
        let mut budget = self.budget.borrow_mut();
        if budget.visit() && budget.retain(bytes) {
            Ok(())
        } else {
            Err(StorageError::Busy)
        }
    }

    fn nodes(&self, rows: Vec<StoredNode>) -> Result<Vec<StoredNode>, StorageError> {
        for n in &rows {
            self.charge(
                n.unit_key.len()
                    + n.node_id.len()
                    + n.kind.len()
                    + n.name.len()
                    + n.payload.to_string().len(),
            )?;
        }
        Ok(rows)
    }
}

macro_rules! forward {
    ($($name:ident($($arg:ident:$ty:ty),*) -> $out:ty;)*) => {$ (
        fn $name(&self, $($arg:$ty),*) -> Result<$out, StorageError> { self.read.$name($($arg),*) }
    )*};
}

impl IndexRead for BoundedIndex<'_> {
    forward! {
        source_config(scope_key:&str) -> Option<StoredSourceConfig>;
        repository_source(repository:&Id) -> Option<StoredRepositorySource>;
        repository_at(location:&str) -> Option<Id>;
        worktrees(repository:&Id) -> Vec<StoredWorktree>;
        run(id:&Id) -> Option<StoredIndexRun>;
        runs(filter:&IndexRunFilter,after:Option<&RunPosition>,limit:u16) -> Vec<StoredIndexRun>;
        active_run(repository:&Id,overlay:Option<&Id>) -> Option<StoredIndexRun>;
        batch_runs(batch:&Id) -> Vec<StoredIndexRun>;
        latest_run(repository:&Id,overlay:Option<&Id>) -> Option<StoredIndexRun>;
        claimable_runs(now_ms:i64,limit:u16) -> Vec<StoredIndexRun>;
        running_runs(limit:u16) -> Vec<StoredIndexRun>;
        generation(id:&Id) -> Option<StoredGeneration>;
        generations(repository:&Id) -> Vec<StoredGeneration>;
        generation_in_view(generation:&Id) -> bool;
        generations_in_state(state:&str,limit:u16) -> Vec<StoredGeneration>;
        published(repository:&Id) -> Option<StoredPublished>;
        view(view:&Id) -> Option<StoredView>;
        live_pins(view:&Id,now_ms:i64) -> i64;
        overlay(id:&Id) -> Option<StoredOverlay>;
        overlay_for(repository:&Id,worktree:&Id) -> Option<StoredOverlay>;
        overlay_at(repository:&Id,commit:&str) -> Option<StoredOverlay>;
        overlays(repository:&Id) -> Vec<StoredOverlay>;
        overlays_due(now_ms:i64,limit:u16) -> Vec<StoredOverlay>;
        prunable_ref_overlays(ended_before_ms:i64,keep:u32,limit:u16) -> Vec<StoredOverlay>;
    }

    fn units(
        &self,
        generation: &Id,
        after: Option<&str>,
        limit: u16,
    ) -> Result<Vec<StoredUnit>, StorageError> {
        let rows = self.read.units(generation, after, limit)?;
        for u in &rows {
            self.charge(u.unit_key.len() + u.content_digest.len() + u.payload.to_string().len())?;
        }
        Ok(rows)
    }
    fn unit(&self, generation: &Id, unit_key: &str) -> Result<Option<StoredUnit>, StorageError> {
        self.charge(unit_key.len())?;
        self.read.unit(generation, unit_key)
    }
    fn nodes(&self, generation: &Id, unit_key: &str) -> Result<Vec<StoredNode>, StorageError> {
        self.nodes(self.read.nodes(generation, unit_key)?)
    }
    fn edges(&self, generation: &Id, unit_key: &str) -> Result<Vec<StoredEdge>, StorageError> {
        let rows = self.read.edges(generation, unit_key)?;
        for e in &rows {
            self.charge(
                e.unit_key.len()
                    + e.edge_id.len()
                    + e.source_node_id.len()
                    + e.target_node_id.len()
                    + e.payload.to_string().len(),
            )?;
        }
        Ok(rows)
    }
    fn nodes_named(
        &self,
        generation: &Id,
        name: &str,
        limit: u16,
    ) -> Result<Vec<StoredNode>, StorageError> {
        self.nodes(self.read.nodes_named(generation, name, limit)?)
    }
    fn nodes_qualified(
        &self,
        generation: &Id,
        qualified_name: &str,
        limit: u16,
    ) -> Result<Vec<StoredNode>, StorageError> {
        self.nodes(
            self.read
                .nodes_qualified(generation, qualified_name, limit)?,
        )
    }
}

pub(super) fn link(
    read: &dyn IndexRead,
    view: &crate::services::indexing::views::ViewLayers,
    session: &mut crate::services::indexing::worker::ViewLinker,
    unit: &str,
    edges: Vec<StoredEdge>,
    budget: &mut Budget,
) -> Result<Option<Vec<StoredEdge>>, StorageError> {
    let bounded = BoundedIndex {
        read,
        budget: RefCell::new(budget),
    };
    let result = session.link(
        &bounded,
        view.view.repository_id.as_str(),
        &view.layers,
        unit,
        edges,
    );
    match result {
        Err(StorageError::Busy) if bounded.budget.borrow().exhausted.is_some() => Ok(None),
        other => other.map(Some),
    }
}
