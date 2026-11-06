use super::catalog::{Catalog, Query, profile};
use super::*;
use futures_util::TryStreamExt;
use tiberius::{AuthMethod, Client, Config, EncryptionLevel};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

const QUERIES: &[Query] = &[
    Query {
        sql: r#"SELECT CASE WHEN DATALENGTH(metadata.document)>65536 THEN NULL ELSE metadata.document END FROM (SELECT (SELECT s.name AS [schema],'Schema' AS kind,s.name AS name FOR JSON PATH,WITHOUT_ARRAY_WRAPPER) AS document FROM sys.schemas s WHERE s.name NOT IN('sys','INFORMATION_SCHEMA','guest') AND s.schema_id<16384) metadata"#,
    },
    Query {
        sql: r#"SELECT CASE WHEN DATALENGTH(metadata.document)>65536 THEN NULL ELSE metadata.document END FROM (SELECT (SELECT s.name AS [schema],CASE o.type WHEN 'V' THEN 'View' ELSE 'Table' END AS kind,o.name AS name,LEFT(m.definition,65536) AS definition,CAST(CASE WHEN DATALENGTH(m.definition)>131072 THEN 1 ELSE 0 END AS bit) AS truncated,JSON_QUERY((SELECT CAST(ep.value AS nvarchar(4000)) AS comment FOR JSON PATH,WITHOUT_ARRAY_WRAPPER)) AS metadata FOR JSON PATH,WITHOUT_ARRAY_WRAPPER) AS document FROM sys.objects o JOIN sys.schemas s ON s.schema_id=o.schema_id LEFT JOIN sys.sql_modules m ON m.object_id=o.object_id LEFT JOIN sys.extended_properties ep ON ep.major_id=o.object_id AND ep.minor_id=0 AND ep.name='MS_Description' WHERE o.type IN('U','V') AND o.is_ms_shipped=0) metadata"#,
    },
    Query {
        sql: r#"SELECT CASE WHEN DATALENGTH(metadata.document)>65536 THEN NULL ELSE metadata.document END FROM (SELECT (SELECT s.name AS [schema],'Column' AS kind,c.name AS name,o.name AS parent,CASE o.type WHEN 'V' THEN 'View' ELSE 'Table' END AS parentKind,JSON_QUERY((SELECT CONVERT(varchar(20),c.column_id) AS ordinal,t.name+CASE WHEN t.name IN('varchar','nvarchar','char','nchar','varbinary','binary') THEN '('+CASE WHEN c.max_length=-1 THEN 'max' ELSE CONVERT(varchar(20),CASE WHEN t.name IN('nvarchar','nchar') THEN c.max_length/2 ELSE c.max_length END) END+')' WHEN t.name IN('decimal','numeric') THEN '('+CONVERT(varchar(20),c.precision)+','+CONVERT(varchar(20),c.scale)+')' ELSE '' END AS dataType,c.is_nullable AS nullable,CAST(CASE WHEN EXISTS(SELECT 1 FROM sys.indexes i JOIN sys.index_columns ic ON ic.object_id=i.object_id AND ic.index_id=i.index_id WHERE i.object_id=o.object_id AND i.is_primary_key=1 AND ic.column_id=c.column_id) THEN 1 ELSE 0 END AS bit) AS primaryKey,LEFT(dc.definition,4096) AS defaultValue,CASE WHEN c.is_computed=1 THEN 'Computed' WHEN c.is_identity=1 THEN 'Identity' END AS extra,CAST(ep.value AS nvarchar(4000)) AS comment FOR JSON PATH,WITHOUT_ARRAY_WRAPPER)) AS metadata FOR JSON PATH,WITHOUT_ARRAY_WRAPPER) AS document FROM sys.columns c JOIN sys.objects o ON o.object_id=c.object_id JOIN sys.schemas s ON s.schema_id=o.schema_id JOIN sys.types t ON t.user_type_id=c.user_type_id LEFT JOIN sys.default_constraints dc ON dc.object_id=c.default_object_id LEFT JOIN sys.extended_properties ep ON ep.major_id=c.object_id AND ep.minor_id=c.column_id AND ep.name='MS_Description' WHERE o.type IN('U','V') AND o.is_ms_shipped=0) metadata"#,
    },
    Query {
        sql: r#"SELECT CASE WHEN DATALENGTH(metadata.document)>65536 THEN NULL ELSE metadata.document END FROM (SELECT (SELECT s.name AS [schema],'Index' AS kind,i.name AS name,o.name AS parent,JSON_QUERY((SELECT i.is_unique AS [unique],i.type_desc AS indexType,JSON_QUERY(COALESCE((SELECT '['+STRING_AGG(CONVERT(nvarchar(max),'"'+STRING_ESCAPE(c.name,'json')+'"'),',') WITHIN GROUP(ORDER BY ic.key_ordinal)+']' FROM sys.index_columns ic JOIN sys.columns c ON c.object_id=ic.object_id AND c.column_id=ic.column_id WHERE ic.object_id=i.object_id AND ic.index_id=i.index_id AND ic.key_ordinal>0),'[]')) AS columnNames FOR JSON PATH,WITHOUT_ARRAY_WRAPPER)) AS metadata FOR JSON PATH,WITHOUT_ARRAY_WRAPPER) AS document FROM sys.indexes i JOIN sys.objects o ON o.object_id=i.object_id JOIN sys.schemas s ON s.schema_id=o.schema_id WHERE i.index_id>0 AND i.name IS NOT NULL AND o.is_ms_shipped=0) metadata"#,
    },
    Query {
        sql: r#"SELECT CASE WHEN DATALENGTH(metadata.document)>65536 THEN NULL ELSE metadata.document END FROM (SELECT (SELECT s.name AS [schema],'Constraint' AS kind,kc.name AS name,o.name AS parent,JSON_QUERY((SELECT CASE kc.type WHEN 'PK' THEN 'PrimaryKey' ELSE 'Unique' END AS constraintType,JSON_QUERY(COALESCE((SELECT '['+STRING_AGG(CONVERT(nvarchar(max),'"'+STRING_ESCAPE(c.name,'json')+'"'),',') WITHIN GROUP(ORDER BY ic.key_ordinal)+']' FROM sys.index_columns ic JOIN sys.columns c ON c.object_id=ic.object_id AND c.column_id=ic.column_id WHERE ic.object_id=kc.parent_object_id AND ic.index_id=kc.unique_index_id AND ic.key_ordinal>0),'[]')) AS columnNames FOR JSON PATH,WITHOUT_ARRAY_WRAPPER)) AS metadata FOR JSON PATH,WITHOUT_ARRAY_WRAPPER) AS document FROM sys.key_constraints kc JOIN sys.objects o ON o.object_id=kc.parent_object_id JOIN sys.schemas s ON s.schema_id=o.schema_id WHERE o.is_ms_shipped=0) metadata"#,
    },
    Query {
        sql: r#"SELECT CASE WHEN DATALENGTH(metadata.document)>65536 THEN NULL ELSE metadata.document END FROM (SELECT (SELECT s.name AS [schema],'Constraint' AS kind,fk.name AS name,o.name AS parent,JSON_QUERY((SELECT rs.name AS [schema],ro.name AS [table] FOR JSON PATH,WITHOUT_ARRAY_WRAPPER)) AS reference,JSON_QUERY((SELECT 'ForeignKey' AS constraintType,rs.name+'.'+ro.name AS referencedTable,JSON_QUERY((SELECT '['+STRING_AGG(CONVERT(nvarchar(max),'"'+STRING_ESCAPE(pc.name,'json')+'"'),',') WITHIN GROUP(ORDER BY fkc.constraint_column_id)+']' FROM sys.foreign_key_columns fkc JOIN sys.columns pc ON pc.object_id=fkc.parent_object_id AND pc.column_id=fkc.parent_column_id WHERE fkc.constraint_object_id=fk.object_id)) AS columnNames,JSON_QUERY((SELECT '['+STRING_AGG(CONVERT(nvarchar(max),'"'+STRING_ESCAPE(rc.name,'json')+'"'),',') WITHIN GROUP(ORDER BY fkc.constraint_column_id)+']' FROM sys.foreign_key_columns fkc JOIN sys.columns rc ON rc.object_id=fkc.referenced_object_id AND rc.column_id=fkc.referenced_column_id WHERE fkc.constraint_object_id=fk.object_id)) AS referencedColumns,fk.update_referential_action_desc+'/'+fk.delete_referential_action_desc AS extra FOR JSON PATH,WITHOUT_ARRAY_WRAPPER)) AS metadata FOR JSON PATH,WITHOUT_ARRAY_WRAPPER) AS document FROM sys.foreign_keys fk JOIN sys.objects o ON o.object_id=fk.parent_object_id JOIN sys.schemas s ON s.schema_id=o.schema_id JOIN sys.objects ro ON ro.object_id=fk.referenced_object_id JOIN sys.schemas rs ON rs.schema_id=ro.schema_id WHERE o.is_ms_shipped=0) metadata"#,
    },
    Query {
        sql: r#"SELECT CASE WHEN DATALENGTH(metadata.document)>65536 THEN NULL ELSE metadata.document END FROM (SELECT (SELECT s.name AS [schema],'Constraint' AS kind,cc.name AS name,o.name AS parent,JSON_QUERY((SELECT 'Check' AS constraintType,LEFT(cc.definition,65536) AS checkClause FOR JSON PATH,WITHOUT_ARRAY_WRAPPER)) AS metadata FOR JSON PATH,WITHOUT_ARRAY_WRAPPER) AS document FROM sys.check_constraints cc JOIN sys.objects o ON o.object_id=cc.parent_object_id JOIN sys.schemas s ON s.schema_id=o.schema_id WHERE o.is_ms_shipped=0) metadata"#,
    },
    Query {
        sql: r#"SELECT CASE WHEN DATALENGTH(metadata.document)>65536 THEN NULL ELSE metadata.document END FROM (SELECT (SELECT s.name AS [schema],'Procedure' AS kind,o.name AS name,LEFT(m.definition,65536) AS definition,JSON_QUERY((SELECT o.type_desc AS routineType,JSON_QUERY((SELECT COALESCE(NULLIF(p.name,''),'return') AS name,CONVERT(varchar(20),p.parameter_id) AS ordinal,CASE WHEN p.parameter_id=0 THEN 'RETURN' WHEN p.is_output=1 THEN 'OUT' ELSE 'IN' END AS mode,t.name AS dataType,p.is_nullable AS nullable FROM sys.parameters p JOIN sys.types t ON t.user_type_id=p.user_type_id WHERE p.object_id=o.object_id ORDER BY p.parameter_id FOR JSON PATH)) AS parameters FOR JSON PATH,WITHOUT_ARRAY_WRAPPER)) AS metadata FOR JSON PATH,WITHOUT_ARRAY_WRAPPER) AS document FROM sys.objects o JOIN sys.schemas s ON s.schema_id=o.schema_id LEFT JOIN sys.sql_modules m ON m.object_id=o.object_id WHERE o.type IN('P','PC','FN','IF','TF','FS','FT') AND o.is_ms_shipped=0) metadata"#,
    },
    Query {
        sql: r#"SELECT CASE WHEN DATALENGTH(metadata.document)>65536 THEN NULL ELSE metadata.document END FROM (SELECT (SELECT s.name AS [schema],'Type' AS kind,t.name AS name,JSON_QUERY((SELECT t.name AS dataType,CASE WHEN t.is_table_type=1 THEN 'TableType' WHEN t.is_assembly_type=1 THEN 'ClrType' ELSE 'AliasType' END AS extra FOR JSON PATH,WITHOUT_ARRAY_WRAPPER)) AS metadata FOR JSON PATH,WITHOUT_ARRAY_WRAPPER) AS document FROM sys.types t JOIN sys.schemas s ON s.schema_id=t.schema_id WHERE t.is_user_defined=1) metadata"#,
    },
];
fn classify(error: tiberius::error::Error) -> MetadataFailure {
    if matches!(error.code(), Some(18456 | 229 | 916 | 4060)) {
        MetadataFailure::AccessDenied
    } else {
        MetadataFailure::Unavailable
    }
}
async fn connect(
    c: &Collector,
    target: &Target,
    database: &str,
) -> Result<Client<Compat<TcpStream>>, MetadataFailure> {
    let mut config = Config::new();
    config.host(&target.host);
    config.port(target.port);
    config.database(database);
    config.authentication(AuthMethod::sql_server(
        &target.username,
        c.request
            .password
            .as_ref()
            .ok_or(MetadataFailure::CredentialsRequired)?
            .expose(),
    ));
    match target.tls {
        SchemaTlsMode::LoopbackPlaintext => config.encryption(EncryptionLevel::NotSupported),
        SchemaTlsMode::VerifyFull => config.encryption(EncryptionLevel::Required),
        SchemaTlsMode::VerifyCa => return Err(MetadataFailure::UnsupportedProfile),
    };
    if let Some(pem) = &c.request.ca_certificate {
        config.trust_cert_ca_bundle(pem.clone());
    }
    let tcp = TcpStream::connect((target.host.as_str(), target.port))
        .await
        .map_err(|_| MetadataFailure::Unavailable)?;
    tcp.set_nodelay(true)
        .map_err(|_| MetadataFailure::Unavailable)?;
    Client::connect(config, tcp.compat_write())
        .await
        .map_err(classify)
}
pub(super) async fn read(
    mut c: Collector,
    target: Target,
) -> Result<MetadataSnapshot, MetadataFailure> {
    let database = if target.catalog.is_empty() {
        "master"
    } else {
        &target.catalog
    };
    let mut conn = connect(&c, &target, database).await?;
    let row = conn
        .simple_query("SELECT CONVERT(varchar(128),SERVERPROPERTY('ProductVersion'))")
        .await
        .map_err(classify)?
        .into_row()
        .await
        .map_err(classify)?
        .ok_or(MetadataFailure::InvalidMetadata)?;
    let version = row
        .try_get::<&str, _>(0)
        .map_err(|_| MetadataFailure::InvalidMetadata)?
        .ok_or(MetadataFailure::InvalidMetadata)?;
    if !version.starts_with("16.") {
        return Err(MetadataFailure::UnsupportedProfile);
    }
    let tls = if matches!(target.tls, SchemaTlsMode::LoopbackPlaintext) {
        "LoopbackPlaintext"
    } else {
        "VerifyFull; platform roots plus explicit CA"
    };
    let profile = profile(
        "tiberius-0.13.0/native-tls",
        version,
        tls,
        &[
            "Catalog",
            "Schema",
            "Table",
            "View",
            "Column",
            "Index",
            "Constraint",
            "Procedure",
            "Type",
        ],
        &["GraphEntity", "GraphEdge"],
        &[
            "SQL password authentication; VerifyFull or explicit numeric-loopback plaintext. VerifyCa is refused by this qualified profile.",
            "Fixed sys catalog statements enforce this adapter's metadata boundary; use a metadata-only login with VIEW DEFINITION for full visibility.",
        ],
    )?;
    if c.request.test_only {
        conn.close().await.map_err(classify)?;
        return Ok(c.finish(profile));
    }
    let names = if !target.catalog.is_empty() {
        vec![target.catalog.clone()]
    } else {
        let mut names = Vec::new();
        let mut rows=conn.simple_query("SELECT name FROM sys.databases WHERE state=0 AND database_id>4 AND HAS_DBACCESS(name)=1 ORDER BY name").await.map_err(classify)?.into_row_stream();
        while let Some(row) = rows.try_next().await.map_err(classify)? {
            c.check()?;
            let name = row
                .try_get::<&str, _>(0)
                .map_err(|_| MetadataFailure::InvalidMetadata)?
                .ok_or(MetadataFailure::InvalidMetadata)?;
            if !c.request.config.catalogs.0.is_empty()
                && !c
                    .request
                    .config
                    .catalogs
                    .0
                    .iter()
                    .any(|v| v.0.as_str() == name)
            {
                continue;
            }
            if names.len() == 128 {
                c.gap("metadata_catalog_limit");
                break;
            }
            names.push(name.into());
        }
        if c.request
            .config
            .catalogs
            .0
            .iter()
            .any(|v| !names.iter().any(|n: &String| n == v.0.as_str()))
        {
            return Err(MetadataFailure::AccessDenied);
        }
        names
    };
    conn.close().await.map_err(classify)?;
    for name in names {
        let mut conn = connect(&c, &target, &name).await?;
        conn.simple_query("SET LOCK_TIMEOUT 500; SET XACT_ABORT ON; BEGIN TRANSACTION")
            .await
            .map_err(classify)?
            .into_results()
            .await
            .map_err(classify)?;
        let mut catalog = Catalog::new(c, name)?;
        for query in QUERIES {
            let mut rows = conn
                .simple_query(query.sql)
                .await
                .map_err(classify)?
                .into_row_stream();
            while let Some(row) = rows.try_next().await.map_err(classify)? {
                let raw = row
                    .try_get::<&str, _>(0)
                    .map_err(|_| MetadataFailure::InvalidMetadata)?;
                let Some(raw) = raw else {
                    catalog.collector.gap("metadata_row_byte_limit");
                    continue;
                };
                if !catalog.ingest(raw)? {
                    break;
                }
            }
        }
        conn.simple_query("ROLLBACK")
            .await
            .map_err(classify)?
            .into_results()
            .await
            .map_err(classify)?;
        conn.close().await.map_err(classify)?;
        c = catalog.collector;
    }
    Ok(c.finish(profile))
}
