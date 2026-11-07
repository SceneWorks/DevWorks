CREATE DATABASE metadata_fixture;
GO
CREATE LOGIN metadata_reader WITH PASSWORD=N'devworks_metadata_reader', CHECK_POLICY=OFF;
GO
USE metadata_fixture;
GO
CREATE USER metadata_reader FOR LOGIN metadata_reader;
GRANT CONNECT TO metadata_reader;
GRANT VIEW DEFINITION TO metadata_reader;
DENY SELECT ON SCHEMA::dbo TO metadata_reader;
CREATE TABLE dbo.parent(id INT NOT NULL CONSTRAINT parent_pk PRIMARY KEY, note NVARCHAR(255) NOT NULL CONSTRAINT parent_note_default DEFAULT N'metadata default');
CREATE TABLE dbo.child(id INT NOT NULL CONSTRAINT child_pk PRIMARY KEY, parent_id INT NOT NULL, CONSTRAINT child_parent_fk FOREIGN KEY(parent_id) REFERENCES dbo.parent(id));
CREATE INDEX child_pair ON dbo.child(parent_id,id);
INSERT dbo.parent(id,note) VALUES(1,N'SOURCE_ROW_SENTINEL_NEVER_INDEX');
GO
CREATE VIEW dbo.read_parent AS SELECT id,note FROM dbo.parent;
GO
CREATE PROCEDURE dbo.describe_parent @input_id INT AS SELECT id FROM dbo.parent WHERE id=@input_id;
GO
SELECT CONVERT(VARCHAR(128),SERVERPROPERTY('ProductVersion')) AS engine_version;
USE metadata_fixture;
GO
DECLARE @ddl NVARCHAR(MAX)=N'CREATE TABLE dbo.meta_large_default(value NVARCHAR(MAX) DEFAULT(N''' + REPLICATE(CAST(N'x' AS NVARCHAR(MAX)),6000) + N'''));';
EXEC sys.sp_executesql @ddl;
GO

-- Indexed views must retain View parent identity for both columns and indexes.
SET ANSI_NULLS ON;
SET ANSI_PADDING ON;
SET ANSI_WARNINGS ON;
SET ARITHABORT ON;
SET CONCAT_NULL_YIELDS_NULL ON;
SET QUOTED_IDENTIFIER ON;
SET NUMERIC_ROUNDABORT OFF;
GO
CREATE VIEW dbo.meta_indexed_view WITH SCHEMABINDING AS SELECT id,note FROM dbo.parent;
GO
CREATE UNIQUE CLUSTERED INDEX meta_indexed_view_key ON dbo.meta_indexed_view(id);
GO

USE metadata_fixture;
GO
CREATE TYPE dbo.meta_table_ids AS TABLE (id INT NOT NULL PRIMARY KEY, note NVARCHAR(20), CHECK(id>0));
GO
DECLARE @ddl NVARCHAR(MAX)=N'CREATE VIEW dbo.meta_oversized_definition AS SELECT CAST(1 AS INT) AS id /*'+REPLICATE(CAST(N'x' AS NVARCHAR(MAX)),40000)+N'*/';
EXEC sys.sp_executesql @ddl;
GO
EXEC sys.sp_addextendedproperty @name=N'MS_Description',@value=N'column description',@level0type=N'SCHEMA',@level0name=N'dbo',@level1type=N'TABLE',@level1name=N'parent',@level2type=N'COLUMN',@level2name=N'id';
EXEC sys.sp_addextendedproperty @name=N'MS_Description',@value=N'index description',@level0type=N'SCHEMA',@level0name=N'dbo',@level1type=N'TABLE',@level1name=N'parent',@level2type=N'INDEX',@level2name=N'parent_pk';
GO
DECLARE @metadata_parameters nvarchar(max);
;WITH p(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM p WHERE n<129)
 SELECT @metadata_parameters=STRING_AGG(CONVERT(nvarchar(max),'@p'+CONVERT(varchar(10),n)+' int'),',') WITHIN GROUP(ORDER BY n)
 FROM p OPTION(MAXRECURSION 0);
EXEC('CREATE PROCEDURE dbo.meta_many_parameters '+@metadata_parameters+' AS RETURN 0');
GO
-- No metadata_reader user in this database: explicit selection must fail a probe.
USE master;
GO
CREATE DATABASE metadata_inaccessible;
GO
USE metadata_fixture;
GO

CREATE TABLE dbo.meta_filtered_index(email nvarchar(200), active bit NOT NULL);
CREATE UNIQUE INDEX meta_filtered_email ON dbo.meta_filtered_index(email) WHERE active=1;
GO
