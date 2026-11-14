//! Stored revision bodies, inheritance resolution and revision planning (pure over the
//! configuration read port).
use super::catalog::{CATALOG_REVISION, ScopeKind, SettingDescriptor, catalog, descriptor};
use super::validate::{Issue, check_value, codes};
use crate::ports::storage::{ConfigScope, ConfigurationRead, StorageError, Unavailable};
use crate::protocol::data::DataScope;
use crate::protocol::records::{SchemaValue, SettingAssignment, SettingEntry};
use crate::protocol::support::{BoundedText, DynamicJson};
use crate::protocol::{ProjectId, Revision};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

/// Configuration namespace of the settings catalog.
pub const NAMESPACE: &str = "settings";
/// Version of the stored revision body (`values`, `origin`, `author`, ...).
pub const BODY_SCHEMA_VERSION: i64 = 1;
/// Revision 1 of every scope is its implicit initial state: nothing configured. A
/// `SettingEntry` from the workspace at revision 1 is therefore the built-in default.
pub const INITIAL_REVISION: i64 = 1;

pub fn kind_of(s: &ConfigScope) -> ScopeKind {
    match s {
        ConfigScope::Workspace => ScopeKind::Workspace,
        ConfigScope::Project(_) => ScopeKind::Project,
    }
}

/// One scope's complete configured values at one revision.
#[derive(Clone, Debug, PartialEq)]
pub struct Layer {
    pub scope: ConfigScope,
    pub revision: i64,
    /// Key → stored `SchemaValue` JSON (`{"schema": ..., "value": ...}`).
    pub values: BTreeMap<String, Value>,
}
impl Layer {
    pub fn initial(scope: ConfigScope) -> Self {
        Self {
            scope,
            revision: INITIAL_REVISION,
            values: BTreeMap::new(),
        }
    }
    fn from_body(scope: ConfigScope, revision: i64, body: &Value) -> Result<Self, StorageError> {
        let corrupt = |m: &str| StorageError::unavailable(Unavailable::Corrupt, m.to_owned());
        let values = body
            .get("values")
            .and_then(Value::as_object)
            .ok_or_else(|| corrupt("settings revision without values"))?;
        Ok(Self {
            scope,
            revision,
            values: values.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        })
    }
}

/// The scope's current layer (implicit revision 1 when nothing is stored).
pub fn current_layer(
    read: &dyn ConfigurationRead,
    scope: &ConfigScope,
) -> Result<Layer, StorageError> {
    match read.current(scope, NAMESPACE)? {
        Some(r) => Layer::from_body(scope.clone(), r.revision, &r.body),
        None => Ok(Layer::initial(scope.clone())),
    }
}

/// The scope's layer at an exact revision; `None` when that revision does not exist.
pub fn layer_at(
    read: &dyn ConfigurationRead,
    scope: &ConfigScope,
    revision: i64,
) -> Result<Option<Layer>, StorageError> {
    if revision == INITIAL_REVISION {
        return Ok(Some(Layer::initial(scope.clone())));
    }
    read.revision(scope, NAMESPACE, revision)?
        .map(|r| Layer::from_body(scope.clone(), r.revision, &r.body))
        .transpose()
}

/// Current layers in precedence order: the project (when addressed), then the workspace.
pub fn current_chain(
    read: &dyn ConfigurationRead,
    scope: &ConfigScope,
) -> Result<Vec<Layer>, StorageError> {
    let mut layers = Vec::with_capacity(2);
    if let ConfigScope::Project(_) = scope {
        layers.push(current_layer(read, scope)?);
    }
    layers.push(current_layer(read, &ConfigScope::Workspace)?);
    Ok(layers)
}

/// A stored value written against a schema this build does not publish for its key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredSchemaMismatch {
    pub key: String,
    pub scope: ConfigScope,
}

fn schema_matches(d: &SettingDescriptor, stored: &Value) -> bool {
    serde_json::to_value(d.schema_ref()).ok().as_ref() == stored.get("schema")
}

pub fn entry(
    d: &SettingDescriptor,
    source: &ConfigScope,
    revision: i64,
    value: Value,
) -> SettingEntry {
    SettingEntry {
        key: BoundedText(d.key.to_owned()),
        value: SchemaValue {
            schema: d.schema_ref(),
            value: DynamicJson(value),
        },
        source_scope: DataScope::from(source),
        revision: Revision::new(revision).expect("positive revision"),
    }
}

/// The source chain for `d` over `layers` (precedence order): the effective value first, then
/// every shadowed layer, ending with the built-in default (workspace revision 1). A
/// harness-enforced setting always resolves to its built-in value only.
pub fn chain(
    d: &SettingDescriptor,
    layers: &[Layer],
) -> Result<Vec<SettingEntry>, StoredSchemaMismatch> {
    let mut out = Vec::new();
    if !d.locked() {
        for layer in layers {
            if !d.allows(kind_of(&layer.scope)) {
                continue;
            }
            if let Some(stored) = layer.values.get(d.key) {
                if !schema_matches(d, stored) {
                    return Err(StoredSchemaMismatch {
                        key: d.key.to_owned(),
                        scope: layer.scope.clone(),
                    });
                }
                let value = stored.get("value").cloned().unwrap_or(Value::Null);
                out.push(entry(d, &layer.scope, layer.revision, value));
            }
        }
    }
    out.push(entry(
        d,
        &ConfigScope::Workspace,
        INITIAL_REVISION,
        d.default_value(),
    ));
    Ok(out)
}

/// Effective values for every catalog key over `layers` (used to diff adoption).
pub fn effective_values(layers: &[Layer]) -> BTreeMap<&'static str, Value> {
    catalog()
        .iter()
        .filter_map(|d| {
            let e = chain(d, layers).ok()?.into_iter().next()?;
            Some((d.key, e.value.value.0))
        })
        .collect()
}

/// The validated complete next revision of one scope.
#[derive(Clone, Debug)]
pub struct Plan {
    pub values: BTreeMap<String, Value>,
    /// Catalog settings whose configured value at this scope changes.
    pub changed: Vec<&'static SettingDescriptor>,
    /// Whether the stored revision would differ at all (including cleared unknown keys).
    pub differs: bool,
    pub issues: Vec<Issue>,
}
impl Plan {
    pub fn blocking(&self) -> impl Iterator<Item = &Issue> {
        self.issues.iter().filter(|i| i.blocking)
    }
    pub fn is_valid(&self) -> bool {
        self.blocking().next().is_none()
    }
}

/// Merges `entries` onto `current` (a `null` value clears the scope's value so it inherits)
/// and validates the **complete** resulting revision, including values carried forward.
pub fn plan(current: &Layer, entries: &[SettingAssignment]) -> Plan {
    let scope = kind_of(&current.scope);
    let mut values = current.values.clone();
    let mut issues = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for a in entries {
        let key = a.key.0.as_str();
        if !seen.insert(key) {
            issues.push(Issue::blocking(
                key,
                codes::DUPLICATE_KEY,
                "A setting can be assigned once per revision",
            ));
            continue;
        }
        let Some(d) = descriptor(key) else {
            // A key this build no longer knows can only be cleared, never set.
            if !(a.value.value.0.is_null() && values.remove(key).is_some()) {
                issues.push(Issue::blocking(key, codes::UNKNOWN_KEY, "Unknown setting"));
            }
            continue;
        };
        if d.locked() {
            issues.push(Issue::blocking(
                key,
                codes::HARNESS_ENFORCED,
                format!(
                    "{} is enforced by the harness and cannot be changed",
                    d.label
                ),
            ));
            continue;
        }
        if !d.allows(scope) {
            issues.push(Issue::blocking(
                key,
                codes::SCOPE_NOT_ALLOWED,
                format!("{} cannot be configured at this scope", d.label),
            ));
            continue;
        }
        let assigned = serde_json::to_value(&a.value).expect("schema value");
        if !schema_matches(d, &assigned) {
            issues.push(Issue::blocking(
                key,
                codes::SCHEMA_MISMATCH,
                "The value was written for a different setting schema",
            ));
            continue;
        }
        if a.value.value.0.is_null() {
            values.remove(key);
            continue;
        }
        let found = check_value(d, &a.value.value.0);
        if found.iter().any(|i| i.blocking) {
            issues.extend(found);
            continue;
        }
        issues.extend(found);
        values.insert(key.to_owned(), assigned);
    }
    for (key, stored) in &values {
        match descriptor(key) {
            None => issues.push(Issue::blocking(
                key.clone(),
                codes::STORED_UNKNOWN,
                "The current revision holds a setting this version does not know; clear it to save",
            )),
            Some(d) if !schema_matches(d, stored) => issues.push(Issue::blocking(
                key.clone(),
                codes::SCHEMA_MISMATCH,
                "The current revision holds a value under an older schema; replace it to save",
            )),
            Some(_) => {}
        }
    }
    let changed = catalog()
        .iter()
        .filter(|d| values.get(d.key) != current.values.get(d.key))
        .collect();
    Plan {
        differs: values != current.values,
        values,
        changed,
        issues,
    }
}

/// Stored revision body.
pub fn body(plan: &Plan, author: &crate::protocol::ActorId, now_ms: i64, origin: Value) -> Value {
    let values: Map<String, Value> = plan
        .values
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    json!({
        "catalogRevision": CATALOG_REVISION,
        "values": values,
        "author": author.0.as_str(),
        "createdAtMs": now_ms,
        "origin": origin,
        "changedKeys": plan.changed.iter().map(|d| d.key).collect::<Vec<_>>(),
    })
}

/// Assignments that restore `target` exactly (keys set since are cleared back to inheritance).
pub fn restore_assignments(current: &Layer, target: &Layer) -> Vec<SettingAssignment> {
    let mut keys: Vec<&String> = current.values.keys().chain(target.values.keys()).collect();
    keys.sort();
    keys.dedup();
    keys.into_iter()
        .filter(|k| current.values.get(*k) != target.values.get(*k))
        .filter_map(|k| {
            let value = match target.values.get(k) {
                Some(stored) => serde_json::from_value::<SchemaValue>(stored.clone()).ok()?,
                None => SchemaValue {
                    schema: descriptor(k)?.schema_ref(),
                    value: DynamicJson(Value::Null),
                },
            };
            Some(SettingAssignment {
                key: BoundedText(k.clone()),
                value,
            })
        })
        .collect()
}

/// Which revisions of the chain a running agent last adopted (SET-04).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdoptedRevisions {
    pub workspace: i64,
    pub project: Option<i64>,
}

pub fn layers_at(
    read: &dyn ConfigurationRead,
    project: Option<&ProjectId>,
    at: &AdoptedRevisions,
) -> Result<Option<Vec<Layer>>, StorageError> {
    let mut layers = Vec::with_capacity(2);
    if let (Some(p), Some(r)) = (project, at.project) {
        match layer_at(read, &ConfigScope::Project(p.clone()), r)? {
            Some(l) => layers.push(l),
            None => return Ok(None),
        }
    }
    match layer_at(read, &ConfigScope::Workspace, at.workspace)? {
        Some(l) => layers.push(l),
        None => return Ok(None),
    }
    Ok(Some(layers))
}
