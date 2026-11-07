# DW-36 delivery evidence (acceptance pending)

The current work branch wires the seven revision5 metadata operations, native target
adapters, immutable structural storage, runtime workers and maintenance, recurring
jobs, connection-owned CA custody and the native Databases manager/explorer. The
retained encryption-key utility shares SchemaService and native Human ingress.
The sections below preserve chronological checkpoints; earlier statements of
unfinished implementation describe those checkpoints, not the current branch.
Fresh exact-head review, platform CI, merge and final acceptance still gate delivery.

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

## Native database manager checkpoint (2026-11-06)

The Databases route now uses the same selected-operation gateway as agents. It manages source
creation/editing, file selection, human password intake, exact CA artifact attachment, project
access/exposure settings, connection tests, indexing and source/index removal. Structural reads
retain an explicit immutable view, with object/filter/child/relationship paging and exact endpoint
inspection. Source definitions, columns, keys/constraints, routines, adapter limits and freshness
are inspectable. Password text is excluded from saved draft state; pending commands retain their
original frames. Sync-all enumerates every catalog page before revision-checked admissions,
retains remaining work across a close, and reports admissions separately from actual completion.

Actual native UI cases pass3/3 in17.82s: real SQLite create/index/multiple object pages/column
inspection/removal preserving the original row sentinel; draft restart without password bytes;
and101-source sync-all beyond the first100-source page. The full workflow repeat passes7.59s.
Native read-only storage/worker/credential/source authority cases pass8/8 in6.71s. Seven recorded
contract checks pass0.26s over1865 domain cases/597 admissions. Independent jsonschema4.25.1
validates38 DW36 cases. All1859 previous fixture semantics and all earlier schema files are
unchanged. Strict workspace/all-target checks pass (latest cached final check0.22s).

Native testing caught unlabeled selectors, an invalid form TLS value, an unreachable long-page
paging control, and controls whose automatic egui IDs changed with live metadata. Selectors now
carry accessible labels, TLS values match the declared modes, the page scrolls, and interactive
buttons use explicit UI roots. Temporary geometry/timing diagnostics were removed.

These are local implementation checkpoints, not item acceptance. Retained schema pins,
scheduled sync, remaining adapter/TLS qualification and full reviewed CI/merge acceptance remain.

## Retained schema read checkpoint (2026-11-06)

Revision5 source results now include an owner/store/restore/scope-bound readView when a current
sealed metadata head can be captured. Internal query preparation rechecks writer-side authority
before creating the ten-minute retention lease; the final result and each drill-down resolve
current access independently. Listing again can reuse a still-live identity without extending
its expiry. Expired identities are never revived. Configuration/access revisions are independent
of ordinary indexing status/publication revisions, so reindexing preserves an explicitly pinned
old view and its cursor while source configuration changes revoke it. The native Databases screen
uses the selected pin for object and relationship reads; background source updates do not retarget
an existing selection.

All nine native SQLite/source/worker/credential cases pass8.20s, including a real target-file
reindex proving retained old metadata/cursor continuation, fresh current capture, wrong-owner and
wrong-scope rejection, expiry without revival, live project-access revocation, and unchanged source
rows. Forty revision5 DW36 wire cases pass typed and independent JSON Schema4.25.1 validation;
the complete domain corpus is1867 fixtures. Strict workspace/all-target checks pass17.40s.
The new regression also passes alone0.83s. These checks execute no learned inference or real
OS credential access. Migration34's current checksum is
0172c4261c1994bfc3545aeb6061e128d5c22c041daf8468da0f8c050ef8b661.

Bounded obsolete-view retention maintenance, scheduled synchronization, remaining adapter/TLS
and supported-x86 SQLServer qualification, full native acceptance and reviewed CI/merge remain
required parts of DW36. Earlier paragraphs are historical checkpoints, not current completion.

The complete native Databases UI suite passes3/3 in14.33s with pin-based exploration. The full
create/sync/page/inspect/remove flow still leaves source rows intact;101-source admission and
password-free draft restart cases remain unchanged.

## Bounded retention and verified TLS checkpoint (2026-11-06)

Native runtime maintenance now expires at most500 lease records and removes at most500 obsolete
metadata rows per pass. It preserves current source heads, live leases and the two newest sealed
views; relations precede objects and empty headers. Any remaining lease prevents a cascade from
escaping the page budget. Native target-file reindex tests prove live old-view retention and
expiry pruning without target I/O. All nine native schema/storage/worker cases pass8.12s on the
new dependency composition. Migration34's current checksum is
624240afaf525442d27d6a76d36e245319e436a224a3f7d48cd9d17f93d18385.

All eleven explicitly dispatched actual-server metadata cases pass0.30s across MariaDB11.4,
MySQL8.4, PostgreSQL17 and functional-emulated SQLServer16. TLS cases supply an isolated CA in
memory per connection, accept matching DNS, refuse an unknown CA and hostname mismatch, and
qualify VerifyCa where supported. SQLServer explicitly refuses VerifyCa and supports VerifyFull.
No OS trust installation or real credential facility is involved. The fixture script successfully
recreates its own isolated SQLServer plus certificate setup and runs both native cases0.59s;
its report explicitly says host arm64/nativeX86 false. The new dedicated CI workflow requires
an actual x86 host by default. Local emulation is not native-x86 acceptance.

Actual macOS SQLServer TLS initially failed with native-tls/Security Framework. Upstream
[Tiberius documents this incompatibility](https://github.com/tiberius-rs/tiberius#encryption-tlsssl).
Its rustls backend with bundled roots plus an explicit CA passes. This introduces Tiberius's
aws-lc build dependencies; existing app transports retain their own explicit providers. SQLx's
VerifyCa verifier additionally needs an unambiguous process provider when both providers compile;
network metadata intake selects ring only if no provider is already installed. All four TLS cases
pass together, including the VerifyCa path which reproduced a dual-provider panic before this fix.

A separate PostgreSQL metadata_limits fixture reproduces a6000-character default being silently
truncated while snapshot/object coverage claimed Complete. The regression is RED0.10s before
explicit truncation flags and per-object gap propagation, GREEN0.11s afterward. Analogous bounded
PostgreSQL definitions report their truncation. The reader still never queries source rows.
Strict workspace/all-target Clippy passes16.56s; native Databases UI passes3/3 in14.35s with the
new retention worker and crypto composition. No learned inference runs.

DW36 remains incomplete. In particular, the current worker reads CA artifacts under the requesting
principal; a source shared across projects needs narrowly authorized connection-owned certificate
custody without granting general artifact access. This seam, scheduled sync, remaining metadata
coverage/fault cases, native-x86 SQLServer CI, reviewed integration and acceptance remain required.

Connection CA custody checkpoint (7 November): configuration now captures a bounded
public-certificate PEM copy after the configuring actor reads the exact managed
artifact. The writer rechecks current artifact/version visibility before committing;
the private connection record retains only that selected immutable material. Metadata
workers use the connection copy under their original actor and current source/scope
fences. General artifact access is unchanged. Clearing/removing the configuration
removes its copy, and an already recorded configuration retry does not reopen a
subsequently deleted upload. No OS trust store or Keychain is accessed.

Twelve native SQLite/gateway/source cases pass10.36s. New cases exercise a real
project-B agent indexing a shared connection whose original CA artifact belongs
privately to project A, exact retry after original-artifact deletion, actor attribution,
invalid/private-key/oversized refusals with a queued index preserved, and original
artifact deletion between preparation and the writer commit. The custody reader is
explicitly recorded and does not establish X.509/TLS validity; the separate actual
engine TLS qualifications above cover transport. Disabling only the fresh writer
visibility check makes the race case fail its original SourceChanged assertion0.79s;
the temporary fault is removed. Strict workspace/all-target checks pass38.76s.

An investigated suspicion that an unrelated refused configuration cancelled its
accepted index did not reproduce: the gateway savepoint already rolls back all
handler writes on DomainDecision::Refused. Both original-order controls passed.
No cancellation-order product change was retained or claimed.
The composed native Databases UI suite also passes all3 cases14.39s after CA custody.

Scheduled sync checkpoint: schemas.index is qualified in the existing native job
catalog. Its additive current-source arguments select a connection; each actual
run captures its current revision once and durably retains the exact gateway frame
and invocation. Explicit manual CAS arguments retain their existing meaning. A
private connection uses a connection/run/scope-bound snapshot of actual human job
consent, with current source ownership/project access and enabled-state checks.
The executing actor stays Host, and no general source/artifact/data permission is
granted. Cancellation follows only the actual accepted child delivery.

Two native runtime cases pass10.67s: actual SQLite target ALTERTABLE between recurring
runs, exact replay after a lost committed acknowledgment with one read, original
Host/jobRun attribution, private discovery remaining denied, active child cancellation
preserving the prior view, queued project-access revocation before target I/O, and
later authorized execution at a fresh source revision. The complete36-case native
jobs suite passes130.77s, including prior Help/Conventions/recovery/cancellation
checks. Workspace/all-target strict checks pass39.83s. No inference is executed.

Actual PostgreSQL restricted-role regression reproduces a hidden routine definition
with false Complete coverage0.11s. Shared catalog projection now marks a missing
View/Procedure definition as Partial at both object and snapshot level. MariaDB's
restricted fixture exposes the same condition; explicit owner credentials prove
actual definitions and Complete coverage remain available. All13 dispatched native
network/coverage/TLS cases pass0.36s. No extra source grants are added to make the
restricted role appear complete. Supported x86 SQLServer CI and final integrated
review/CI/merge acceptance remain pending.

Adapter edge checkpoint (7 November): PostgreSQL accepts all PEM roots from the
selected connection CA bundle. The actual TLS case with an unrelated first root
reproduces InvalidTarget0.08s before the fix and succeeds with the matching second
root afterward. Each root is parsed into the per-connection connector only.

SQLite now preserves user names resembling the reserved prefix, groups composite
foreign keys into one constraint, resolves implicit referenced primary-key order
and case-insensitive target identity, and enumerates UNIQUE constraints. Oversized
defaults and omitted DDL retain explicit per-object/snapshot Partial coverage; CHECK
expressions remain visible in table DDL with the profile's enumeration limitation.
Three original native regressions fail0.05s, and the final5-case suite passes0.03s.
The revised bounded-default oracle names its exact qualified column and explicit
view column. Reverting only the default bound reproduces InvalidMetadata0.04s;
the temporary fault is removed before the final green run.

MariaDB/MySQL retain actual CHECK clauses, ordered composite columns and declared
routine parameter order. MariaDB's table identity binds duplicate CHECK names;
MySQL's different catalog uses its own compile-time SQL variant. Both original
CHECK regressions fail0.04s. The explicit large-default cases also qualify bounded
Partial results on MariaDB, MySQL and SQLServer. The simple Complete-profile cases
exclude that separately tested oversized fixture using the documented glob selector;
restricted/owner routine visibility remains independently checked.

All19 dispatched actual-network cases pass0.38s, including TLS chain/hostname
refusals and the new limits/constraint cases. SQLServer remains local x86 emulation
in this evidence; the dedicated native-x86 workflow now runs all3 corresponding
cases. Twelve native source/gateway cases pass10.80s; four native Databases UI cases
pass19.36s including visible per-object coverage, bounded value and unchanged source
bytes; two native schema-job cases pass10.71s. Workspace/all-target strict checks
pass39.11s before final current-main integration. No learned computation, real
credential store access or OS trust changes occur. Review/CI/merge/acceptance remain
required; this checkpoint does not complete DW-36.

Final retained utility checkpoint: the frozen admin GenerateKey endpoint and UI
specify a32-byte CSPRNG AES key encoded as standard padded Base64. SchemaService
now owns this same native Human-only utility using the existing Entropy/OsEntropy
boundary. The value is a redacted transient Secret, masked in the screen, explicitly
copyable/clearable and dropped on navigation. It never changes source configuration,
creates a credential handle, or enters protocol/history/persisted UI memory.
Thirteen native source/custody/utility cases pass11.39s; all5 native Databases UI
cases pass24.98s, including exact clipboard command bytes and privacy/clear behavior.
Inspect actions bring their actual object details into view. One earlier
uninstrumented missing-coverage-label event remains unattributed; that case passed
unchanged alone, and the final full suite preserves its assertions and bounds.
Final workspace/all-target strict checks pass11.86s. The retained-source audit maps
all source entries without dropping the admin utility or conflating metadata with
DW37's data operations. Review, actual supported-x86CI, merge and acceptance remain.


PR48 review regressions: missing SQLite WITHOUT ROWID index DDL and lazy plaintext
connection probes both reproduce against the published implementation (two failures,
0.04s); corrected file-reader suite passes6cases0.02s. A retained snapshot with more
than500 physical rows reproduces partial published visibility after one pruning
page (1.35s). Internal state2 now atomically hides the retiring view before any row
removal, stays distinct from unpublished staging, cannot be resealed, and continues
bounded cleanup. Direct readers and Exact gateway selection reject the retiring
view; native storage13cases pass11.28s, including unchanged pin/access/expiry checks.

The scheduled Ready-child case proves terminal pre-dispatch cancellation without
executing the metadata reader. Removing its exact matching-activity cleanup fails
the original assertion6.43s; all3 native schema jobs pass17.51s after restoration.
The prior published view/configuration revision survive and a later scheduled run
is admitted with a new operation identity.

Actual MySQL8.4/MariaDB11.4 and PostgreSQL17 fixtures now expose130 earlier visible
catalogs. Selection is bound inside SQL before discovery limits. Reverting those
adapters reproduces3 selection failures0.04s. Cross-database MySQL/Maria FK fixtures
retain both FOREIGN_KEY and REFERENCES edges; reverting only target-catalog identity
reproduces2 edge failures0.10s. A real indexed SQLServer view retains a View parent
for its index and HAS_INDEX edge; the original adapter fails that assertion0.07s.
All23 explicit actual-server cases pass0.48s with the corrections. New admin fixture
scripts are scoped to disposable qualification containers; no source row values
are collected and no real Keychain or OS trust settings are accessed.

The initial native-x86 workflow42326825930 passed on8299dfd1. The expanded workflow
runs4 SQLServer cases including indexed views and must qualify the corrected head
again. Workspace/all-target strict checks pass43.02s. Review/CI/merge/acceptance are
still pending; the initial platform result is not reused as final changed-head proof.

Final restored-tree integration: all5 native Databases UI cases pass21.10s and all23
actual-server cases pass0.44s. The7 review findings each have a native failing
regression with their correction removed; temporary faults are fully restored.


CI integration corrections after f204101e: generator revision-set expectations now
explicitly include the6 revision5 operations (13 generator tests10.82s, zero drift).
The migration33 test still asserts the exact1–33 prefix/checksum while permitting
additive migrations (native0.75s).

A controlled native background-inventory refusal while a sync-all command awaits its
actual receipt reproduces unintended batch cancellation6.89s. Only the inventory
scan can now cancel its own batch; an ordinary refresh failure preserves the admitted
command and remaining captured sources. A second controlled native case reproduces
abandonment of an explicit object search when source status sees a publication that
changed during lease preparation (6.80s). The UI refreshes the same source capture
once, including later inventory pages, then resumes the read or reports unavailability.
Source changes invalidate earlier read replies. The full7-case native Databases suite
passes27.27s; final workspace/all-target strict checks5.97s pass. These concrete bugs
are qualified by their regressions; the earlier uninstrumented Ubuntu wrong-object
click and macOS70/101 event are not retroactively assigned a sole cause.

The earlier main Windows convention context-Help timeout is separately tracked as
DW139. Failure-only route/topic/loading/error diagnostics preserve all assertions
and timeouts; the unchanged instrumented case passes13.55s locally. Its original
cause is still unknown, so that pass is not claimed as a repair. Review/CI/merge and
final acceptance remain pending on the next published head.

## Second review corrections (2026-11-07, acceptance pending)

Empty MySQL defaults now use a dedicated bounded0–4096-character primitive, preserving
empty versus absent values and the existing upper limit. Three new wire cases cover
empty, exact-limit and over-limit values. All1867 earlier fixture entries and71 earlier
schema files remain unchanged; the1870-case corpus passes all7 native recorded checks,
all8 revision checks,13 generator checks and43 independent DW36 JSON Schema cases.

Actual-server regressions cover explicit missing-catalog connection tests, a40K SQLServer
view definition retained as a bounded partial parent, extended-property class isolation,
and table-type columns/indexes/constraints under their public Type parent. All29 adapter
network cases pass1.07s. The original table-type orphan premise was not reproduced on
SQLServer16.0.4305.5: internal TT objects were filtered as system-shipped. The new case
instead qualifies full public table-type structure, including resolvable child parents.

The actual production desktop runtime now enables socket I/O. Its controlled loopback
regression fails with the previous timer-only builder and passes with the production
builder. Four explicitly dispatched real-server cases pass14.20s through Engine,
SchemaService, durable workers and publication: MariaDB, MySQL, PostgreSQL and SQLServer.
Credential facilities are controlled SessionSecrets; ordinary CI ignores these cases.
The dedicated isolated SQLServer workflow now also exercises this production path.

Unavailable/denied secret facilities degrade credential capability instead of aborting
boot. A controlled unavailable store regression fails before the correction and passes
6.99s after it, including SQLite publication and native Settings access. Genuine reference
reconciliation failures still fail boot. Switching a saved network source to SQLite clears
credential/CA references, username, TLS and pending password/CA input; native file selection,
save and actual SQLite publication pass7.01s. Endpoint-to-children exploration clears the
exact-object filter; the original workflow assertion fails without that correction.

A controlled late-layout case demonstrates why queued pointer coordinates can miss an
intended editor control; native AccessKit activation preserves its identity. New source
setup checks use that native input route while existing pointer workflows remain. No sole
cause is assigned to DW139's earlier uninstrumented Windows Help timeout. Database failure
diagnostics now include actual source status. The live route registry describes the actual
combined database manager/explorer, and offline UI displays its exact availability reason;
the existing integrated-route and offline checks pass unchanged (7.29s and0.25s).
Fresh complete UI/storage/jobs/strict checks, exact-head review, platform CI, supported-x86
qualification, merge and final retained-source acceptance still gate delivery.

Final local qualification on the second review tree:10 native DB UI cases pass39.80s,
13 source/storage/custody cases pass11.44s, strict workspace/all-target checks pass44.41s
(and0.23s after explicitly declaring Tokio net). A newly created isolated SQLServer
fixture passes7 adapter cases plus1 production Engine case, then is removed. The local
report is arm64/nativeX86=false; fresh actual-x86 CI is still required. A first scheduled
job command used a plural filter and ran zero cases; it is excluded from evidence.

Third-review PostgreSQL multibyte boundary: an actual view containing50,000 three-byte
characters reproduces the missing-parent assertion0.09s on3f9417f7. The server-side
byte-bound fallback now strips only definition/metadata payload while retaining bounded
structural identities and reference fields, marked Partial. All30 actual adapter cases
pass0.48s. Two PostgreSQL production Engine cases pass7.12s, including actual durable
publication and Partial coverage for that same multibyte view, plus the original normal
network workflow. Workspace/all-target strict checks pass44.57s and0.49s after the new
native case. The fixture changes only disposable DDL; no source rows, real credentials,
OS trust or learned inference are used. Fresh exact-head review and CI are required.
