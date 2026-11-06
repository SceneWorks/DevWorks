# Diagnostics, logs, and evidence

Diagnostics and logs explain actual native failures and unavailable runtime capabilities. Use the correlated diagnostic or operation ID from a refused action. Filter by current scope, operation, severity, or source where offered, and retain the relevant bounded records with exact evidence artifacts.

Logs intentionally avoid secret values, request bodies containing credentials, and sensitive URL queries. A credential reference or profile name is not the credential. Do not paste passwords, tokens, or raw provider material into an ordinary diagnostic to work around redaction.

## Investigate a failure

Identify the attempted operation and its state. Check the target's current revision, adapter availability, role/scope, independent blockers, and source identity. Distinguish a validation refusal before dispatch, a provider/process failure, cancellation, and an uncertain external effect. Reconcile uncertainty before rerunning a mutation.

The default log retention is 30 days and 50,000 records; actual settings are in [Settings reference](help:settings-reference). Retention bounds diagnostic records, not original evidence artifacts. A collected log does not authorize deleting an artifact still referenced elsewhere.

If a reader, helper, index producer, provider, or OS notification bridge is unsupported on this host, report that limitation with its native diagnostic. Do not present a missing adapter as a successful empty result. See [Recovery](help:recovery), [Resources](help:resources), and [Add-ons](help:addons).
