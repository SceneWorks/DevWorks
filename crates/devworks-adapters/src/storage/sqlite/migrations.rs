//! Versioned schema migrations with a checksum ledger, checked before any write access (STORE-06).
//!
//! The ledger (`schema_migrations`) is authoritative; `user_version` is only an aid. Each
//! migration owns one `BEGIN IMMEDIATE` transaction that changes the schema and updates
//! `store_meta` versions and the ledger together, followed by foreign-key and quick integrity
//! checks. An applied migration whose checksum differs from this build's text is an
//! incompatibility, never permission to rerun edited SQL. Only transactional, non-destructive
//! migrations are applied automatically; no backup is ever taken automatically (STORE-07).
use super::conn::db;
use devworks_core::fingerprint::sha256_hex;
use devworks_core::ports::storage::{AccessMode, AppliedMigration, StorageError};
use rusqlite::{Connection, OptionalExtension, params};

#[derive(Clone, Copy, Debug)]
pub struct Migration {
    pub version: i64,
    /// Oldest build version that can still read a store at this version.
    pub minimum_reader: i64,
    /// Oldest build version that can still write a store at this version.
    pub minimum_writer: i64,
    pub sql: &'static str,
    /// Recorded SHA-256 of `sql`. Shipped migrations are immutable: a test fails when the text
    /// no longer matches, because every store that applied the old text would refuse to open.
    pub expected_sha256: &'static str,
    /// The migration rebuilds a table other tables reference (SQLite's documented table-rebuild
    /// procedure: create, copy, drop, rename). Foreign-key enforcement is switched off for its
    /// one transaction, because dropping a referenced table would otherwise fail, and the full
    /// foreign-key check must still pass before it commits. Enforcement is restored afterwards
    /// whatever the outcome.
    pub rebuilds_tables: bool,
}
impl Migration {
    pub fn checksum(&self) -> String {
        sha256_hex(self.sql.as_bytes())
    }
}

const SYSTEMONE_0064_SHA256: &str =
    "1862c1bf4f8a16e13eb6c34c73515c8ad144fffebe64f7f6742adec95fec592a";
const KEEP_WORKING_SHA256: &str =
    "70360b84c1f238653ca0ef7272447c4d35b123fefc9df28bc3daada15fd6fcab";

/// This build's migrations, in version order. The last version is the build's schema version.
pub const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        minimum_reader: 1,
        minimum_writer: 1,
        sql: include_str!("migrations/0001_initial.sql"),
        expected_sha256: MIGRATION_0001_SHA256,
        rebuilds_tables: false,
    },
    // Writers older than 2 would not bind claims to their boot epoch, so they may not write.
    Migration {
        version: 2,
        minimum_reader: 1,
        minimum_writer: 2,
        sql: include_str!("migrations/0002_claim_boot.sql"),
        expected_sha256: MIGRATION_0002_SHA256,
        rebuilds_tables: false,
    },
    // A version-2 build can still read deliveries, but writing them without partition/sequence
    // and revision bookkeeping would break ordered delivery, so writers must understand 3.
    Migration {
        version: 3,
        minimum_reader: 1,
        minimum_writer: 3,
        sql: include_str!("migrations/0003_durable_delivery.sql"),
        expected_sha256: MIGRATION_0003_SHA256,
        rebuilds_tables: false,
    },
    // Additive (a new table only). Applying a migration overwrites the store's writer floor, so
    // it keeps the floor of migration 3: it must never re-admit writers that cannot keep ordered
    // delivery bookkeeping or bind claims to their boot.
    Migration {
        version: 4,
        minimum_reader: 1,
        minimum_writer: 3,
        sql: include_str!("migrations/0004_settings.sql"),
        expected_sha256: MIGRATION_0004_SHA256,
        rebuilds_tables: false,
    },
    // Additive tables. Writers older than 5 would ignore principal revocation and authority
    // grants, so they may not write; the floor also stays at or above every earlier floor.
    // Readers older than 5 treat the `mcp_client` actor kind as corrupt, so they may not read
    // either: older builds open such a store read-refused instead of misreporting corruption.
    Migration {
        version: 5,
        minimum_reader: 5,
        minimum_writer: 5,
        sql: include_str!("migrations/0005_authority.sql"),
        expected_sha256: MIGRATION_0005_SHA256,
        rebuilds_tables: false,
    },
    // Additive columns, tables and indexes. Applying a migration overwrites both store floors,
    // so it keeps the highest floors so far (5/5). Nothing here needs a higher reader floor:
    // builds 5 and older never read artifact rows, and the added columns all have defaults, so
    // their existing queries see the same results. Raising the writer floor is not needed
    // either: no earlier build writes artifact tables. Lowering either floor would re-admit
    // builds that migration 5 refuses.
    Migration {
        version: 6,
        minimum_reader: 5,
        minimum_writer: 5,
        sql: include_str!("migrations/0006_artifacts.sql"),
        expected_sha256: MIGRATION_0006_SHA256,
        rebuilds_tables: false,
    },
    // Additive columns (all defaulted or nullable), tables and indexes. It keeps the highest
    // floors so far (5/5), because applying a migration overwrites both store floors and a lower
    // value would re-admit builds that migration 5 refuses. No higher floor is needed: builds 6
    // and older never read or write sessions, transcript entries or any of the new tables (no
    // earlier build has a session repository), and the defaulted columns leave their results
    // unchanged.
    Migration {
        version: 7,
        minimum_reader: 5,
        minimum_writer: 5,
        sql: include_str!("migrations/0007_sessions.sql"),
        expected_sha256: MIGRATION_0007_SHA256,
        rebuilds_tables: false,
    },
    // Rebuilds `artifacts` (nullable project columns for the workspace scope) and adds the
    // receipt reply records. Both floors rise to 8: a build 7 or older reads every artifact row
    // as project-owned and would report a workspace artifact as corrupt, and as a writer it
    // would delete receipt-retained artifacts and record receipts without their publication
    // intent. 8 is at least every earlier floor (5/5).
    Migration {
        version: 8,
        minimum_reader: 8,
        minimum_writer: 8,
        sql: include_str!("migrations/0008_receipt_replies.sql"),
        expected_sha256: MIGRATION_0008_SHA256,
        rebuilds_tables: true,
    },
    // Additive indexes and the tool_invocations table (backfilled from the recorded responses).
    // The reader floor stays at 8: no earlier build reads tool_invocations, and the indexes leave
    // every query's results unchanged. The writer floor rises to 9: a build 8 writer would append
    // tool responses without their invocation facts, so revision-2 tool usage would silently
    // miss them. Both are at least every earlier floor (8/8).
    Migration {
        version: 9,
        minimum_reader: 8,
        minimum_writer: 9,
        sql: include_str!("migrations/0009_history_feeds.sql"),
        expected_sha256: MIGRATION_0009_SHA256,
        rebuilds_tables: false,
    },
    // Additive columns (all defaulted or nullable), tables and indexes. The reader floor stays
    // at 8: no earlier build reads resources, leases or owned processes (the migration 1
    // resources table was never written), and the defaulted columns leave every existing query
    // unchanged. The writer floor stays at 9: no earlier build writes any of these records, so
    // it cannot break their fencing or queue invariants. Both are at least every earlier floor.
    Migration {
        version: 10,
        minimum_reader: 8,
        minimum_writer: 9,
        sql: include_str!("migrations/0010_resources_processes.sql"),
        expected_sha256: MIGRATION_0010_SHA256,
        rebuilds_tables: false,
    },
    // Additive columns (all defaulted or nullable), tables and indexes for run root controls.
    // The writer floor rises to 11: a build 10 or older has no run-control gate, so as a writer
    // it would claim and run the root-scoped deliveries of a paused, held or stopped root and
    // bypass the restart hold; it must open this store read-only. The reader floor stays at 8:
    // no earlier build reads root controls, activations, assignments, blockers, run instructions
    // or session turns (the migration 1 tables were never written by a build), and the defaulted
    // columns leave every query an earlier reader makes unchanged, so reading stays safe. Both
    // are at least every earlier floor.
    Migration {
        version: 11,
        minimum_reader: 8,
        minimum_writer: 11,
        sql: include_str!("migrations/0011_run_controls.sql"),
        expected_sha256: MIGRATION_0011_SHA256,
        rebuilds_tables: false,
    },
    // Additive: the retained diagnostic log, its retention boundary and export intents (three
    // tables and their indexes), independent of every table of migration 11. The writer floor is
    // 11, the floor migration 11 set: applying a migration overwrites the store's floors, so a
    // lower value would re-admit the build-10 writers that migration 11 refuses (they would
    // bypass run controls). It need not rise to 12: a build-11 writer never writes these tables,
    // so it cannot break the log's sequence, retention boundary or export claims, and every
    // other record it writes is unchanged here. The reader floor stays at 8: no earlier build
    // reads these tables, and they leave every query an earlier reader makes unchanged.
    Migration {
        version: 12,
        minimum_reader: 8,
        minimum_writer: 11,
        sql: include_str!("migrations/0012_diagnostics.sql"),
        expected_sha256: MIGRATION_0012_SHA256,
        rebuilds_tables: false,
    },
    // Additive: the router catalog (seeded once), tier cursors, session pins, cooldowns and
    // request bindings (five tables and their indexes), independent of every table of
    // migrations 11-12.
    // The writer floor stays at 11 (the highest so far): applying a migration overwrites the
    // store's floors, so a lower value would re-admit the build-10 writers migration 11 refuses.
    // It need not rise: no earlier build writes these tables, so it cannot break their cursor
    // and pin compare-and-set rules, and every record it writes is unchanged here. The reader
    // floor stays at 8: no earlier build reads these tables, and every query an earlier reader
    // makes is unchanged.
    Migration {
        version: 13,
        minimum_reader: 8,
        minimum_writer: 11,
        sql: include_str!("migrations/0013_router.sql"),
        expected_sha256: MIGRATION_0013_SHA256,
        rebuilds_tables: false,
    },
    // Additive: project and repository side tables, the index view registry and repository
    // texts, and four indexes; the migration 1 tables keep their shape. The writer floor rises
    // to 14: a build without this migration (up to and including one with migration 13) treats
    // only `Archived` as closed to work, so as a writer it would dispatch the work of a project
    // tombstoned `Deleted`, and it would accept configuration, sessions, artifacts and resources
    // for such a project. The reader floor stays at 8: no earlier build reads the new tables,
    // and the indexes leave every query an earlier reader makes unchanged. Both are at least
    // every earlier floor.
    Migration {
        version: 14,
        minimum_reader: 8,
        minimum_writer: 14,
        sql: include_str!("migrations/0014_projects.sql"),
        expected_sha256: MIGRATION_0014_SHA256,
        rebuilds_tables: false,
    },
    // Additive: provider connections and SystemOne decision models (two tables and their
    // indexes), one nullable column on router_models and a correction of still-unedited seed
    // rows; independent of every table of migration 14. The writer floor stays at 14 (the
    // highest so far): applying a migration overwrites the store's floors, so a lower value
    // would re-admit the writers migration 14 refuses (they would dispatch the work of a deleted
    // project). It need not rise: no earlier build writes the new tables, and the only earlier
    // writer of router_models names its columns and leaves connection_id untouched, so a binding
    // it does not know is kept, never cleared; a binding that no longer matches the model's
    // profile only makes that model ineligible. The reader floor stays at 8: earlier readers
    // select explicit columns, so the added column and tables leave every query they make
    // unchanged.
    Migration {
        version: 15,
        minimum_reader: 8,
        minimum_writer: 14,
        sql: include_str!("migrations/0015_router_connections.sql"),
        expected_sha256: MIGRATION_0015_SHA256,
        rebuilds_tables: false,
    },
    // Additive: the work detail, queue revision, plan-approval request, readiness and stop
    // coverage side tables and five indexes; the migration 1 work tables keep their shape. The
    // writer floor rises to 16: a build without this migration evaluates dispatch eligibility
    // without the work fences written here (unverified or open prerequisites, a card's own
    // unlink pause under its new feature, a card moved to another feature, the parent feature's
    // controls over a linked card), so as a writer it would dispatch work those records hold,
    // and it would settle a feature's stop without the linked cards it came to cover. The reader
    // floor stays at 8: no earlier build reads the new tables, and the indexes leave every query
    // an earlier reader makes unchanged. Both are at least every earlier floor.
    Migration {
        version: 16,
        minimum_reader: 8,
        minimum_writer: 16,
        sql: include_str!("migrations/0016_work.sql"),
        expected_sha256: MIGRATION_0016_SHA256,
        rebuilds_tables: false,
    },
    // Additive: columns (all defaulted or nullable) on `schedules` and `job_runs`, which no
    // earlier build ever wrote (migration 1 declared them for this item), their indexes, and four
    // new tables (run links, discovery checkpoints, action identities, per-run usage);
    // independent of every table of migrations 14-16. The writer floor is 16, the highest so far
    // (migration 16 raised it): applying a migration overwrites the store's floors, so a lower
    // value would re-admit the writers migrations 14 and 16 refuse (they would dispatch a
    // deleted project's work, or work the migration 16 fences hold). It need not rise to 17: no
    // earlier build reads or writes any of these records, so it cannot break the one-active-run
    // index, the title uniqueness or the occurrence identities, and every record it does write
    // is unchanged here; a project an earlier writer archives still fences its schedules, since
    // every claim, dispatch and resumption checks the project's lifecycle and archive epoch
    // itself. The reader floor stays at 8: no earlier build reads these tables, and every query
    // an earlier reader makes returns the same results.
    Migration {
        version: 17,
        minimum_reader: 8,
        minimum_writer: 16,
        sql: include_str!("migrations/0017_jobs.sql"),
        expected_sha256: MIGRATION_0017_SHA256,
        rebuilds_tables: false,
    },
    // Additive: the unified Knowledge tables, their indexes and two FTS5 projections, referencing
    // only actors, projects and sessions (migration 1/7 tables, unchanged).
    // The writer floor stays at 16 (the highest so far): applying a migration overwrites the
    // store's floors, so a lower value would re-admit the writers migrations 14 and 16 refuse. It
    // need not rise: no earlier build writes these tables, so none can break their
    // compare-and-set revisions, scope revisions or lexical rows, and every record an earlier
    // writer writes is unchanged here. The reader floor stays at 8: no earlier build reads these
    // tables, and every query an earlier reader makes returns the same results.
    Migration {
        version: 18,
        minimum_reader: 8,
        minimum_writer: 16,
        sql: include_str!("migrations/0018_knowledge.sql"),
        expected_sha256: MIGRATION_0018_SHA256,
        rebuilds_tables: false,
    },
    // Additive (DW-023): index source configuration, observed source identity, worktrees, index
    // runs and overlays (five tables), defaulted payload columns on the migration 1 unit and edge
    // tables, and six indexes. The writer floor rises to 19 (above every earlier
    // floor): an earlier writer's `repositories.remove_index` removes only the migration 14
    // registry rows, so it would leave this build's published pointer, views and graph payload
    // describing an index the registry no longer lists (a removed index that still reads as
    // current). The reader floor stays at 8: no earlier build reads the new tables or columns,
    // and every query an earlier reader makes returns the same rows (the added columns are
    // defaulted). Both are at least every earlier floor.
    Migration {
        version: 19,
        minimum_reader: 8,
        minimum_writer: 19,
        sql: include_str!("migrations/0019_indexing.sql"),
        expected_sha256: MIGRATION_0019_SHA256,
        rebuilds_tables: false,
    },
    // Additive: the package registry head, package records, verified versions, approval
    // proposals, retained state generations, index sources and availability observations (seven
    // tables and an index), independent of every table of migrations 11-19 (it references only
    // its own tables). The writer floor stays at 19, the highest so far: applying a migration
    // overwrites both floors, so a lower writer floor would re-admit the writers migrations 11, 14,
    // 16 and 19 refuse. It need not rise: no earlier build reads or writes these tables, and the
    // package handler state rows (migration 1) a generation publication rewrites were never
    // written by an earlier build. The reader floor stays at 8: no earlier build reads these
    // tables, and every query an earlier reader makes is unchanged.
    Migration {
        version: 20,
        minimum_reader: 8,
        minimum_writer: 19,
        sql: include_str!("migrations/0020_packages.sql"),
        expected_sha256: MIGRATION_0020_SHA256,
        rebuilds_tables: false,
    },
    // Additive: owned browser sessions and their observation index (two tables and their
    // indexes; versions 1-20 are retained). The writer floor
    // stays at 19 (the highest so far): applying a migration overwrites the store's floors, so
    // a lower value would re-admit the writers migration 14 refuses. It need not rise: an
    // earlier writer never writes these tables, and every record it writes is unchanged here
    // (artifact, operation, process and actor rows are never deleted, so the references cannot
    // break). The reader floor stays at 8: no earlier build reads them.
    Migration {
        version: 21,
        minimum_reader: 8,
        minimum_writer: 19,
        sql: include_str!("migrations/0021_browser_sessions.sql"),
        expected_sha256: MIGRATION_0021_SHA256,
        rebuilds_tables: false,
    },
    // Additive owned Git identities; preserves all earlier reader/writer floors.
    Migration {
        version: 22,
        minimum_reader: 8,
        minimum_writer: 19,
        sql: include_str!("migrations/0022_git_worktrees.sql"),
        expected_sha256: MIGRATION_0022_SHA256,
        rebuilds_tables: false,
    },
    // Old writers cannot preserve invocation-bound provenance on new browser evidence.
    Migration {
        version: 23,
        minimum_reader: 8,
        minimum_writer: 23,
        sql: include_str!("migrations/0023_browser_source.sql"),
        expected_sha256: MIGRATION_0023_SHA256,
        rebuilds_tables: false,
    },
    // Additive immutable eval reports; previous readers and writers preserve these rows.
    Migration {
        version: 24,
        minimum_reader: 8,
        minimum_writer: 23,
        sql: include_str!("migrations/0024_eval_runs.sql"),
        expected_sha256: MIGRATION_0024_SHA256,
        rebuilds_tables: false,
    },
    // Additive concrete pin snapshots. Old writers would silently follow catalog identity
    // changes without replacing the pin, so only writers that preserve snapshots may write.
    Migration {
        version: 25,
        minimum_reader: 8,
        minimum_writer: 25,
        sql: include_str!("migrations/0025_router_pin_identity.sql"),
        expected_sha256: MIGRATION_0025_SHA256,
        rebuilds_tables: false,
    },
    // Earlier writers do not know allocation ownership or its membership fences.
    Migration {
        version: 26,
        minimum_reader: 8,
        minimum_writer: 26,
        sql: include_str!("migrations/0026_allocations.sql"),
        expected_sha256: MIGRATION_0026_SHA256,
        rebuilds_tables: false,
    },
    // Earlier writers cannot capture reviewed plan content or preserve decision wakeup bindings.
    Migration {
        version: 27,
        minimum_reader: 8,
        minimum_writer: 27,
        sql: include_str!("migrations/0027_decisions.sql"),
        expected_sha256: MIGRATION_0027_SHA256,
        rebuilds_tables: false,
    },
    // New writers preserve canonical attempt ownership, reservations and historical tariff identity.
    Migration {
        version: 28,
        minimum_reader: 8,
        minimum_writer: 28,
        sql: include_str!("migrations/0028_usage.sql"),
        expected_sha256: MIGRATION_0028_SHA256,
        rebuilds_tables: true,
    },
    // Earlier writers cannot preserve native account identity when credentials change.
    Migration {
        version: 29,
        minimum_reader: 8,
        minimum_writer: 29,
        sql: include_str!("migrations/0029_connection_account_identity.sql"),
        expected_sha256: MIGRATION_0029_SHA256,
        rebuilds_tables: false,
    },
    // Earlier writers cannot preserve Help generation and artifact reference invariants.
    Migration {
        version: 30,
        minimum_reader: 8,
        minimum_writer: 30,
        sql: include_str!("migrations/0030_help.sql"),
        expected_sha256: MIGRATION_0030_SHA256,
        rebuilds_tables: false,
    },
    Migration {
        version: 31,
        minimum_reader: 8,
        minimum_writer: 31,
        sql: include_str!("migrations/0031_graph.sql"),
        expected_sha256: MIGRATION_0031_SHA256,
        rebuilds_tables: false,
    },
    // Earlier writers cannot preserve convention revision/resource/index identity.
    Migration {
        version: 32,
        minimum_reader: 8,
        minimum_writer: 32,
        sql: include_str!("migrations/0032_conventions.sql"),
        expected_sha256: MIGRATION_0032_SHA256,
        rebuilds_tables: false,
    },
    // Embedding candidates and their host qualification are additive. Writers must preserve
    // the new explicit selection and evidence fences; readers keep the existing floor.
    Migration {
        version: 33,
        minimum_reader: 8,
        minimum_writer: 33,
        sql: include_str!("migrations/0033_embedding_spaces.sql"),
        expected_sha256: MIGRATION_0033_SHA256,
        rebuilds_tables: false,
    },
    Migration {
        version: 34,
        minimum_reader: 8,
        minimum_writer: 34,
        sql: include_str!("migrations/0034_schemas.sql"),
        expected_sha256: MIGRATION_0034_SHA256,
        rebuilds_tables: false,
    },
    // Prior writers do not enforce settings credential retirement fences.
    Migration {
        version: 35,
        minimum_reader: 8,
        minimum_writer: 35,
        sql: include_str!("migrations/0035_settings_secrets.sql"),
        expected_sha256: MIGRATION_0035_SHA256,
        rebuilds_tables: false,
    },
    // Compact purge identities and receipt outcomes require both new readers and writers.
    Migration {
        version: 36,
        minimum_reader: 36,
        minimum_writer: 36,
        sql: include_str!("migrations/0036_project_purge.sql"),
        expected_sha256: "fb74cdc793221065ca0afdc99361bc2f7ddb1987fbc02b68bf718147e8970fcc",
        rebuilds_tables: false,
    },
    // Preserve existing managed-file custody while recognizing all owned UI staging.
    Migration {
        version: 37,
        minimum_reader: 37,
        minimum_writer: 37,
        sql: include_str!("migrations/0037_client_staging_custody.sql"),
        expected_sha256: "b925f2ce095801f5509565911ab540f3a700fce65602142d2cc137f865a881a7",
        rebuilds_tables: true,
    },
    Migration {
        version: 38,
        minimum_reader: 38,
        minimum_writer: 38,
        sql: include_str!("migrations/0038_purge_effect_leases.sql"),
        expected_sha256: "a3c200a2009cb12e4c929a2e8f4168cbef203def4c625ac92f40e9dfb7ee87de",
        rebuilds_tables: false,
    },
    // A prior writer cannot preserve the new ownership record during project deletion.
    Migration {
        version: 39,
        minimum_reader: 38,
        minimum_writer: 39,
        sql: include_str!("migrations/0039_owned_containers.sql"),
        expected_sha256: "30dbdac29f79956c98216d29b7bfe62fb9bb609844e3eaf8f926e4cabdf08eb9",
        rebuilds_tables: false,
    },
    Migration {
        version: 40,
        minimum_reader: 40,
        minimum_writer: 40,
        sql: include_str!("migrations/0040_assignment_purpose.sql"),
        expected_sha256: "9224b731e04bf7436ed9ac85bea92aa682c60dea8366cbbe86a60e81bfa4afc4",
        rebuilds_tables: false,
    },
    Migration {
        version: 41,
        minimum_reader: 41,
        minimum_writer: 41,
        sql: include_str!("migrations/0041_agent_messages.sql"),
        expected_sha256: "1b149a91b78726f84ac1d9688a93a99b5f6ff08f85e64e975550e4c8ee385b71",
        rebuilds_tables: false,
    },
    Migration {
        version: 42,
        minimum_reader: 42,
        minimum_writer: 42,
        sql: include_str!("migrations/0042_agent_assignment_context.sql"),
        expected_sha256: "e68758ec4bceb6d9a82af3962fc785f547c265fc88008d40cb8f65e5263339f8",
        rebuilds_tables: false,
    },
    Migration {
        version: 43,
        minimum_reader: 43,
        minimum_writer: 43,
        sql: include_str!("migrations/0043_direct_message_handover.sql"),
        expected_sha256: "d751ab36d72095b103122be4844443ceb6a4660a9a27cfbef331cbb9f8312193",
        rebuilds_tables: false,
    },
    Migration {
        version: 44,
        minimum_reader: 44,
        minimum_writer: 44,
        sql: include_str!("migrations/0044_session_repository_scope.sql"),
        expected_sha256: "186dc02b965f875bfdb5ae310656fffee7ac7153ecf4c4005df37944160738ec",
        rebuilds_tables: false,
    },
    Migration {
        version: 45,
        minimum_reader: 45,
        minimum_writer: 45,
        sql: include_str!("migrations/0045_session_activity.sql"),
        expected_sha256: "c117bab6b4e30d44470069bbdc062a349a088c0950ac54e7bd78c71cab6c5295",
        rebuilds_tables: false,
    },
    Migration {
        version: 46,
        minimum_reader: 46,
        minimum_writer: 46,
        sql: include_str!("migrations/0046_provider_context_head.sql"),
        expected_sha256: "c287952aeacac7156e94be67cdfa6fd791a25e4bd161dcb7d606edc2f77c9a34",
        rebuilds_tables: false,
    },
    // Agent message cursors must never retreat when a project's rows are purged.
    Migration {
        version: 47,
        minimum_reader: 46,
        minimum_writer: 47,
        sql: include_str!("migrations/0047_agent_message_heads.sql"),
        expected_sha256: "2e60f717ffeb968368dc1afcac4c4ea7c4f7fe5c5da3db161885c6c990bd6551",
        rebuilds_tables: false,
    },
    Migration {
        version: 48,
        minimum_reader: 48,
        minimum_writer: 48,
        sql: include_str!("migrations/0048_membership_context_epoch.sql"),
        expected_sha256: "5626f94f862a82cc0a15a1b2caefc05d837fbc0df66f1ff48d909ba8ac993d3a",
        rebuilds_tables: true,
    },
    // Prior writers cannot preserve instruction revision approval and activation invariants.
    Migration {
        version: 49,
        minimum_reader: 48,
        minimum_writer: 49,
        sql: include_str!("migrations/0049_instructions.sql"),
        expected_sha256: "88286f435638ddda4798562d9437a1c3f042d33706327f9a21c3dd611cfac380",
        rebuilds_tables: false,
    },
    // Prior writers would neither carry nor acknowledge watcher events at turn boundaries; the
    // new tables are invisible to prior readers.
    Migration {
        version: 50,
        minimum_reader: 48,
        minimum_writer: 50,
        sql: include_str!("migrations/0050_watchers.sql"),
        expected_sha256: "e69c28572b504955df3fd47dacea1fd3c9edee5cd72f9e8a22d392528caeb43a",
        rebuilds_tables: false,
    },
    // Prior writers cannot keep a probe's schedule or a timer's due time, and the source CHECK
    // widens, so the watchers table is rebuilt.
    Migration {
        version: 51,
        minimum_reader: 51,
        minimum_writer: 51,
        sql: include_str!("migrations/0051_watcher_sources.sql"),
        expected_sha256: "6f21259b2a9b29021600e2515886b0d0626ceabfdf095cf1c157f2b45610f3ec",
        rebuilds_tables: true,
    },
    // Prior builds name the dropped tables and the two dropped columns, so neither a prior
    // reader nor a prior writer may open the store. feature_plans and transcript_chunks are
    // rebuilt without the columns that referenced dropped tables.
    Migration {
        version: 52,
        minimum_reader: 52,
        minimum_writer: 52,
        sql: include_str!("migrations/0052_drop_unused_tables.sql"),
        expected_sha256: "ff5d30440733379049f8a3f9cd64a9ff535bcc819a8337edb77fcd5fea989ed0",
        rebuilds_tables: true,
    },
    // Prior writers cannot keep a watcher's match state or record more than one event per
    // watcher, and the source CHECK widens, so both watcher tables are rebuilt.
    Migration {
        version: 53,
        minimum_reader: 53,
        minimum_writer: 53,
        sql: include_str!("migrations/0053_watcher_matches.sql"),
        expected_sha256: "98b89612ce3caecfafb3533de350b23c0cf6f883e6834944ec8a71a7901a9677",
        rebuilds_tables: true,
    },
    // Indexes, maintained Knowledge tallies and per-server MCP catalogs for bounded reads. A
    // prior build would read a server's catalog from its scope's configuration, where it no
    // longer is, so neither a prior reader nor a prior writer may open the store.
    Migration {
        version: 54,
        minimum_reader: 54,
        minimum_writer: 54,
        sql: include_str!("migrations/0054_bounded_reads.sql"),
        expected_sha256: "0368b2a55b935d1720e26269a5b7805a37df20d81a0f4e210611cd2c6eb452c2",
        rebuilds_tables: false,
    },
    // Two partial indexes only: prior builds read and write the store unchanged.
    Migration {
        version: 55,
        minimum_reader: 54,
        minimum_writer: 54,
        sql: include_str!("migrations/0055_root_activity_indexes.sql"),
        expected_sha256: "5aac48f5b8bb5d074caf82c25eb26595cc6cfce2f04831854edff3eb1e3c5e0e",
        rebuilds_tables: false,
    },
    // A prior writer would refresh a repository source without its default branch.
    Migration {
        version: 56,
        minimum_reader: 56,
        minimum_writer: 56,
        sql: include_str!("migrations/0056_repository_default_branch.sql"),
        expected_sha256: "432211adc752b138515ce31f0fec31e1d36e0ff55fc4f7b586a6d15e33276aca",
        rebuilds_tables: false,
    },
    // Prior builds read and write session_usage and job_run_usage, so neither a prior reader
    // nor a prior writer may open the store. Source rows without a usage origin are copied
    // into usage_origins first; no table another table references is rebuilt.
    Migration {
        version: 57,
        minimum_reader: 57,
        minimum_writer: 57,
        sql: include_str!("migrations/0057_drop_usage_copies.sql"),
        expected_sha256: "bc4b4453bc4f1b35027b1655b588e221013752dbfd3ffe79db0ef0e0844a3885",
        rebuilds_tables: false,
    },
    // A prior writer would remove an overlay view without removing its links, so it may not open
    // the store; a prior reader never reads them. No existing table changes.
    Migration {
        version: 58,
        minimum_reader: 57,
        minimum_writer: 58,
        sql: include_str!("migrations/0058_graph_view_links.sql"),
        expected_sha256: "603129a67f6c0c58f0bf383eff2065d44e5b2598c8077d30d599b6b8f66f0efc",
        rebuilds_tables: false,
    },
    // One partial index only: prior builds read and write the store unchanged.
    Migration {
        version: 59,
        minimum_reader: 57,
        minimum_writer: 58,
        sql: include_str!("migrations/0059_graph_edges_target_qualified.sql"),
        expected_sha256: "78a4b778482cc81cf7e48ec8daf29cce7c97af15b9d40f6ba88622b3bc244024",
        rebuilds_tables: false,
    },
    // Two indexes only: prior builds read and write the store unchanged.
    Migration {
        version: 60,
        minimum_reader: 57,
        minimum_writer: 58,
        sql: include_str!("migrations/0060_graph_node_lookups.sql"),
        expected_sha256: "40e14b2566cee47db4daeb04f96427fd6ff34f21951cf482bc1868b4942e0325",
        rebuilds_tables: false,
    },
    // A prior writer would remove a view without removing its unresolved count, so it may not
    // open the store; a prior reader never reads it. No existing table changes.
    Migration {
        version: 61,
        minimum_reader: 57,
        minimum_writer: 61,
        sql: include_str!("migrations/0061_graph_view_unresolved.sql"),
        expected_sha256: "910fd21382ea8c47bd6daf7b0afad5ad9d4b0991bd51612abfa44186ce86d002",
        rebuilds_tables: false,
    },
    // One new table and its index: a prior reader never reads it. A prior writer writes revisions
    // without links (harmless), and a prior build's project purge refuses the unreviewed schema.
    Migration {
        version: 62,
        minimum_reader: 57,
        minimum_writer: 61,
        sql: include_str!("migrations/0062_instruction_revision_lessons.sql"),
        expected_sha256: "4348986bcf858ecf1989b6554f3ba3c31bf40cf754363a4a8e35758aa6c848e6",
        rebuilds_tables: false,
    },
    // Older readers ignore the new column. Older writers cannot keep it in sync with offsets.
    Migration {
        version: 63,
        minimum_reader: 57,
        minimum_writer: 63,
        sql: include_str!("migrations/0063_watcher_file_scan.sql"),
        expected_sha256: "231e7cc738901c470870a1bf210371febea4ea30d836e73b3d2cf85c02cea09d",
        rebuilds_tables: false,
    },
    Migration {
        version: 64,
        minimum_reader: 57,
        minimum_writer: 64,
        sql: include_str!("migrations/0064_systemone_pool.sql"),
        expected_sha256: SYSTEMONE_0064_SHA256,
        rebuilds_tables: false,
    },
    // Older writers cannot distinguish automatic and ordinary project reservations.
    Migration {
        version: 65,
        minimum_reader: 57,
        minimum_writer: 65,
        sql: include_str!("migrations/0065_keep_working_claims.sql"),
        expected_sha256: KEEP_WORKING_SHA256,
        rebuilds_tables: false,
    },
    // Older writers cannot interpret automatic intent or protect its first input.
    Migration {
        version: 66,
        minimum_reader: 66,
        minimum_writer: 66,
        sql: include_str!("migrations/0066_auto_routing.sql"),
        expected_sha256: AUTO_ROUTING_0066_SHA256,
        rebuilds_tables: false,
    },
    // Existing readers ignore this independent aggregate. Earlier writers do not update its rows.
    Migration {
        version: 67,
        minimum_reader: 66,
        minimum_writer: 67,
        sql: include_str!("migrations/0067_analysis.sql"),
        expected_sha256: "a604659ef41800df6389f9094fd42f27c15e4d7623d8718a9efae1c533d1ae01",
        rebuilds_tables: false,
    },
    // Older readers ignore this workspace/source policy. Require current writers to preserve it.
    Migration {
        version: 68,
        minimum_reader: 66,
        minimum_writer: 68,
        sql: include_str!("migrations/0068_database_sensitive_policy.sql"),
        expected_sha256: "a702716325ba7e39d34906db691762fdb215b7321b67e2ea2bd5fa637901ab69",
        rebuilds_tables: false,
    },
    Migration {
        version: 69,
        minimum_reader: 66,
        minimum_writer: 69,
        sql: include_str!("migrations/0069_selected_work_import.sql"),
        expected_sha256: "dab03fc587c9d7b3fb8cfc1a3ad9f9caedff1ba7bd0f52739e02552982267ce6",
        rebuilds_tables: false,
    },
    Migration {
        version: 70,
        minimum_reader: 68,
        minimum_writer: 70,
        sql: include_str!("migrations/0070_operational_retention.sql"),
        expected_sha256: "9cf44f5dc2de0ab5b235807f1abedca2866b83b2b1724729bd120bebe0c6548a",
        rebuilds_tables: false,
    },
    Migration {
        version: 71,
        minimum_reader: 68,
        minimum_writer: 71,
        sql: include_str!("migrations/0071_workspace_archive.sql"),
        expected_sha256: "ebf85fad80712a1b22f554570f1fa668c15a807fe69095cd6891e302a5eefcde",
        rebuilds_tables: false,
    },
    Migration {
        version: 72,
        minimum_reader: 68,
        minimum_writer: 72,
        sql: include_str!("migrations/0072_app_updates.sql"),
        expected_sha256: "d1ebef486a64086c5867aee1e1e62bdda234bfc91a5306b0565e01cb9401a945",
        rebuilds_tables: false,
    },
    Migration {
        version: 73,
        minimum_reader: 68,
        minimum_writer: 73,
        sql: include_str!("migrations/0073_document_index.sql"),
        expected_sha256: "86858d73dcabddcf74b3962cd4640748ff124cdbd878c82c6d5e08bd27e6515f",
        rebuilds_tables: false,
    },
    Migration {
        version: 74,
        minimum_reader: 68,
        minimum_writer: 74,
        sql: include_str!("migrations/0074_document_lexical.sql"),
        expected_sha256: "468f2f78d01262bff6935776e3cfd34f0e04a6d94c360219c048aa40eaaf38f6",
        rebuilds_tables: false,
    },
    Migration {
        version: 75,
        minimum_reader: 68,
        minimum_writer: 75,
        sql: include_str!("migrations/0075_document_vectors.sql"),
        expected_sha256: "62eb7aba9b1a09afb4e34d4a29051da30a82d88f0d8139bad07be6aff763e269",
        rebuilds_tables: false,
    },
    Migration {
        version: 76,
        minimum_reader: 68,
        minimum_writer: 76,
        sql: include_str!("migrations/0076_document_vision.sql"),
        expected_sha256: "e8f23f6b6cd58789dbbc05676dbb8ea9f22d0de36145101a5960094997941c1c",
        rebuilds_tables: false,
    },
    Migration {
        version: 77,
        minimum_reader: 68,
        minimum_writer: 77,
        sql: include_str!("migrations/0077_document_activity.sql"),
        expected_sha256: "0c06990075b6703f1be0dd23e6720a3dedd6f2ea84e90a77307b70bce42781c3",
        rebuilds_tables: false,
    },
    Migration {
        version: 78,
        minimum_reader: 68,
        minimum_writer: 78,
        sql: include_str!("migrations/0078_dreaming.sql"),
        expected_sha256: "8e2fe487746aaa58fc4c80bd692a181fd17934f934ccee4c808fb9e921d7e72c",
        rebuilds_tables: false,
    },
    // The signed DW-62 candidate published this exact SQL at 77. Older readers ignore it.
    Migration {
        version: 79,
        minimum_reader: 68,
        minimum_writer: 79,
        sql: include_str!("migrations/0079_document_transfers.sql"),
        expected_sha256: DOCUMENT_TRANSFERS_SHA256,
        rebuilds_tables: false,
    },
];
/// Recorded checksum of migration 17; a test fails if the shipped text changes.
pub const MIGRATION_0017_SHA256: &str =
    "c4c2ed1f2231f0cc56c9b55593ba481a39fb0b688fcef61579eb361972274fae";
/// Recorded checksum of migration 16; a test fails if the shipped text changes.
pub const MIGRATION_0016_SHA256: &str =
    "4868dd86ca4af8c5d4e77aa54061dcd0b1e17aac0bf5f5506fdbe231305f847c";

/// Recorded checksum of migration 18; a test fails if the shipped text changes.
pub const MIGRATION_0018_SHA256: &str =
    "0461f125bd4b1f5889d535749f33214d379cf7965dd5fb701d13e09648853127";

/// Recorded checksum of migration 19; a test fails if the shipped text changes.
pub const MIGRATION_0019_SHA256: &str =
    "ed77284397dee952eb7feccf17843669545a61f0c0f35797b0fc350f4a32121f";
/// Recorded checksum of migration 14; a test fails if the shipped text changes.
pub const MIGRATION_0014_SHA256: &str =
    "c15736262585b6da18c679eecb6d1263d9263c3b9272725c4d0fa1bc5cd14534";
/// Recorded checksum of migration 15; a test fails if the shipped text changes.
pub const MIGRATION_0015_SHA256: &str =
    "950ca955706f93e49bd4abafbaedbeac0f60342bfd1e5001fde47b5202985243";
/// Recorded checksum of migration 13; a test fails if the shipped text changes.
pub const MIGRATION_0013_SHA256: &str =
    "a666371fe419bf0b2a63e69762a8ca8d5fa9dfb123d4ca019fc2db98087c004b";
/// Recorded checksum of migration 12; a test fails if the shipped text changes.
pub const MIGRATION_0012_SHA256: &str =
    "dbcf28e8dc5a8eb6e4b801fb9f976021cd5e14cbf9e2b5b82af38f008c6cedba";
/// Recorded checksum of migration 11; a test fails if the shipped text changes.
pub const MIGRATION_0011_SHA256: &str =
    "8607df6d8f6fd0fe885e21595e7e5245422916b8bdf0d0679fd7f0ca5b0f19af";
/// Recorded checksum of migration 10; a test fails if the shipped text changes.
pub const MIGRATION_0010_SHA256: &str =
    "7fab1dd563202155578de4ffe4e14bf89647e97815685f0e58a886a051c20f01";
/// Recorded checksum of migration 9; a test fails if the shipped text changes.
pub const MIGRATION_0009_SHA256: &str =
    "0a893b2c15371f9d9d251bb45814eb9666ee883f94a5706a165184f5fd88c128";
/// Recorded checksum of migration 8; a test fails if the shipped text changes.
pub const MIGRATION_0008_SHA256: &str =
    "682af46e64f4ee7d6d854acdbec0862309a2e37262cf639124ccfb0c57a79cb9";
/// Recorded checksum of migration 7; a test fails if the shipped text changes.
pub const MIGRATION_0007_SHA256: &str =
    "2863f135528fc5a7c7499515872eb17a5acbddd1874fbd108d91ad2cd1253077";
/// Recorded checksum of migration 6; a test fails if the shipped text changes.
pub const MIGRATION_0006_SHA256: &str =
    "7a2ebf3518bed204be5592dcdee4ad203b1971efc421c26535a1a6478571e768";
/// Recorded checksum of migration 5; a test fails if the shipped text changes.
pub const MIGRATION_0005_SHA256: &str =
    "3b8c62996c4506f42eb45c0e76db77d4a7160135090c48f533eaec821e48372f";
/// Recorded checksum of migration 4; a test fails if the shipped text changes.
pub const MIGRATION_0004_SHA256: &str =
    "15188a10f9c8f49ffde96563e636ccda2116024a7e6bda1e3e01096e25d7bad9";
/// Recorded checksum of migration 2; a test fails if the shipped text changes.
pub const MIGRATION_0002_SHA256: &str =
    "a400cacbe5ef06f2045e6fe839c35c3adbe46dcc52a4aa110608498d88da1d37";
/// Recorded checksum of migration 1; a test fails if the shipped text changes.
pub const MIGRATION_0001_SHA256: &str =
    "c64cc140ad8de8c4c2ca7e7d3e3efac32966cea8bfc0bdf5e46e9c55206bd6a4";
/// Recorded checksum of migration 3.
pub const MIGRATION_0003_SHA256: &str =
    "b08289a2ef6eea452e7b7513c4c811564b90c2bd550445cb932212805be24ddc";

#[derive(Clone, Debug)]
pub(crate) struct Meta {
    pub store_id: String,
    pub schema_version: i64,
    pub minimum_reader: i64,
    pub minimum_writer: i64,
    pub restore_epoch: i64,
}

pub(crate) fn read_meta(conn: &Connection) -> Result<Option<Meta>, StorageError> {
    let has_meta: bool = db(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='store_meta')",
        [],
        |r| r.get(0),
    ))?;
    if !has_meta {
        return Ok(None);
    }
    db(conn
        .query_row(
            "SELECT store_id,schema_version,minimum_reader,minimum_writer,\
                restore_epoch FROM store_meta WHERE singleton=1",
            [],
            |r| {
                Ok(Meta {
                    store_id: r.get(0)?,
                    schema_version: r.get(1)?,
                    minimum_reader: r.get(2)?,
                    minimum_writer: r.get(3)?,
                    restore_epoch: r.get(4)?,
                })
            },
        )
        .optional())
}

pub(crate) fn ledger(conn: &Connection) -> Result<Vec<AppliedMigration>, StorageError> {
    let mut stmt = db(conn.prepare(
        "SELECT version,checksum,applied_at,app_build FROM schema_migrations ORDER BY version",
    ))?;
    let rows = db(stmt.query_map([], |r| {
        Ok(AppliedMigration {
            version: r.get(0)?,
            checksum: r.get(1)?,
            applied_at_ms: r.get(2)?,
            app_build: r.get(3)?,
        })
    }))?;
    db(rows.collect())
}

fn user_tables(conn: &Connection) -> Result<i64, StorageError> {
    db(conn.query_row(
        "SELECT count(*) FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%'",
        [],
        |r| r.get(0),
    ))
}

pub(super) fn verify_after(conn: &Connection) -> Result<(), StorageError> {
    let fk: i64 = db(
        conn.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| {
            r.get(0)
        }),
    )?;
    let quick: String = db(conn.query_row("PRAGMA quick_check", [], |r| r.get(0)))?;
    if fk != 0 || quick != "ok" {
        return Err(StorageError::IncompatibleSchema(format!(
            "post-migration check failed: {fk} foreign-key violations, quick_check={quick}"
        )));
    }
    Ok(())
}

/// Applies one migration in its own transaction; on any error nothing of it remains.
fn apply(
    conn: &Connection,
    m: &Migration,
    bootstrap_store_id: Option<&str>,
    now_ms: i64,
    app_build: &str,
) -> Result<(), StorageError> {
    if m.rebuilds_tables {
        // A no-op inside a transaction, so it is switched before BEGIN.
        db(conn.pragma_update(None, "foreign_keys", false))?;
    }
    let outcome = apply_in_transaction(conn, m, bootstrap_store_id, now_ms, app_build, None);
    if m.rebuilds_tables {
        let restored = db(conn.pragma_update(None, "foreign_keys", true));
        outcome?;
        return restored;
    }
    outcome
}

fn apply_in_transaction(
    conn: &Connection,
    m: &Migration,
    bootstrap_store_id: Option<&str>,
    now_ms: i64,
    app_build: &str,
    already_applied: Option<&Migration>,
) -> Result<(), StorageError> {
    db(conn.execute_batch("BEGIN IMMEDIATE"))?;
    let result = (|| {
        if let Some(previous) = already_applied {
            let moved = db(conn.execute(
                "UPDATE schema_migrations SET version=?1 WHERE version=?2 AND checksum=?3",
                params![previous.version, m.version, previous.checksum()],
            ))?;
            if moved != 1 {
                return Err(StorageError::IncompatibleSchema(
                    "parallel migration 64 changed before reconciliation".into(),
                ));
            }
        }
        db(conn.execute_batch(m.sql))?;
        let latest = already_applied.unwrap_or(m);
        match bootstrap_store_id {
            Some(store_id) => {
                db(conn.execute(
                    "INSERT INTO store_meta(singleton,store_id,schema_version,minimum_reader,\
                    minimum_writer,restore_epoch) VALUES(1,?1,?2,?3,?4,0)",
                    params![
                        store_id,
                        latest.version,
                        latest.minimum_reader,
                        latest.minimum_writer
                    ],
                ))?;
            }
            None => update_schema_meta(conn, latest)?,
        };
        record_applied(conn, m, now_ms, app_build)?;
        db(conn.pragma_update(None, "user_version", latest.version))?;
        // Validate inside the transaction: a migration that breaks foreign keys or integrity is
        // never committed.
        verify_after(conn)
    })();
    match result {
        Ok(()) => db(conn.execute_batch("COMMIT")),
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

fn record_applied(
    conn: &Connection,
    migration: &Migration,
    now_ms: i64,
    app_build: &str,
) -> Result<(), StorageError> {
    db(conn.execute(
        "INSERT INTO schema_migrations(version,checksum,applied_at,app_build) VALUES(?1,?2,?3,?4)",
        params![migration.version, migration.checksum(), now_ms, app_build],
    ))?;
    Ok(())
}

fn update_schema_meta(conn: &Connection, latest: &Migration) -> Result<(), StorageError> {
    db(conn.execute(
        "UPDATE store_meta SET schema_version=?1,minimum_reader=?2,minimum_writer=?3 WHERE singleton=1",
        params![latest.version, latest.minimum_reader, latest.minimum_writer],
    ))?;
    Ok(())
}

/// DW-F4/F5 published the keep-working SQL at version 64 before DW-F6 combined it with
/// SystemOne. Only the exact former layout at 64 can adopt the canonical ledger.
fn split_migration<'a>(
    migrations: &'a [Migration],
    meta: &Meta,
    applied: &AppliedMigration,
) -> Option<(&'a Migration, &'a Migration)> {
    if meta.schema_version != 64
        || meta.minimum_writer != 64
        || meta.minimum_reader != 57
        || applied.version != 64
        || applied.checksum != KEEP_WORKING_SHA256
    {
        return None;
    }
    let systemone = migrations.get(63)?;
    let keep_working = migrations.get(64)?;
    (systemone.version == 64
        && systemone.expected_sha256 == SYSTEMONE_0064_SHA256
        && keep_working.version == 65
        && keep_working.expected_sha256 == KEEP_WORKING_SHA256)
        .then_some((systemone, keep_working))
}

const AUTO_ROUTING_0066_SHA256: &str =
    "ff33a5f40322cc4f8524b653d2e2418531fabcbaba261311a6bcfd25829da83f";
const APP_OPERATIONS_VERSION: usize = 72;
const FEATURE_DOCUMENT_DIGESTS: [&str; 6] = [
    "86858d73dcabddcf74b3962cd4640748ff124cdbd878c82c6d5e08bd27e6515f",
    "468f2f78d01262bff6935776e3cfd34f0e04a6d94c360219c048aa40eaaf38f6",
    "62eb7aba9b1a09afb4e34d4a29051da30a82d88f0d8139bad07be6aff763e269",
    "e8f23f6b6cd58789dbbc05676dbb8ea9f22d0de36145101a5960094997941c1c",
    "0c06990075b6703f1be0dd23e6720a3dedd6f2ea84e90a77307b70bce42781c3",
    "8e2fe487746aaa58fc4c80bd692a181fd17934f934ccee4c808fb9e921d7e72c",
];
const COMBINED_VERSION: usize = APP_OPERATIONS_VERSION + FEATURE_DOCUMENT_DIGESTS.len();

#[derive(Clone, Copy, PartialEq, Eq)]
struct DocumentLineage {
    prefix: usize,
    count: usize,
    minimum_reader: i64,
}

fn documents(migrations: &[Migration]) -> Option<&[Migration]> {
    let documents = migrations.get(APP_OPERATIONS_VERSION..COMBINED_VERSION)?;
    documents
        .iter()
        .zip(FEATURE_DOCUMENT_DIGESTS)
        .enumerate()
        .all(|(index, (migration, digest))| {
            migration.version == (APP_OPERATIONS_VERSION + index + 1) as i64
                && !migration.rebuilds_tables
                && migration.checksum() == digest
        })
        .then_some(documents)
}

fn matches_catalogue(applied: &[AppliedMigration], migrations: &[Migration]) -> bool {
    applied.len() == migrations.len()
        && applied.iter().zip(migrations).all(|(saved, expected)| {
            saved.version == expected.version && saved.checksum == expected.checksum()
        })
}

fn user_version(conn: &Connection) -> Result<i64, StorageError> {
    db(conn.pragma_query_value(None, "user_version", |row| row.get(0)))
}

/// The Documents feature published two exact aliases for the shared SQL suffix. A profile
/// with gaps, mixed suffixes or mismatched SQLite version witnesses matches neither alias.
fn document_lineage(
    migrations: &[Migration],
    meta: &Meta,
    applied: &[AppliedMigration],
    sqlite_version: i64,
) -> Option<DocumentLineage> {
    if meta.schema_version != sqlite_version || meta.minimum_writer != sqlite_version {
        return None;
    }
    let version = usize::try_from(sqlite_version).ok()?;
    if applied.len() != version {
        return None;
    }
    let (prefix, limit, minimum_reader) = match meta.minimum_reader {
        57 => (63, 5, 57),
        66 => (66, FEATURE_DOCUMENT_DIGESTS.len(), 66),
        _ => return None,
    };
    let count = version.checked_sub(prefix)?;
    if !(1..=limit).contains(&count)
        || !matches_catalogue(&applied[..prefix], &migrations[..prefix])
        || !applied[prefix..]
            .iter()
            .zip(FEATURE_DOCUMENT_DIGESTS)
            .enumerate()
            .all(|(index, (saved, digest))| {
                saved.version == (prefix + index + 1) as i64 && saved.checksum == digest
            })
    {
        return None;
    }
    documents(migrations)?;
    Some(DocumentLineage {
        prefix,
        count,
        minimum_reader,
    })
}

fn exact_catalogue<'a>(
    migrations: &'a [Migration],
    meta: &Meta,
    applied: &[AppliedMigration],
    sqlite_version: i64,
) -> Option<&'a [Migration]> {
    let known = migrations.get(..usize::try_from(meta.schema_version).ok()?)?;
    let last = known.last()?;
    (sqlite_version == meta.schema_version
        && last.version == meta.schema_version
        && last.minimum_reader == meta.minimum_reader
        && last.minimum_writer == meta.minimum_writer
        && matches_catalogue(applied, known))
    .then_some(known)
}

/// The signed test candidate published versions 67–76 before the intelligence branch
/// reached main. Match its complete checksum prefix, including its original reader floor.
fn application_lineage(
    migrations: &[Migration],
    meta: &Meta,
    applied: &[AppliedMigration],
    sqlite_version: i64,
) -> Option<usize> {
    if meta.schema_version != sqlite_version || meta.minimum_writer != sqlite_version {
        return None;
    }
    let count = usize::try_from(sqlite_version).ok()?.checked_sub(66)?;
    let canonical = migrations.get(68..68 + count)?;
    let reader = if count == 1 { 66 } else { 68 };
    if !(1..=10).contains(&count)
        || meta.minimum_reader != reader
        || applied.len() != 66 + count
        || !matches_catalogue(&applied[..66], &migrations[..66])
        || !applied[66..]
            .iter()
            .zip(canonical)
            .all(|(old, new)| old.version + 2 == new.version && old.checksum == new.checksum())
    {
        return None;
    }
    Some(count)
}

fn reconcile_application(
    conn: &Connection,
    migrations: &[Migration],
    count: usize,
    now_ms: i64,
    app_build: &str,
) -> Result<i64, StorageError> {
    let tx = db(rusqlite::Transaction::new_unchecked(
        conn,
        rusqlite::TransactionBehavior::Immediate,
    ))?;
    let meta = read_meta(&tx)?.ok_or_else(document_lineage_changed)?;
    if application_lineage(migrations, &meta, &ledger(&tx)?, user_version(&tx)?) != Some(count) {
        return Err(document_lineage_changed());
    }
    for migration in migrations[68..68 + count].iter().rev() {
        if db(tx.execute(
            "UPDATE schema_migrations SET version=?1 WHERE version=?2 AND checksum=?3",
            params![
                migration.version,
                migration.version - 2,
                migration.checksum()
            ],
        ))? != 1
        {
            return Err(document_lineage_changed());
        }
    }
    for migration in migrations[66..68]
        .iter()
        .chain(&migrations[68 + count..COMBINED_VERSION])
    {
        db(tx.execute_batch(migration.sql))?;
        record_applied(&tx, migration, now_ms, app_build)?;
    }
    let current = &migrations[COMBINED_VERSION - 1];
    update_schema_meta(&tx, current)?;
    db(tx.pragma_update(None, "user_version", current.version))?;
    verify_after(&tx)?;
    db(tx.commit())?;
    Ok(current.version)
}

const ANALYSIS_SHA256: &str = "a604659ef41800df6389f9094fd42f27c15e4d7623d8718a9efae1c533d1ae01";
const DOCUMENT_TRANSFERS_SHA256: &str =
    "7e4772630d9f1125814cb53ffa641fcf371afea8c7801d2c6065c590b61b95d4";
const DW62_VERSION: i64 = 78;

/// The signed DW-62 candidate (23751f08d) published schema 78: the application suffix at
/// 67–76, document transfers at 77 and repository analysis at 78, without the database policy
/// at 68. Rebuild that exact catalogue from the canonical entries so its witnesses can be
/// compared. The transfers entry is found by checksum, because a composed build may place other
/// independent migrations before it. Refuse when this build lacks any entry the profile needs.
struct Dw62 {
    source: Vec<Migration>,
    transfers: usize,
}

fn dw62_catalogue(migrations: &[Migration]) -> Option<Dw62> {
    let analysis = migrations.get(66)?;
    let application = migrations.get(68..COMBINED_VERSION)?;
    let transfers = COMBINED_VERSION
        + migrations[COMBINED_VERSION..]
            .iter()
            .position(|migration| migration.checksum() == DOCUMENT_TRANSFERS_SHA256)?;
    if analysis.version != 67
        || analysis.checksum() != ANALYSIS_SHA256
        || application.first()?.version != 69
        || documents(migrations).is_none()
    {
        return None;
    }
    let mut source = migrations[..66].to_vec();
    source.extend(application.iter().enumerate().map(|(index, migration)| {
        let version = migration.version - 2;
        Migration {
            version,
            minimum_reader: if index == 0 { 66 } else { 68 },
            minimum_writer: version,
            ..*migration
        }
    }));
    for (version, migration) in [(77, &migrations[transfers]), (DW62_VERSION, analysis)] {
        source.push(Migration {
            version,
            minimum_reader: 68,
            minimum_writer: version,
            ..*migration
        });
    }
    Some(Dw62 { source, transfers })
}

/// Only the complete DW-62 ledger with matching store and SQLite version witnesses is admitted.
fn dw62_lineage(
    migrations: &[Migration],
    meta: &Meta,
    applied: &[AppliedMigration],
    sqlite_version: i64,
) -> Option<Dw62> {
    if meta.schema_version != DW62_VERSION
        || sqlite_version != DW62_VERSION
        || meta.minimum_reader != 68
        || meta.minimum_writer != DW62_VERSION
    {
        return None;
    }
    let lineage = dw62_catalogue(migrations)?;
    matches_catalogue(applied, &lineage.source).then_some(lineage)
}

fn dw62_missing(
    migrations: &[Migration],
    transfers: usize,
) -> impl Iterator<Item = &Migration> + Clone {
    std::iter::once(&migrations[67]).chain(&migrations[COMBINED_VERSION..transfers])
}

/// Rekey each retained DW-62 ledger row to its canonical version and apply the canonical entries
/// it lacks up to document transfers, in one transaction. Recorded checksums, times and builds
/// stay unchanged. As in [`apply`], foreign-key enforcement is off for a transaction that runs a
/// table rebuild, and the in-transaction foreign-key check must still pass before it commits.
fn reconcile_dw62(
    conn: &Connection,
    migrations: &[Migration],
    now_ms: i64,
    app_build: &str,
) -> Result<i64, StorageError> {
    let rebuilds = dw62_catalogue(migrations).is_some_and(|lineage| {
        dw62_missing(migrations, lineage.transfers).any(|migration| migration.rebuilds_tables)
    });
    if rebuilds {
        // A no-op inside a transaction, so it is switched before BEGIN.
        db(conn.pragma_update(None, "foreign_keys", false))?;
    }
    let outcome = reconcile_dw62_in_transaction(conn, migrations, now_ms, app_build);
    if rebuilds {
        let restored = db(conn.pragma_update(None, "foreign_keys", true));
        let version = outcome?;
        restored?;
        return Ok(version);
    }
    outcome
}

fn reconcile_dw62_in_transaction(
    conn: &Connection,
    migrations: &[Migration],
    now_ms: i64,
    app_build: &str,
) -> Result<i64, StorageError> {
    let tx = db(rusqlite::Transaction::new_unchecked(
        conn,
        rusqlite::TransactionBehavior::Immediate,
    ))?;
    let meta = read_meta(&tx)?.ok_or_else(dw62_lineage_changed)?;
    let Dw62 { source, transfers } =
        dw62_lineage(migrations, &meta, &ledger(&tx)?, user_version(&tx)?)
            .ok_or_else(dw62_lineage_changed)?;
    let missing = dw62_missing(migrations, transfers);
    // Source and canonical versions overlap, so park the suffix above every known version first.
    const PARKED: i64 = 1_000_000;
    db(tx.execute(
        "UPDATE schema_migrations SET version=version+?1 WHERE version>66",
        [PARKED],
    ))?;
    let targets = migrations[68..COMBINED_VERSION]
        .iter()
        .chain([&migrations[transfers], &migrations[66]]);
    for (old, target) in source[66..].iter().zip(targets) {
        let moved = db(tx.execute(
            "UPDATE schema_migrations SET version=?1 WHERE version=?2 AND checksum=?3",
            params![target.version, old.version + PARKED, target.checksum()],
        ))?;
        if moved != 1 {
            return Err(dw62_lineage_changed());
        }
    }
    for migration in missing {
        db(tx.execute_batch(migration.sql))?;
        record_applied(&tx, migration, now_ms, app_build)?;
    }
    let current = &migrations[transfers];
    update_schema_meta(&tx, current)?;
    db(tx.pragma_update(None, "user_version", current.version))?;
    if !matches_catalogue(&ledger(&tx)?, &migrations[..=transfers]) {
        return Err(dw62_lineage_changed());
    }
    verify_after(&tx)?;
    db(tx.commit())?;
    Ok(current.version)
}

fn dw62_lineage_changed() -> StorageError {
    StorageError::IncompatibleSchema("DW-62 migration ledger changed before reconciliation".into())
}

/// Source catalogues are retained for archive attestation. Replaying one in a separate
/// empty reference database never changes the source or authorizes a migration.
pub(super) fn checkpoint_catalogue(
    conn: &Connection,
    migrations: &[Migration],
) -> Result<(Meta, Vec<Migration>), StorageError> {
    let meta = read_meta(conn)?.ok_or_else(|| {
        StorageError::IncompatibleSchema("the checkpoint has no workspace store metadata".into())
    })?;
    let applied = ledger(conn)?;
    let sqlite_version = user_version(conn)?;
    if let Some(lineage) = document_lineage(migrations, &meta, &applied, sqlite_version) {
        let mut source = migrations[..lineage.prefix].to_vec();
        source.extend(
            migrations[APP_OPERATIONS_VERSION..APP_OPERATIONS_VERSION + lineage.count]
                .iter()
                .enumerate()
                .map(|(index, migration)| {
                    let version = (lineage.prefix + index + 1) as i64;
                    Migration {
                        version,
                        minimum_reader: lineage.minimum_reader,
                        minimum_writer: version,
                        ..*migration
                    }
                }),
        );
        return Ok((meta, source));
    }
    if let Some(lineage) = dw62_lineage(migrations, &meta, &applied, sqlite_version) {
        return Ok((meta, lineage.source));
    }
    if let Some(count) = application_lineage(migrations, &meta, &applied, sqlite_version) {
        let mut source = migrations[..66].to_vec();
        source.extend(
            migrations[68..68 + count]
                .iter()
                .enumerate()
                .map(|(index, migration)| {
                    let version = migration.version - 2;
                    Migration {
                        version,
                        minimum_reader: if index == 0 { 66 } else { 68 },
                        minimum_writer: version,
                        ..*migration
                    }
                }),
        );
        return Ok((meta, source));
    }
    if let Some(suffix) = intelligence_branch(migrations, &meta, &applied)
        && sqlite_version == meta.schema_version
    {
        let mut source = migrations[..63].to_vec();
        source.extend(
            suffix[3..]
                .iter()
                .enumerate()
                .map(|(index, migration)| Migration {
                    version: migration.version - 3,
                    minimum_reader: 57,
                    minimum_writer: if index == 0 { 63 } else { 65 },
                    ..*migration
                }),
        );
        return Ok((meta, source));
    }
    if let Some(known) = exact_catalogue(migrations, &meta, &applied, sqlite_version) {
        return Ok((meta, known.to_vec()));
    }
    Err(StorageError::IncompatibleSchema(
        "checkpoint ledger differs from the shipped migration catalogue".into(),
    ))
}

/// Rekey the admitted historical document rows and apply the independent missing suffix
/// within one transaction. Preserve original recorded checksums, timestamps and builds.
fn reconcile_documents(
    conn: &Connection,
    migrations: &[Migration],
    lineage: DocumentLineage,
    now_ms: i64,
    app_build: &str,
) -> Result<i64, StorageError> {
    let tx = db(rusqlite::Transaction::new_unchecked(
        conn,
        rusqlite::TransactionBehavior::Immediate,
    ))?;
    let current = read_meta(&tx)?.ok_or_else(document_lineage_changed)?;
    if document_lineage(migrations, &current, &ledger(&tx)?, user_version(&tx)?) != Some(lineage) {
        return Err(document_lineage_changed());
    }
    let (common, documents) = migrations[..COMBINED_VERSION].split_at(APP_OPERATIONS_VERSION);
    let missing = common[lineage.prefix..]
        .iter()
        .chain(&documents[lineage.count..]);
    if missing.clone().any(|entry| entry.rebuilds_tables) {
        return Err(document_lineage_changed());
    }
    // Move high keys first because the source and canonical document ranges can overlap.
    for (index, destination) in documents[..lineage.count].iter().enumerate().rev() {
        let old_version = (lineage.prefix + index + 1) as i64;
        let updated = db(tx.execute(
            "UPDATE schema_migrations SET version=?1 WHERE version=?2 AND checksum=?3",
            params![
                destination.version,
                old_version,
                FEATURE_DOCUMENT_DIGESTS[index]
            ],
        ))?;
        if updated != 1 {
            return Err(document_lineage_changed());
        }
    }
    for migration in missing {
        db(tx.execute_batch(migration.sql))?;
        record_applied(&tx, migration, now_ms, app_build)?;
    }
    let current = &migrations[COMBINED_VERSION - 1];
    update_schema_meta(&tx, current)?;
    db(tx.pragma_update(None, "user_version", current.version))?;
    verify_after(&tx)?;
    db(tx.commit())?;
    Ok(current.version)
}

fn document_lineage_changed() -> StorageError {
    StorageError::IncompatibleSchema(
        "document migration lineage changed before reconciliation".into(),
    )
}

// DW-F7 test builds used 64 for repository analysis and 65 for database policy.
// The production ledger already owns those numbers. Recognize only the two complete
// DW-F7 suffixes so partially applied, unknown or counterfeit histories still fail closed.
fn intelligence_branch<'a>(
    migrations: &'a [Migration],
    meta: &Meta,
    applied: &[AppliedMigration],
) -> Option<&'a [Migration]> {
    let canonical = migrations.get(63..68)?;
    let [systemone, keep_working, routing, analysis, database] = canonical else {
        return None;
    };
    if systemone.version != 64
        || systemone.expected_sha256 != SYSTEMONE_0064_SHA256
        || keep_working.version != 65
        || keep_working.expected_sha256 != KEEP_WORKING_SHA256
        || routing.version != 66
        || analysis.version != 67
        || database.version != 68
        || applied.len() != usize::try_from(meta.schema_version).ok()?
        || meta.minimum_reader != 57
    {
        return None;
    }
    let legacy_analysis = applied.get(63)?;
    if legacy_analysis.version != 64
        || legacy_analysis.checksum != analysis.checksum()
        || !migrations[..63]
            .iter()
            .zip(applied)
            .all(|(wanted, actual)| {
                wanted.version == actual.version && wanted.checksum() == actual.checksum
            })
    {
        return None;
    }
    match (meta.schema_version, meta.minimum_writer) {
        (64, 63) => Some(&canonical[..4]),
        (65, 65) if applied[64].version == 65 && applied[64].checksum == database.checksum() => {
            Some(canonical)
        }
        _ => None,
    }
}

fn reconcile_intelligence_branch(
    conn: &Connection,
    migrations: &[Migration],
    now_ms: i64,
    app_build: &str,
) -> Result<i64, StorageError> {
    db(conn.execute_batch("BEGIN IMMEDIATE"))?;
    let result = (|| {
        let meta = read_meta(conn)?
            .ok_or_else(|| StorageError::IncompatibleSchema("missing store metadata".into()))?;
        let applied = ledger(conn)?;
        let canonical = intelligence_branch(migrations, &meta, &applied).ok_or_else(|| {
            StorageError::IncompatibleSchema(
                "DW-F7 migration ledger changed before reconciliation".into(),
            )
        })?;
        for (old, target) in (64..).zip(&canonical[3..]) {
            let moved = db(conn.execute(
                "UPDATE schema_migrations SET version=?1 WHERE version=?2 AND checksum=?3",
                params![target.version, old, target.checksum()],
            ))?;
            if moved != 1 {
                return Err(StorageError::IncompatibleSchema(
                    "DW-F7 ledger entry disappeared during reconciliation".into(),
                ));
            }
        }
        for m in &canonical[..3] {
            db(conn.execute_batch(m.sql))?;
            db(conn.execute(
                "INSERT INTO schema_migrations(version,checksum,applied_at,app_build)
                 VALUES(?1,?2,?3,?4)",
                params![m.version, m.checksum(), now_ms, app_build],
            ))?;
        }
        let latest = canonical.last().expect("the DW-F7 suffix is nonempty");
        db(conn.execute(
            "UPDATE store_meta SET schema_version=?1,minimum_reader=?2,minimum_writer=?3
             WHERE singleton=1",
            params![latest.version, latest.minimum_reader, latest.minimum_writer],
        ))?;
        db(conn.pragma_update(None, "user_version", latest.version))?;
        verify_after(conn)?;
        Ok(latest.version)
    })();
    match result {
        Ok(version) => {
            db(conn.execute_batch("COMMIT"))?;
            Ok(version)
        }
        Err(error) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(error)
        }
    }
}

/// Checks store identity, minimum reader/writer versions and the migration ledger without
/// writing anything. `Ok(None)` means an empty file that may be bootstrapped.
pub(crate) fn check(
    conn: &Connection,
    migrations: &[Migration],
) -> Result<Option<(Meta, AccessMode)>, StorageError> {
    let build = migrations.last().map_or(0, |m| m.version);
    let meta = match read_meta(conn)? {
        Some(meta) => meta,
        None if user_tables(conn)? == 0 => return Ok(None),
        None => {
            return Err(StorageError::IncompatibleSchema(
                "database is not a DevWorks store (no store_meta)".into(),
            ));
        }
    };
    if meta.minimum_reader > build {
        return Err(StorageError::IncompatibleSchema(format!(
            "store schema {} requires reader version {}; this build is {build}",
            meta.schema_version, meta.minimum_reader
        )));
    }
    let applied = ledger(conn)?;
    if let Some(last) = applied.last()
        && last.version > meta.schema_version
    {
        return Err(StorageError::IncompatibleSchema(format!(
            "migration {} is recorded above store schema {}",
            last.version, meta.schema_version
        )));
    }
    let sqlite_version = user_version(conn)?;
    if document_lineage(migrations, &meta, &applied, sqlite_version).is_some()
        || intelligence_branch(migrations, &meta, &applied).is_some()
        || application_lineage(migrations, &meta, &applied, sqlite_version).is_some()
        || dw62_lineage(migrations, &meta, &applied, sqlite_version).is_some()
    {
        return Ok(Some((meta, AccessMode::ReadWrite)));
    }
    // A colliding version must match one complete, known catalogue. Keep the deployed
    // forward-compatibility rules for later, additive versions after verifying their known prefix.
    if (67..=build).contains(&meta.schema_version) {
        if exact_catalogue(migrations, &meta, &applied, sqlite_version).is_none() {
            return Err(StorageError::IncompatibleSchema(
                "store version witnesses differ from the known migration catalogue".into(),
            ));
        }
    } else {
        for m in migrations
            .iter()
            .filter(|m| m.version <= meta.schema_version)
        {
            match applied.iter().find(|a| a.version == m.version) {
                Some(a) if a.checksum == m.checksum() => {}
                Some(a) if split_migration(migrations, &meta, a).is_some() => {}
                Some(a) => {
                    return Err(StorageError::IncompatibleSchema(format!(
                        "migration {} checksum {} does not match this build ({})",
                        m.version,
                        a.checksum,
                        m.checksum()
                    )));
                }
                None => {
                    return Err(StorageError::IncompatibleSchema(format!(
                        "migration {} missing from the ledger",
                        m.version
                    )));
                }
            }
        }
    }
    let access = if meta.minimum_writer > build {
        AccessMode::ReadOnly {
            reason: format!(
                "store schema {} requires writer version {}; this build is {build} \
                    (diagnostics/export only)",
                meta.schema_version, meta.minimum_writer
            ),
        }
    } else {
        AccessMode::ReadWrite
    };
    Ok(Some((meta, access)))
}

/// Bootstraps an empty store or applies pending migrations, after [`check`]; only called when
/// this build may write. Returns the resulting metadata.
pub(crate) fn prepare(
    conn: &Connection,
    migrations: &[Migration],
    new_store_id: impl FnOnce() -> String,
    now_ms: i64,
    app_build: &str,
) -> Result<(Meta, AccessMode), StorageError> {
    let (meta, access) = match check(conn, migrations)? {
        Some(found) => found,
        None => {
            let first = migrations
                .first()
                .ok_or_else(|| StorageError::IncompatibleSchema("no migrations".into()))?;
            apply(conn, first, Some(&new_store_id()), now_ms, app_build)?;
            check(conn, migrations)?.ok_or_else(|| {
                StorageError::IncompatibleSchema("bootstrap lost store_meta".into())
            })?
        }
    };
    if let AccessMode::ReadOnly { .. } = access {
        return Ok((meta, access));
    }
    let applied = ledger(conn)?;
    let mut version = meta.schema_version;
    if let Some(lineage) = document_lineage(migrations, &meta, &applied, user_version(conn)?) {
        version = reconcile_documents(conn, migrations, lineage, now_ms, app_build)?;
    } else if let Some(count) =
        application_lineage(migrations, &meta, &applied, user_version(conn)?)
    {
        version = reconcile_application(conn, migrations, count, now_ms, app_build)?;
    } else if dw62_lineage(migrations, &meta, &applied, user_version(conn)?).is_some() {
        version = reconcile_dw62(conn, migrations, now_ms, app_build)?;
    } else if intelligence_branch(migrations, &meta, &applied).is_some() {
        version = reconcile_intelligence_branch(conn, migrations, now_ms, app_build)?;
    } else if version == 64
        && let Some((systemone, keep_working)) = applied
            .iter()
            .find_map(|applied| split_migration(migrations, &meta, applied))
    {
        apply_in_transaction(conn, systemone, None, now_ms, app_build, Some(keep_working))?;
        version = keep_working.version;
    }
    for m in migrations.iter().filter(|m| m.version > version) {
        apply(conn, m, None, now_ms, app_build)?;
    }
    let meta = read_meta(conn)?
        .ok_or_else(|| StorageError::IncompatibleSchema("store_meta disappeared".into()))?;
    Ok((meta, AccessMode::ReadWrite))
}

/// Recorded checksum of migration 20; shipped SQL is immutable.
pub const MIGRATION_0020_SHA256: &str =
    "7cabf466f43dcc1677f62ad14ebb1e96ed9239f2cf505e9646c0a4e38287876a";

/// Recorded checksum of migration 21; shipped SQL is immutable.
pub const MIGRATION_0021_SHA256: &str =
    "27be7d33bb8b6ff768c4de0c2f688ba5442ef699a06130349a054913dcd5bc77";

/// Recorded checksum of migration 22.
pub const MIGRATION_0022_SHA256: &str =
    "2d0815af52a522e23870f9a2af56dc2a6f2f5227fbb842d1377cd49f84e02524";

/// Recorded checksum of migration 23.
pub const MIGRATION_0023_SHA256: &str =
    "bd92e57bf3aa579397301ccbb06deb45570712db11fa2ff229045e0c92c59c93";

/// Recorded checksum of migration 24.
pub const MIGRATION_0024_SHA256: &str =
    "7a648e33e052c1f0e8d2cd5d795f4a68a05f97a97399040b4be4631b7750f44b";

/// Recorded checksum of migration 25.
pub const MIGRATION_0025_SHA256: &str =
    "36b3e80b714419a911ac7e7dd74108b6bb17e4e5efedc3c24b8faf76b349d28f";

/// Recorded checksum of migration 26.
pub const MIGRATION_0026_SHA256: &str =
    "11c429872a1895385d75fd449994beb9bda95ccaf4e920dea63b944da3e2d478";

/// Recorded checksum of migration 27.
pub const MIGRATION_0027_SHA256: &str =
    "dafafed20991660c492ef38f23448111618cecb0b5f061793f266271a75c1c8f";

/// Recorded checksum of migration 28.
pub const MIGRATION_0028_SHA256: &str =
    "e7a0aa389a4f71a7e4bd0956bcd862fbddc55d0ee7ba3d3910478e7e44be3d96";

/// Recorded checksum of migration 29.
pub const MIGRATION_0029_SHA256: &str =
    "7200b277333537224ca24390534988203f57286d8f6158633bed767fded2713e";

/// Recorded checksum of migration 30.
pub const MIGRATION_0030_SHA256: &str =
    "641e930e10aa7fed31aa0e8bef49a0fb441a0d9cd2ff14aa66bab3fb87b36e68";

/// Recorded checksum of migration 31.
pub const MIGRATION_0031_SHA256: &str =
    "4d3d1401b9da77b2511dbef64f1a1cc057a56f0818f970b7976ba967d7892963";

/// Recorded checksum of migration 32; updated only before its first publication.
pub const MIGRATION_0032_SHA256: &str =
    "1a860d2cb4e54abeb61cd07a3c07520de4c6fbb148c774758211ea98b14b6dbb";

/// Recorded checksum of migration 33.
pub const MIGRATION_0033_SHA256: &str =
    "2c1687e7ee2d039a99fbe5ddc088a8bbd275322905315b343db4ebf17c39f4bb";

/// Recorded checksum of database metadata storage migration34.
pub const MIGRATION_0034_SHA256: &str =
    "ab4492daa17015c6f409259dc313abede9035c2fa9bf4c412c42746cdbdf151b";

/// Recorded checksum of settings credential custody migration35.
pub const MIGRATION_0035_SHA256: &str =
    "6810fcf899c738c2e15e2963b9df3f4525e8899581659e7e1896775d3327c17a";
