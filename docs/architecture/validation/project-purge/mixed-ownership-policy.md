# DW-83 mixed ownership rules — version1 selectors

These frozen selectors resolve the mixed-table partition; they are not an executable
purge or permission to delete. P denotes the exact reviewed project ID. Owned work,
sessions, artifacts, events and operations are materialized ID sets from the same
manifest snapshot. A textual occurrence of P never establishes ownership.

| Table | Selected rows and disposition |
| --- | --- |
| actors | Global identities remain. Revoke project principal bindings with a monotonic revision. Never infer actor ownership from work/session use. |
| artifacts | Current Project scope=P establishes ownership, not origin_project_id. Preserve live explicit shares to surviving projects, workspace/current-other-project owners, and independently retained system publications. Remove unshared owned content; retain only minimal identities required by surviving typed references. Receipt artifacts use receipt ownership separately. |
| artifact_blob_pins | Pins protect content until their actual holder releases it. Remove only a holder owned by the selected purge and proved quiescent; preserve backup/read/global/unknown holders. An unknown holder prevents byte deletion, rather than authorizing unlink. |
| blobs | Content-addressed, globally shared. Select exact generations whose last surviving live-version/capture/import/reference and pin disappear in this manifest. Recheck all references and reader pins at unlink. Never remove a blob merely because one owned artifact used it. |
| authority_grants | Decode the typed GrantBinding. Revoke bindings to P or selected owned roots/operations, preserving independent workspace/other-project grants. Keep opaque revocation identity/fence, remove private binding payload when no longer needed. |
| browser_observations | Follow browser_sessions.project_id=P. Remove observation content and its owned artifact references; independently shared artifact bytes follow the artifact keeper rule. |
| command_receipts | Join result receipt.commandId to the actual CommandDecided event whose project_id=P. scope_key is not ownership. Retain exact replay fences and minimal original outcome; remove private result content through an explicit purged-content representation. Unknown legacy attribution refuses preview until reconciled. |
| receipt_replies | Follow the selected receipt composite key. Remove private reply artifact/version/schema content through the receipt policy; preserve minimal command identity and original committed outcome without permitting reexecution. |
| external_cursors | Global source/consumer progress is independently owned. Preserve it; source_id strings do not establish project ownership. If a future producer introduces a project-owned cursor, its typed attribution must be added to policy before preview is enabled. |
| github_intake_clock | Global scheduler clock remains unchanged. |
| graph_read_pins | Select the digest of the serialized typed Project(P) DataScope. scope_json is the exact GraphScope view set, not project attribution: decode/validate that view set and its child leases. Discard only the selected leases. Workspace/other-project digests remain. Owner namespaces are not reversible ownership selectors. |
| graph_read_pin_views | Follow selected graph pins; release only their corresponding index pin holders. Preserve global index views, repository captures and unrelated pins. |
| schema_read_pins | Decode StoredSchemaReadPin and select its canonical Project(P) scope digest. Remove only these leases; preserve global source/view/index registrations and workspace/other-project pins. |
| integration_links | Remove edges whose selected work or project-owned integration record is removed. Independently owned records/history remain; cross-scope references render a minimal deleted identity. |
| knowledge_source_heads | Global source revision observations remain while surviving claim bases need them. Remove only exact source refs whose entire known owning basis is removed and which have no surviving references. A source_ref may be private text; never copy it into purge audit. Unknown attribution refuses content removal. |
| notification_audience | Global current Human identity remains. |
| os_notification_deliveries | Follow selected project-owned notifications; delete their copied delivery detail. Preserve other-project/workspace notification history and the global audience. |
| operations | Select actual owned root/assignment lineage or project-attributed accepted CommandDecided/receipt operation. Native artifacts.stage_file revision1 additionally owns its closed payload.projectId: work root and assignment are deliberately absent. Validate the exact journal fingerprint, native source and kept-version provenance; foreign/unknown fields refuse assessment. Evidence/observed/input producers record their actual project before bytes. Existing scope-less journals never gain guessed ownership from artifact origin or path text. Preserve minimal operation identities needed by independently owned publication/deduplication references, scrub private payloads, revoke execution fences, and remove owned attempts/queue/approval records. No operation may be restarted from a purged payload. Unknown rootless attribution refuses preview. |
| package_handler_commits | Independent package processing deduplication survives. Native intents contain only actionKey and operationId, not command arguments. Preserve minimal package/subscription/event/receipt/checkpoint identity and original outcome, retaining a minimal event identity if required; remove automatically copied project-private fields if introduced by a recognized typed format. Never sweep global package state. |
| resources | scope=project and project_id=P selects the local logical registration and its owned leases/grants. Machine/global resources and other-project resources remain. No physical resource, filesystem object or Git resource is changed. |

Managed non-SQL state also needs ownership coverage. HostArtifactSources binds
upload/attachment/work-output handles to their project in memory; removing a
project revokes those handles without deleting user input or Git work-output files.
DiagnosticExports writes host-owned staging bytes and removes them on decided save
or its ten-minute sweep. Its current ticket-to-path map loses project attribution
on restart: purge requires durable exact file attribution or a proved-empty legacy
staging set before it can claim physical completion. Neither a global directory
sweep nor a path containing P is an ownership proof.

ui-state.json contains both scoped Human drafts and automatically copied view state.
Scope determines ownership: selected session-bound drafts and typed P-owned pending
frames are part of the manifest; workspace/other-project drafts remain. The exact
file/client policy is managed-file-and-client-policy.md. Never infer ownership from
arbitrary prose or the navigation selection alone.

The companion receipt, storage-guard and managed-file/client policies settle the
remaining step0 boundaries. Their combined version1 is recorded before product
implementation; runtime preview still fails closed on unknown schema or ownership.
