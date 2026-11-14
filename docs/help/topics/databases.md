# Databases and schema capabilities

The Databases destination groups database connections and schema exploration. DevWorks reads database metadata only. No registered operation runs SQL you or an agent supplies, and none writes to a source database. Operations named `database.read`, `database.execute_write` and the other row or write operations have no handler, so row browsing, queries and migrations are not available.

## Schema sources

The `schemas.*` operations manage metadata sources and read what was captured:

- `schemas.configure` adds or edits a source connection, or sets its scope. Only a person can do this, and an edit needs the source's current revision. A password goes in through the app's own entry field, never through a message or tool call, and it is stored as a handle.
- `schemas.index` captures a source's metadata. With `testOnly` it checks the connection without publishing anything. `schemas.refresh` captures again and publishes a new immutable view only if the capture succeeds. `schemas.remove_index` removes a source's published index.
- `schemas.connections` lists the sources in a scope, `schemas.objects` lists catalogs, schemas, tables, views, columns, indexes, constraints, procedures, types and graph entities or edges, filtered by kind, parent or text, and `schemas.relationships` lists the links between the objects you name.

Reads work on one published view and page through it, so a page is consistent with the view it started from. Each read checks your current access again. Keeping a view does not grant access to it. A failed refresh keeps the last published view, and it is never shown as an empty successful schema. Capture runs in the background, so a cancelled or revoked source cannot publish afterwards. Refreshes can also run on a schedule you authorize. A schedule only reads metadata.

The metadata readers accept one release line each: PostgreSQL 17, SQL Server 16, MySQL 8.4 and MariaDB 11.4. SQLite is read from a file. A server on another release is refused, not guessed.

When you report a relationship, give the source and the view's revision. Rows and credentials can be sensitive, and the project's tool permissions and redaction still apply. See [Indexing](help:indexing), [Diagnostics](help:diagnostics), [Permissions](help:permissions), and [Capability reference](help:capability-reference).
