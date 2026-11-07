//! Fixed catalog query results. Each wire row contains structural metadata only.
use super::*;
use devworks_core::protocol::records::SchemaAdapterProfile;
use std::collections::BTreeMap;

pub(super) const MAX_ROW_BYTES: usize = 128 * 1024;
#[derive(Clone, Copy)]
pub(super) struct Query {
    pub sql: &'static str,
}
pub(super) struct Catalog {
    pub collector: Collector,
    pub catalog: String,
    schema_keys: BTreeMap<String, String>,
}
impl Catalog {
    pub fn new(mut collector: Collector, catalog: String) -> Result<Self, MetadataFailure> {
        let root = key(&[&catalog, "Catalog"]);
        let node = MetadataObject {
            key: root,
            parent_key: None,
            kind: "Catalog".into(),
            qualified_name: catalog.clone(),
            metadata: metadata(&catalog, &catalog, None, json!({}))?,
            coverage: coverage(),
        };
        collector.object(node)?;
        Ok(Self {
            collector,
            catalog,
            schema_keys: BTreeMap::new(),
        })
    }
    pub fn root(&self) -> String {
        key(&[&self.catalog, "Catalog"])
    }
    pub fn schema(&mut self, name: &str) -> Result<String, MetadataFailure> {
        if let Some(value) = self.schema_keys.get(name) {
            return Ok(value.clone());
        }
        let root = self.root();
        let id = key(&[&self.catalog, name, "Schema"]);
        let node = MetadataObject {
            key: id.clone(),
            parent_key: Some(root.clone()),
            kind: "Schema".into(),
            qualified_name: format!("{}.{}", self.catalog, name),
            metadata: metadata(name, &self.catalog, Some(name), json!({}))?,
            coverage: coverage(),
        };
        if self.collector.object(node)? {
            self.collector.relationship(&root, &id, "CONTAINS")?;
            self.schema_keys.insert(name.into(), id.clone());
        }
        Ok(id)
    }
    pub fn ingest(&mut self, raw: &str) -> Result<bool, MetadataFailure> {
        self.collector.check()?;
        if raw.len() > MAX_ROW_BYTES {
            self.collector.gap("metadata_row_byte_limit");
            return Ok(true);
        }
        let row: Value = serde_json::from_str(raw).map_err(|_| MetadataFailure::InvalidMetadata)?;
        let field = |name| {
            row.get(name)
                .and_then(Value::as_str)
                .ok_or(MetadataFailure::InvalidMetadata)
        };
        let schema = field("schema")?;
        if !self.collector.request.config.schemas.0.is_empty()
            && !self
                .collector
                .request
                .config
                .schemas
                .0
                .iter()
                .any(|s| s.0.as_str() == schema)
        {
            return Ok(true);
        }
        let kind = field("kind")?;
        let name = field("name")?;
        if kind == "Schema" {
            self.schema(name)?;
            return Ok(true);
        }
        let parent = row.get("parent").and_then(Value::as_str);
        let parent_kind = row
            .get("parentKind")
            .and_then(Value::as_str)
            .unwrap_or("Table");
        let selection = parent.unwrap_or(name);
        let selected_name = format!("{}.{}.{}", self.catalog, schema, selection);
        if !self.collector.includes(selection, &selected_name) {
            return Ok(true);
        }
        let schema_id = self.schema(schema)?;
        let parent_id = parent
            .map(|p| key(&[&self.catalog, schema, parent_kind, p]))
            .unwrap_or(schema_id);
        let id = if let Some(parent) = parent {
            key(&[&self.catalog, schema, parent_kind, parent, kind, name])
        } else {
            key(&[&self.catalog, schema, kind, name])
        };
        let mut details = row.get("metadata").cloned().unwrap_or_else(|| json!({}));
        // Catalog SQL emits native JSON arrays/bools on all qualified engines. Reject a
        // malformed row rather than coercing a metadata result or reading source table data.
        for member in [
            "columnKey",
            "extra",
            "comment",
            "dataType",
            "indexType",
            "routineType",
        ] {
            if details.get(member).and_then(Value::as_str) == Some("") {
                details
                    .as_object_mut()
                    .ok_or(MetadataFailure::InvalidMetadata)?
                    .remove(member);
            }
        }
        for member in ["columnNames", "referencedColumns", "parameters"] {
            if details.get(member).is_none() {
                details[member] = json!([]);
            }
        }
        if let Some(definition) = row.get("definition").filter(|v| v.is_string()) {
            details["definitionSql"] = definition.clone();
        }
        if let Some(parameters) = details.get_mut("parameters").and_then(Value::as_array_mut) {
            parameters.sort_by_key(|p| {
                p.get("ordinal")
                    .and_then(Value::as_str)
                    .and_then(|s| s.parse::<u64>().ok())
            });
        }
        let qualified = if parent.is_some() {
            format!("{selected_name}.{name}")
        } else {
            selected_name
        };
        let mut node = MetadataObject {
            key: id.clone(),
            parent_key: Some(parent_id.clone()),
            kind: kind.into(),
            qualified_name: qualified,
            metadata: metadata(name, &self.catalog, Some(schema), details)?,
            coverage: coverage(),
        };
        let missing_check = kind == "Constraint"
            && node
                .metadata
                .constraint_type
                .as_ref()
                .is_some_and(|t| t.0.eq_ignore_ascii_case("check"))
            && node.metadata.check_clause.is_none();
        if missing_check
            || matches!(kind, "View" | "Procedure")
                && row
                    .get("definition")
                    .and_then(Value::as_str)
                    .is_none_or(|d| d.is_empty())
        {
            self.collector.gap("metadata_definition_unavailable");
            node.coverage.state = CoverageState::Partial;
            node.coverage
                .gaps
                .push(Id::new("metadata_definition_unavailable").expect("static gap"));
        }
        if row
            .get("truncated")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            self.collector.gap("metadata_definition_byte_limit");
            node.coverage.state = CoverageState::Partial;
            node.coverage
                .gaps
                .push(Id::new("metadata_definition_byte_limit").expect("static gap"));
        }
        if !self.collector.object(node)? {
            return Ok(false);
        }
        let relation = match kind {
            "Column" => "HAS_COLUMN",
            "Index" => "HAS_INDEX",
            "Constraint" => "HAS_CONSTRAINT",
            _ => "CONTAINS",
        };
        self.collector.relationship(&parent_id, &id, relation)?;
        if let Some(reference) = row.get("reference").filter(|v| !v.is_null()) {
            let target_schema = reference
                .get("schema")
                .and_then(Value::as_str)
                .unwrap_or(schema);
            let target_table = reference
                .get("table")
                .and_then(Value::as_str)
                .ok_or(MetadataFailure::InvalidMetadata)?;
            let target_catalog = reference
                .get("catalog")
                .and_then(Value::as_str)
                .unwrap_or(&self.catalog);
            let target = key(&[target_catalog, target_schema, "Table", target_table]);
            self.collector
                .relationship(&parent_id, &target, "FOREIGN_KEY")?;
            self.collector.relationship(&id, &target, "REFERENCES")?;
        }
        Ok(true)
    }
}
pub(super) fn profile(
    driver: &str,
    version: &str,
    tls: &str,
    supported: &[&str],
    unsupported: &[&str],
    limitations: &[&str],
) -> Result<SchemaAdapterProfile, MetadataFailure> {
    typed(
        json!({"driver":driver,"serverVersion":version,"authentication":"Password","tls":tls,"readOnlyBoundary":"Explicit source credentials and fixed server catalog statements; no caller SQL or source-table SELECT", "supportedMetadata":supported,"unsupportedMetadata":unsupported,"limitations":limitations}),
    )
}
