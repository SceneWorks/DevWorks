//! SQLite package registry records (migration 15): the registry head, package records,
//! verified versions, approval proposals and retained state generations behind
//! [`PackageRegistryRead`] and [`PackageRegistry`].
use super::conn::db;
use super::repos::{Snapshot, Unit};
use devworks_core::ports::storage::*;
use devworks_core::protocol::{ActorId, Id, Revision, RevisionDimension};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde_json::Value;

fn invalid(m: impl Into<String>) -> StorageError {
    StorageError::InvalidRecord(m.into())
}
fn corrupt(m: impl Into<String>) -> StorageError {
    StorageError::unavailable(Unavailable::Corrupt, m)
}
fn json_of(s: &str) -> Result<Value, StorageError> {
    serde_json::from_str(s).map_err(|e| corrupt(format!("stored package JSON: {e}")))
}
fn opt_json(s: Option<String>) -> Result<Option<Value>, StorageError> {
    s.as_deref().map(json_of).transpose()
}
fn text(v: &Value) -> String {
    v.to_string()
}
fn object(field: &str, v: &Value) -> Result<(), StorageError> {
    if v.is_object() {
        Ok(())
    } else {
        Err(invalid(format!("{field} must be a JSON object")))
    }
}
fn digest(field: &str, d: &str) -> Result<(), StorageError> {
    if devworks_core::fingerprint::is_sha256_hex(d) {
        Ok(())
    } else {
        Err(invalid(format!("{field} must be a SHA-256 digest")))
    }
}
fn package_id(p: &str) -> Result<(), StorageError> {
    if Id::new(p).is_err() {
        return Err(invalid("package id"));
    }
    Ok(())
}
fn rev(v: i64) -> Revision {
    Revision::new(v.max(1)).expect("positive revision")
}

const PACKAGE: &str = "package_id,revision,enabled,installed,active_digest,predecessor_digest,pending_digest,state_generation,config_json,config_revision,trust_json,failure_json,created_at,updated_at";
type PackageRaw = (
    String,
    i64,
    i64,
    i64,
    Option<String>,
    Option<String>,
    Option<String>,
    i64,
    Option<String>,
    i64,
    Option<String>,
    Option<String>,
    i64,
    i64,
);
fn package_row(r: &Row<'_>) -> rusqlite::Result<PackageRaw> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(6)?,
        r.get(7)?,
        r.get(8)?,
        r.get(9)?,
        r.get(10)?,
        r.get(11)?,
        r.get(12)?,
        r.get(13)?,
    ))
}
fn package_of(r: PackageRaw) -> Result<StoredPackage, StorageError> {
    Ok(StoredPackage {
        package_id: r.0,
        revision: r.1,
        enabled: r.2 == 1,
        installed: r.3 == 1,
        active_digest: r.4,
        predecessor_digest: r.5,
        pending_digest: r.6,
        state_generation: r.7,
        config: opt_json(r.8)?,
        config_revision: r.9,
        trust: opt_json(r.10)?,
        failure: opt_json(r.11)?,
        created_at_ms: r.12,
        updated_at_ms: r.13,
    })
}
fn package(conn: &Connection, id: &str) -> Result<Option<StoredPackage>, StorageError> {
    db(conn
        .prepare_cached(&format!(
            "SELECT {PACKAGE} FROM packages WHERE package_id=?1"
        ))
        .and_then(|mut s| s.query_row([id], package_row).optional()))?
    .map(package_of)
    .transpose()
}
fn packages(
    conn: &Connection,
    after: Option<&str>,
    limit: u16,
) -> Result<Vec<StoredPackage>, StorageError> {
    let limit = i64::from(limit.clamp(1, 500));
    let rows: Vec<PackageRaw> = db(conn
        .prepare_cached(&format!(
            "SELECT {PACKAGE} FROM packages WHERE package_id > ?1 ORDER BY package_id LIMIT ?2"
        ))
        .and_then(|mut s| {
            s.query_map(params![after.unwrap_or(""), limit], package_row)?
                .collect()
        }))?;
    rows.into_iter().map(package_of).collect()
}

const VERSION: &str = "package_id,manifest_digest,bundle_digest,version,publisher_id,signer_key,inspection_json,provenance_json,phase,waits_json,approval_json,created_at,updated_at";
type VersionRaw = (
    String,
    String,
    String,
    String,
    String,
    Option<String>,
    String,
    String,
    String,
    String,
    Option<String>,
    i64,
    i64,
);
fn version_row(r: &Row<'_>) -> rusqlite::Result<VersionRaw> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(6)?,
        r.get(7)?,
        r.get(8)?,
        r.get(9)?,
        r.get(10)?,
        r.get(11)?,
        r.get(12)?,
    ))
}
fn version_of(r: VersionRaw) -> Result<StoredPackageVersion, StorageError> {
    let waits: Vec<String> = serde_json::from_str(&r.9).map_err(|_| corrupt("stored waits"))?;
    Ok(StoredPackageVersion {
        package_id: r.0,
        manifest_digest: r.1,
        bundle_digest: r.2,
        version: r.3,
        publisher_id: r.4,
        signer_key: r.5,
        inspection: json_of(&r.6)?,
        provenance: json_of(&r.7)?,
        phase: VersionPhase::parse(&r.8).ok_or_else(|| corrupt("stored version phase"))?,
        waits,
        approval: opt_json(r.10)?,
        created_at_ms: r.11,
        updated_at_ms: r.12,
    })
}
fn version(
    conn: &Connection,
    pkg: &str,
    d: &str,
) -> Result<Option<StoredPackageVersion>, StorageError> {
    db(conn
        .prepare_cached(&format!(
            "SELECT {VERSION} FROM package_versions WHERE package_id=?1 AND manifest_digest=?2"
        ))
        .and_then(|mut s| s.query_row([pkg, d], version_row).optional()))?
    .map(version_of)
    .transpose()
}
fn versions(conn: &Connection, pkg: &str) -> Result<Vec<StoredPackageVersion>, StorageError> {
    let rows: Vec<VersionRaw> = db(conn
        .prepare_cached(&format!(
            "SELECT * FROM (SELECT {VERSION} FROM package_versions WHERE package_id=?1 ORDER BY created_at DESC, manifest_digest DESC LIMIT 512) ORDER BY created_at, manifest_digest"
        ))
        .and_then(|mut s| s.query_map([pkg], version_row)?.collect()))?;
    rows.into_iter().map(version_of).collect()
}

const PROPOSAL: &str = "id,revision,package_id,manifest_digest,bundle_digest,version,state,base_package_id,originator,origin_json,summary_json,decided_by,decided_at,decision_json,created_at";
type ProposalRaw = (
    String,
    i64,
    String,
    String,
    String,
    String,
    String,
    Option<String>,
    String,
    String,
    String,
    Option<String>,
    Option<i64>,
    Option<String>,
    i64,
);
fn proposal_row(r: &Row<'_>) -> rusqlite::Result<ProposalRaw> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(6)?,
        r.get(7)?,
        r.get(8)?,
        r.get(9)?,
        r.get(10)?,
        r.get(11)?,
        r.get(12)?,
        r.get(13)?,
        r.get(14)?,
    ))
}
fn actor(s: String) -> Result<ActorId, StorageError> {
    Id::new(s).map(ActorId).map_err(|_| corrupt("stored actor"))
}
fn proposal_of(r: ProposalRaw) -> Result<StoredPackageProposal, StorageError> {
    Ok(StoredPackageProposal {
        id: Id::new(r.0).map_err(|_| corrupt("stored proposal id"))?,
        revision: r.1,
        package_id: r.2,
        manifest_digest: r.3,
        bundle_digest: r.4,
        version: r.5,
        state: r.6,
        base_package_id: r.7,
        originator: actor(r.8)?,
        origin: json_of(&r.9)?,
        summary: json_of(&r.10)?,
        decided_by: r.11.map(actor).transpose()?,
        decided_at_ms: r.12,
        decision: opt_json(r.13)?,
        created_at_ms: r.14,
    })
}
fn proposal(conn: &Connection, id: &str) -> Result<Option<StoredPackageProposal>, StorageError> {
    db(conn
        .prepare_cached(&format!(
            "SELECT {PROPOSAL} FROM package_proposals WHERE id=?1"
        ))
        .and_then(|mut s| s.query_row([id], proposal_row).optional()))?
    .map(proposal_of)
    .transpose()
}
fn proposal_for(
    conn: &Connection,
    pkg: &str,
    d: &str,
) -> Result<Option<StoredPackageProposal>, StorageError> {
    db(conn
        .prepare_cached(&format!(
            "SELECT {PROPOSAL} FROM package_proposals WHERE package_id=?1 AND manifest_digest=?2 ORDER BY created_at DESC, rowid DESC LIMIT 1"
        ))
        .and_then(|mut s| s.query_row([pkg, d], proposal_row).optional()))?
    .map(proposal_of)
    .transpose()
}

fn generation(
    conn: &Connection,
    pkg: &str,
    g: i64,
) -> Result<Option<StoredStateGeneration>, StorageError> {
    db(conn
        .prepare_cached(
            "SELECT package_id,generation,manifest_digest,schema_digest,seeded_from,seed_revision,created_at FROM package_state_generations WHERE package_id=?1 AND generation=?2",
        )
        .and_then(|mut s| {
            s.query_row(params![pkg, g], |r| {
                Ok(StoredStateGeneration {
                    package_id: r.get(0)?,
                    generation: r.get(1)?,
                    manifest_digest: r.get(2)?,
                    schema_digest: r.get(3)?,
                    seeded_from: r.get(4)?,
                    seed_revision: r.get(5)?,
                    created_at_ms: r.get(6)?,
                })
            })
            .optional()
        }))
}
fn generation_state(
    conn: &Connection,
    pkg: &str,
    g: i64,
) -> Result<Option<(i64, Value)>, StorageError> {
    let live: Option<(i64, i64, String)> = db(conn
        .prepare_cached(
            "SELECT generation,revision,state_json FROM package_handler_state WHERE package_id=?1",
        )
        .and_then(|mut s| {
            s.query_row([pkg], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .optional()
        }))?;
    if let Some((generation, revision, state)) = live
        && generation == g
    {
        return Ok(Some((revision, json_of(&state)?)));
    }
    let retained: Option<(Option<i64>, Option<String>)> = db(conn
        .prepare_cached(
            "SELECT retained_revision,retained_state_json FROM package_state_generations WHERE package_id=?1 AND generation=?2",
        )
        .and_then(|mut s| {
            s.query_row(params![pkg, g], |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()
        }))?;
    match retained {
        Some((Some(r), Some(s))) => Ok(Some((r, json_of(&s)?))),
        _ => Ok(None),
    }
}
fn indexes(conn: &Connection) -> Result<Vec<StoredIndex>, StorageError> {
    type Raw = (
        String,
        String,
        String,
        String,
        Option<String>,
        Option<String>,
        i64,
    );
    let rows: Vec<Raw> = db(conn
        .prepare_cached(
            "SELECT source_key,location,public_key,status,index_json,error,fetched_at FROM package_indexes ORDER BY source_key LIMIT 64",
        )
        .and_then(|mut s| {
            s.query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?))
            })?
            .collect()
        }))?;
    rows.into_iter()
        .map(|r| {
            Ok(StoredIndex {
                source_key: r.0,
                location: r.1,
                public_key: r.2,
                status: r.3,
                index: opt_json(r.4)?,
                error: r.5,
                fetched_at_ms: r.6,
            })
        })
        .collect()
}
fn observation(conn: &Connection, pkg: &str) -> Result<Option<StoredObservation>, StorageError> {
    db(conn
        .prepare_cached(
            "SELECT package_id,availability,version,digest,revision FROM package_observations WHERE package_id=?1",
        )
        .and_then(|mut s| {
            s.query_row([pkg], |r| {
                Ok(StoredObservation {
                    package_id: r.get(0)?,
                    availability: r.get(1)?,
                    version: r.get(2)?,
                    digest: r.get(3)?,
                    revision: r.get(4)?,
                })
            })
            .optional()
        }))
}
fn registry_revision(conn: &Connection) -> Result<i64, StorageError> {
    db(conn
        .prepare_cached("SELECT revision FROM package_registry WHERE singleton=1")
        .and_then(|mut s| s.query_row([], |r| r.get(0))))
}

macro_rules! registry_reads {
    ($t:ty) => {
        impl PackageRegistryRead for $t {
            fn registry_revision(&self) -> Result<i64, StorageError> {
                registry_revision(self.conn)
            }
            fn package(&self, package_id: &str) -> Result<Option<StoredPackage>, StorageError> {
                package(self.conn, package_id)
            }
            fn packages(
                &self,
                after: Option<&str>,
                limit: u16,
            ) -> Result<Vec<StoredPackage>, StorageError> {
                packages(self.conn, after, limit)
            }
            fn package_version(
                &self,
                package_id: &str,
                manifest_digest: &str,
            ) -> Result<Option<StoredPackageVersion>, StorageError> {
                version(self.conn, package_id, manifest_digest)
            }
            fn package_versions(
                &self,
                package_id: &str,
            ) -> Result<Vec<StoredPackageVersion>, StorageError> {
                versions(self.conn, package_id)
            }
            fn proposal(&self, id: &Id) -> Result<Option<StoredPackageProposal>, StorageError> {
                proposal(self.conn, id.as_str())
            }
            fn proposal_for(
                &self,
                package_id: &str,
                manifest_digest: &str,
            ) -> Result<Option<StoredPackageProposal>, StorageError> {
                proposal_for(self.conn, package_id, manifest_digest)
            }
            fn state_generation(
                &self,
                package_id: &str,
                generation_no: i64,
            ) -> Result<Option<StoredStateGeneration>, StorageError> {
                generation(self.conn, package_id, generation_no)
            }
            fn generation_state(
                &self,
                package_id: &str,
                generation_no: i64,
            ) -> Result<Option<(i64, Value)>, StorageError> {
                generation_state(self.conn, package_id, generation_no)
            }
            fn indexes(&self) -> Result<Vec<StoredIndex>, StorageError> {
                indexes(self.conn)
            }
            fn observation(
                &self,
                package_id: &str,
            ) -> Result<Option<StoredObservation>, StorageError> {
                observation(self.conn, package_id)
            }
        }
    };
}
registry_reads!(Snapshot<'_>);
registry_reads!(Unit<'_>);

fn check_package(p: &StoredPackage) -> Result<(), StorageError> {
    package_id(&p.package_id)?;
    for (f, d) in [
        ("active digest", &p.active_digest),
        ("predecessor digest", &p.predecessor_digest),
        ("pending digest", &p.pending_digest),
    ] {
        if let Some(d) = d {
            digest(f, d)?;
        }
    }
    if p.state_generation < 0 || p.config_revision < 0 {
        return Err(invalid("package counters"));
    }
    if !p.installed && (p.enabled || p.active_digest.is_some()) {
        return Err(invalid(
            "an uninstalled package is neither enabled nor active",
        ));
    }
    for (f, v) in [
        ("config", &p.config),
        ("trust", &p.trust),
        ("failure", &p.failure),
    ] {
        if let Some(v) = v {
            object(f, v)?;
        }
    }
    Ok(())
}

impl PackageRegistry for Unit<'_> {
    fn advance_registry(&mut self, expected: i64) -> Result<i64, StorageError> {
        let changed = db(self.conn.execute(
            "UPDATE package_registry SET revision=revision+1 WHERE singleton=1 AND revision=?1",
            [expected],
        ))?;
        let current = registry_revision(self.conn)?;
        if changed != 1 {
            return Err(StorageError::conflict(
                RevisionDimension::Registry,
                "package_registry",
                Id::new("registry").expect("valid id"),
                rev(expected),
                rev(current),
            ));
        }
        Ok(current)
    }

    fn insert_package(&mut self, p: &StoredPackage) -> Result<StoredPackage, StorageError> {
        check_package(p)?;
        if package(self.conn, &p.package_id)?.is_some() {
            return Err(StorageError::Duplicate {
                kind: "package",
                id: p.package_id.clone(),
            });
        }
        db(self.conn.execute(
            &format!("INSERT INTO packages({PACKAGE}) VALUES(?1,1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)"),
            params![
                p.package_id,
                i64::from(p.enabled),
                i64::from(p.installed),
                p.active_digest,
                p.predecessor_digest,
                p.pending_digest,
                p.state_generation,
                p.config.as_ref().map(text),
                p.config_revision,
                p.trust.as_ref().map(text),
                p.failure.as_ref().map(text),
                p.created_at_ms,
                p.updated_at_ms.max(p.created_at_ms),
            ],
        ))?;
        package(self.conn, &p.package_id)?.ok_or_else(|| corrupt("package vanished"))
    }

    fn update_package(
        &mut self,
        p: &StoredPackage,
        expected_revision: i64,
    ) -> Result<StoredPackage, StorageError> {
        check_package(p)?;
        let changed = db(self.conn.execute(
            "UPDATE packages SET revision=revision+1,enabled=?3,installed=?4,active_digest=?5,predecessor_digest=?6,pending_digest=?7,state_generation=?8,config_json=?9,config_revision=?10,trust_json=?11,failure_json=?12,updated_at=max(created_at,?13) WHERE package_id=?1 AND revision=?2",
            params![
                p.package_id,
                expected_revision,
                i64::from(p.enabled),
                i64::from(p.installed),
                p.active_digest,
                p.predecessor_digest,
                p.pending_digest,
                p.state_generation,
                p.config.as_ref().map(text),
                p.config_revision,
                p.trust.as_ref().map(text),
                p.failure.as_ref().map(text),
                p.updated_at_ms,
            ],
        ))?;
        let current = package(self.conn, &p.package_id)?;
        if changed != 1 {
            return match current {
                None => Err(StorageError::NotFound {
                    kind: "package",
                    id: p.package_id.clone(),
                }),
                Some(c) => Err(StorageError::conflict(
                    RevisionDimension::Package,
                    "package",
                    Id::new(&p.package_id).map_err(|_| invalid("package id"))?,
                    rev(expected_revision),
                    rev(c.revision),
                )),
            };
        }
        current.ok_or_else(|| corrupt("package vanished"))
    }

    fn insert_version(&mut self, v: &StoredPackageVersion) -> Result<(), StorageError> {
        package_id(&v.package_id)?;
        digest("manifest digest", &v.manifest_digest)?;
        digest("bundle digest", &v.bundle_digest)?;
        if let Some(k) = &v.signer_key {
            digest("signer key", k)?;
        }
        object("inspection", &v.inspection)?;
        object("provenance", &v.provenance)?;
        if let Some(a) = &v.approval {
            object("approval", a)?;
        }
        if version(self.conn, &v.package_id, &v.manifest_digest)?.is_some() {
            return Err(StorageError::Duplicate {
                kind: "package_version",
                id: format!("{}/{}", v.package_id, v.manifest_digest),
            });
        }
        db(self.conn.execute(
            &format!("INSERT INTO package_versions({VERSION}) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)"),
            params![
                v.package_id,
                v.manifest_digest,
                v.bundle_digest,
                v.version,
                v.publisher_id,
                v.signer_key,
                text(&v.inspection),
                text(&v.provenance),
                v.phase.as_str(),
                serde_json::to_string(&v.waits).map_err(|e| invalid(e.to_string()))?,
                v.approval.as_ref().map(text),
                v.created_at_ms,
                v.updated_at_ms.max(v.created_at_ms),
            ],
        ))?;
        Ok(())
    }

    fn update_version(
        &mut self,
        pkg: &str,
        manifest_digest: &str,
        phase: VersionPhase,
        waits: &[String],
        approval: Option<&Value>,
    ) -> Result<(), StorageError> {
        if let Some(a) = approval {
            object("approval", a)?;
        }
        let changed = db(self.conn.execute(
            "UPDATE package_versions SET phase=?3,waits_json=?4,approval_json=?5,updated_at=max(created_at,?6) WHERE package_id=?1 AND manifest_digest=?2",
            params![
                pkg,
                manifest_digest,
                phase.as_str(),
                serde_json::to_string(waits).map_err(|e| invalid(e.to_string()))?,
                approval.map(text),
                self.now,
            ],
        ))?;
        if changed != 1 {
            return Err(StorageError::NotFound {
                kind: "package_version",
                id: format!("{pkg}/{manifest_digest}"),
            });
        }
        Ok(())
    }

    fn insert_proposal(&mut self, p: &StoredPackageProposal) -> Result<(), StorageError> {
        object("origin", &p.origin)?;
        object("summary", &p.summary)?;
        if proposal(self.conn, p.id.as_str())?.is_some() {
            return Err(StorageError::Duplicate {
                kind: "package_proposal",
                id: p.id.as_str().to_owned(),
            });
        }
        db(self.conn.execute(
            &format!("INSERT INTO package_proposals({PROPOSAL}) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)"),
            params![
                p.id.as_str(),
                p.revision.max(1),
                p.package_id,
                p.manifest_digest,
                p.bundle_digest,
                p.version,
                p.state,
                p.base_package_id,
                p.originator.0.as_str(),
                text(&p.origin),
                text(&p.summary),
                p.decided_by.as_ref().map(|a| a.0.as_str().to_owned()),
                p.decided_at_ms,
                p.decision.as_ref().map(text),
                p.created_at_ms,
            ],
        ))?;
        Ok(())
    }

    fn update_proposal(
        &mut self,
        p: &StoredPackageProposal,
        expected_revision: i64,
    ) -> Result<StoredPackageProposal, StorageError> {
        if let Some(d) = &p.decision {
            object("decision", d)?;
        }
        let changed = db(self.conn.execute(
            "UPDATE package_proposals SET revision=revision+1,state=?3,decided_by=?4,decided_at=?5,decision_json=?6 WHERE id=?1 AND revision=?2",
            params![
                p.id.as_str(),
                expected_revision,
                p.state,
                p.decided_by.as_ref().map(|a| a.0.as_str().to_owned()),
                p.decided_at_ms,
                p.decision.as_ref().map(text),
            ],
        ))?;
        let current = proposal(self.conn, p.id.as_str())?;
        if changed != 1 {
            return match current {
                None => Err(StorageError::NotFound {
                    kind: "package_proposal",
                    id: p.id.as_str().to_owned(),
                }),
                Some(c) => Err(StorageError::conflict(
                    RevisionDimension::Package,
                    "package_proposal",
                    p.id.clone(),
                    rev(expected_revision),
                    rev(c.revision),
                )),
            };
        }
        current.ok_or_else(|| corrupt("proposal vanished"))
    }

    fn publish_generation(
        &mut self,
        g: &StoredStateGeneration,
        state: Option<&Value>,
        revision: i64,
    ) -> Result<(), StorageError> {
        package_id(&g.package_id)?;
        digest("schema digest", &g.schema_digest)?;
        if revision < 0 {
            return Err(invalid("state revision"));
        }
        let highest: i64 = db(self.conn.query_row(
            "SELECT coalesce(max(generation),0) FROM package_state_generations WHERE package_id=?1",
            [&g.package_id],
            |r| r.get(0),
        ))?;
        if g.generation <= highest {
            return Err(StorageError::LeaseLost {
                kind: "package_state_generation",
                id: g.package_id.clone(),
            });
        }
        // Retain the retiring generation's final live state.
        db(self.conn.execute(
            "UPDATE package_state_generations SET retained_revision=(SELECT revision FROM package_handler_state h WHERE h.package_id=?1),retained_state_json=(SELECT state_json FROM package_handler_state h WHERE h.package_id=?1) WHERE package_id=?1 AND generation=(SELECT generation FROM package_handler_state h WHERE h.package_id=?1)",
            [&g.package_id],
        ))?;
        db(self.conn.execute(
            "INSERT INTO package_state_generations(package_id,generation,manifest_digest,schema_digest,seeded_from,seed_revision,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![
                g.package_id,
                g.generation,
                g.manifest_digest,
                g.schema_digest,
                g.seeded_from,
                g.seed_revision,
                g.created_at_ms,
            ],
        ))?;
        // Only the new generation and the one it was seeded from can ever be read again (a
        // rollback carries the current state forward or restores exactly its seed): older
        // retained generations are dropped, so activations never accumulate them.
        db(self.conn.execute(
            "DELETE FROM package_state_generations WHERE package_id=?1 AND generation<>?2 AND generation IS NOT ?3",
            params![g.package_id, g.generation, g.seeded_from],
        ))?;
        let state = state.cloned().unwrap_or(Value::Null);
        // (The live handler state of the new generation.)
        db(self.conn.execute(
            "INSERT INTO package_handler_state(package_id,generation,revision,state_json) VALUES(?1,?2,?3,?4) ON CONFLICT(package_id) DO UPDATE SET generation=excluded.generation,revision=excluded.revision,state_json=excluded.state_json",
            params![g.package_id, g.generation, revision, text(&state)],
        ))?;
        Ok(())
    }

    fn put_index(&mut self, x: &StoredIndex) -> Result<(), StorageError> {
        digest("index source key", &x.source_key)?;
        digest("index public key", &x.public_key)?;
        if !matches!(x.status.as_str(), "Verified" | "Failed") {
            return Err(invalid("index status"));
        }
        if let Some(i) = &x.index {
            object("index", i)?;
        }
        db(self.conn.execute(
            "INSERT INTO package_indexes(source_key,location,public_key,status,index_json,error,fetched_at) VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(source_key) DO UPDATE SET location=excluded.location,public_key=excluded.public_key,status=excluded.status,index_json=excluded.index_json,error=excluded.error,fetched_at=excluded.fetched_at",
            params![
                x.source_key,
                x.location,
                x.public_key,
                x.status,
                x.index.as_ref().map(text),
                x.error.as_deref().map(|e| e.chars().take(4096).collect::<String>()),
                x.fetched_at_ms,
            ],
        ))?;
        Ok(())
    }

    fn retain_indexes(&mut self, keys: &[String]) -> Result<(), StorageError> {
        for existing in indexes(self.conn)? {
            if !keys.contains(&existing.source_key) {
                db(self.conn.execute(
                    "DELETE FROM package_indexes WHERE source_key=?1",
                    [&existing.source_key],
                ))?;
            }
        }
        Ok(())
    }

    fn put_observation(&mut self, o: &StoredObservation) -> Result<(), StorageError> {
        package_id(&o.package_id)?;
        if let Some(d) = &o.digest {
            digest("observed digest", d)?;
        }
        db(self.conn.execute(
            "INSERT INTO package_observations(package_id,availability,version,digest,revision) VALUES(?1,?2,?3,?4,1) ON CONFLICT(package_id) DO UPDATE SET availability=excluded.availability,version=excluded.version,digest=excluded.digest,revision=revision+1",
            params![o.package_id, o.availability, o.version, o.digest],
        ))?;
        Ok(())
    }
}
