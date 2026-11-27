# Index

Rust-analyzer supplies Cargo package identities, Rust definitions, and semantic references for indexing. The helper needs Cargo, rust-analyzer, and rust-src for toolchain 1.98.0. Install the macOS arm64 bundle through Add-ons and include the package in the repository's source configuration.

Cargo reads the captured source and resolves dependencies offline. Locked crates.io dependencies must be present in the local Cargo cache. The index reports missing dependencies, unsupported dependency sources, invalid Cargo paths, resource limits, and compiler failures as partial coverage. Structural results remain available. The helper never runs repository build scripts or proc macros.

The package's source budget is 4 MiB across 4,096 files. The compiler has a 45-second deadline and an 8 MiB SCIP output limit. The helper samples its owned process group's memory against a 2 GiB budget. A run pauses or stops with its owning index operation.
