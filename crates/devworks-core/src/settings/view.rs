//! UI-toolkit-independent Settings view model (SET-01/02/07/08). The native shell renders it;
//! it holds no business rules of its own: values come from `settings.read_effective` /
//! `settings.explain_source` results, local checks reuse [`super::validate`], and saving builds
//! the exact `settings.save_revision` input submitted through the gateway.
//!
//! The view keeps four things apart: the configured/effective value and where it comes from,
//! the user's unsaved draft, validation findings, and runtime health (for example whether a
//! directory is currently available). Health never changes a configured value.
use super::catalog::{Category, ScopeKind, SettingDescriptor, catalog, descriptor};
use super::model::{INITIAL_REVISION, kind_of};
use super::validate::{Issue, check_value, codes};
use crate::operations::settings::SaveRevisionInput;
use crate::ports::storage::ConfigScope;
use crate::protocol::Revision;
use crate::protocol::data::DataScope;
use crate::protocol::records::{SchemaValue, SettingAssignment, SettingEntry, ValidationReport};
use crate::protocol::support::{BoundedList, BoundedText, DynamicJson};
use serde_json::Value;
use std::collections::BTreeMap;

/// Where a value comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// Built-in catalog default.
    Default,
    /// Built-in harness protection; not a configurable value.
    HarnessPolicy,
    Workspace {
        revision: i64,
    },
    Project {
        revision: i64,
    },
}
impl Source {
    fn of(d: &SettingDescriptor, e: &SettingEntry) -> Self {
        let revision = e.revision.get();
        match &e.source_scope {
            _ if d.locked() => Self::HarnessPolicy,
            DataScope::Workspace if revision == INITIAL_REVISION => Self::Default,
            DataScope::Workspace => Self::Workspace { revision },
            DataScope::Project { .. } => Self::Project { revision },
        }
    }
    pub fn label(&self) -> &'static str {
        match self {
            Self::Default => "Default",
            Self::HarnessPolicy => "Enforced by DevWorks",
            Self::Workspace { .. } => "Workspace",
            Self::Project { .. } => "Project",
        }
    }
}

/// The value's relationship to the scope being viewed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// Nothing configured anywhere; the built-in default applies.
    Default,
    /// The viewed project uses the workspace value.
    Inherited,
    /// Configured at the viewed scope.
    Overridden,
    /// Harness-enforced; shown for transparency, never editable.
    Enforced,
    /// App-wide only: a project inherits it and cannot override it.
    WorkspaceOnly,
}

/// Runtime availability, distinct from the configured value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Health {
    NotChecked,
    Available,
    Unavailable(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct LayerView {
    pub source: Source,
    pub value: Value,
}

#[derive(Clone, Debug)]
pub struct Item {
    pub descriptor: &'static SettingDescriptor,
    pub effective: Value,
    pub source: Source,
    pub status: Status,
    /// Precedence order: the effective layer first, then the shadowed layers and the default.
    /// Holds only the effective layer until `settings.explain_source` is applied.
    pub chain: Vec<LayerView>,
    pub editable: bool,
    pub health: Health,
    /// Unsaved draft: `Some(Null)` clears the viewed scope's value so it inherits again.
    pub draft: Option<Value>,
    pub issues: Vec<Issue>,
}
impl Item {
    pub fn is_dirty(&self) -> bool {
        self.draft.is_some()
    }
    /// The value the field shows: the draft when present, else the effective value.
    pub fn shown(&self) -> &Value {
        match &self.draft {
            Some(Value::Null) => self.chain.get(1).map_or(&self.effective, |l| &l.value),
            Some(v) => v,
            None => &self.effective,
        }
    }
    fn matches(&self, terms: &[String]) -> bool {
        let d = self.descriptor;
        let hay = format!(
            "{} {} {} {}",
            d.key,
            d.label,
            d.description,
            d.category.label()
        )
        .to_lowercase();
        terms.iter().all(|t| hay.contains(t.as_str()))
    }
}

/// A save attempt found the scope at a newer revision. Drafts are kept; `changed` lists drafted
/// keys whose underlying value also changed, which the user should review before saving again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conflict {
    pub previous_revision: i64,
    pub current_revision: i64,
    pub changed: Vec<&'static str>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ViewError {
    UnknownKey,
    NotEditable,
}

#[derive(Clone, Debug)]
pub struct SettingsView {
    pub scope: ConfigScope,
    /// The viewed scope's configuration revision: the expected revision of the next save.
    pub revision: i64,
    items: Vec<Item>,
    query: String,
    category: Option<Category>,
    pub conflict: Option<Conflict>,
}

impl SettingsView {
    /// Builds the view from a `settings.read_effective` result for `scope` at `revision`
    /// (the result's read-stamp scope revision). Keys absent from `entries` are omitted.
    pub fn new(scope: ConfigScope, revision: i64, entries: &[SettingEntry]) -> Self {
        let mut view = Self {
            scope,
            revision,
            items: Vec::new(),
            query: String::new(),
            category: None,
            conflict: None,
        };
        view.items = view.build(entries, &BTreeMap::new());
        view
    }

    fn build(&self, entries: &[SettingEntry], keep: &BTreeMap<&str, &Item>) -> Vec<Item> {
        let viewed = kind_of(&self.scope);
        let by_key: BTreeMap<&str, &SettingEntry> =
            entries.iter().map(|e| (e.key.0.as_str(), e)).collect();
        catalog()
            .iter()
            .filter_map(|d| {
                let e = by_key.get(d.key)?;
                let source = Source::of(d, e);
                let status = match (&source, viewed) {
                    (Source::HarnessPolicy, _) => Status::Enforced,
                    (_, ScopeKind::Project) if !d.allows(ScopeKind::Project) => {
                        Status::WorkspaceOnly
                    }
                    (Source::Default, _) => Status::Default,
                    (Source::Project { .. }, ScopeKind::Project)
                    | (Source::Workspace { .. }, ScopeKind::Workspace) => Status::Overridden,
                    _ => Status::Inherited,
                };
                let effective = e.value.value.0.clone();
                let previous = keep.get(d.key);
                Some(Item {
                    descriptor: d,
                    chain: vec![LayerView {
                        source: source.clone(),
                        value: effective.clone(),
                    }],
                    effective,
                    source,
                    status,
                    editable: !d.locked() && d.allows(viewed),
                    health: previous.map_or(Health::NotChecked, |p| p.health.clone()),
                    draft: previous.and_then(|p| p.draft.clone()),
                    issues: previous.map_or_else(Vec::new, |p| p.issues.clone()),
                })
            })
            .collect()
    }

    /// Replaces server state after a save, a refresh or a revision conflict, keeping drafts.
    /// When the revision moved while drafts exist, records a [`Conflict`]; drafts that now equal
    /// the configured value are dropped.
    pub fn refresh(&mut self, revision: i64, entries: &[SettingEntry]) {
        let old: BTreeMap<&str, (Value, Option<Value>)> = self
            .items
            .iter()
            .map(|i| (i.descriptor.key, (i.effective.clone(), i.draft.clone())))
            .collect();
        let keep: BTreeMap<&str, &Item> =
            self.items.iter().map(|i| (i.descriptor.key, i)).collect();
        let mut items = self.build(entries, &keep);
        let mut changed = Vec::new();
        for item in &mut items {
            if let Some((before, Some(_))) = old.get(item.descriptor.key)
                && before != &item.effective
            {
                changed.push(item.descriptor.key);
            }
            if item.draft.as_ref() == Some(&item.effective) && item.status == Status::Overridden {
                item.draft = None;
            }
        }
        let had_drafts = old.values().any(|(_, d)| d.is_some());
        self.conflict = (had_drafts && revision != self.revision).then_some(Conflict {
            previous_revision: self.revision,
            current_revision: revision,
            changed,
        });
        self.revision = revision;
        self.items = items;
    }

    /// Replaces server state after this view's drafts were saved at `revision`.
    pub fn saved(&mut self, revision: i64, entries: &[SettingEntry]) {
        self.discard_all();
        self.refresh(revision, entries);
    }

    /// Applies a `settings.explain_source` chain for one key.
    pub fn apply_explanation(&mut self, key: &str, chain: &[SettingEntry]) {
        if let Some(item) = self.items.iter_mut().find(|i| i.descriptor.key == key) {
            item.chain = chain
                .iter()
                .map(|e| LayerView {
                    source: Source::of(item.descriptor, e),
                    value: e.value.value.0.clone(),
                })
                .collect();
        }
    }

    pub fn item(&self, key: &str) -> Option<&Item> {
        self.items.iter().find(|i| i.descriptor.key == key)
    }
    fn item_mut(&mut self, key: &str) -> Result<&mut Item, ViewError> {
        self.items
            .iter_mut()
            .find(|i| i.descriptor.key == key)
            .ok_or(ViewError::UnknownKey)
    }

    pub fn set_query(&mut self, query: &str) {
        self.query = query.to_owned();
    }
    pub fn set_category(&mut self, category: Option<Category>) {
        self.category = category;
    }
    fn terms(&self) -> Vec<String> {
        self.query
            .split_whitespace()
            .map(str::to_lowercase)
            .collect()
    }
    /// Categories in navigation order with how many items match the current search.
    pub fn categories(&self) -> Vec<(Category, usize)> {
        let terms = self.terms();
        Category::ALL
            .iter()
            .map(|c| {
                let n = self
                    .items
                    .iter()
                    .filter(|i| i.descriptor.category == *c && i.matches(&terms))
                    .count();
                (*c, n)
            })
            .filter(|(_, n)| *n > 0)
            .collect()
    }
    /// Items in the selected category matching every search term (key, label, description or
    /// category). A search spans all categories when no category is selected.
    pub fn visible(&self) -> Vec<&Item> {
        let terms = self.terms();
        self.items
            .iter()
            .filter(|i| self.category.is_none_or(|c| i.descriptor.category == c))
            .filter(|i| i.matches(&terms))
            .collect()
    }

    /// Records an unsaved edit and its local validation (same rules as the service).
    pub fn edit(&mut self, key: &str, value: Value) -> Result<&[Issue], ViewError> {
        let item = self.item_mut(key)?;
        if !item.editable {
            return Err(ViewError::NotEditable);
        }
        item.issues = if value.is_null() {
            Vec::new()
        } else {
            check_value(item.descriptor, &value)
        };
        item.draft =
            (item.status != Status::Overridden || value != item.effective).then_some(value);
        Ok(&item.issues)
    }
    /// Drafts clearing the viewed scope's value so it inherits (or returns to the default).
    pub fn inherit(&mut self, key: &str) -> Result<(), ViewError> {
        let item = self.item_mut(key)?;
        if !item.editable {
            return Err(ViewError::NotEditable);
        }
        item.issues.clear();
        item.draft = (item.status == Status::Overridden).then_some(Value::Null);
        Ok(())
    }
    pub fn discard(&mut self, key: &str) {
        if let Ok(item) = self.item_mut(key) {
            item.draft = None;
            item.issues.clear();
        }
    }
    pub fn discard_all(&mut self) {
        for item in &mut self.items {
            item.draft = None;
            item.issues.clear();
        }
        self.conflict = None;
    }
    pub fn is_dirty(&self) -> bool {
        self.items.iter().any(Item::is_dirty)
    }
    pub fn dirty_keys(&self) -> Vec<&'static str> {
        self.items
            .iter()
            .filter(|i| i.is_dirty())
            .map(|i| i.descriptor.key)
            .collect()
    }
    /// Whether any draft has a blocking local issue.
    pub fn has_blocking_issues(&self) -> bool {
        self.items
            .iter()
            .any(|i| i.is_dirty() && i.issues.iter().any(|x| x.blocking))
    }

    /// The `settings.validate` / `settings.save_revision` input for the current drafts.
    pub fn save_input(&self) -> Option<SaveRevisionInput> {
        let entries: Vec<SettingAssignment> = self
            .items
            .iter()
            .filter_map(|i| {
                Some(SettingAssignment {
                    key: BoundedText(i.descriptor.key.to_owned()),
                    value: SchemaValue {
                        schema: i.descriptor.schema_ref(),
                        value: DynamicJson(i.draft.clone()?),
                    },
                })
            })
            .collect();
        if entries.is_empty() {
            return None;
        }
        Some(SaveRevisionInput {
            scope: super::model::data_scope(&self.scope),
            expected_revision: Revision::new(self.revision).ok()?,
            entries: BoundedList(entries),
        })
    }

    /// Applies a `settings.validate` report: findings attach to their setting; advisory
    /// directory findings become health, never a changed value.
    pub fn apply_report(&mut self, report: &ValidationReport) {
        for item in &mut self.items {
            item.issues.clear();
        }
        for issue in &report.issues.0 {
            let key = issue.path.0.as_str();
            let Some(d) = descriptor(key) else { continue };
            let Ok(item) = self.item_mut(d.key) else {
                continue;
            };
            if issue.code.0 == codes::PATH_UNAVAILABLE {
                item.health = Health::Unavailable(issue.message.0.clone());
                continue;
            }
            item.issues.push(Issue {
                path: key.to_owned(),
                code: codes_of(&issue.code.0),
                message: issue.message.0.clone(),
                blocking: true,
            });
        }
    }

    pub fn set_health(&mut self, key: &str, health: Health) {
        if let Ok(item) = self.item_mut(key) {
            item.health = health;
        }
    }
}

fn codes_of(code: &str) -> &'static str {
    use codes::*;
    [
        UNKNOWN_KEY,
        SCOPE_NOT_ALLOWED,
        HARNESS_ENFORCED,
        SCHEMA_MISMATCH,
        DUPLICATE_KEY,
        STALE_REVISION,
        TYPE,
        RANGE,
        CHOICE,
        PATH_INVALID,
        PATH_NOT_ABSOLUTE,
        RAW_CREDENTIAL,
        STORED_UNKNOWN,
    ]
    .into_iter()
    .find(|c| *c == code)
    .unwrap_or("issue.other")
}
