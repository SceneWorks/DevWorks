//! Explicit native wire clients; no connection strings, ambient password files or caller SQL.
use super::*;
use devworks_core::protocol::records::{DbEngine, SchemaAuthMode, SchemaTlsMode};
use std::net::IpAddr;
use std::time::Duration;

mod catalog;
mod mysql;
mod postgres;
mod sql_server;

#[derive(Default)]
pub struct NetworkMetadataReader;
impl MetadataReader for NetworkMetadataReader {
    fn read(&self, request: MetadataRequest) -> MetadataFuture {
        Box::pin(async move {
            let target = Target::parse(&request)?;
            let token = request.cancellation.clone();
            let deadline = request.limits.deadline;
            let cancelled = async {
                while !token.is_cancelled() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            };
            // The operation owns each socket and drives PostgreSQL's connection inline. Losing
            // this select drops the physical wire owner, never a detached database callback.
            tokio::select! {
                biased;
                _ = cancelled => Err(MetadataFailure::Cancelled),
                _ = tokio::time::sleep_until(deadline.into()) => Err(MetadataFailure::Deadline),
                result = async {
                    let c = Collector::new(request)?;
                    c.check()?;
                    match c.request.config.engine {
                        DbEngine::MariaDb | DbEngine::MySql => mysql::read(c, target).await,
                        DbEngine::Postgres => postgres::read(c, target).await,
                        DbEngine::SqlServer => sql_server::read(c, target).await,
                        DbEngine::Sqlite => Err(MetadataFailure::UnsupportedProfile),
                    }
                } => result,
            }
        })
    }
}
struct Target {
    host: String,
    port: u16,
    catalog: String,
    username: String,
    tls: SchemaTlsMode,
}
impl Target {
    fn parse(request: &MetadataRequest) -> Result<Self, MetadataFailure> {
        let scheme = match request.config.engine {
            DbEngine::MariaDb => "mariadb",
            DbEngine::MySql => "mysql",
            DbEngine::Postgres => "postgres",
            DbEngine::SqlServer => "sqlserver",
            DbEngine::Sqlite => return Err(MetadataFailure::UnsupportedProfile),
        };
        let url = url::Url::parse(request.config.endpoint.0.as_str())
            .map_err(|_| MetadataFailure::InvalidTarget)?;
        if url.scheme() != scheme
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(MetadataFailure::InvalidTarget);
        }
        let host = url
            .host_str()
            .ok_or(MetadataFailure::InvalidTarget)?
            .trim_matches(['[', ']'])
            .to_owned();
        let port = url.port().unwrap_or(match request.config.engine {
            DbEngine::MariaDb | DbEngine::MySql => 3306,
            DbEngine::Postgres => 5432,
            DbEngine::SqlServer => 1433,
            DbEngine::Sqlite => unreachable!(),
        });
        // Decode the catalog once and pass it only as a native database-name field, never
        // as SQL or another connection string. Encoded legal names retain their identity.
        let catalog = percent_encoding::percent_decode_str(
            url.path().strip_prefix('/').unwrap_or(url.path()),
        )
        .decode_utf8()
        .map_err(|_| MetadataFailure::InvalidTarget)?
        .into_owned();
        if host.is_empty()
            || host.contains([',', '/', '\\'])
            || port == 0
            || catalog.chars().any(char::is_control)
            || catalog.chars().count() > 256
        {
            return Err(MetadataFailure::InvalidTarget);
        }
        if !catalog.is_empty()
            && !request.config.catalogs.0.is_empty()
            && !request
                .config
                .catalogs
                .0
                .iter()
                .any(|v| v.0.as_str() == catalog)
        {
            return Err(MetadataFailure::InvalidTarget);
        }
        if !matches!(
            request.config.authentication,
            Some(SchemaAuthMode::Password)
        ) {
            return Err(MetadataFailure::UnsupportedProfile);
        }
        let username = request
            .config
            .username
            .as_ref()
            .ok_or(MetadataFailure::CredentialsRequired)?
            .0
            .as_str()
            .to_owned();
        if username.is_empty() || request.password.as_ref().is_none_or(|p| p.is_empty()) {
            return Err(MetadataFailure::CredentialsRequired);
        }
        let tls = request
            .config
            .tls
            .clone()
            .unwrap_or(SchemaTlsMode::VerifyFull);
        if matches!(tls, SchemaTlsMode::LoopbackPlaintext)
            && !host
                .parse::<IpAddr>()
                .is_ok_and(|address| address.is_loopback())
        {
            return Err(MetadataFailure::InvalidTarget);
        }
        if matches!(tls, SchemaTlsMode::LoopbackPlaintext) && request.ca_certificate.is_some() {
            return Err(MetadataFailure::UnsupportedProfile);
        }
        Ok(Self {
            host,
            port,
            catalog,
            username,
            tls,
        })
    }
}
