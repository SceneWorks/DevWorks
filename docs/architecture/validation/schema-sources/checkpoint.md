# DW-36 implementation checkpoint (unfinished)

The work branch contains revision5 metadata contracts, native target metadata
adapters, migration34 and immutable structural storage, plus the shared service's
configuration/removal and source/object/relationship queries. Runtime composition
and native Databases screens are not yet registered or advertised.

Native qualification to date uses isolated fixtures only:

- SQLite: two real file cases, read-only opens, source identity and structural
  extraction, cancellation and bounded partials.
- MariaDB11.4.13, MySQL8.4.13 and PostgreSQL17: three actual native wire cases pass
  together0.30s on numeric-loopback plaintext fixture connections. Metadata includes
  tables/views/columns/defaults/indexes/foreign keys and routines; stable identities,
  no indexed row sentinel, object-limit partial, bad-credential refusal and
  cancellation are checked. The PostgreSQL metadata role has no table SELECT grant.
- Five real SQLite/gateway cases pass3.84s: visibility before pagination, source CAS,
  invisible staged views and durable publication witnesses, selected revision5
  gateway/HostAuthority configuration replay/refusals, shared-project credential and
  foreign-project redaction, cursor expiry after access changes, and credential intake.
- Core and adapters all-target strict checks pass29.72s. Migration34's checksum is
  f18c99268849c8e0aff347c898ab09a63e598bff6285c34c8ce1da98f6c6908c.

Password intake uses a separate store/purpose SecretStore namespace and immutable
human-bound handles. Password whitespace is preserved. Protocol/SQLite records hold
only opaque references. A submitted handle is reserved against UI cancellation until
its durable custody is resolved; a fresh service reconciles unreferenced handles once
before opening intake. Tests use SessionSecrets only, preserve referenced passwords
and other namespaces, and never touch OS Keychain or real credentials. Unpublished
reserved handles are conservatively retained until the next startup reconciliation.

Remaining work is still part of DW-36: durable index/refresh/test operation ownership,
source/authority/control fences, cancellation/recovery and scheduled/sync-all UI;
exact schema read pins; certificate artifact custody; complete native Databases
management/exploration; SQL Server16 on supported x86 hardware and actual TLS
qualification; per-adapter metadata edge/coverage obligations; revision5 canonical
fixtures; current stack integration; full native acceptance, review, CI and merge.
The SQL Server driver has compiled but has not run against an actual SQL Server.
Plaintext loopback qualification does not establish TLS behavior. No project or item
acceptance follows from this checkpoint, and no learned inference was executed.

SQL Server functional checkpoint: actual SQL Server16.0.4305.5 under isolated
x86 emulation passes the native TDS metadata case (1/1,0.91s), including structural
objects/routines/FKs/stable IDs, no row sentinel, bounded partials, bad credentials
and cancellation. The fixture login has VIEW DEFINITION and an explicit schema
SELECT denial. This validates static query functionality under this fixture only;
supported x86-host and TLS qualification remain pending. An initial command used
a nonmatching test filter and ran zero cases; it is excluded from evidence.

Current-stack merge d089b9c1 regenerates the shared revision5 envelopes with the
current convention revision4 fields. All70 pre-existing schema files and all1,814
prior fixture objects remain byte-exact/identical.32 new revision5 source/configuration,
engine-declaration, result, pre-revision and null-refusal cases pass independent
JSON Schema4.25.1 validation; all seven native recorded-contract tests pass0.23s,
covering1,846 domain fixtures and596 distinct native operation/mode/revision bindings.
These are wire/admission checks, separate from target-server/runtime qualification.

PostgreSQL edge qualification found two real adapter defects before service publication:
`NOT LIKE 'pg_%'` treated `_` as a wildcard and silently excluded a legal pgx_user
schema as Complete/empty; a materialized-view index used the default Table parent
identity instead of its retained View identity. The new actual-server cases failed
both assertions. Catalog filters now compare the literal pg_ prefix, and index rows
carry the correct parent kind. All six explicitly dispatched native network cases
pass0.24s across MariaDB/MySQL/PostgreSQL and functional-emulated SQL Server;
core/adapters all-target strict checks pass29.44s. PostgreSQL fixtures still read no
source rows. The retained edge and SQL Server SQL files are isolated fixture setup,
not application migrations or commands against a user database.

## Durable metadata worker checkpoint (2026-11-06)

The shared owner now admits schema index/refresh operations into the native operation journal
and durable queue. Each worker rechecks source revision, actor/scope/policy/control and attempt
fences before local writes. Target metadata I/O stays outside the writer; hidden batches contain
at most500 rows. Final sealing, source pointer and immutable publication witness commit together.
Source edits revoke/cancel pending work; bounded cleanup removes abandoned staging and preserves
sealed views. Test-only refresh keeps the existing published view. Failed target access never
manufactures healthy-empty coverage and releases the source for an explicit retry.

Three actual native SQLite worker cases pass2.08s: interrupted-operation/staging recovery,
no indexed row sentinel, test-only refresh, completion observation after source removal, queued
revocation/re-enable, and missing-target failure followed by successful explicit retry. The first
native gateway test caught and corrected a missing Accepted receipt/operation identity binding.
Core/adapters/engine/desktop runtime composition passes strict workspace/all-target checks41.89s.
Tests use SessionSecrets or no credentials; they never access a fixture OS keychain.

DW36 is still incomplete: native Databases manager/navigation, retained schema pin semantics,
scheduled/sync-all ownership, and remaining adapter/auth/TLS qualification must be implemented
and verified before item acceptance. SQLServer emulation is functional evidence only.
