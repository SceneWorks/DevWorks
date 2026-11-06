# Declarative native UI contributions

A NativeUi contribution supplies the SDK's closed tree: Text, Group, Field, Action, Table and Image. It does not supply arbitrary egui objects, HTML, JavaScript, a WebView, or a protected human confirmation dialog. The tree has bounded depth/node counts and fields/actions use registered IDs and schema-bound data.

A small tree can describe a status and an ordinary action:

```json
{"kind":"Group","children":[{"kind":"Text","text":"Index status"},{"kind":"Action","actionId":"demo-pack/inspect","label":"Inspect"}]}
```

An Action asks for its registered host operation under current scope/permissions. It cannot mint an approval, Continue grant, destructive confirmation or human identity. Use Image only with an exact visible artifact, and Table only with its declared data schema. Read-only fields are not an enforcement substitute for the broker's authority checks.

Keep view/data revisions coherent. A stale view should refresh rather than execute a guessed target. Preserve text labels, keyboard accessibility, bounded errors and a truthful unavailable state when a host renderer or action binding is absent. Do not tell users a page is installed just because a manifest's NativeUi variant decoded.

Configuration and enable/disable lifecycle still use package quiescence and current registry/configuration epochs. See [Tools](help:sdk-tools), [Protocol](help:sdk-protocol), and [Add-ons](help:addons).
