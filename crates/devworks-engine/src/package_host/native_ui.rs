use super::{InvocationKind, InvokeOutcome, InvokeRequest, PackageHost, policy, project_disabled};
use devworks_core::context::{InvocationContext, PrincipalKind};
use devworks_core::ports::IdKind;
use devworks_core::ports::packages::Inspection;
use devworks_core::ports::storage::{ReadOptions, read_snapshot};
use devworks_core::protocol::provider::ArtifactRef;
use devworks_core::protocol::records::PackageAvailability;
use devworks_extension_protocol::package::{Contribution, ContributionEvent, InvocationOutcome};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

pub use devworks_extension_protocol::package::{FieldKind, UiNode};

#[derive(Clone, Debug)]
pub struct NativeView {
    pub id: String,
    pub tree: UiNode,
    pub choices: BTreeMap<String, Vec<String>>,
    pub has_tables: bool,
}

/// The package and version used to render a view. A caller must recheck these before an action.
#[derive(Clone, Debug)]
pub struct NativeViews {
    pub digest: String,
    pub views: Vec<NativeView>,
}

const MAX_FORM_BYTES: usize = 64 * 1024;
const VIEW_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug)]
pub struct ViewData {
    pub revision: i64,
    pub tables: BTreeMap<String, Vec<Value>>,
}

impl PackageHost {
    /// Returns native view descriptors that the current person may open. Nothing in a view
    /// grants authority to execute an action or read an artifact.
    pub fn native_views(
        &self,
        principal: &InvocationContext,
        package_id: &str,
        expected_digest: &str,
    ) -> Result<NativeViews, String> {
        let inspection = self.view_inspection_at(principal, package_id, expected_digest)?;
        let views = if inspection.access.iter().any(|access| access == "ui") {
            self.declared_views(&inspection)?
        } else {
            Vec::new()
        };
        Ok(NativeViews {
            digest: inspection.manifest_digest,
            views,
        })
    }

    /// Stages a form for an action declared in the view the person opened. The resulting
    /// artifact is input to the existing `addons.invoke` command, with the same expected digest.
    pub fn prepare_native_view_action(
        &self,
        principal: &InvocationContext,
        package_id: &str,
        expected_digest: &str,
        view_id: &str,
        action_id: &str,
        form: &Value,
    ) -> Result<ArtifactRef, String> {
        let views = self.native_views(principal, package_id, expected_digest)?;
        if !views
            .views
            .iter()
            .find(|view| view.id == view_id)
            .is_some_and(|view| contains_action(&view.tree, action_id))
        {
            return Err("the displayed package view does not declare this action".into());
        }
        self.stage_view_form(principal, package_id, expected_digest, form, json!({
            "kind": "PackageViewAction", "packageId": package_id, "viewId": view_id, "actionId": action_id,
        }))
    }

    fn stage_view_form(
        &self,
        principal: &InvocationContext,
        package_id: &str,
        expected_digest: &str,
        form: &Value,
        provenance: Value,
    ) -> Result<ArtifactRef, String> {
        let bytes = serde_json::to_vec(form).map_err(|error| error.to_string())?;
        if !form.is_object() || bytes.len() > MAX_FORM_BYTES {
            return Err("a package form must be a JSON object of at most 64 KiB".into());
        }
        self.stage_contribution_input(principal, (package_id, expected_digest), &bytes, provenance)
    }

    /// Fetches data for a published view through its own read-only helper invocation.
    pub fn load_native_view_data(
        &self,
        principal: &InvocationContext,
        package_id: &str,
        digest: &str,
        view_id: &str,
        form: &Value,
    ) -> Result<ViewData, String> {
        let inspection = self.view_inspection_at(principal, package_id, digest)?;
        if !inspection.access.iter().any(|access| access == "ui") {
            return Err("the add-on has no approved UI access".into());
        }
        let (meta, tree) = inspection
            .contributions
            .iter()
            .find(|entry| entry.id == view_id && entry.kind == "NativeUi")
            .and_then(|entry| entry.verified_descriptor())
            .and_then(|descriptor| serde_json::from_value(descriptor.clone()).ok())
            .and_then(|kind| match kind {
                Contribution::NativeUi { meta, view } => Some((meta, view)),
                _ => None,
            })
            .ok_or("the native view is unavailable")?;
        let input_schema = self.contribution_schema(&inspection, &meta.input_schema)?;
        if self
            .service
            .validator()
            .validate(&input_schema, form)
            .is_err()
        {
            return Err("the form does not match the view's input schema".into());
        }
        let output_schema = self.contribution_schema(&inspection, &meta.output_schema)?;
        let input = self.stage_view_form(
            principal,
            package_id,
            digest,
            form,
            json!({
                "kind": "PackageViewRead", "packageId": package_id, "viewId": view_id,
            }),
        )?;
        let scope = super::contributions::scope_of(principal)?;
        let handle = self
            .invoke(InvokeRequest {
                package_id: package_id.to_owned(),
                contribution_id: view_id.to_owned(),
                scope,
                input: serde_json::to_value(input)
                    .and_then(serde_json::from_value)
                    .map_err(|error| error.to_string())?,
                operation_id: self.ids.next_id(IdKind::Operation),
                deadline: VIEW_TIMEOUT,
                allowed_operations: Vec::new(),
                kind: InvocationKind::NativeUi,
                principal: Some(principal.clone()),
                expected_digest: Some(digest.to_owned()),
            })
            .map_err(|error| format!("the view helper is unavailable: {error:?}"))?;
        if !matches!(
            handle.wait(VIEW_TIMEOUT + Duration::from_secs(3)),
            Some(InvokeOutcome::Finished(InvocationOutcome::Completed { .. }))
        ) {
            return Err("the view helper did not complete".into());
        }
        let (events, dropped) = handle.events();
        if dropped != 0 {
            return Err("the package dropped view updates".into());
        }
        let mut newest = None;
        for event in events {
            let ContributionEvent::ViewUpdate { revision, data } = event else {
                continue;
            };
            let revision = String::from(revision)
                .parse::<i64>()
                .map_err(|error| error.to_string())?;
            if newest
                .as_ref()
                .is_some_and(|(previous, _)| revision <= *previous)
                || !handle.produced(&data)
            {
                return Err(
                    "the view update has a stale revision or an unverified artifact".into(),
                );
            }
            newest = Some((revision, data));
        }
        let (revision, data) = newest.ok_or("the package sent no view update")?;
        let reference = serde_json::to_value(data)
            .and_then(serde_json::from_value)
            .map_err(|error| error.to_string())?;
        let ports = self
            .broker
            .get()
            .ok_or("package artifacts are not available")?;
        let (bytes, _) = ports
            .artifacts
            .read_exact(&*self.store, principal, &reference, 1024 * 1024)
            .map_err(|_| "the view data is not a visible artifact")?;
        let data = devworks_core::framing::parse(&bytes).map_err(|error| error.to_string())?;
        if self
            .service
            .validator()
            .validate(&output_schema, &data)
            .is_err()
        {
            return Err("the view data does not match its output schema".into());
        }
        let mut tables = BTreeMap::new();
        self.read_tables(&inspection, &tree, &data, &mut tables)?;
        self.view_inspection_at(principal, package_id, digest)?;
        Ok(ViewData { revision, tables })
    }

    fn read_tables(
        &self,
        inspection: &Inspection,
        tree: &UiNode,
        data: &Value,
        tables: &mut BTreeMap<String, Vec<Value>>,
    ) -> Result<(), String> {
        match tree {
            UiNode::Group { children } => {
                for node in children {
                    self.read_tables(inspection, node, data, tables)?;
                }
            }
            UiNode::Table { data_schema, .. } => {
                let id = data_schema.schema_id.as_str();
                let rows = &data["tables"][id];
                let schema = self.contribution_schema(inspection, data_schema)?;
                if self.service.validator().validate(&schema, rows).is_err() {
                    return Err("a package table does not match its data schema".into());
                }
                let rows = rows
                    .as_array()
                    .filter(|rows| rows.len() <= 500 && rows.iter().all(Value::is_object))
                    .ok_or("package tables take at most 500 object rows")?;
                tables.insert(id.to_owned(), rows.clone());
            }
            _ => {}
        }
        Ok(())
    }

    fn view_inspection_at(
        &self,
        principal: &InvocationContext,
        package_id: &str,
        expected_digest: &str,
    ) -> Result<Inspection, String> {
        let store = self.store.info();
        if principal.principal_kind() != PrincipalKind::Human
            || principal.restore_epoch().get() != store.restore_epoch
            || principal.store_id() != &store.store_id
        {
            return Err("the current person cannot open package views".into());
        }
        let version = read_snapshot(&*self.store, &ReadOptions::default(), |snapshot| {
            if snapshot.actors().actor_kind(principal.principal())? != Some(PrincipalKind::Human) {
                return Ok(None);
            }
            let packages = self.service.registry(snapshot.packages())?;
            let Some((package, Some(active))) = packages.get(package_id) else {
                return Ok(None);
            };
            let registry = policy::Registry {
                packages: &packages,
                runtimes: &**self.service.runtimes(),
                host: self.service.platform(),
            };
            let enabled = principal
                .project_id()
                .is_none_or(|project| !project_disabled(package, project.0.as_str()));
            Ok((enabled
                && active.manifest_digest == expected_digest
                && matches!(
                    registry.availability(package, Some(active)).0,
                    PackageAvailability::Ready
                ))
            .then(|| active.clone()))
        })
        .map_err(|error| error.to_string())?
        .ok_or("the add-on or displayed version is unavailable")?;
        self.verified_inspection(&version)
    }

    fn declared_views(&self, inspection: &Inspection) -> Result<Vec<NativeView>, String> {
        let tools: BTreeSet<&str> = inspection
            .contributions
            .iter()
            .filter(|entry| entry.kind == "Tool" && entry.verified_descriptor().is_some())
            .map(|entry| entry.id.as_str())
            .collect();
        inspection
            .contributions
            .iter()
            .filter(|entry| entry.kind == "NativeUi")
            .map(|entry| {
                let descriptor = entry
                    .verified_descriptor()
                    .ok_or("a package view descriptor cannot be verified")?;
                let Contribution::NativeUi { meta, view } =
                    serde_json::from_value(descriptor.clone())
                        .map_err(|_| "the package view descriptor is invalid")?
                else {
                    return Err("the contribution is not a native view".into());
                };
                validate_bindings(&view, &tools, inspection)?;
                let schema = self.contribution_schema(inspection, &meta.input_schema)?;
                let mut choices = BTreeMap::new();
                collect_choices(&view, &schema, &mut choices);
                Ok(NativeView {
                    choices,
                    has_tables: contains_table(&view),
                    id: meta.id.as_str().to_owned(),
                    tree: view,
                })
            })
            .collect()
    }
}

fn validate_bindings(
    node: &UiNode,
    tools: &BTreeSet<&str>,
    inspection: &Inspection,
) -> Result<(), String> {
    match node {
        UiNode::Group { children } => {
            for child in children {
                validate_bindings(child, tools, inspection)?;
            }
        }
        UiNode::Action { action_id, .. } if !tools.contains(action_id.as_str()) => {
            return Err("a package view action does not name a tool in its version".into());
        }
        UiNode::Table { data_schema, .. } => {
            let reference = serde_json::to_value(data_schema).map_err(|error| error.to_string())?;
            if inspection.schema_file(&reference).is_none() {
                return Err("a package view table has no verified data schema".into());
            }
        }
        _ => {}
    }
    Ok(())
}

fn contains_action(node: &UiNode, expected: &str) -> bool {
    match node {
        UiNode::Group { children } => children.iter().any(|node| contains_action(node, expected)),
        UiNode::Action { action_id, .. } => action_id.as_str() == expected,
        _ => false,
    }
}

fn collect_choices(tree: &UiNode, schema: &Value, fields: &mut BTreeMap<String, Vec<String>>) {
    match tree {
        UiNode::Group { children } => {
            for child in children {
                collect_choices(child, schema, fields);
            }
        }
        UiNode::Field {
            field_id,
            field_kind: FieldKind::Select,
            ..
        } => {
            if let Some(values) = schema["properties"][field_id.as_str()]["enum"].as_array() {
                let options = values
                    .iter()
                    .filter_map(Value::as_str)
                    .take(100)
                    .map(str::to_owned)
                    .collect();
                fields.insert(field_id.as_str().to_owned(), options);
            }
        }
        _ => {}
    }
}

fn contains_table(tree: &UiNode) -> bool {
    match tree {
        UiNode::Group { children } => children.iter().any(contains_table),
        UiNode::Table { .. } => true,
        _ => false,
    }
}
