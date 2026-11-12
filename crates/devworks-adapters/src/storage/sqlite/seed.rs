//! Test support: seed an empty data root with a store that was migrated once per process.
//!
//! Bootstrapping a store applies every shipped migration in its own flushed transaction, each
//! followed by `foreign_key_check` and `quick_check`. Test suites that open a fresh store per test
//! paid that cost hundreds of times: about 3.5 s per store on the 3-vCPU macOS CI runner, most of
//! the adapters and engine suites' time. [`seed_migrated_store`] instead copies a database file
//! migrated once (through the production writer connection), then sets the bootstrap identity,
//! ledger time and build the caller would have bootstrapped with. The result is exactly what a
//! fresh bootstrap writes: `seeded_store_matches_a_fresh_bootstrap` compares every schema object
//! and row. Opening the seeded root runs the normal existing-store path (gate, check, boot).
//!
//! Only test builds compile this: the crate's own tests and dependants that enable the
//! `test-support` feature from a dev-dependency.
use super::conn::{self, Role};
use super::{DATABASE_FILE, MIGRATIONS, migrations};
use rusqlite::{Connection, params};
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

/// The migrated database file, built on first use.
fn template() -> &'static [u8] {
    static BYTES: OnceLock<Vec<u8>> = OnceLock::new();
    BYTES.get_or_init(|| {
        let dir = tempfile::tempdir().expect("template directory");
        let path = dir.path().join(DATABASE_FILE);
        let conn =
            conn::open(&path, Role::Writer, Duration::from_secs(5)).expect("open template store");
        migrations::prepare(&conn, MIGRATIONS, || "template".into(), 0, "template")
            .expect("migrate template store");
        // Closing the only connection checkpoints the WAL into the main file and removes it.
        drop(conn);
        std::fs::read(&path).expect("read template store")
    })
}

/// Writes a fully migrated store into `data_root`, as if [`super::SqliteStore::open`] had just
/// bootstrapped it with identity `store_id` at `now_ms` for `app_build`, and before it recorded
/// its first boot. Panics if `data_root` already holds a database or on any I/O failure.
pub fn seed_migrated_store(data_root: &Path, store_id: &str, now_ms: i64, app_build: &str) {
    std::fs::create_dir_all(data_root).expect("create data root");
    let path = data_root.join(DATABASE_FILE);
    assert!(!path.exists(), "{} already exists", path.display());
    std::fs::write(&path, template()).expect("write seeded store");
    let conn = Connection::open(&path).expect("open seeded store");
    conn.execute_batch("BEGIN IMMEDIATE").unwrap();
    conn.execute(
        "UPDATE store_meta SET store_id=?1 WHERE singleton=1",
        [store_id],
    )
    .expect("set store identity");
    conn.execute(
        "UPDATE schema_migrations SET applied_at=?1, app_build=?2",
        params![now_ms, app_build],
    )
    .expect("set migration ledger");
    conn.execute_batch("COMMIT").unwrap();
}
