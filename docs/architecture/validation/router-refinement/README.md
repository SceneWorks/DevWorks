# Router refinement evidence

1 November 2026. This pass adopts internal request, routing, streaming, auth, cache, accounting and price-refresh contracts. It does not implement a Rust router or qualify live provider access.

- [Source audit](source-audit.json): inspected simple-router revision `a556b3531b577173195bbc3876b556196447721d`, source digests, retained behavior and intentional differences. Generated CodeGraph summaries are orientation, not authoritative wire behavior. Its source tests were inspected selectively, not executed here.
- [Contract fixture probe](contract-probe.json): 33 checks using a small reference decoder/assembler, synthetic usage/prices and actual SQLite constraints. Exercises fragmented UTF-8/SSE data, terminal/error distinctions, native call/result identity/order, cumulative/cache accounting examples, cost classification, auxiliary request attribution, attempt uniqueness and prospective price publication. These are architecture mechanics, not actual Rust encoder/decoder, OAuth or provider SDK tests.
- [Schema mechanics](sqlite-mechanics.json): reruns the existing 28 SQLite mechanics checks against the evolved schema, including the router records. The previous persistence-pass schema/results remain separately preserved.
- [Native schema/vector load](vector-schema-check.json): the retained native sqlite-vec probe loads the evolved schema and repeats its five scoped-distance/dimension checks. No model execution.

Reproduce from the repository root, using the bundled Python runtime. `tools/storage-eval` was removed from the main branch because nothing runs it; its source and README remain at commit [`f76506ec`](https://github.com/SceneWorks/DevWorks/tree/f76506ec33c9563d19e03bc68a4b7e9e65178271/tools/storage-eval). Check out that commit to run the commands below.

```sh
python3 tools/planning/probe_router_contracts.py --output-dir /path/to/owned/evidence
python3 tools/storage-eval/persistence_contract_probe.py --output-dir /path/to/owned/evidence
cargo run --locked --offline --release --manifest-path docs/architecture/validation/persistence-refinement/vector-probe/Cargo.toml --target-dir tools/storage-eval/target -- docs/architecture/persistence-schema.sql
```

No provider credentials were read, no login was initiated, and no live inference/cache test was run. Official documentation was read for API contracts and pricing-source design, not to pin a current model inventory or copy live tariffs into the product.

G03 remains responsible for actual native request/stream/auth/secret-store fixtures, subscription compatibility, endpoint-specific option and usage semantics, official price-source parser fixtures and explicit later live qualification. G12 owns representative latency, token/price-estimation quality and resource behavior. The product choices now permit unpriced custom usage with visible partial monetary coverage and count subscription API-equivalent estimates toward feature limits.
