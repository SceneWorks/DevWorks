//! Projection identity metadata is shared by every adapter, outside query timing.
use crate::workload::{Node, Scope};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, path::Path};

#[derive(Default, Serialize, Deserialize)]
pub struct NativeMap {
    pub forward: HashMap<u64, u64>,
    pub reverse: HashMap<u64, u64>,
    pub scopes: HashMap<u64, Scope>,
}
impl NativeMap {
    pub fn insert(&mut self, node: &Node, native: u64) {
        self.forward.insert(node.id, native);
        self.reverse.insert(native, node.id);
        self.scopes.insert(node.id, node.scope());
    }
    pub fn native(&self, gid: u64) -> Result<u64> {
        self.forward
            .get(&gid)
            .copied()
            .with_context(|| format!("Unknown logical ID {gid}"))
    }
    pub fn logical(&self, native: u64) -> Result<u64> {
        self.reverse
            .get(&native)
            .copied()
            .with_context(|| format!("Unknown native ID {native}"))
    }
    pub fn permitted(&self, gid: u64, scope: Scope) -> bool {
        self.scopes.get(&gid) == Some(&scope)
    }
    pub fn save(&self, path: &Path) -> Result<()> {
        let mut f = std::fs::File::create(path)?;
        serde_json::to_writer(&mut f, self)?;
        f.sync_all()?;
        Ok(())
    }
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        Ok(serde_json::from_reader(std::fs::File::open(path)?)?)
    }
    pub fn normalize_neighbors(
        &self,
        ids: impl IntoIterator<Item = u64>,
        scope: Scope,
    ) -> Result<Vec<u64>> {
        let mut out = Vec::new();
        for id in ids {
            let gid = self.logical(id)?;
            if self.permitted(gid, scope) {
                out.push(gid);
            }
        }
        Ok(crate::workload::sorted(out))
    }
}
