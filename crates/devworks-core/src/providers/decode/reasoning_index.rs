//! First-compatible native detail lookup. An absent type/ID remains a wildcard until filled.
use serde_json::Value;
use std::collections::{BTreeSet, HashMap};
#[derive(Clone, PartialEq, Eq, Hash)]
enum Identity {
    Index(Value),
    Id(Value),
}
#[derive(Clone, PartialEq, Eq, Hash)]
enum Field {
    Any,
    Missing,
    Exact(Value),
}
#[derive(Clone, PartialEq, Eq, Hash)]
struct Key(Identity, Field, Field);
#[derive(Default)]
pub(super) struct ReasoningIndex {
    positions: HashMap<Key, BTreeSet<usize>>,
}
fn value<'a>(entry: &'a Value, name: &str) -> Option<&'a Value> {
    entry.get(name).filter(|v| !v.is_null())
}
fn field(entry: &Value, name: &str) -> Field {
    value(entry, name).map_or(Field::Missing, |v| Field::Exact(v.clone()))
}
fn keys(entry: &Value) -> Vec<Key> {
    let identities = [
        value(entry, "index").map(|v| Identity::Index(v.clone())),
        value(entry, "id").map(|v| Identity::Id(v.clone())),
    ];
    let types = [Field::Any, field(entry, "type")];
    let ids = [Field::Any, field(entry, "id")];
    let mut keys = Vec::with_capacity(8);
    for identity in identities.into_iter().flatten() {
        for t in &types {
            for id in &ids {
                keys.push(Key(identity.clone(), t.clone(), id.clone()));
            }
        }
    }
    keys
}
impl ReasoningIndex {
    pub fn clear(&mut self) {
        self.positions.clear();
    }
    pub fn insert(&mut self, position: usize, entry: &Value) {
        for key in keys(entry) {
            self.positions.entry(key).or_default().insert(position);
        }
    }
    pub fn remove(&mut self, position: usize, entry: &Value) {
        for key in keys(entry) {
            if let Some(set) = self.positions.get_mut(&key) {
                set.remove(&position);
                if set.is_empty() {
                    self.positions.remove(&key);
                }
            }
        }
    }
    /// Probe at most four compatibility keys. Return the earliest compatible original position.
    pub fn find(&self, fragment: &Value) -> (Option<usize>, usize) {
        let identity = if let Some(v) = value(fragment, "index") {
            Identity::Index(v.clone())
        } else if let Some(v) = value(fragment, "id") {
            Identity::Id(v.clone())
        } else {
            return (None, 0);
        };
        let choices = |name| {
            value(fragment, name).map_or_else(
                || vec![Field::Any],
                |v| vec![Field::Exact(v.clone()), Field::Missing],
            )
        };
        let mut found = None;
        let mut probes = 0;
        for t in choices("type") {
            for id in choices("id") {
                probes += 1;
                if let Some(position) = self
                    .positions
                    .get(&Key(identity.clone(), t.clone(), id))
                    .and_then(|set| set.first())
                    .copied()
                {
                    found = Some(found.map_or(position, |old: usize| old.min(position)));
                }
            }
        }
        (found, probes)
    }
}
