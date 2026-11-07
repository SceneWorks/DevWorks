# DW-88 settings credential collection (acceptance pending)

Settings credential values stay in the existing SecretStore namespace for the data
root. Migration35 records opaque handles and Writing/Ready/Retained/Deleting/Deleted
custody state, never a value. Its writer floor prevents older writers bypassing
retirement fences. Prior migrations remain byte-identical.

A native stage reserves custody before the external write and returns a reference
only after a live Ready publication. The staging bound is30minutes and survives
restart. Previously untracked legacy orphans receive one conservative30minute grace
period when first observed. A late or failed put cannot publish a retired reference;
its exact unpublished handle is cleaned after the write completes.

Collection checks every retained settings revision and proposal across all scopes,
including old rollback targets and decided proposal bodies. It atomically marks an
unreferenced eligible handle Deleting before its external deletion. Save, proposal,
reconcile, approval and rollback use the same writer-time validation and retention
fence. A racing save either commits first and protects its reference, or receives a
recorded validation refusal instructing re-entry. SecretStore failures retain the
tombstone for retry; Deleted handles cannot be introduced again. Provider references
and other data-root namespaces remain protected.

Native evidence uses only SessionSecrets and a controlled wrapper around it. Eight
custody cases pass5.91s: restart/expiry, exact rollback after501 newer revisions,
pending/reconciled/decided proposal reachability, delete/save ordering, failed delete
retry, late put completion, legacy grace/provider/namespace isolation and migration
checksum/writer floor. The original connection owner fails the unchanged restart
expiry assertion0.88s (it removes0 keys instead of1); the corrected owner passes.

The wider connection suite passes55cases44.50s and settings13cases10.45s. All4 native
credential-entry/removal UI cases pass14.43s, both native web consumers pass7.53s, and
workspace/all-target strict checks pass46.00s. These earlier wider suites precede the
additional legacy-isolation case; production behavior is unchanged by that addition.
No real Keychain, credential facility, OS trust settings, remote provider or learned
inference was accessed during validation. Review, CI, merge and final acceptance
remain required.

The ninth review batch reproduced successful cleanup clearing a failed intake's
availability state (new native SQLite/controlled-secret regression,0.76s).
Failed intake now records its original error after best-effort cleanup, preserving
the native session-only fallback. All9 custody cases pass6.95s,4 native settings UI
cases14.47s and strict workspace/all-target checks39.10s. No real OS secret store
is accessed by these fixtures.
