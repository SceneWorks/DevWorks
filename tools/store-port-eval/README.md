# Native persistence boundary experiment

Disposable architecture feasibility subset, not the production persistence adapter. `core.rs` defines driver-free repository/unit-of-work/read-view ports. SQL and SQLite handles live only in private `sqlite.rs`; the executable owns only a temporary fixture database.

The probe uses the existing architecture SQL schema plus fixture graph/vector tables, actual bundled SQLite and sqlite-vec. It exercises drop rollback, atomic domain/receipt/history/outbox changes, replay versus revision conflicts, whole-unit overlays, scoped exact vector/FTS selection, publication comparisons, bounded pins and a second-connection membership revocation before text materialization. One compile-fail doctest checks transaction-borrow lifetime. The selected Cargo pins are experimental candidates; other targets and the production async write owner are unqualified.

See [evidence and reproduction](../../docs/architecture/validation/data-delivery-contracts/README.md) for exact versions, checks, limits and commands. No inference, real credentials or user data are used. Never point the executable at a user database: its fixture setup requires an owned disposable path.
