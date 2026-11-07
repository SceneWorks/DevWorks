use super::catalog::{Catalog, MAX_ROW_BYTES, Query, profile};
use super::*;
use futures_util::TryStreamExt;
use sqlx::mysql::{MySqlConnectOptions, MySqlSslMode};
use sqlx::{ConnectOptions, Connection, Row};

macro_rules! constraint_query {
    ($table_guard:literal) => { concat!(r#"SELECT CASE WHEN OCTET_LENGTH(metadata.document)>131072 THEN NULL ELSE CAST(metadata.document AS CHAR CHARACTER SET utf8mb4) END FROM (SELECT JSON_OBJECT('schema',tc.CONSTRAINT_SCHEMA,'kind','Constraint','name',tc.CONSTRAINT_NAME,'parent',tc.TABLE_NAME,'truncated',JSON_EXTRACT(IF(CHAR_LENGTH(cc.CHECK_CLAUSE)>65536,'true','false'),'$'),'metadata',JSON_OBJECT('constraintType',tc.CONSTRAINT_TYPE,'checkClause',LEFT(cc.CHECK_CLAUSE,65536),'columnNames',COALESCE((SELECT JSON_EXTRACT(CONCAT('[',GROUP_CONCAT(JSON_QUOTE(k.COLUMN_NAME) ORDER BY k.ORDINAL_POSITION SEPARATOR ','),']'),'$') FROM INFORMATION_SCHEMA.KEY_COLUMN_USAGE k WHERE k.CONSTRAINT_SCHEMA=tc.CONSTRAINT_SCHEMA AND k.TABLE_NAME=tc.TABLE_NAME AND k.CONSTRAINT_NAME=tc.CONSTRAINT_NAME),JSON_ARRAY()))) AS document FROM INFORMATION_SCHEMA.TABLE_CONSTRAINTS tc LEFT JOIN INFORMATION_SCHEMA.CHECK_CONSTRAINTS cc ON cc.CONSTRAINT_SCHEMA=tc.CONSTRAINT_SCHEMA AND cc.CONSTRAINT_NAME=tc.CONSTRAINT_NAME "#, $table_guard, r#" WHERE tc.CONSTRAINT_SCHEMA=? ORDER BY tc.TABLE_NAME,tc.CONSTRAINT_NAME) metadata"#) };
}

const QUERIES: &[Query] = &[
    Query {
        sql: r#"SELECT CASE WHEN OCTET_LENGTH(metadata.document)>131072 THEN NULL ELSE CAST(metadata.document AS CHAR CHARACTER SET utf8mb4) END FROM (SELECT JSON_OBJECT('schema',t.TABLE_SCHEMA,'kind',IF(t.TABLE_TYPE='VIEW','View','Table'),'name',t.TABLE_NAME,'definition',LEFT(v.VIEW_DEFINITION,65536),'truncated',JSON_EXTRACT(IF(OCTET_LENGTH(v.VIEW_DEFINITION)>65536,'true','false'),'$'),'metadata',JSON_OBJECT('comment',t.TABLE_COMMENT)) AS document FROM INFORMATION_SCHEMA.TABLES t LEFT JOIN INFORMATION_SCHEMA.VIEWS v ON v.TABLE_SCHEMA=t.TABLE_SCHEMA AND v.TABLE_NAME=t.TABLE_NAME WHERE t.TABLE_SCHEMA=? ORDER BY t.TABLE_NAME) metadata"#,
    },
    Query {
        sql: r#"SELECT CASE WHEN OCTET_LENGTH(metadata.document)>131072 THEN NULL ELSE CAST(metadata.document AS CHAR CHARACTER SET utf8mb4) END FROM (SELECT JSON_OBJECT('schema',c.TABLE_SCHEMA,'kind','Column','name',c.COLUMN_NAME,'parent',c.TABLE_NAME,'parentKind',IF(t.TABLE_TYPE='VIEW','View','Table'),'truncated',JSON_EXTRACT(IF(CHAR_LENGTH(c.COLUMN_DEFAULT)>4096,'true','false'),'$'),'metadata',JSON_OBJECT('ordinal',CAST(c.ORDINAL_POSITION AS CHAR),'dataType',c.COLUMN_TYPE,'nullable',JSON_EXTRACT(IF(c.IS_NULLABLE='YES','true','false'),'$'),'primaryKey',JSON_EXTRACT(IF(c.COLUMN_KEY='PRI','true','false'),'$'),'defaultValue',LEFT(c.COLUMN_DEFAULT,4096),'columnKey',c.COLUMN_KEY,'extra',c.EXTRA,'comment',c.COLUMN_COMMENT)) AS document FROM INFORMATION_SCHEMA.COLUMNS c JOIN INFORMATION_SCHEMA.TABLES t ON t.TABLE_SCHEMA=c.TABLE_SCHEMA AND t.TABLE_NAME=c.TABLE_NAME WHERE c.TABLE_SCHEMA=? ORDER BY c.TABLE_NAME,c.ORDINAL_POSITION) metadata"#,
    },
    Query {
        sql: r#"SELECT CASE WHEN OCTET_LENGTH(metadata.document)>131072 THEN NULL ELSE CAST(metadata.document AS CHAR CHARACTER SET utf8mb4) END FROM (SELECT JSON_OBJECT('schema',s.TABLE_SCHEMA,'kind','Index','name',s.INDEX_NAME,'parent',s.TABLE_NAME,'metadata',JSON_OBJECT('unique',JSON_EXTRACT(IF(MIN(s.NON_UNIQUE)=0,'true','false'),'$'),'indexType',MIN(s.INDEX_TYPE),'columnNames',JSON_EXTRACT(CONCAT('[',GROUP_CONCAT(JSON_QUOTE(COALESCE(s.COLUMN_NAME,'<expression>')) ORDER BY s.SEQ_IN_INDEX SEPARATOR ','),']'),'$'))) AS document FROM INFORMATION_SCHEMA.STATISTICS s WHERE s.TABLE_SCHEMA=? GROUP BY s.TABLE_SCHEMA,s.TABLE_NAME,s.INDEX_NAME ORDER BY s.TABLE_NAME,s.INDEX_NAME) metadata"#,
    },
    Query {
        sql: constraint_query!(""),
    },
    Query {
        sql: r#"SELECT CASE WHEN OCTET_LENGTH(metadata.document)>131072 THEN NULL ELSE CAST(metadata.document AS CHAR CHARACTER SET utf8mb4) END FROM (SELECT JSON_OBJECT('schema',k.TABLE_SCHEMA,'kind','Constraint','name',CONCAT(k.CONSTRAINT_NAME,':',k.ORDINAL_POSITION),'parent',k.TABLE_NAME,'metadata',JSON_OBJECT('constraintType','ForeignKeyColumn','ordinal',CAST(k.ORDINAL_POSITION AS CHAR),'columnNames',JSON_ARRAY(k.COLUMN_NAME),'referencedTable',CONCAT(k.REFERENCED_TABLE_SCHEMA,'.',k.REFERENCED_TABLE_NAME),'referencedColumns',JSON_ARRAY(k.REFERENCED_COLUMN_NAME)),'reference',JSON_OBJECT('schema',k.REFERENCED_TABLE_SCHEMA,'table',k.REFERENCED_TABLE_NAME)) AS document FROM INFORMATION_SCHEMA.KEY_COLUMN_USAGE k WHERE k.TABLE_SCHEMA=? AND k.REFERENCED_TABLE_NAME IS NOT NULL ORDER BY k.TABLE_NAME,k.CONSTRAINT_NAME,k.ORDINAL_POSITION) metadata"#,
    },
    Query {
        sql: r#"SELECT CASE WHEN OCTET_LENGTH(metadata.document)>131072 THEN NULL ELSE CAST(metadata.document AS CHAR CHARACTER SET utf8mb4) END FROM (SELECT JSON_OBJECT('schema',r.ROUTINE_SCHEMA,'kind','Procedure','name',r.SPECIFIC_NAME,'definition',LEFT(r.ROUTINE_DEFINITION,65536),'truncated',JSON_EXTRACT(IF(OCTET_LENGTH(r.ROUTINE_DEFINITION)>65536,'true','false'),'$'),'metadata',JSON_OBJECT('routineType',r.ROUTINE_TYPE,'comment',r.ROUTINE_COMMENT,'parameters',COALESCE((SELECT JSON_ARRAYAGG(JSON_OBJECT('name',COALESCE(p.PARAMETER_NAME,'return'),'ordinal',CAST(p.ORDINAL_POSITION AS CHAR),'mode',COALESCE(p.PARAMETER_MODE,'RETURN'),'dataType',COALESCE(p.DTD_IDENTIFIER,p.DATA_TYPE),'nullable',JSON_EXTRACT('true','$'))) FROM INFORMATION_SCHEMA.PARAMETERS p WHERE p.SPECIFIC_SCHEMA=r.ROUTINE_SCHEMA AND p.SPECIFIC_NAME=r.SPECIFIC_NAME),JSON_ARRAY()))) AS document FROM INFORMATION_SCHEMA.ROUTINES r WHERE r.ROUTINE_SCHEMA=? ORDER BY r.SPECIFIC_NAME) metadata"#,
    },
];
fn classify(error: sqlx::Error) -> MetadataFailure {
    let code = error
        .as_database_error()
        .and_then(|e| e.try_downcast_ref::<sqlx::mysql::MySqlDatabaseError>())
        .map(|e| e.number());
    match code {
        Some(1044 | 1045 | 1142 | 1227) => MetadataFailure::AccessDenied,
        _ => MetadataFailure::Unavailable,
    }
}
pub(super) async fn read(
    c: Collector,
    target: Target,
) -> Result<MetadataSnapshot, MetadataFailure> {
    let maria = matches!(c.request.config.engine, DbEngine::MariaDb);
    let mode = match target.tls {
        SchemaTlsMode::VerifyFull => MySqlSslMode::VerifyIdentity,
        SchemaTlsMode::VerifyCa => MySqlSslMode::VerifyCa,
        SchemaTlsMode::LoopbackPlaintext => MySqlSslMode::Disabled,
    };
    let mut options = MySqlConnectOptions::new()
        .host(&target.host)
        .port(target.port)
        .username(&target.username)
        .password(
            c.request
                .password
                .as_ref()
                .ok_or(MetadataFailure::CredentialsRequired)?
                .expose(),
        )
        .ssl_mode(mode)
        .statement_cache_capacity(0)
        .disable_statement_logging();
    if let Some(pem) = &c.request.ca_certificate {
        options = options.ssl_ca_from_pem(pem.clone());
    }
    let mut conn = options.connect().await.map_err(classify)?;
    let version: String = sqlx::query_scalar("SELECT VERSION()")
        .fetch_one(&mut conn)
        .await
        .map_err(classify)?;
    if maria != version.contains("MariaDB")
        || !(if maria {
            version.starts_with("11.4.")
        } else {
            version.starts_with("8.4.")
        })
    {
        return Err(MetadataFailure::UnsupportedProfile);
    }
    sqlx::raw_sql(if maria {
        "SET SESSION max_statement_time=30"
    } else {
        "SET SESSION max_execution_time=30000"
    })
    .execute(&mut conn)
    .await
    .map_err(classify)?;
    sqlx::raw_sql("SET SESSION group_concat_max_len=131072")
        .execute(&mut conn)
        .await
        .map_err(classify)?;
    sqlx::raw_sql("START TRANSACTION READ ONLY")
        .execute(&mut conn)
        .await
        .map_err(classify)?;
    let tls = match target.tls {
        SchemaTlsMode::VerifyFull => "VerifyFull; bundled Mozilla roots plus explicit CA",
        SchemaTlsMode::VerifyCa => "VerifyCa; bundled Mozilla roots plus explicit CA",
        SchemaTlsMode::LoopbackPlaintext => "LoopbackPlaintext",
    };
    let profile = profile(
        "sqlx-mysql-0.9.0",
        &version,
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
        ],
        &["Type", "GraphEntity", "GraphEdge"],
        &[
            "Password authentication; one configured server; fixed INFORMATION_SCHEMA only.",
            "Server catalog grants determine visibility; grant metadata visibility for full coverage.",
        ],
    )?;
    if c.request.test_only {
        conn.close().await.map_err(classify)?;
        return Ok(c.finish(profile));
    }
    let mut collector = c;
    let mut names = Vec::new();
    {
        let mut rows=sqlx::query("SELECT SCHEMA_NAME FROM INFORMATION_SCHEMA.SCHEMATA WHERE SCHEMA_NAME NOT IN('information_schema','mysql','performance_schema','sys') ORDER BY SCHEMA_NAME LIMIT 129").fetch(&mut conn);
        while let Some(row) = rows.try_next().await.map_err(classify)? {
            collector.check()?;
            let name: String = row
                .try_get(0)
                .map_err(|_| MetadataFailure::InvalidMetadata)?;
            if (!target.catalog.is_empty() && name != target.catalog)
                || (!collector.request.config.catalogs.0.is_empty()
                    && !collector
                        .request
                        .config
                        .catalogs
                        .0
                        .iter()
                        .any(|v| v.0.as_str() == name))
            {
                continue;
            }
            if names.len() == 128 {
                collector.gap("metadata_catalog_limit");
                break;
            }
            names.push(name);
        }
    }
    if (!target.catalog.is_empty() && !names.contains(&target.catalog))
        || collector
            .request
            .config
            .catalogs
            .0
            .iter()
            .any(|v| !names.iter().any(|n| n == v.0.as_str()))
    {
        return Err(MetadataFailure::AccessDenied);
    }
    for name in names {
        let mut catalog = Catalog::new(collector, name.clone())?;
        catalog.schema(&name)?;
        for query in QUERIES {
            // Both dialects use compile-time SQL. MariaDB permits equal CHECK names per table;
            // MySQL's CHECK_CONSTRAINTS catalog has no TABLE_NAME column.
            let statement = if maria && query.sql == constraint_query!("") {
                constraint_query!(" AND cc.TABLE_NAME=tc.TABLE_NAME ")
            } else {
                query.sql
            };
            let mut rows = sqlx::query(statement).bind(&name).fetch(&mut conn);
            while let Some(row) = rows.try_next().await.map_err(classify)? {
                let raw: Option<String> = row
                    .try_get(0)
                    .map_err(|_| MetadataFailure::InvalidMetadata)?;
                let Some(raw) = raw else {
                    catalog.collector.gap("metadata_row_byte_limit");
                    continue;
                };
                if raw.len() > MAX_ROW_BYTES {
                    catalog.collector.gap("metadata_row_byte_limit");
                    continue;
                }
                if !catalog.ingest(&raw)? {
                    break;
                }
            }
        }
        collector = catalog.collector;
    }
    sqlx::raw_sql("ROLLBACK")
        .execute(&mut conn)
        .await
        .map_err(classify)?;
    conn.close().await.map_err(classify)?;
    Ok(collector.finish(profile))
}
