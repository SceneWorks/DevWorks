//! The existing complete-view linker with query-wide work and retention accounting.
use super::budget::Budget;
use crate::ports::storage::*;
use crate::protocol::Id;
use std::cell::RefCell;

struct BoundedIndex<'a> {
    read: &'a dyn IndexRead,
    graph: &'a dyn GraphRead,
    scope: GraphScope,
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
        let mut after = None;
        let mut rows = Vec::new();
        loop {
            let page = self.graph.graph_nodes(
                &self.scope,
                &GraphNodeFilter {
                    unit_key: Some(unit_key.into()),
                    ..Default::default()
                },
                after.as_ref(),
                32,
            )?;
            let more = page.has_more;
            for node in page.items {
                after = Some(GraphPosition {
                    repository_id: node.repository_id.clone(),
                    unit_key: node.node.unit_key.clone(),
                    item_id: node.node.node_id.clone(),
                });
                self.charge(
                    node.node.payload.to_string().len()
                        + node.node.name.len()
                        + node.node.node_id.len()
                        + node.node.unit_key.len()
                        + std::mem::size_of::<StoredNode>(),
                )?;
                if node.generation_id == *generation {
                    rows.push(node.node);
                }
            }
            if !more {
                break;
            }
        }
        Ok(rows)
    }
    fn edges(&self, generation: &Id, unit_key: &str) -> Result<Vec<StoredEdge>, StorageError> {
        let mut after = None;
        let mut rows = Vec::new();
        loop {
            let page = self.graph.graph_edges(
                &self.scope,
                &GraphEdgeFilter::default(),
                after.as_ref(),
                32,
            )?;
            let more = page.has_more;
            for edge in page.items {
                after = Some(GraphPosition {
                    repository_id: edge.repository_id.clone(),
                    unit_key: edge.edge.unit_key.clone(),
                    item_id: edge.edge.edge_id.clone(),
                });
                self.charge(
                    edge.edge.payload.to_string().len()
                        + edge.edge.source_node_id.len()
                        + edge.edge.target_node_id.len()
                        + edge.edge.unit_key.len()
                        + std::mem::size_of::<StoredEdge>(),
                )?;
                if edge.generation_id == *generation && edge.edge.unit_key == unit_key {
                    rows.push(edge.edge);
                }
            }
            if !more {
                break;
            }
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
    read: &dyn ReadSnapshot,
    view: &crate::services::indexing::views::ViewLayers,
    session: &mut crate::services::indexing::worker::ViewLinker,
    unit: &str,
    edges: Vec<StoredEdge>,
    budget: &mut Budget,
) -> Result<Option<Vec<StoredEdge>>, StorageError> {
    let bounded = BoundedIndex {
        read: read.indexing(),
        graph: read.graph(),
        scope: GraphScope {
            views: vec![GraphViewBinding {
                repository_id: view.view.repository_id.clone(),
                view_id: view.view.view_id.clone(),
                view_digest: view.view.view_digest.clone(),
            }],
        },
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
