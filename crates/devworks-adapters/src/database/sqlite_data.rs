//! SQLite data execution uses the actual author's access trace on the open target connection.
//! The read-only catalog reader remains a separate, fixed-statement capability.
use super::NameScope;
use crate::fs::NativeFilesystem;
use devworks_core::authority::destructive::{EntryKind, FileIdentity, FilesystemResolver};
use devworks_core::fingerprint::sha256_hex;
use devworks_core::ports::database::data::*;
use devworks_core::protocol::records::DbEngine;
use devworks_core::services::database::statement::{DataMode, safe_builtin};
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::types::{Value, ValueRef};
use rusqlite::{Connection, ErrorCode, OpenFlags, params_from_iter};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub struct SqliteDataAdapter {
    protected_roots: Arc<[PathBuf]>,
}

impl SqliteDataAdapter {
    pub fn new(protected_roots: Vec<PathBuf>) -> Self {
        Self {
            protected_roots: protected_roots.into(),
        }
    }
}

impl DataAdapter for SqliteDataAdapter {
    fn capabilities(&self, engine: &DbEngine) -> DataCapabilities {
        let supported = cfg!(any(unix, windows)) && matches!(engine, DbEngine::Sqlite);
        DataCapabilities {
            read: supported,
            write: supported,
            schema_change: supported,
            atomic_ddl: supported,
        }
    }

    fn execute(&self, request: DataRequest, guard: Arc<dyn DispatchGuard>) -> DataFuture {
        let supported = self.capabilities(&request.config.engine);
        let protected = self.protected_roots.clone();
        Box::pin(async move {
            if !supported.read {
                return Err(refused(FailureKind::UnsupportedProfile));
            }
            let read = request.mode == DataMode::Read;
            tokio::task::spawn_blocking(move || execute(request, guard, &protected))
                .await
                .map_err(|_| {
                    if read {
                        refused(FailureKind::Unavailable)
                    } else {
                        DataFailure::Unknown {
                            known: empty_mutation(),
                        }
                    }
                })?
        })
    }
}

fn refused(reason: FailureKind) -> DataFailure {
    DataFailure::Refused(reason)
}

const fn empty_mutation() -> MutationResult {
    MutationResult {
        affected_rows: None,
        observed_statements: 0,
        schema_changed: false,
    }
}

fn check(request: &DataRequest) -> Result<(), FailureKind> {
    if request.cancellation.is_cancelled() {
        return Err(FailureKind::Cancelled);
    }
    if Instant::now() >= request.limits.deadline {
        return Err(FailureKind::Deadline);
    }
    Ok(())
}

fn resolve(path: &str) -> Result<(PathBuf, FileIdentity), FailureKind> {
    if !Path::new(path).is_absolute() {
        return Err(FailureKind::InvalidTarget);
    }
    let resolved = NativeFilesystem::new()
        .resolve(path, None)
        .map_err(|_| FailureKind::InvalidTarget)?;
    if resolved.entry_kind != EntryKind::File || resolved.parent_traversal {
        return Err(FailureKind::InvalidTarget);
    }
    let id = resolved.entry.identity.ok_or(FailureKind::InvalidTarget)?;
    let path = resolved.entry.path;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if !std::fs::symlink_metadata(&path).is_ok_and(|metadata| {
            metadata.is_file()
                && metadata.nlink() == 1
                && metadata.dev() == id.volume
                && metadata.ino() == id.file
        }) {
            return Err(FailureKind::InvalidTarget);
        }
    }
    Ok((path, id))
}

fn verify_file(conn: &Connection, path: &Path, expected: FileIdentity) -> Result<(), FailureKind> {
    let path = path.to_str().ok_or(FailureKind::InvalidTarget)?;
    if resolve(path)?.1 != expected {
        return Err(FailureKind::InvalidTarget);
    }
    verify_open_handle(conn, expected)?;
    if resolve(path)?.1 != expected {
        return Err(FailureKind::InvalidTarget);
    }
    Ok(())
}

#[cfg(unix)]
fn verify_open_handle(conn: &Connection, _: FileIdentity) -> Result<(), FailureKind> {
    let mut moved = 1i32;
    // The VFS checks the file of this SQLite pager against its filename. Pair this with the
    // app's physical identity check on both sides. Unsupported VFSes fail closed.
    let code = unsafe {
        rusqlite::ffi::sqlite3_file_control(
            conn.handle(),
            c"main".as_ptr(),
            rusqlite::ffi::SQLITE_FCNTL_HAS_MOVED,
            (&raw mut moved).cast(),
        )
    };
    if code != rusqlite::ffi::SQLITE_OK || moved != 0 {
        return Err(FailureKind::InvalidTarget);
    }
    Ok(())
}

#[cfg(windows)]
fn verify_open_handle(conn: &Connection, expected: FileIdentity) -> Result<(), FailureKind> {
    use std::os::windows::io::RawHandle;
    let mut handle: RawHandle = std::ptr::null_mut();
    // SQLite retains ownership. The borrowed handle is inspected only while conn is alive.
    let code = unsafe {
        rusqlite::ffi::sqlite3_file_control(
            conn.handle(),
            c"main".as_ptr(),
            rusqlite::ffi::SQLITE_FCNTL_WIN32_GET_HANDLE,
            (&raw mut handle).cast(),
        )
    };
    if code != rusqlite::ffi::SQLITE_OK
        || crate::fs::single_link_handle_identity(handle) != Some(expected)
    {
        return Err(FailureKind::InvalidTarget);
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn verify_open_handle(_: &Connection, _: FileIdentity) -> Result<(), FailureKind> {
    Err(FailureKind::UnsupportedProfile)
}

fn execute(
    request: DataRequest,
    guard: Arc<dyn DispatchGuard>,
    protected: &[PathBuf],
) -> Result<DataOutcome, DataFailure> {
    check(&request).map_err(refused)?;
    if request.password.is_some()
        || request.ca_certificate.is_some()
        || request
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
        return Err(refused(FailureKind::InvalidTarget));
    }
    let read = request.mode == DataMode::Read;
    if request.statements.is_empty() || (!read && !request.config.writes_enabled) {
        return Err(refused(FailureKind::PermissionDenied));
    }
    let (path, id) = resolve(request.config.endpoint.0.as_str()).map_err(refused)?;
    if request.file_authority != Some(id) {
        return Err(refused(FailureKind::InvalidTarget));
    }
    for root in protected {
        let root = root
            .canonicalize()
            .map_err(|_| refused(FailureKind::InvalidTarget))?;
        if path.starts_with(&root) {
            return Err(refused(FailureKind::PermissionDenied));
        }
    }
    let scope =
        Arc::new(NameScope::new(&request.config).map_err(|_| refused(FailureKind::InvalidTarget))?);
    let flags = (if read {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    } else {
        OpenFlags::SQLITE_OPEN_READ_WRITE
    }) | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    let mut conn = Connection::open_with_flags(&path, flags)
        .map_err(|error| refused(classify(&request, &error)))?;
    verify_file(&conn, &path, id).map_err(refused)?;
    conn.busy_timeout(Duration::from_millis(500))
        .map_err(|error| refused(classify(&request, &error)))?;
    let token = request.cancellation.clone();
    let deadline = request.limits.deadline;
    conn.progress_handler(
        1_000,
        Some(move || token.is_cancelled() || Instant::now() >= deadline),
    )
    .map_err(|error| refused(classify(&request, &error)))?;
    conn.execute_batch(if read {
        "PRAGMA trusted_schema=OFF; PRAGMA query_only=ON; BEGIN;"
    } else {
        "PRAGMA trusted_schema=OFF; PRAGMA foreign_keys=ON; BEGIN IMMEDIATE;"
    })
    .map_err(|error| refused(classify(&request, &error)))?;
    // The fixed catalog query takes the read snapshot before the user statement is prepared.
    verify_file(&conn, &path, id).map_err(refused)?;
    if let Some(expected) = &request.expected_schema_digest
        && schema_digest(&conn).map_err(|error| refused(classify(&request, &error)))? != *expected
    {
        return Err(refused(FailureKind::StaleSchema));
    }

    let read_digest = if read && request.include_schema_digest {
        Some(schema_digest(&conn).map_err(|error| refused(classify(&request, &error)))?)
    } else {
        None
    };
    let values: Vec<Value> = request
        .parameters
        .iter()
        .map(input_value)
        .collect::<Result<_, _>>()?;
    let mut result = empty_mutation();
    for (pos, statement) in request.statements.iter().enumerate() {
        let blocked = Arc::new(Mutex::new(None));
        let outcome = (|| {
            check(&request).map_err(refused)?;
            verify_file(&conn, &path, id).map_err(refused)?;
            let restricted =
                restricted_tables(&conn).map_err(|error| refused(classify(&request, &error)))?;
            let guard_for_hook = guard.clone();
            let refused_in_hook = Arc::clone(&blocked);
            let audit = Audit {
                scope: Arc::clone(&scope),
                restricted,
                guard: guard_for_hook,
                denied: refused_in_hook,
                mode: request.mode,
                ddl: statement.schema_change,
            };
            conn.authorizer(Some(move |context: AuthContext<'_>| {
                audit.authorize(context)
            }))
            .map_err(|error| refused(classify(&request, &error)))?;
            let mut prepared = conn
                .prepare(statement.sql())
                .map_err(|error| operation_error(&request, &blocked, &error))?;
            guard.before_statement(pos)?;
            let bound = if request.mode == DataMode::Migration {
                &[]
            } else {
                values.as_slice()
            };
            if read {
                return read_rows(&request, &mut prepared, bound, &blocked)
                    .map(|rows| Some(DataOutcome::Read(rows)));
            }
            let affected = prepared
                .execute(params_from_iter(bound))
                .map_err(|error| operation_error(&request, &blocked, &error))?;
            result.observed_statements += 1;
            result.schema_changed |= statement.schema_change;
            if !statement.schema_change {
                result.affected_rows = result
                    .affected_rows
                    .unwrap_or(0)
                    .checked_add(affected as i64);
            }
            Ok(None)
        })();
        let hook_removed = conn.authorizer(None::<fn(AuthContext<'_>) -> Authorization>);
        if hook_removed.is_err() {
            return Err(DataFailure::Unknown {
                known: empty_mutation(),
            });
        }
        match outcome {
            Ok(Some(DataOutcome::Read(mut rows))) => {
                rows.schema_digest.clone_from(&read_digest);
                check(&request).map_err(refused)?;
                verify_file(&conn, &path, id).map_err(refused)?;
                conn.execute_batch("ROLLBACK")
                    .map_err(|error| refused(classify(&request, &error)))?;
                return Ok(DataOutcome::Read(rows));
            }
            Ok(Some(DataOutcome::Mutation(_))) => unreachable!("only read outcomes exit this loop"),
            Ok(None) => {}
            Err(failure) => return Err(rollback(&conn, failure, result.observed_statements > 0)),
        }
    }
    if let Err(failure) = check(&request)
        .and_then(|()| verify_file(&conn, &path, id))
        .map_err(refused)
        .and_then(|()| guard.before_commit())
    {
        return Err(rollback(&conn, failure, true));
    }
    if let Err(error) = conn.execute_batch("COMMIT") {
        let reason = classify(&request, &error);
        return if conn.is_autocommit() {
            Err(DataFailure::Unknown {
                known: empty_mutation(),
            })
        } else {
            Err(rollback(&conn, refused(reason), true))
        };
    }
    guard
        .observed_statement(request.statements.len() - 1, result)
        .map_err(|_| DataFailure::Unknown { known: result })?;
    Ok(DataOutcome::Mutation(result))
}

fn rollback(conn: &Connection, failure: DataFailure, previously_executed: bool) -> DataFailure {
    let was_open = !conn.is_autocommit();
    if was_open && conn.execute_batch("ROLLBACK").is_err() {
        return DataFailure::Unknown {
            known: empty_mutation(),
        };
    }
    match failure {
        DataFailure::Refused(reason) if previously_executed => DataFailure::RolledBack(reason),
        other => other,
    }
}

fn classify(request: &DataRequest, error: &rusqlite::Error) -> FailureKind {
    if let Err(reason) = check(request) {
        return reason;
    }
    match error.sqlite_error_code() {
        Some(
            ErrorCode::AuthorizationForStatementDenied
            | ErrorCode::PermissionDenied
            | ErrorCode::ReadOnly,
        ) => FailureKind::PermissionDenied,
        Some(ErrorCode::ConstraintViolation) => FailureKind::Constraint,
        Some(ErrorCode::SchemaChanged) => FailureKind::StaleSchema,
        Some(ErrorCode::TooBig) => FailureKind::ResultLimit,
        Some(ErrorCode::Unknown) | None => FailureKind::UnsupportedStatement,
        _ => FailureKind::Unavailable,
    }
}

fn operation_error(
    request: &DataRequest,
    denied: &Mutex<Option<DataFailure>>,
    error: &rusqlite::Error,
) -> DataFailure {
    denied
        .lock()
        .ok()
        .and_then(|mut cause| cause.take())
        .unwrap_or_else(|| refused(classify(request, error)))
}

fn schema_digest(conn: &Connection) -> rusqlite::Result<String> {
    let mut digest_input = Vec::new();
    let mut statement = conn.prepare(
        "SELECT type,name,tbl_name,sql FROM main.sqlite_schema WHERE name NOT GLOB 'sqlite_stat*'
         ORDER BY type,name,tbl_name",
    )?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        for index in 0..4 {
            let value = row.get_ref(index)?;
            let bytes = match value {
                ValueRef::Text(bytes) => bytes,
                ValueRef::Null => &[],
                _ => return Err(rusqlite::Error::InvalidQuery),
            };
            if digest_input.len().saturating_add(bytes.len()) >= 16 * 1024 * 1024 {
                return Err(rusqlite::Error::InvalidQuery);
            }
            digest_input.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
            digest_input.extend_from_slice(bytes);
        }
    }
    Ok(sha256_hex(&digest_input))
}

fn restricted_tables(conn: &Connection) -> rusqlite::Result<BTreeSet<String>> {
    conn.prepare(
        "SELECT name FROM pragma_table_list WHERE schema='main' AND type IN ('virtual','shadow')",
    )?
    .query_map([], |row| {
        row.get::<_, String>(0)
            .map(|name| name.to_ascii_lowercase())
    })?
    .collect()
}

struct Audit {
    scope: Arc<NameScope>,
    restricted: BTreeSet<String>,
    guard: Arc<dyn DispatchGuard>,
    denied: Arc<Mutex<Option<DataFailure>>>,
    mode: DataMode,
    ddl: bool,
}

impl Audit {
    fn table_allowed(&self, table: &str) -> bool {
        let folded = table.to_ascii_lowercase();
        !self.restricted.contains(&folded)
            && !folded.starts_with("sqlite_")
            && self.scope.includes(table, &format!("main.{table}"))
    }

    fn authorize(&self, context: AuthContext<'_>) -> Authorization {
        let result = self.access(context);
        if let Err(failure) = result {
            if let Ok(mut cause) = self.denied.lock() {
                *cause = Some(failure);
            }
            Authorization::Deny
        } else {
            Authorization::Allow
        }
    }

    fn access(&self, ctx: AuthContext<'_>) -> Result<(), DataFailure> {
        let internal_catalog = |name: &str| matches!(name, "sqlite_master" | "sqlite_schema");
        let allowed = match ctx.action {
            _ if ctx.database_name.is_some_and(|db| db != "main") => false,
            AuthAction::Select | AuthAction::Recursive => true,
            AuthAction::Function { function_name } => {
                safe_builtin(&function_name.to_ascii_lowercase())
                    || (self.ddl
                        && matches!(
                            function_name,
                            "sqlite_rename_column"
                                | "sqlite_rename_table"
                                | "sqlite_rename_test"
                                | "sqlite_rename_quotefix"
                                | "sqlite_drop_column"
                        ))
            }
            AuthAction::Read {
                table_name,
                column_name,
            } if self.table_allowed(table_name) => {
                if !column_name.is_empty() {
                    self.guard.check_columns(&[PhysicalColumn {
                        catalog: "main".into(),
                        schema: "main".into(),
                        table: table_name.into(),
                        column: column_name.into(),
                    }])?;
                }
                true
            }
            AuthAction::Read { table_name, .. }
            | AuthAction::Insert { table_name }
            | AuthAction::Delete { table_name }
            | AuthAction::Update { table_name, .. }
                if self.ddl && internal_catalog(table_name) =>
            {
                true
            }
            AuthAction::Insert { table_name } | AuthAction::Delete { table_name }
                if !self.ddl && self.mode != DataMode::Read =>
            {
                self.table_allowed(table_name)
            }
            AuthAction::Update {
                table_name,
                column_name,
            } if !self.ddl && self.mode != DataMode::Read && self.table_allowed(table_name) => {
                self.guard.check_columns(&[PhysicalColumn {
                    catalog: "main".into(),
                    schema: "main".into(),
                    table: table_name.into(),
                    column: column_name.into(),
                }])?;
                true
            }
            AuthAction::CreateIndex { table_name, .. }
            | AuthAction::DropIndex { table_name, .. }
            | AuthAction::CreateTable { table_name }
            | AuthAction::DropTable { table_name }
            | AuthAction::CreateView {
                view_name: table_name,
            }
            | AuthAction::DropView {
                view_name: table_name,
            }
            | AuthAction::AlterTable {
                database_name: "main",
                table_name,
            } if self.ddl => self.table_allowed(table_name),
            AuthAction::Reindex { .. } if self.ddl => true,
            _ => false,
        };
        if allowed {
            Ok(())
        } else {
            Err(refused(FailureKind::PermissionDenied))
        }
    }
}

fn input_value(value: &Scalar) -> Result<Value, DataFailure> {
    Ok(match value {
        Scalar::Null => Value::Null,
        Scalar::Boolean(value) => Value::Integer(i64::from(*value)),
        Scalar::Integer(value) => Value::Integer(*value),
        // SQLite does not provide an exact DECIMAL bind type.
        Scalar::Decimal(_) => return Err(refused(FailureKind::UnsupportedValue)),
        Scalar::Text(value) => Value::Text(value.clone()),
        Scalar::Bytes(value) => Value::Blob(value.clone()),
    })
}

fn read_rows(
    request: &DataRequest,
    prepared: &mut rusqlite::Statement<'_>,
    parameters: &[Value],
    blocked: &Mutex<Option<DataFailure>>,
) -> Result<ReadResult, DataFailure> {
    let count = prepared.column_count();
    if count > request.limits.columns {
        return Err(refused(FailureKind::ResultLimit));
    }
    let columns = prepared
        .column_names()
        .into_iter()
        .map(|name| Column {
            name: name.into(),
            data_type: "Dynamic".into(),
        })
        .collect::<Vec<_>>();
    let mut size = columns
        .iter()
        .map(|column| column.name.len() + column.data_type.len())
        .sum::<usize>();
    if size > request.limits.bytes {
        return Err(refused(FailureKind::ResultLimit));
    }
    let mut cursor = prepared
        .query(params_from_iter(parameters))
        .map_err(|error| operation_error(request, blocked, &error))?;
    let mut rows = Vec::new();
    let mut truncated = false;
    while let Some(row) = cursor
        .next()
        .map_err(|error| operation_error(request, blocked, &error))?
    {
        check(request).map_err(refused)?;
        if rows.len() == request.limits.rows {
            truncated = true;
            break;
        }
        let mut values = Vec::with_capacity(count);
        for index in 0..count {
            let value = row
                .get_ref(index)
                .map_err(|_| refused(FailureKind::UnsupportedValue))?;
            let bytes = match value {
                ValueRef::Text(bytes) | ValueRef::Blob(bytes) => bytes.len(),
                _ => 32,
            };
            size = size.saturating_add(bytes.saturating_add(16));
            if size > request.limits.bytes {
                return Err(refused(FailureKind::ResultLimit));
            }
            values.push(match value {
                ValueRef::Null => Scalar::Null,
                ValueRef::Integer(value) => Scalar::Integer(value),
                ValueRef::Real(value) if value.is_finite() => Scalar::Decimal(value.to_string()),
                ValueRef::Real(_) => return Err(refused(FailureKind::UnsupportedValue)),
                ValueRef::Text(value) => Scalar::Text(
                    std::str::from_utf8(value)
                        .map_err(|_| refused(FailureKind::UnsupportedValue))?
                        .to_owned(),
                ),
                ValueRef::Blob(value) => Scalar::Bytes(value.to_vec()),
            });
        }
        rows.push(values);
    }
    Ok(ReadResult {
        columns,
        rows,
        truncated,
        schema_digest: None,
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use devworks_core::protocol::Id;
    use devworks_core::protocol::records::ConnectionConfig;
    use devworks_core::services::database::statement;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct Guard {
        reject_commit: bool,
        observed: AtomicUsize,
    }

    impl DispatchGuard for Guard {
        fn check_columns(&self, columns: &[PhysicalColumn]) -> Result<(), DataFailure> {
            if columns
                .iter()
                .any(|column| column.table == "private_items" && column.column == "secret")
            {
                return Err(refused(FailureKind::PermissionDenied));
            }
            Ok(())
        }

        fn before_statement(&self, _: usize) -> Result<(), DataFailure> {
            Ok(())
        }

        fn before_commit(&self) -> Result<(), DataFailure> {
            if self.reject_commit {
                return Err(refused(FailureKind::PermissionDenied));
            }
            Ok(())
        }

        fn observed_statement(
            &self,
            _: usize,
            mutation: MutationResult,
        ) -> Result<(), DataFailure> {
            self.observed
                .store(mutation.observed_statements, Ordering::SeqCst);
            Ok(())
        }
    }

    struct Target {
        directory: tempfile::TempDir,
        path: PathBuf,
    }

    impl Target {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("data.sqlite");
            Connection::open(&path)
                .unwrap()
                .execute_batch(
                    "CREATE TABLE public_items (id INTEGER PRIMARY KEY, secret TEXT);
                 CREATE TABLE private_items (id INTEGER PRIMARY KEY, secret TEXT);
                 INSERT INTO public_items VALUES (1, 'a'), (2, 'b');
                 INSERT INTO private_items VALUES (1, 'private');
                 CREATE VIEW read_private AS SELECT secret FROM private_items;",
                )
                .unwrap();
            Self { directory, path }
        }

        fn request(&self, mode: DataMode, sql: &str) -> DataRequest {
            let path = self.path.to_str().unwrap();
            let config: ConnectionConfig = serde_json::from_value(serde_json::json!({
                "name": "Test database",
                "engine": "Sqlite",
                "endpoint": path,
                "catalogs": [],
                "schemas": [],
                "includePatterns": [],
                "excludePatterns": [],
                "writesEnabled": true,
                "authentication": "File",
                "enabled": true
            }))
            .unwrap();
            DataRequest {
                id: Id::new("data-target").unwrap(),
                mode,
                config,
                password: None,
                ca_certificate: None,
                statements: statement::check(&DbEngine::Sqlite, mode, sql).unwrap(),
                parameters: vec![],
                expected_schema_digest: None,
                include_schema_digest: false,
                limits: DataLimits {
                    deadline: Instant::now() + Duration::from_secs(5),
                    rows: 1,
                    bytes: 1024,
                    columns: 16,
                },
                cancellation: Default::default(),
                file_authority: Some(resolve(path).unwrap().1),
            }
        }
    }

    #[test]
    fn native_read_binds_values_and_checks_views_joins_and_output_limits() {
        let target = Target::new();
        let mut read = target.request(
            DataMode::Read,
            "SELECT secret FROM public_items WHERE id > ? ORDER BY id",
        );
        read.parameters.push(Scalar::Integer(0));
        let Ok(DataOutcome::Read(rows)) = execute(read, Arc::new(Guard::default()), &[]) else {
            panic!("the native read failed");
        };
        assert!(rows.truncated);
        assert!(rows.rows == vec![vec![Scalar::Text("a".into())]]);

        for sql in [
            "SELECT secret FROM read_private",
            "SELECT p.secret FROM public_items p JOIN private_items q ON p.id = q.id WHERE q.secret IS NOT NULL",
            "SELECT private_items.secret AS alias FROM private_items",
        ] {
            let read = target.request(DataMode::Read, sql);
            assert!(
                matches!(
                    execute(read, Arc::new(Guard::default()), &[]),
                    Err(DataFailure::Refused(FailureKind::PermissionDenied))
                ),
                "{sql}"
            );
        }
    }

    #[test]
    fn native_write_needs_current_authority_and_records_only_the_confirmed_commit() {
        let target = Target::new();
        let guard = Arc::new(Guard {
            reject_commit: true,
            ..Guard::default()
        });
        let write = target.request(
            DataMode::Write,
            "INSERT INTO public_items (id, secret) VALUES (3, 'c')",
        );
        assert!(matches!(
            execute(write, guard.clone(), &[]),
            Err(DataFailure::RolledBack(FailureKind::PermissionDenied))
        ));
        assert_eq!(guard.observed.load(Ordering::SeqCst), 0);

        let guard = Arc::new(Guard::default());
        let write = target.request(
            DataMode::Write,
            "INSERT INTO public_items (id, secret) VALUES (3, 'c')",
        );
        let Ok(DataOutcome::Mutation(result)) = execute(write, guard.clone(), &[]) else {
            panic!("the authorized write failed");
        };
        assert_eq!(result.affected_rows, Some(1));
        assert_eq!(guard.observed.load(Ordering::SeqCst), 1);
        let db = Connection::open(&target.path).unwrap();
        assert_eq!(
            db.query_row("SELECT secret FROM public_items WHERE id=3", [], |row| {
                row.get::<_, String>(0)
            })
            .unwrap(),
            "c"
        );
    }

    #[test]
    fn native_migration_is_atomic_and_checks_schema_on_the_active_handle() {
        let target = Target::new();
        let db = Connection::open(&target.path).unwrap();
        let current_digest = schema_digest(&db).unwrap();
        let mut stale = target.request(DataMode::Schema, "CREATE TABLE new_items (id INTEGER)");
        stale.expected_schema_digest = Some("invalid".into());
        assert!(matches!(
            execute(stale, Arc::new(Guard::default()), &[]),
            Err(DataFailure::Refused(FailureKind::StaleSchema))
        ));

        let mut migration = target.request(DataMode::Migration,
            "CREATE TABLE new_items (id INTEGER PRIMARY KEY); INSERT INTO new_items (id) VALUES (1); INSERT INTO new_items (id) VALUES (1)");
        migration.expected_schema_digest = Some(current_digest.clone());
        assert!(matches!(
            execute(migration, Arc::new(Guard::default()), &[]),
            Err(DataFailure::RolledBack(FailureKind::Constraint))
        ));
        assert_eq!(schema_digest(&db).unwrap(), current_digest);

        let mut migration = target.request(DataMode::Migration,
            "CREATE TABLE new_items (id INTEGER PRIMARY KEY); INSERT INTO new_items (id) VALUES (1)");
        migration.expected_schema_digest = Some(current_digest.clone());
        let Ok(DataOutcome::Mutation(result)) = execute(migration, Arc::new(Guard::default()), &[])
        else {
            panic!("the native migration failed");
        };
        assert!(result.schema_changed);
        assert_eq!(result.observed_statements, 2);
        assert_ne!(schema_digest(&db).unwrap(), current_digest);
    }

    #[test]
    fn exact_selected_file_and_protected_directory_are_checked_before_execution() {
        let target = Target::new();
        let read = target.request(DataMode::Read, "SELECT id FROM public_items");
        assert!(matches!(
            execute(
                read,
                Arc::new(Guard::default()),
                &[target.directory.path().into()]
            ),
            Err(DataFailure::Refused(FailureKind::PermissionDenied))
        ));
        let mut read = target.request(DataMode::Read, "SELECT id FROM public_items");
        read.file_authority = Some(FileIdentity { volume: 0, file: 0 });
        assert!(matches!(
            execute(read, Arc::new(Guard::default()), &[]),
            Err(DataFailure::Refused(FailureKind::InvalidTarget))
        ));
        let read = target.request(DataMode::Read, "SELECT id FROM public_items");
        read.cancellation.cancel();
        assert!(matches!(
            execute(read, Arc::new(Guard::default()), &[]),
            Err(DataFailure::Refused(FailureKind::Cancelled))
        ));
    }
}
