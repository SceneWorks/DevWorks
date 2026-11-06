//! SQLite schema catalogs only: no view, virtual-table content or source data is selected.
use super::*;
use devworks_core::protocol::records::DbEngine;
use rusqlite::{Connection, OpenFlags, params};
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
        "limitations":["One file source exposes its main schema; SQLite has no stored-procedure catalog."]}),
    )?;
    if c.request.test_only {
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
    let mut tables=conn.prepare("SELECT name,type,CASE WHEN length(sql)<=65536 THEN sql END,length(sql)>65536 FROM main.sqlite_schema WHERE type IN('table','view') AND name NOT LIKE 'sqlite_%' ORDER BY name COLLATE BINARY").map_err(|_|failure(&c))?;
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
        if row
            .get::<_, Option<bool>>(3)
            .map_err(|_| MetadataFailure::InvalidMetadata)?
            .unwrap_or(false)
        {
            c.gap("sqlite_definition_size_limit");
        }
        let node = object(
            if kind == "view" { "View" } else { "Table" },
            &name,
            Some(&schema_key),
            qualified.clone(),
            json!({}),
            definition,
        )?;
        let table = node.key.clone();
        if !c.object(node)? {
            break;
        }
        c.relationship(&schema_key, &table, "CONTAINS")?;
        let mut columns=conn.prepare("SELECT cid,name,type,\"notnull\",dflt_value,pk,hidden FROM pragma_table_xinfo(?1,'main') ORDER BY cid").map_err(|_|failure(&c))?;
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
            let col = object(
                "Column",
                &column_name,
                Some(&table),
                format!("{qualified}.{column_name}"),
                details,
                None,
            )?;
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
            let definition: Option<String> = conn
                .query_row(
                    "SELECT sql FROM main.sqlite_schema WHERE type='index' AND name=?1",
                    [&index_name],
                    |r| r.get(0),
                )
                .map_err(|_| failure(&c))?;
            let node = object(
                "Index",
                &index_name,
                Some(&table),
                format!("{qualified}.{index_name}"),
                json!({"unique":index.get::<_,bool>(1).map_err(|_|MetadataFailure::InvalidMetadata)?,"columnNames":names,"indexType":"BTree"}),
                definition,
            )?;
            let id = node.key.clone();
            if !c.object(node)? {
                return Ok(c.finish(profile));
            }
            c.relationship(&table, &id, "HAS_INDEX")?;
        }
        let mut foreign=conn.prepare("SELECT id,seq,\"table\",\"from\",\"to\",on_update,on_delete,\"match\" FROM pragma_foreign_key_list(?1,'main') ORDER BY id,seq").map_err(|_|failure(&c))?;
        let mut foreign = foreign.query(params![name]).map_err(|_| failure(&c))?;
        while let Some(fk) = foreign.next().map_err(|_| failure(&c))? {
            c.check()?;
            let id: i64 = fk.get(0).map_err(|_| MetadataFailure::InvalidMetadata)?;
            let seq: i64 = fk.get(1).map_err(|_| MetadataFailure::InvalidMetadata)?;
            let target: String = fk.get(2).map_err(|_| MetadataFailure::InvalidMetadata)?;
            let from: String = fk.get(3).map_err(|_| MetadataFailure::InvalidMetadata)?;
            let to: Option<String> = fk.get(4).map_err(|_| MetadataFailure::InvalidMetadata)?;
            let constraint = object(
                "Constraint",
                &format!("foreign-key-{id}-{seq}"),
                Some(&table),
                format!("{qualified}.foreign-key-{id}-{seq}"),
                json!({"constraintType":"ForeignKey","columnNames":[from],"referencedTable":format!("main.{target}"),"referencedColumns":to.into_iter().collect::<Vec<_>>()}),
                None,
            )?;
            let cid = constraint.key.clone();
            if !c.object(constraint)? {
                return Ok(c.finish(profile));
            }
            c.relationship(&table, &cid, "HAS_CONSTRAINT")?;
            // The immutable constraint keeps exact declared target names even when a table is
            // outside selection. Relationship endpoints are retained only when visible.
            let target_id = key(&["main", "Table", &schema_key, &target]);
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
}
