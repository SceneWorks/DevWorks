# Databases and schema capabilities

Databases manages source connections, explores published schema, and runs bounded SQL through the Data tab. Enable read or write access explicitly on the selected connection and for the project that will use it. A connection can grant schema discovery independently of data access.

## Run SQL

Select a source and open **Data**. Choose **Read data**, enter one read statement, and run it. A read returns up to 100 rows in the desktop. **Capture schema digest** records the live schema precondition for a later schema operation. For a network database, enter the planned migration or direct schema SQL in the preview field so the digest covers the relations the change will use. Use bound parameters to pass values separately from the SQL. A binary value is returned as an immutable artifact linked to your current source access.

**Write data** accepts an authorized `INSERT`, `UPDATE`, or `DELETE` when the connection allows writes. Use **Apply migration** for a versioned migration. Supply the exact artifact, repository commit, and repository-relative file, then check the captured schema digest before submitting. Use **Direct schema change** with a recorded reason when a versioned migration is unavailable. Both paths check target permissions separately from access to the migration file.

After submitting a change, use **Check operation** to see the recorded outcome. An unknown outcome requires observation of the original operation. DevWorks keeps the operation identity and does not resend its SQL automatically.

A workspace administrator can list, save, or delete sensitive column rules in **Data**. Rules apply to the physical source columns, including when a query uses aliases or views. The retained `mysql_readonly_query` tool uses the same checks and accepts 1–500 rows, with a default of 100.

## Schema sources

The `schemas.*` operations manage metadata sources and read what was captured:

- `schemas.configure` adds or edits a source connection, or sets its scope. Only a person can do this, and an edit needs the source's current revision. A password goes in through the app's own entry field, never through a message or tool call, and it is stored as a handle.
- `schemas.index` captures a source's metadata. With `testOnly` it checks the connection without publishing anything. `schemas.refresh` captures again and publishes a new immutable view only if the capture succeeds. `schemas.remove_index` removes a source's published index.
- `schemas.connections` lists the sources in a scope, `schemas.objects` lists catalogs, schemas, tables, views, columns, indexes, constraints, procedures, types and graph entities or edges, filtered by kind, parent or text, and `schemas.relationships` lists the links between the objects you name.

## Code that uses a table

`schemas.code_references` lists the code that uses one table, view or column of a published view. For each use it gives the repository, file, line and definition, what found the use, and a short excerpt. It reads the published code graph of every repository you can see in the scope, and the code graph's node detail shows the same uses. A name matches without regard to case, whether or not the code qualifies it with the schema. Code that names a table without its schema matches that table in every schema.

Code indexing records a use when the table name is written in the code:

- SQL written whole in a string literal: `SELECT`, `INSERT`, `UPDATE`, `DELETE`, `WITH` and `MERGE` statements, with the columns a statement names unambiguously.
- An ORM mapping that names its table or column: JPA and Hibernate in Java and Kotlin, Entity Framework Core in C#, SQLAlchemy and Django in Python, ActiveRecord in Ruby, Eloquent and Doctrine in PHP, TypeORM and Sequelize in TypeScript and JavaScript, GORM in Go, and Diesel and SeaORM in Rust.

These are not recorded, because the code does not write the table name: names an ORM derives by convention (ActiveRecord plural class names, Django `app_model` names, GORM snake-case names, Entity Framework `DbSet` names, Sequelize model names), SQL built by joining strings or by interpolation, Swift and Scala code, and Prisma schema files. SQL written in lowercase with no punctuation (for example `select name from users`) is not recognised, because it reads like ordinary text; `SELECT name FROM users` or a statement with a comma, parenthesis or `=` is.

Reads work on one published view and page through it, so a page is consistent with the view it started from. Each read checks your current access again. Keeping a view does not grant access to it. A failed refresh keeps the last published view, and it is never shown as an empty successful schema. Capture runs in the background, so a cancelled or revoked source cannot publish afterwards. Refreshes can also run on a schedule you authorize. A schedule only reads metadata.

The metadata readers accept one release line each: PostgreSQL 17, SQL Server 16, MySQL 8.4 and MariaDB 11.4. SQLite is read from a file. A server on another release is refused, not guessed.

When you report a relationship, give the source and the view's revision. Rows and credentials can be sensitive, and the project's tool permissions and redaction still apply. See [Indexing](help:indexing), [Diagnostics](help:diagnostics), [Permissions](help:permissions), and [Capability reference](help:capability-reference).
