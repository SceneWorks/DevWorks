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
