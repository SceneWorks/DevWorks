# Visual architecture of the implemented code (VIS-01..03)

These diagrams describe the Rust code that exists in the workspace today: the actual structs,
traits, functions and ownership of each major subsystem, and the workflows whose correctness
depends on order, recovery and failure handling. They are versioned with the code; update the
`.mmd` source and its rendered `.svg` in the same change that alters the structure or workflow
they show. The earlier [architecture diagrams](../architecture-diagrams/manifest.json) remain the
proposed design baseline; where they differ, the code and these diagrams describe what is built.

**Legend.** Every box is an implemented type, trait (`<<trait>>`), function (`<<fn>>`) or module
unless it carries a `<<planned: DW-n>>` stereotype or the text "planned"; planned boxes name the
CodeGraph item that delivers them and are seams, not code. In class diagrams `*--` is ownership
(composition), `o--` a shared `Arc` reference, `..|>` a trait implementation and `..>` a use.
No inheritance is drawn because the code has none; supertraits appear only where they exist.

| Diagram | Kind | What it shows | Source to read |
| --- | --- | --- | --- |
| [01 Gateway and admission](01-gateway-admission.svg) ([source](01-gateway-admission.mmd)) | class | One admission path: `HostIngress` mints `InvocationContext`; `UseCases` dispatches the host-selected operation to its one handler through `Gateway`, which consults `AuthorityPort` (`HostAuthority`, wrapped by the desktop's `StoreAuthority`) and constructs `AdmittedQuery`/`AdmittedCommand`. | `crates/devworks-core/src/{gateway.rs,services/mod.rs,context.rs,authority/service.rs,ports/mod.rs}`, `crates/devworks-desktop/src/platform.rs` |
| [02 Storage unit of work](02-storage-unit-of-work.svg) ([source](02-storage-unit-of-work.mmd)) | class | `ApplicationStore`, `WriteJob`, `WriteTransaction`, `ReadSnapshot`, `run_write`/`read_snapshot`, and `SqliteStore` with its single writer thread, read pool, data-root lock and migration gate. | `crates/devworks-core/src/ports/storage.rs`, `crates/devworks-adapters/src/storage/sqlite/` |
| [03 Runtime composition](03-runtime-composition.svg) ([source](03-runtime-composition.mmd)) | class | `Composition` → `boot` → `Runtime`, and what the runtime owns: `Dispatcher` with its `Consumer`s (the required receipt reply consumer first), `Housekeeper` with `ProcessSupervisor` and `ResourceService`, `KeepAwake`, artifact collection. | `crates/devworks-engine/src/{lib.rs,runtime.rs,dispatch.rs,resources.rs,replies.rs}` |
| [04 Services and ports](04-services-and-ports.svg) ([source](04-services-and-ports.mmd)) | class | The registered services (artifacts, sessions, history, settings, queue, resources) with their operations, the core ports they depend on and the native adapters that implement them, including workspace upload tickets. | `crates/devworks-core/src/services/`, `crates/devworks-core/src/settings/`, `crates/devworks-adapters/src/{fs,paths.rs,power.rs,process}` |
| [05 Desktop shell](05-desktop-shell.svg) ([source](05-desktop-shell.mmd)) | class | `Engine`/`Host`/`DesktopBackend`, the `Backend` boundary and `Client` request pool, `Shell` and its view state (`Chat`, `SettingsScreen`, `Completions`, `Memory`, the `Screen` registry), `Dialogs` and `HostControl`; planned seams for the coordinator (DW-16), router selectors (DW-17) and notification feed (DW-14). | `crates/devworks-desktop/src/` |
| [10 Boot order](10-boot-order.svg) ([source](10-boot-order.mmd)) | sequence | Store open, the eight boot steps in their fixed order with each `BootError`, user registration after recovery, and the offline shell when the store cannot be opened. | `crates/devworks-desktop/src/host.rs`, `crates/devworks-engine/src/runtime.rs` |
| [11 Durable recovery](11-durable-recovery.svg) ([source](11-durable-recovery.mmd)) | state | `OperationState` and `QueueState` transitions, including boot classification (`Preflighted → RetryPending`, `Dispatching → OutcomeUnknown`), reconciliation, operator resolution and poison acknowledgment. | `crates/devworks-engine/src/dispatch.rs`, `crates/devworks-core/src/services/queue.rs` |
| [12 Receipt replies](12-receipt-replies.svg) ([source](12-receipt-replies.mmd)) | sequence | A command's receipt and reply intent committed together, replay and idempotency conflict, lost acknowledgment, fenced reply publication, the two crash points and how `history.command_receipt` reports each state. | `crates/devworks-core/src/{gateway.rs,services/artifacts/replies.rs,services/history/mod.rs}`, `crates/devworks-engine/src/replies.rs` |
| [13 Process and lease states](13-process-and-lease-states.svg) ([source](13-process-and-lease-states.mmd)) | state | `ProcessPhase` (launch, bounded stop, boot recovery by pid and birth token, `Lost`/`Uncertain` confirmed only by a person) and `LeasePhase` with physical handoff only after reconciliation. | `crates/devworks-core/src/services/{process.rs,resources/mod.rs}`, `crates/devworks-core/src/ports/storage/resources.rs` |
| [14 Process cancellation](14-process-cancellation.svg) ([source](14-process-cancellation.mmd)) | sequence | Cooperative stop, grace, forced kill within one bound, `Reaped`/`Uncertain`, record-write failure, and the lease reconciliation that grants the next waiter. | `crates/devworks-core/src/services/process.rs`, `crates/devworks-engine/src/resources.rs` |
| [15 Shell request and close](15-shell-request-and-close.svg) ([source](15-shell-request-and-close.mmd)) | sequence | A UI action through `Client` workers to `Runtime::command`, slot-superseded replies, runtime hints re-querying visible data incrementally, and the close path to `Runtime::shutdown` and release of the data root. | `crates/devworks-desktop/src/{shell.rs,backend.rs,host.rs}` |
| [16 Shell close states](16-shell-close-states.svg) ([source](16-shell-close-states.mmd)) | state | `shell::Lifecycle`: `Open → Quiescing → Persisting → ShuttingDown → Exiting → Closed`, the offline path, and the Quiescing seam (`HostControl::begin_quiesce`/`quiesced`) where run controls (DW-9) will pause active work roots. | `crates/devworks-desktop/src/shell.rs` |

Coordinator delegation, routed inference and tool execution, indexing, approvals, Git delivery
and the executable package lifecycle (also named by VIS-02) have no implementation yet; their
proposed workflows stay in the [architecture baseline](../architecture-diagrams/manifest.json) and
the items that build them add implemented diagrams here.

## Editing and rendering

Sources are Mermaid 11.12.0 text. Render with the authoring-only Mermaid CLI (not an application
dependency), strict security, Arial and HTML labels disabled, against a locally installed Chrome:

```sh
npm install @mermaid-js/mermaid-cli@11.12.0        # in a scratch directory, PUPPETEER_SKIP_DOWNLOAD=1
echo '{"executablePath": "<path to Chrome>"}' > puppeteer.json
echo '{"theme":"neutral","securityLevel":"strict","fontFamily":"Arial","flowchart":{"htmlLabels":false},"class":{"htmlLabels":false}}' > mermaid.json
for f in docs/architecture/visual/*.mmd; do
  npx mmdc -q -i "$f" -o "${f%.mmd}.svg" -p puppeteer.json -c mermaid.json -b white
done
```

Avoid `;` and `::` inside labels (Mermaid treats them as separators) and give states that share
a name in different composite states their own IDs. Rendered SVGs are committed beside their
sources and need no network to read. A rendered diagram is documentation, not test evidence.
