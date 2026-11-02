# GraphQLite evaluation evidence

The revised indexed adapter is the primary common-workload comparison; original native-ID adapter measurements remain preserved. Both matrices passed all 18 cases. Native cache diagnostics intentionally reproduce stale results; read the observations even when case_errors is empty.

- `graphqlite-results-indexed.json` and `graphqlite-results-initial.json`: workload aggregates with ranges.
- `runs/key-measured/20261101T194050977085Z`: revised configs, results, raw samples and resource logs; `cache-profiles` contains nine native-cache profiles.
- `runs/measured/20261101T192949196362Z`: original matrix and nine earlier cache profiles.
- `runs/freshness-008` through `freshness-010`: final strengthened coherence/rollback/generation-reuse diagnostics. Earlier attempts are retained, not substituted for these results.
- `runs/recovery-final`: final owned-process fault probes; earlier batches preserved.
- `runs/source-native-id-v1` and `runs/source-indexed-v2`: exact timed source snapshots. `harness-final`: final coherence helper and runner sources.
- `logs`: build, unit-test, query-plan, environment and execution records, including corrected harness compilation attempts.
- `verification.json`: entry points, provenance and unchanged timed source assertions.

Absolute paths in original records identify the original environment. For reproduction, extract the archive and use fresh absolute fixture/work paths. `harness-final` is the complete small workspace source; build only eval-graphqlite and eval-sqlite as the report shows. The report authoring script is captured for audit; its original fixed paths require adjustment if reused elsewhere.

The downloadable archive adds `source.json.gz`, the identical compressed fixture, and the published GraphQLite crate payload. Generated database files, compiled helpers and build caches are excluded. A checksum inventory covers every included file. No source data was uploaded or sent to providers.
