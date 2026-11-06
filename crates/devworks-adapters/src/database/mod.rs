//! Native target database metadata adapters, separate from internal application persistence.
pub mod network;
pub mod sqlite;

use devworks_core::ports::database::*;
use devworks_core::protocol::data::{Coverage, CoverageState};
use devworks_core::protocol::records::SchemaObjectMetadata;
use devworks_core::protocol::{Check, Id};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::collections::BTreeSet;

fn typed<T: DeserializeOwned + Check>(value: Value) -> Result<T, MetadataFailure> {
    let value: T = serde_json::from_value(value).map_err(|_| MetadataFailure::InvalidMetadata)?;
    value
        .check()
        .map_err(|_| MetadataFailure::InvalidMetadata)?;
    Ok(value)
}
fn coverage() -> Coverage {
    Coverage {
        state: CoverageState::Complete,
        gaps: Vec::new(),
        more_gaps: false,
    }
}
fn key(parts: &[&str]) -> String {
    format!(
        "db-{}",
        devworks_core::fingerprint::sha256_hex(
            serde_json::to_string(parts)
                .expect("string identity")
                .as_bytes()
        )
    )
}
fn metadata(
    name: &str,
    catalog: &str,
    schema: Option<&str>,
    extra: Value,
) -> Result<SchemaObjectMetadata, MetadataFailure> {
    let mut value = json!({"name":name,"catalog":catalog,"columnNames":[],"referencedColumns":[],"parameters":[]});
    if let Some(schema) = schema {
        value["schema"] = json!(schema);
    }
    let Value::Object(extra) = extra else {
        return Err(MetadataFailure::InvalidMetadata);
    };
    value.as_object_mut().unwrap().extend(extra);
    typed(value)
}
struct Collector {
    request: MetadataRequest,
    objects: Vec<MetadataObject>,
    relationships: Vec<MetadataRelationship>,
    bytes: usize,
    exhausted: bool,
    coverage: Coverage,
    include: regex::RegexSet,
    exclude: regex::RegexSet,
}
impl Collector {
    fn new(request: MetadataRequest) -> Result<Self, MetadataFailure> {
        let compile = |patterns: Vec<String>| {
            regex::RegexSetBuilder::new(patterns.iter().map(|p| {
                format!(
                    "^{}$",
                    regex::escape(p).replace("\\*", ".*").replace("\\?", ".")
                )
            }))
            .size_limit(8 * 1024 * 1024)
            .build()
            .map_err(|_| MetadataFailure::InvalidTarget)
        };
        let include = compile(
            request
                .config
                .include_patterns
                .0
                .iter()
                .map(|p| p.0.as_str().to_owned())
                .collect(),
        )?;
        let exclude = compile(
            request
                .config
                .exclude_patterns
                .0
                .iter()
                .map(|p| p.0.as_str().to_owned())
                .collect(),
        )?;
        Ok(Self {
            request,
            objects: Vec::new(),
            relationships: Vec::new(),
            bytes: 0,
            exhausted: false,
            coverage: coverage(),
            include,
            exclude,
        })
    }
    fn check(&self) -> Result<(), MetadataFailure> {
        if self.request.cancellation.is_cancelled() {
            Err(MetadataFailure::Cancelled)
        } else if std::time::Instant::now() >= self.request.limits.deadline {
            Err(MetadataFailure::Deadline)
        } else {
            Ok(())
        }
    }
    fn includes(&self, name: &str, qualified: &str) -> bool {
        (self.include.is_empty() || self.include.is_match(name) || self.include.is_match(qualified))
            && !(self.exclude.is_match(name) || self.exclude.is_match(qualified))
    }
    fn gap(&mut self, gap: &str) {
        self.coverage.state = CoverageState::Partial;
        let gap = Id::new(gap).expect("static gap identifier");
        if !self.coverage.gaps.contains(&gap) {
            if self.coverage.gaps.len() < 128 {
                self.coverage.gaps.push(gap);
            } else {
                self.coverage.more_gaps = true;
            }
        }
    }
    fn retain(&mut self, bytes: usize) -> bool {
        if self
            .bytes
            .checked_add(bytes)
            .is_none_or(|total| total > self.request.limits.max_bytes)
        {
            self.gap("metadata_byte_limit");
            self.exhausted = true;
            false
        } else {
            self.bytes += bytes;
            true
        }
    }
    fn object(&mut self, object: MetadataObject) -> Result<bool, MetadataFailure> {
        self.check()?;
        if self.exhausted {
            return Ok(false);
        }
        if self.objects.len() >= self.request.limits.max_objects {
            self.gap("metadata_object_limit");
            self.exhausted = true;
            return Ok(false);
        }
        let bytes = std::mem::size_of::<MetadataObject>()
            + object.key.len()
            + object.qualified_name.len()
            + object.parent_key.as_ref().map_or(0, String::len)
            + serde_json::to_vec(&object.metadata)
                .map_err(|_| MetadataFailure::InvalidMetadata)?
                .len();
        if !self.retain(bytes) {
            return Ok(false);
        }
        self.objects.push(object);
        Ok(true)
    }
    fn relationship(&mut self, from: &str, to: &str, kind: &str) -> Result<bool, MetadataFailure> {
        self.check()?;
        if self.exhausted {
            return Ok(false);
        }
        if self.relationships.len() >= self.request.limits.max_relationships {
            self.gap("metadata_relationship_limit");
            return Ok(false);
        }
        let row = MetadataRelationship {
            key: key(&[from, to, kind]),
            from_key: from.into(),
            to_key: to.into(),
            kind: kind.into(),
            inferred: false,
        };
        if !self.retain(
            std::mem::size_of::<MetadataRelationship>()
                + row.key.len()
                + from.len()
                + to.len()
                + kind.len(),
        ) {
            return Ok(false);
        }
        self.relationships.push(row);
        Ok(true)
    }
    fn finish(
        mut self,
        profile: devworks_core::protocol::records::SchemaAdapterProfile,
    ) -> MetadataSnapshot {
        let keys: BTreeSet<_> = self.objects.iter().map(|o| o.key.as_str()).collect();
        self.relationships
            .retain(|r| keys.contains(r.from_key.as_str()) && keys.contains(r.to_key.as_str()));
        self.objects.sort_by(|a, b| a.key.cmp(&b.key));
        self.relationships.sort_by(|a, b| a.key.cmp(&b.key));
        self.relationships.dedup_by(|a, b| a.key == b.key);
        MetadataSnapshot {
            profile,
            coverage: self.coverage,
            objects: self.objects,
            relationships: self.relationships,
        }
    }
}

/// Composition-root adapter: one core port for the file and native wire profiles.
#[derive(Default)]
pub struct NativeMetadataReader;
impl MetadataReader for NativeMetadataReader {
    fn read(&self, request: MetadataRequest) -> MetadataFuture {
        match request.config.engine {
            devworks_core::protocol::records::DbEngine::Sqlite => {
                sqlite::SqliteMetadataReader.read(request)
            }
            _ => network::NetworkMetadataReader.read(request),
        }
    }
}
