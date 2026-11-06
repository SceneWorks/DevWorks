# Native Help integrity performance qualification

Recorded 2026-11-06 against PR41 source `75b02053`, using the actual signed-package,
SQLite, native file and Help runtime fixture
`near_limit_guide_is_bounded_full_tail_unicode_and_reused_across_registry_churn`.
No provider, model execution, network or operating-system secrets were used.

The guide contained **264,375,372 bytes**. The full-source digest, Unicode tail,
checkpoint boundary, changed package source, current/historical references,
cancellation, deadline and allocation assertions remained enabled. The original
30-second publication watchdog and legal source bound were unchanged.

## Controlled comparison

The same AArch64 machine reported SHA-2 hardware support. Temporary instrumentation
measured the owner phases and cancellation callback counts; it is excluded from the
shipping code. All measurements below use the immutable bundle commitment cache.
The only change between columns is `[profile.dev.package.sha2] opt-level = 3`.

| Measurement | Default dependency compilation | SHA-2 optimization |
|---|---:|---:|
| Full native case | 49.76 s | 15.22 s |
| First guide publication | 7,798 ms | 2,307 ms |
| Current package verification | 1,876 ms | 116 ms |
| Full content projection | 2,022 ms | 169 ms |
| First artifact staging/copy | 2,036 ms | 183 ms |
| Repeated unchanged generation | 7,428 ms | 1,924 ms |
| Generation after registry churn | 5,533 ms | 1,820 ms |
| Largest measured allocation | 131,438 B | 131,438 B |
| Peak additional live memory | 690,340 B | 690,508 B |

The verification, projection and first staging phases respectively made **4,085,
16,138 and 4,038 cancellation callbacks in both runs**. These are callback counts,
not claimed SQLite-query counts: the existing control checks storage every sixteenth
callback. No integrity pass or cancellation check was removed. Both native runs
passed every assertion.

Before the immutable bundle cache, the same native case passed in 54.41 s. The
cache separately removes repeated hashing of executable-immutable bundled assets;
it does not cache the current registry, handler composition or authority state.
Its unit test preserves the previous exact commitment bytes and file order. The
composition regression compares the exact current bundled catalog IDs and still
checks every authored topic/reference plus immutable history across regeneration.

## Scope and limits

The Cargo override applies to the resolved SHA-2 dependencies, including versions
0.11.1 and 0.10.9; neither dependency version nor algorithm changes. Dev/test builds
otherwise retain their normal optimization settings.

The resolved 0.11.1 implementation selects x86 SHA/SSE4.1 or AArch64 SHA-2 after
runtime feature detection, with a software fallback. The measurements above are
**ARM hardware-backend evidence, not Windows timing evidence**. Existing Windows CI
showed a 30-second publication timeout but did not record enough phase/journal data
to prove its precise terminal or throughput cause. Windows CI remains required
acceptance evidence; this correction does not claim that Windows already passes.

Reproduction command, with the original fixture and assertions:

```sh
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 cargo test --locked -p devworks-engine \
  --test packages help_guides::near_limit_guide -- --nocapture
```

## Final source checks

After removing temporary instrumentation, the full-size native guide case passed
in **14.88 s** (largest allocation 131,438 B, peak additional live memory 690,180 B).
The exact bundled-ID/authored-history composition regression passed (1/1), the
core Help tests passed (5/5), and core/adapters/engine all-target Clippy with
`-D warnings`, rustfmt and diff checks passed. These are local AArch64 results.
