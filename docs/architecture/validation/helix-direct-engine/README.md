# Helix direct-engine compile probe

This minimal disposable consumer verifies pinned dependency resolution and type checking. It is not DevWorks application scaffolding, a linked/runtime test or a benchmark. Run `cargo check --locked -j 2 --manifest-path Cargo.toml` from this directory. The compilation log replaces the original local probe path with `<probe>`; package and revision output is preserved.

Observed environment: macOS 27.0.1 arm64, Rust/Cargo 1.98.0. `HelixDB` and `HelixDbSource` imports passed. Queries, disk durability, target-platform packaging and performance were not exercised.
