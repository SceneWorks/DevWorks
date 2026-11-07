//! SQLite schema catalogs only: no view, virtual-table content or source data is selected.
use super::*;
use devworks_core::protocol::records::DbEngine;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

#[derive(Default)]
pub struct SqliteMetadataReader;
impl MetadataReader for SqliteMetadataReader {
    fn read(&self, request: MetadataRequest) -> MetadataFuture {
        Box::pin(async move {
            tokio::task::spawn_blocking(move || read(request))
                .await
                .map_err(|_| MetadataFailure::Unavailable)?
        })
    }
}
fn failure(c: &Collector) -> MetadataFailure {
    c.check().err().unwrap_or(MetadataFailure::Unavailable)
}
fn object(
    kind: &str,
    name: &str,
    parent: Option<&str>,
    qualified: String,
    details: Value,
    definition: Option<String>,
) -> Result<MetadataObject, MetadataFailure> {
    let mut details = details;
    if let Some(definition) = definition {
        details["definitionSql"] = json!(definition);
    }
    Ok(MetadataObject {
        key: key(&["main", kind, parent.unwrap_or(""), name]),
        parent_key: parent.map(str::to_owned),
        kind: kind.into(),
        qualified_name: qualified,
        metadata: metadata(name, "main", Some("main"), details)?,
        coverage: coverage(),
    })
}
fn partial(c: &mut Collector, node: &mut MetadataObject, reason: &str) {
    c.gap(reason);
    node.coverage.state = CoverageState::Partial;
    node.coverage
        .gaps
        .push(Id::new(reason).expect("static metadata gap"));
}
fn read(request: MetadataRequest) -> Result<MetadataSnapshot, MetadataFailure> {
    if !matches!(request.config.engine, DbEngine::Sqlite)
        || request.password.is_some()
        || request.ca_certificate.is_some()
    {
        return Err(MetadataFailure::UnsupportedProfile);
    }
    let path = Path::new(request.config.endpoint.0.as_str());
    if !path.is_absolute()
        || !std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_file())
    {
        return Err(MetadataFailure::InvalidTarget);
    }
    if request
        .config
        .catalogs
        .0
        .iter()
        .any(|v| v.0.as_str() != "main")
        || request
            .config
            .schemas
            .0
            .iter()
            .any(|v| v.0.as_str() != "main")
    {
        return Err(MetadataFailure::InvalidTarget);
    }
    // Resolve platform parent aliases (for example macOS /var) before SQLite
    // NOFOLLOW checks every component; the explicitly selected final file above is not a symlink.
    let path = std::fs::canonicalize(path).map_err(|_| MetadataFailure::InvalidTarget)?;
    let mut c = Collector::new(request)?;
    c.check()?;
    let mut conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .map_err(|_| MetadataFailure::Unavailable)?;
    conn.busy_timeout(Duration::from_millis(500))
        .map_err(|_| MetadataFailure::Unavailable)?;
    let token = c.request.cancellation.clone();
    let deadline = c.request.limits.deadline;
    conn.progress_handler(
        1000,
        Some(move || token.is_cancelled() || std::time::Instant::now() >= deadline),
    )
    .map_err(|_| MetadataFailure::Unavailable)?;
    conn.execute_batch("PRAGMA query_only=ON; PRAGMA trusted_schema=OFF; BEGIN;")
        .map_err(|_| failure(&c))?;
    let profile = typed(
        json!({"driver":"rusqlite-0.41.1","serverVersion":rusqlite::version(),"authentication":"File","tls":"NotApplicable",
        "readOnlyBoundary":"SQLite READONLY open, query_only and fixed catalog statements",
        "supportedMetadata":["Catalog","Schema","Table","View","Column","Index","Constraint"],"unsupportedMetadata":["Procedure","Type","GraphEntity","GraphEdge"],
        "limitations":["One file source exposes its main schema; SQLite has no stored-procedure catalog.","CHECK expressions remain in table DDL and are not separately enumerated."]}),
    )?;
    if c.request.test_only {
        // Opening SQLite is lazy: force a fixed catalog read before declaring a healthy target.
        conn.query_row(
            "SELECT count(*) FROM (SELECT name FROM main.sqlite_schema LIMIT 1)",
            [],
            |r| r.get::<_, i64>(0),
        )
        .map_err(|_| failure(&c))?;
        conn.execute_batch("ROLLBACK").map_err(|_| failure(&c))?;
        return Ok(c.finish(profile));
    }
    let catalog = object("Catalog", "main", None, "main".into(), json!({}), None)?;
    let root = catalog.key.clone();
    if !c.object(catalog)? {
        return Ok(c.finish(profile));
    }
    let schema = object(
        "Schema",
        "main",
        Some(&root),
        "main".into(),
        json!({}),
        None,
    )?;
    let schema_key = schema.key.clone();
    if !c.object(schema)? {
        return Ok(c.finish(profile));
    }
    c.relationship(&root, &schema_key, "CONTAINS")?;
    let mut tables=conn.prepare("SELECT name,type,CASE WHEN length(sql)<=65536 THEN sql END,length(sql)>65536 FROM main.sqlite_schema WHERE type IN('table','view') AND substr(lower(name),1,7)<>'sqlite_' ORDER BY name COLLATE BINARY").map_err(|_|failure(&c))?;
    let mut rows = tables.query([]).map_err(|_| failure(&c))?;
    while let Some(row) = rows.next().map_err(|_| failure(&c))? {
        c.check()?;
        let name: String = row.get(0).map_err(|_| MetadataFailure::InvalidMetadata)?;
        let kind: String = row.get(1).map_err(|_| MetadataFailure::InvalidMetadata)?;
        let qualified = format!("main.{name}");
        if !c.includes(&name, &qualified) {
            continue;
        }
        let definition: Option<String> =
            row.get(2).map_err(|_| MetadataFailure::InvalidMetadata)?;
        let truncated = row
            .get::<_, Option<bool>>(3)
            .map_err(|_| MetadataFailure::InvalidMetadata)?
            .unwrap_or(false);
        let mut node = object(
            if kind == "view" { "View" } else { "Table" },
            &name,
            Some(&schema_key),
            qualified.clone(),
            json!({}),
            definition,
        )?;
        if truncated {
            partial(&mut c, &mut node, "sqlite_definition_size_limit");
        }
        let table = node.key.clone();
        if !c.object(node)? {
            break;
        }
        c.relationship(&schema_key, &table, "CONTAINS")?;
        let mut columns=conn.prepare("SELECT cid,name,type,\"notnull\",substr(dflt_value,1,4096),pk,hidden,length(dflt_value)>4096 FROM pragma_table_xinfo(?1,'main') ORDER BY cid").map_err(|_|failure(&c))?;
        let mut columns = columns.query([&name]).map_err(|_| failure(&c))?;
        let mut primary = Vec::new();
        while let Some(column) = columns.next().map_err(|_| failure(&c))? {
            c.check()?;
            let ordinal: i64 = column
                .get(0)
                .map_err(|_| MetadataFailure::InvalidMetadata)?;
            let column_name: String = column
                .get(1)
                .map_err(|_| MetadataFailure::InvalidMetadata)?;
            let data_type: String = column
                .get(2)
                .map_err(|_| MetadataFailure::InvalidMetadata)?;
            let pk: i64 = column
                .get(5)
                .map_err(|_| MetadataFailure::InvalidMetadata)?;
            if pk > 0 {
                primary.push((pk, column_name.clone()));
            }
            let hidden: i64 = column
                .get(6)
                .map_err(|_| MetadataFailure::InvalidMetadata)?;
            let mut details = json!({"ordinal":(ordinal+1).to_string(),"nullable":!column.get::<_,bool>(3).map_err(|_|MetadataFailure::InvalidMetadata)?,"primaryKey":pk>0});
            if !data_type.is_empty() {
                details["dataType"] = json!(data_type);
            }
            if let Some(default) = column
                .get::<_, Option<String>>(4)
                .map_err(|_| MetadataFailure::InvalidMetadata)?
            {
                details["defaultValue"] = json!(default);
            }
            if hidden != 0 {
                details["extra"] = json!(match hidden {
                    1 => "Hidden",
                    2 => "GeneratedVirtual",
                    _ => "GeneratedStored",
                });
            }
            let mut col = object(
                "Column",
                &column_name,
                Some(&table),
                format!("{qualified}.{column_name}"),
                details,
                None,
            )?;
            if column
                .get::<_, Option<bool>>(7)
                .map_err(|_| MetadataFailure::InvalidMetadata)?
                .unwrap_or(false)
            {
                partial(&mut c, &mut col, "sqlite_default_size_limit");
            }
            let id = col.key.clone();
            if !c.object(col)? {
                return Ok(c.finish(profile));
            }
            c.relationship(&table, &id, "HAS_COLUMN")?;
        }
        if !primary.is_empty() {
            primary.sort();
            let names: Vec<_> = primary.into_iter().map(|(_, name)| name).collect();
            let pk = object(
                "Constraint",
                "PRIMARY KEY",
                Some(&table),
                format!("{qualified}.PRIMARY KEY"),
                json!({"constraintType":"PrimaryKey","columnNames":names}),
                None,
            )?;
            let id = pk.key.clone();
            if !c.object(pk)? {
                break;
            }
            c.relationship(&table, &id, "HAS_CONSTRAINT")?;
        }
        let mut indices=conn.prepare("SELECT name,\"unique\",origin,partial FROM pragma_index_list(?1,'main') ORDER BY name COLLATE BINARY").map_err(|_|failure(&c))?;
        let mut indices = indices.query([&name]).map_err(|_| failure(&c))?;
        while let Some(index) = indices.next().map_err(|_| failure(&c))? {
            let index_name: String = index.get(0).map_err(|_| MetadataFailure::InvalidMetadata)?;
            let mut cols = conn
                .prepare(
                    "SELECT name FROM pragma_index_xinfo(?1,'main') WHERE key=1 ORDER BY seqno",
                )
                .map_err(|_| failure(&c))?;
            let mut cols = cols.query([&index_name]).map_err(|_| failure(&c))?;
            let mut names = Vec::new();
            let mut expression = false;
            while let Some(col) = cols.next().map_err(|_| failure(&c))? {
                c.check()?;
                if let Some(name) = col
                    .get::<_, Option<String>>(0)
                    .map_err(|_| MetadataFailure::InvalidMetadata)?
                {
                    names.push(name);
                } else {
                    expression = true;
                }
            }
            if expression {
                c.gap("sqlite_expression_index_columns");
            }
            let (definition,truncated): (Option<String>,Option<bool>) = conn.query_row(
                "SELECT CASE WHEN length(sql)<=65536 THEN sql END,length(sql)>65536 FROM main.sqlite_schema WHERE type='index' AND name=?1",
                [&index_name], |r| Ok((r.get(0)?,r.get(1)?))).optional().map_err(|_|failure(&c))?.unwrap_or((None, None));
            let origin: String = index.get(2).map_err(|_| MetadataFailure::InvalidMetadata)?;
            if origin == "u" {
                let constraint = object(
                    "Constraint",
                    &index_name,
                    Some(&table),
                    format!("{qualified}.unique.{index_name}"),
                    json!({"constraintType":"Unique","columnNames":names}),
                    None,
                )?;
                let cid = constraint.key.clone();
                if !c.object(constraint)? {
                    return Ok(c.finish(profile));
                }
                c.relationship(&table, &cid, "HAS_CONSTRAINT")?;
            }
            let mut node = object(
                "Index",
                &index_name,
                Some(&table),
                format!("{qualified}.{index_name}"),
                json!({"unique":index.get::<_,bool>(1).map_err(|_|MetadataFailure::InvalidMetadata)?,"columnNames":names,"indexType":"BTree"}),
                definition,
            )?;
            if truncated.unwrap_or(false) {
                partial(&mut c, &mut node, "sqlite_definition_size_limit");
            }
            if expression {
                partial(&mut c, &mut node, "sqlite_expression_index_columns");
            }
            let id = node.key.clone();
            if !c.object(node)? {
                return Ok(c.finish(profile));
            }
            c.relationship(&table, &id, "HAS_INDEX")?;
        }
        let mut foreign=conn.prepare("SELECT id,seq,\"table\",\"from\",\"to\" FROM pragma_foreign_key_list(?1,'main') ORDER BY id,seq").map_err(|_|failure(&c))?;
        let mut foreign = foreign.query(params![name]).map_err(|_| failure(&c))?;
        type ForeignColumns = (String, Vec<String>, Vec<Option<String>>);
        let mut groups: BTreeMap<i64, ForeignColumns> = BTreeMap::new();
        let mut count = 0;
        while let Some(fk) = foreign.next().map_err(|_| failure(&c))? {
            c.check()?;
            if count >= c.request.limits.max_objects {
                let id: i64 = fk.get(0).map_err(|_| MetadataFailure::InvalidMetadata)?;
                groups.remove(&id);
                c.gap("sqlite_foreign_key_row_limit");
                break;
            }
            count += 1;
            let id: i64 = fk.get(0).map_err(|_| MetadataFailure::InvalidMetadata)?;
            let declared: String = fk.get(2).map_err(|_| MetadataFailure::InvalidMetadata)?;
            let group = groups
                .entry(id)
                .or_insert_with(|| (declared, Vec::new(), Vec::new()));
            group
                .1
                .push(fk.get(3).map_err(|_| MetadataFailure::InvalidMetadata)?);
            group
                .2
                .push(fk.get(4).map_err(|_| MetadataFailure::InvalidMetadata)?);
        }
        for (id, (declared, columns, declared_columns)) in groups {
            c.check()?;
            let actual:Option<String>=conn.query_row("SELECT name FROM main.sqlite_schema WHERE type='table' AND name=?1 COLLATE NOCASE",[&declared],|r|r.get(0)).optional().map_err(|_|failure(&c))?;
            let target = actual.as_deref().unwrap_or(&declared);
            let mut unavailable = actual.is_none();
            let referenced = if declared_columns.iter().all(Option::is_none) {
                let mut pk = conn
                    .prepare(
                        "SELECT name FROM pragma_table_xinfo(?1,'main') WHERE pk>0 ORDER BY pk",
                    )
                    .map_err(|_| failure(&c))?;
                let names = pk
                    .query_map([target], |r| r.get::<_, String>(0))
                    .map_err(|_| failure(&c))?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|_| failure(&c))?;
                unavailable |= names.len() != columns.len();
                names
            } else {
                unavailable |= declared_columns.iter().any(Option::is_none);
                declared_columns.into_iter().flatten().collect()
            };
            let mut constraint = object(
                "Constraint",
                &format!("foreign-key-{id}"),
                Some(&table),
                format!("{qualified}.foreign-key-{id}"),
                json!({"constraintType":"ForeignKey","columnNames":columns,"referencedTable":format!("main.{target}"),"referencedColumns":referenced}),
                None,
            )?;
            if unavailable {
                partial(
                    &mut c,
                    &mut constraint,
                    "sqlite_foreign_key_target_unavailable",
                );
            }
            let cid = constraint.key.clone();
            if !c.object(constraint)? {
                return Ok(c.finish(profile));
            }
            c.relationship(&table, &cid, "HAS_CONSTRAINT")?;
            // Source names are case-insensitive; immutable endpoints use actual catalog identity.
            let target_id = key(&["main", "Table", &schema_key, target]);
            c.relationship(&table, &target_id, "FOREIGN_KEY")?;
        }
    }
    conn.execute_batch("ROLLBACK").map_err(|_| failure(&c))?;
    Ok(c.finish(profile))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    struct Target(std::path::PathBuf);
    impl Target {
        fn new(sql: &str) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "devworks-schema-target-{}-{}.sqlite",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::SeqCst)
            ));
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(sql).unwrap();
            drop(conn);
            Self(path)
        }
        fn request(&self) -> MetadataRequest {
            MetadataRequest {
                connection_id:Id::new("database-target").unwrap(),
                config:serde_json::from_value(json!({"name":"Database metadata","engine":"Sqlite","endpoint":self.0.to_str().unwrap(),"catalogs":[],"schemas":[],"includePatterns":[],"excludePatterns":[],"writesEnabled":false,"authentication":"File","enabled":true})).unwrap(),
                password:None,ca_certificate:None,test_only:false,limits:MetadataLimits::default(),cancellation:Default::default(),
            }
        }
    }
    impl Drop for Target {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    fn run(request: MetadataRequest) -> Result<MetadataSnapshot, MetadataFailure> {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
            .block_on(SqliteMetadataReader.read(request))
    }
    #[test]
    fn native_sqlite_catalog_retains_structure_and_never_reads_rows_or_executes_views() {
        let target=Target::new("PRAGMA foreign_keys=ON;
            CREATE TABLE parent(id INTEGER PRIMARY KEY, note TEXT);
            CREATE TABLE child(id INTEGER PRIMARY KEY,parent_id INTEGER REFERENCES parent(id), amount REAL DEFAULT 2.5,
                doubled REAL GENERATED ALWAYS AS(amount*2) STORED);
            CREATE UNIQUE INDEX child_pair ON child(parent_id,amount);
            CREATE VIEW read_parent AS SELECT note FROM parent;
            INSERT INTO parent VALUES(1,'SOURCE_ROW_SENTINEL_NEVER_INDEX');
            INSERT INTO child(id,parent_id) VALUES(2,1);");
        let before = std::fs::read(&target.0).unwrap();
        let snapshot = run(target.request()).unwrap();
        assert!(matches!(snapshot.coverage.state, CoverageState::Complete));
        assert!(
            snapshot
                .objects
                .iter()
                .any(|o| o.kind == "View" && o.metadata.name.0 == "read_parent")
        );
        let amount = snapshot
            .objects
            .iter()
            .find(|o| o.kind == "Column" && o.qualified_name == "main.child.amount")
            .unwrap();
        assert_eq!(amount.metadata.data_type.as_ref().unwrap().0, "REAL");
        assert_eq!(amount.metadata.default_value.as_ref().unwrap().0, "2.5");
        let generated = snapshot
            .objects
            .iter()
            .find(|o| o.qualified_name == "main.child.doubled")
            .unwrap();
        assert_eq!(
            generated.metadata.extra.as_ref().unwrap().0,
            "GeneratedStored"
        );
        let index = snapshot
            .objects
            .iter()
            .find(|o| o.metadata.name.0 == "child_pair")
            .unwrap();
        assert_eq!(index.metadata.unique, Some(true));
        assert_eq!(
            index
                .metadata
                .column_names
                .0
                .iter()
                .map(|n| n.0.as_str())
                .collect::<Vec<_>>(),
            ["parent_id", "amount"]
        );
        assert!(snapshot.objects.iter().any(|o| {
            o.metadata
                .constraint_type
                .as_ref()
                .is_some_and(|k| k.0 == "PrimaryKey")
        }));
        assert!(snapshot.objects.iter().any(|o| {
            o.metadata
                .constraint_type
                .as_ref()
                .is_some_and(|k| k.0 == "ForeignKey")
        }));
        assert!(
            snapshot
                .relationships
                .iter()
                .any(|r| r.kind == "FOREIGN_KEY")
        );
        assert!(!format!("{snapshot:?}").contains("SOURCE_ROW_SENTINEL_NEVER_INDEX"));
        assert_eq!(
            std::fs::read(&target.0).unwrap(),
            before,
            "indexing must not mutate the source file"
        );
        let again = run(target.request()).unwrap();
        assert_eq!(
            snapshot.objects.iter().map(|o| &o.key).collect::<Vec<_>>(),
            again.objects.iter().map(|o| &o.key).collect::<Vec<_>>()
        );
    }
    #[test]
    fn empty_metadata_is_healthy_and_selection_limits_and_cancellation_are_explicit() {
        let empty = Target::new("");
        let snapshot = run(empty.request()).unwrap();
        assert!(matches!(snapshot.coverage.state, CoverageState::Complete));
        assert!(!snapshot.objects.iter().any(|o| o.kind == "Table"));
        assert!(
            snapshot
                .profile
                .unsupported_metadata
                .0
                .iter()
                .any(|k| matches!(
                    k,
                    devworks_core::protocol::records::MetadataObjectKind::Procedure
                ))
        );
        let target = Target::new("CREATE TABLE retained(id INT); CREATE TABLE excluded(id INT);");
        let mut request = target.request();
        request.config.include_patterns = serde_json::from_value(json!(["main.retained"])).unwrap();
        let filtered = run(request).unwrap();
        assert!(
            filtered
                .objects
                .iter()
                .any(|o| o.metadata.name.0 == "retained")
        );
        assert!(
            !filtered
                .objects
                .iter()
                .any(|o| o.metadata.name.0 == "excluded")
        );
        let mut request = target.request();
        request.limits.max_objects = 2;
        let bounded = run(request).unwrap();
        assert!(matches!(bounded.coverage.state, CoverageState::Partial));
        assert_eq!(bounded.objects.len(), 2);
        let request = target.request();
        request.cancellation.cancel();
        assert!(matches!(run(request), Err(MetadataFailure::Cancelled)));
        let mut request = target.request();
        request.limits.deadline = std::time::Instant::now();
        assert!(matches!(run(request), Err(MetadataFailure::Deadline)));
        let mut request = target.request();
        request.config.endpoint = serde_json::from_value(json!("relative.sqlite")).unwrap();
        assert!(matches!(run(request), Err(MetadataFailure::InvalidTarget)));
        let mut request = target.request();
        request.test_only = true;
        let probe = run(request).unwrap();
        assert!(probe.objects.is_empty());
    }
    #[test]
    fn native_sqlite_keeps_user_names_resembling_reserved_prefix() {
        let target = Target::new("CREATE TABLE sqliteX_records(id INTEGER);");
        let snapshot = run(target.request()).unwrap();
        assert!(
            snapshot
                .objects
                .iter()
                .any(|o| o.kind == "Table" && o.metadata.name.0 == "sqliteX_records")
        );
    }
    #[test]
    fn native_sqlite_composite_implicit_foreign_key_resolves_case_and_primary_order() {
        let target = Target::new(
            "CREATE TABLE ParentTable(second INTEGER,first INTEGER,PRIMARY KEY(second,first),UNIQUE(first,second)); CREATE TABLE child(b INTEGER,a INTEGER,FOREIGN KEY(b,a) REFERENCES parenttable);",
        );
        let snapshot = run(target.request()).unwrap();
        let constraints = snapshot
            .objects
            .iter()
            .filter(|o| {
                o.metadata
                    .constraint_type
                    .as_ref()
                    .is_some_and(|t| t.0 == "ForeignKey")
            })
            .collect::<Vec<_>>();
        assert_eq!(
            constraints.len(),
            1,
            "one composite declaration is one constraint"
        );
        let fk = constraints[0];
        assert_eq!(
            fk.metadata
                .column_names
                .0
                .iter()
                .map(|n| n.0.as_str())
                .collect::<Vec<_>>(),
            ["b", "a"]
        );
        assert_eq!(
            fk.metadata
                .referenced_columns
                .0
                .iter()
                .map(|n| n.0.as_str())
                .collect::<Vec<_>>(),
            ["second", "first"]
        );
        assert_eq!(
            fk.metadata.referenced_table.as_ref().unwrap().0,
            "main.ParentTable"
        );
        let parent = snapshot
            .objects
            .iter()
            .find(|o| o.kind == "Table" && o.metadata.name.0 == "ParentTable")
            .unwrap();
        assert!(
            snapshot
                .relationships
                .iter()
                .any(|r| r.kind == "FOREIGN_KEY" && r.to_key == parent.key)
        );
        assert!(snapshot.objects.iter().any(|o| {
            o.metadata
                .constraint_type
                .as_ref()
                .is_some_and(|t| t.0 == "Unique")
                && o.metadata
                    .column_names
                    .0
                    .iter()
                    .map(|n| n.0.as_str())
                    .collect::<Vec<_>>()
                    == ["first", "second"]
        }));
    }
    #[test]
    fn native_sqlite_reports_bounded_defaults_and_omitted_ddl_as_partial_per_object() {
        let target = Target::new(&format!(
            "CREATE TABLE defaults(value TEXT DEFAULT '{}'); CREATE VIEW oversized(value) AS SELECT 1 /*{}*/;",
            "x".repeat(6000),
            "x".repeat(66000)
        ));
        let snapshot = run(target.request()).unwrap();
        assert!(matches!(snapshot.coverage.state, CoverageState::Partial));
        let column = snapshot
            .objects
            .iter()
            .find(|o| o.kind == "Column" && o.qualified_name == "main.defaults.value")
            .unwrap();
        assert!(matches!(column.coverage.state, CoverageState::Partial));
        assert!(
            column
                .metadata
                .default_value
                .as_ref()
                .unwrap()
                .0
                .chars()
                .count()
                <= 4096
        );
        let view = snapshot
            .objects
            .iter()
            .find(|o| o.kind == "View" && o.metadata.name.0 == "oversized")
            .unwrap();
        assert!(view.metadata.definition_sql.is_none());
        assert!(matches!(view.coverage.state, CoverageState::Partial));
        assert!(
            view.coverage
                .gaps
                .iter()
                .any(|g| g.as_str() == "sqlite_definition_size_limit")
        );
    }
    #[test]
    fn native_sqlite_without_rowid_primary_index_has_no_catalog_ddl() {
        let target = Target::new("CREATE TABLE no_rowid(id TEXT PRIMARY KEY) WITHOUT ROWID;");
        let snapshot = run(target.request()).unwrap();
        assert!(matches!(snapshot.coverage.state, CoverageState::Complete));
        let index = snapshot.objects.iter().find(|o| o.kind == "Index").unwrap();
        assert!(index.metadata.definition_sql.is_none());
        assert!(
            snapshot
                .relationships
                .iter()
                .any(|r| r.kind == "HAS_INDEX" && r.to_key == index.key)
        );
    }

    #[test]
    fn native_sqlite_probe_reads_catalog_and_refuses_plaintext_without_writes() {
        let target = Target::new("CREATE TABLE valid(id INTEGER PRIMARY KEY);");
        let original = std::fs::read(&target.0).unwrap();
        let mut probe = target.request();
        probe.test_only = true;
        assert!(run(probe).unwrap().objects.is_empty());
        assert_eq!(std::fs::read(&target.0).unwrap(), original);
        std::fs::write(&target.0, b"ordinary text is not a SQLite database").unwrap();
        let mut probe = target.request();
        probe.test_only = true;
        assert!(matches!(run(probe), Err(MetadataFailure::Unavailable)));
        assert_eq!(
            std::fs::read(&target.0).unwrap(),
            b"ordinary text is not a SQLite database"
        );
    }
}
