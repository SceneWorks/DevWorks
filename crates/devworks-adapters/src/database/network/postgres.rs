use super::catalog::{Catalog, Query, profile};
use super::*;
use futures_util::TryStreamExt;
use tokio_postgres::{Client, Config, config::SslMode};

const QUERIES: &[Query] = &[
    Query {
        sql: r#"SELECT json_build_object('schema',n.nspname,'kind','Schema','name',n.nspname)::text FROM pg_catalog.pg_namespace n WHERE left(n.nspname,3)<>'pg_' AND n.nspname<>'information_schema' AND pg_catalog.has_schema_privilege(n.oid,'USAGE') ORDER BY n.nspname"#,
    },
    Query {
        sql: r#"SELECT json_build_object('schema',n.nspname,'kind',CASE WHEN c.relkind IN('v','m') THEN 'View' ELSE 'Table' END,'name',c.relname,'definition',CASE WHEN c.relkind IN('v','m') THEN left(pg_catalog.pg_get_viewdef(c.oid,true),65536) END,'truncated',CASE WHEN c.relkind IN('v','m') THEN length(pg_catalog.pg_get_viewdef(c.oid,true))>65536 ELSE false END,'metadata',json_build_object('comment',pg_catalog.obj_description(c.oid,'pg_class'),'extra',CASE c.relkind WHEN 'm' THEN 'MaterializedView' WHEN 'p' THEN 'PartitionedTable' WHEN 'f' THEN 'ForeignTable' END))::text FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace WHERE c.relkind IN('r','p','v','m','f') AND left(n.nspname,3)<>'pg_' AND n.nspname<>'information_schema' AND pg_catalog.has_schema_privilege(n.oid,'USAGE') ORDER BY n.nspname,c.relname"#,
    },
    Query {
        sql: r#"SELECT json_build_object('schema',n.nspname,'kind','Column','name',a.attname,'parent',c.relname,'parentKind',CASE WHEN c.relkind IN('v','m') THEN 'View' ELSE 'Table' END,'truncated',COALESCE(length(pg_catalog.pg_get_expr(d.adbin,d.adrelid))>4096,false),'metadata',json_build_object('ordinal',a.attnum::text,'dataType',pg_catalog.format_type(a.atttypid,a.atttypmod),'nullable',NOT a.attnotnull,'primaryKey',EXISTS(SELECT 1 FROM pg_catalog.pg_index i WHERE i.indrelid=c.oid AND i.indisprimary AND a.attnum=ANY(i.indkey)),'defaultValue',left(pg_catalog.pg_get_expr(d.adbin,d.adrelid),4096),'extra',CASE WHEN a.attgenerated<>'' THEN 'Generated' WHEN a.attidentity<>'' THEN 'Identity' END,'comment',pg_catalog.col_description(c.oid,a.attnum)))::text FROM pg_catalog.pg_attribute a JOIN pg_catalog.pg_class c ON c.oid=a.attrelid JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace LEFT JOIN pg_catalog.pg_attrdef d ON d.adrelid=c.oid AND d.adnum=a.attnum WHERE a.attnum>0 AND NOT a.attisdropped AND c.relkind IN('r','p','v','m','f') AND left(n.nspname,3)<>'pg_' AND n.nspname<>'information_schema' AND pg_catalog.has_schema_privilege(n.oid,'USAGE') ORDER BY n.nspname,c.relname,a.attnum"#,
    },
    Query {
        sql: r#"SELECT json_build_object('schema',n.nspname,'kind','Index','name',ic.relname,'parent',c.relname,'parentKind',CASE WHEN c.relkind IN('v','m') THEN 'View' ELSE 'Table' END,'definition',left(pg_catalog.pg_get_indexdef(i.indexrelid),65536),'truncated',length(pg_catalog.pg_get_indexdef(i.indexrelid))>65536,'metadata',json_build_object('unique',i.indisunique,'indexType',am.amname,'columnNames',(SELECT json_agg(pg_catalog.pg_get_indexdef(i.indexrelid,k,true) ORDER BY k) FROM pg_catalog.generate_series(1,i.indnkeyatts) k)))::text FROM pg_catalog.pg_index i JOIN pg_catalog.pg_class c ON c.oid=i.indrelid JOIN pg_catalog.pg_class ic ON ic.oid=i.indexrelid JOIN pg_catalog.pg_am am ON am.oid=ic.relam JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace WHERE left(n.nspname,3)<>'pg_' AND n.nspname<>'information_schema' AND pg_catalog.has_schema_privilege(n.oid,'USAGE') ORDER BY n.nspname,c.relname,ic.relname"#,
    },
    Query {
        sql: r#"SELECT json_build_object('schema',n.nspname,'kind','Constraint','name',con.conname,'parent',c.relname,'definition',left(pg_catalog.pg_get_constraintdef(con.oid,true),65536),'truncated',length(pg_catalog.pg_get_constraintdef(con.oid,true))>65536,'reference',CASE WHEN con.contype='f' THEN json_build_object('schema',rn.nspname,'table',rc.relname) END,'metadata',json_build_object('constraintType',CASE con.contype WHEN 'p' THEN 'PrimaryKey' WHEN 'u' THEN 'Unique' WHEN 'f' THEN 'ForeignKey' WHEN 'c' THEN 'Check' WHEN 'x' THEN 'Exclusion' ELSE con.contype::text END,'columnNames',COALESCE((SELECT json_agg(a.attname ORDER BY k.ordinality) FROM pg_catalog.unnest(con.conkey) WITH ORDINALITY k(attnum,ordinality) JOIN pg_catalog.pg_attribute a ON a.attrelid=c.oid AND a.attnum=k.attnum),'[]'::json),'referencedTable',CASE WHEN con.contype='f' THEN rn.nspname||'.'||rc.relname END,'referencedColumns',COALESCE((SELECT json_agg(a.attname ORDER BY k.ordinality) FROM pg_catalog.unnest(con.confkey) WITH ORDINALITY k(attnum,ordinality) JOIN pg_catalog.pg_attribute a ON a.attrelid=rc.oid AND a.attnum=k.attnum),'[]'::json),'checkClause',CASE WHEN con.contype='c' THEN left(pg_catalog.pg_get_constraintdef(con.oid,true),65536) END))::text FROM pg_catalog.pg_constraint con JOIN pg_catalog.pg_class c ON c.oid=con.conrelid JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace LEFT JOIN pg_catalog.pg_class rc ON rc.oid=con.confrelid LEFT JOIN pg_catalog.pg_namespace rn ON rn.oid=rc.relnamespace WHERE left(n.nspname,3)<>'pg_' AND n.nspname<>'information_schema' AND pg_catalog.has_schema_privilege(n.oid,'USAGE') ORDER BY n.nspname,c.relname,con.conname"#,
    },
    Query {
        sql: r#"SELECT json_build_object('schema',r.routine_schema,'kind','Procedure','name',r.specific_name,'definition',left(r.routine_definition,65536),'truncated',COALESCE(length(r.routine_definition)>65536,false),'metadata',json_build_object('routineType',r.routine_type,'dataType',r.data_type,'parameters',COALESCE((SELECT json_agg(json_build_object('name',COALESCE(p.parameter_name,'parameter-'||p.ordinal_position::text),'ordinal',p.ordinal_position::text,'mode',p.parameter_mode,'dataType',COALESCE(p.udt_schema||'.'||p.udt_name,p.data_type),'nullable',true) ORDER BY p.ordinal_position) FROM information_schema.parameters p WHERE p.specific_catalog=r.specific_catalog AND p.specific_schema=r.specific_schema AND p.specific_name=r.specific_name),'[]'::json)))::text FROM information_schema.routines r WHERE left(r.routine_schema,3)<>'pg_' AND r.routine_schema<>'information_schema' ORDER BY r.routine_schema,r.specific_name"#,
    },
    Query {
        sql: r#"SELECT json_build_object('schema',n.nspname,'kind','Type','name',t.typname,'definition',CASE WHEN t.typtype='e' THEN (SELECT string_agg(e.enumlabel,', ' ORDER BY e.enumsortorder) FROM pg_catalog.pg_enum e WHERE e.enumtypid=t.oid) END,'metadata',json_build_object('dataType',pg_catalog.format_type(t.oid,NULL),'comment',pg_catalog.obj_description(t.oid,'pg_type'),'extra',CASE t.typtype WHEN 'e' THEN 'Enum' WHEN 'd' THEN 'Domain' WHEN 'c' THEN 'Composite' WHEN 'r' THEN 'Range' WHEN 'm' THEN 'Multirange' ELSE 'Base' END))::text FROM pg_catalog.pg_type t JOIN pg_catalog.pg_namespace n ON n.oid=t.typnamespace WHERE left(n.nspname,3)<>'pg_' AND n.nspname<>'information_schema' AND t.typelem=0 AND pg_catalog.has_schema_privilege(n.oid,'USAGE') ORDER BY n.nspname,t.typname"#,
    },
];
fn classify(error: tokio_postgres::Error) -> MetadataFailure {
    match error.code().map(|s| s.code()) {
        Some("28000" | "28P01" | "42501") => MetadataFailure::AccessDenied,
        _ => MetadataFailure::Unavailable,
    }
}
async fn session(
    c: Collector,
    target: &Target,
    database: &str,
    discovery: bool,
) -> Result<
    (
        Collector,
        Vec<String>,
        devworks_core::protocol::records::SchemaAdapterProfile,
    ),
    MetadataFailure,
> {
    let mut config = Config::new();
    config.host(&target.host).port(target.port).dbname(database).user(&target.username).password(c.request.password.as_ref().ok_or(MetadataFailure::CredentialsRequired)?.expose()).application_name("DevWorks metadata").connect_timeout(Duration::from_secs(10)).options("-c default_transaction_read_only=on -c statement_timeout=30000 -c lock_timeout=500 -c search_path=pg_catalog");
    config.ssl_mode(if matches!(target.tls, SchemaTlsMode::LoopbackPlaintext) {
        SslMode::Disable
    } else {
        SslMode::Require
    });
    let mut tls = native_tls::TlsConnector::builder();
    if matches!(target.tls, SchemaTlsMode::VerifyCa) {
        tls.danger_accept_invalid_hostnames(true);
    }
    if let Some(pem) = &c.request.ca_certificate {
        use rustls::pki_types::{CertificateDer, pem::PemObject};
        let mut count = 0;
        for root in CertificateDer::pem_slice_iter(pem) {
            let root = root.map_err(|_| MetadataFailure::InvalidTarget)?;
            tls.add_root_certificate(
                native_tls::Certificate::from_der(root.as_ref())
                    .map_err(|_| MetadataFailure::InvalidTarget)?,
            );
            count += 1;
        }
        if count == 0 {
            return Err(MetadataFailure::InvalidTarget);
        }
    }
    let tls = postgres_native_tls::MakeTlsConnector::new(
        tls.build()
            .map_err(|_| MetadataFailure::UnsupportedProfile)?,
    );
    let (client, connection) = config.connect(tls).await.map_err(classify)?;
    let work = read_catalog(c, target, database, discovery, &client);
    tokio::pin!(connection);
    tokio::select! {
        result=work => result,
        _=&mut connection => Err(MetadataFailure::Unavailable),
    }
}
async fn read_catalog(
    c: Collector,
    target: &Target,
    database: &str,
    discovery: bool,
    client: &Client,
) -> Result<
    (
        Collector,
        Vec<String>,
        devworks_core::protocol::records::SchemaAdapterProfile,
    ),
    MetadataFailure,
> {
    let version: String = client
        .query_one("SHOW server_version", &[])
        .await
        .map_err(classify)?
        .try_get(0)
        .map_err(|_| MetadataFailure::InvalidMetadata)?;
    if !version.starts_with("17.") {
        return Err(MetadataFailure::UnsupportedProfile);
    }
    let tls = match target.tls {
        SchemaTlsMode::VerifyFull => "VerifyFull; platform roots plus explicit CA",
        SchemaTlsMode::VerifyCa => "VerifyCa; platform roots plus explicit CA",
        SchemaTlsMode::LoopbackPlaintext => "LoopbackPlaintext",
    };
    let profile = profile(
        "tokio-postgres-0.7.18/native-tls",
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
            "Type",
        ],
        &["GraphEntity", "GraphEdge"],
        &[
            "Password authentication; explicit native client configuration, no PGPASS/PGOPTIONS/PGSSL environment.",
            "Metadata visibility follows source grants; read-only transactions and fixed catalog queries.",
        ],
    )?;
    if c.request.test_only {
        return Ok((c, Vec::new(), profile));
    }
    if discovery {
        let mut names = Vec::new();
        let selected = c
            .request
            .config
            .catalogs
            .0
            .iter()
            .map(|v| v.0.as_str())
            .collect::<Vec<_>>();
        let rows=client.query("SELECT datname FROM pg_catalog.pg_database WHERE datallowconn AND NOT datistemplate AND pg_catalog.has_database_privilege(oid,'CONNECT') AND (cardinality($1::text[])=0 OR datname=ANY($1::text[])) ORDER BY datname LIMIT 129",&[&selected]).await.map_err(classify)?;
        let mut c = c;
        for row in rows {
            c.check()?;
            let name: String = row
                .try_get(0)
                .map_err(|_| MetadataFailure::InvalidMetadata)?;
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
            names.push(name);
        }
        if c.request
            .config
            .catalogs
            .0
            .iter()
            .any(|v| !names.iter().any(|n| n == v.0.as_str()))
        {
            return Err(MetadataFailure::AccessDenied);
        }
        return Ok((c, names, profile));
    }
    client
        .batch_execute("BEGIN READ ONLY")
        .await
        .map_err(classify)?;
    let mut catalog = Catalog::new(c, database.into())?;
    for query in QUERIES {
        // Keep bounded structural identities/references when payload bytes exceed the wire
        // limit. Skipping a parent would strand later columns/indexes and fail publication.
        // PostgreSQL identifiers are bounded; unbounded payload lives in these two fields.
        let bounded = format!(
            "SELECT CASE WHEN octet_length(document)>131072 THEN ((document::jsonb - 'metadata' - 'definition') || jsonb_build_object('truncated',true))::text ELSE document END FROM ({}) metadata(document)",
            query.sql
        );
        let rows = client
            .query_raw(&bounded, std::iter::empty::<&str>())
            .await
            .map_err(classify)?;
        tokio::pin!(rows);
        while let Some(row) = rows.try_next().await.map_err(classify)? {
            let raw: Option<&str> = row
                .try_get(0)
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
    client.batch_execute("ROLLBACK").await.map_err(classify)?;
    Ok((catalog.collector, Vec::new(), profile))
}
pub(super) async fn read(
    c: Collector,
    target: Target,
) -> Result<MetadataSnapshot, MetadataFailure> {
    let database = if target.catalog.is_empty() {
        "postgres"
    } else {
        &target.catalog
    };
    let discovery = target.catalog.is_empty() && !c.request.test_only;
    let (mut c, names, mut profile) = session(c, &target, database, discovery).await?;
    if discovery {
        for name in names {
            let (next, _, p) = session(c, &target, &name, false).await?;
            c = next;
            profile = p;
        }
    }
    Ok(c.finish(profile))
}
