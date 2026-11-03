//! SQLite host authority ledger: `principal_authority` and `authority_grants` (migration 5), and
//! the protected-root read over the registered `repositories`.
use super::conn::db;
use super::repos::{Snapshot, Unit, actor_kind, parse_principal};
use devworks_core::ports::GrantUse;
use devworks_core::ports::storage::*;
use devworks_core::protocol::{ActorId, Id, ProjectId, Revision, RevisionDimension};
use devworks_core::settings::catalog::AgentRole;
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde_json::Value;

fn invalid(m: impl Into<String>) -> StorageError {
    StorageError::InvalidRecord(m.into())
}
fn corrupt(m: impl Into<String>) -> StorageError {
    StorageError::unavailable(Unavailable::Corrupt, m)
}
fn id_of(s: String) -> Result<Id, StorageError> {
    Id::new(s).map_err(|e| corrupt(format!("stored identifier: {e}")))
}
fn conflict(kind: &'static str, id: &str, expected: i64, current: i64) -> StorageError {
    let rev = |n: i64| Revision::new(n.max(1)).expect("positive");
    StorageError::conflict(
        RevisionDimension::Authority,
        kind,
        Id::new(id).unwrap_or_else(|_| Id::new("invalid").expect("static")),
        rev(expected),
        rev(current),
    )
}

const PRINCIPAL_COLUMNS: &str = "p.actor_id,a.kind,p.role,p.project_id,p.tool_families_json,p.state,p.revision,p.bound_by,p.updated_at";
type PrincipalRaw = (
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    String,
    i64,
    String,
    i64,
);
fn principal_row(r: &Row<'_>) -> rusqlite::Result<PrincipalRaw> {
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
    ))
}
fn principal(conn: &Connection, actor: &ActorId) -> Result<Option<PrincipalBinding>, StorageError> {
    let raw = db(db(conn.prepare_cached(&format!(
        "SELECT {PRINCIPAL_COLUMNS} FROM principal_authority p JOIN actors a ON a.id=p.actor_id WHERE p.actor_id=?1"
    )))?
    .query_row([actor.0.as_str()], principal_row)
    .optional())?;
    let Some((actor, kind, role, project, tools, state, revision, bound_by, updated)) = raw else {
        return Ok(None);
    };
    let role = role
        .map(|r| AgentRole::parse(&r).ok_or_else(|| corrupt(format!("stored role {r}"))))
        .transpose()?;
    let tool_families = tools
        .map(|t| {
            serde_json::from_str::<Vec<String>>(&t)
                .map_err(|e| corrupt(format!("stored tool families: {e}")))
        })
        .transpose()?;
    Ok(Some(PrincipalBinding {
        actor: ActorId(id_of(actor)?),
        kind: parse_principal(&kind)?,
        role,
        project_id: project.map(id_of).transpose()?.map(ProjectId),
        tool_families,
        active: match state.as_str() {
            "Active" => true,
            "Revoked" => false,
            other => return Err(corrupt(format!("stored principal state {other}"))),
        },
        revision,
        bound_by: ActorId(id_of(bound_by)?),
        updated_at_ms: updated,
    }))
}

const GRANT_COLUMNS: &str = "id,actor_id,kind,issued_by,revision,state,binding_json,expires_at,uses_remaining,created_at,revoked_by";
type GrantRaw = (
    String,
    String,
    String,
    String,
    i64,
    String,
    String,
    Option<i64>,
    Option<i64>,
    i64,
    Option<String>,
);
fn grant(conn: &Connection, id: &Id) -> Result<Option<AuthorityGrant>, StorageError> {
    let raw: Option<GrantRaw> = db(db(conn.prepare_cached(&format!(
        "SELECT {GRANT_COLUMNS} FROM authority_grants WHERE id=?1"
    )))?
    .query_row([id.as_str()], |r| {
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
        ))
    })
    .optional())?;
    let Some((
        id,
        actor,
        kind,
        issued_by,
        revision,
        state,
        binding,
        expires,
        uses,
        created,
        revoked,
    )) = raw
    else {
        return Ok(None);
    };
    Ok(Some(AuthorityGrant {
        id: id_of(id)?,
        actor: ActorId(id_of(actor)?),
        kind: GrantKind::parse(&kind)
            .ok_or_else(|| corrupt(format!("stored grant kind {kind}")))?,
        issued_by: ActorId(id_of(issued_by)?),
        revision,
        state: match state.as_str() {
            "Active" => GrantState::Active,
            "Consumed" => GrantState::Consumed,
            "Revoked" => GrantState::Revoked,
            other => return Err(corrupt(format!("stored grant state {other}"))),
        },
        binding: serde_json::from_str(&binding)
            .map_err(|e| corrupt(format!("stored grant binding: {e}")))?,
        expires_at_ms: expires,
        uses_remaining: uses,
        created_at_ms: created,
        revoked_by: revoked.map(id_of).transpose()?.map(ActorId),
    }))
}

fn repositories(conn: &Connection) -> Result<Vec<RegisteredRepository>, StorageError> {
    let mut s = db(
        conn.prepare_cached("SELECT id,canonical_location,revision FROM repositories ORDER BY id")
    )?;
    let rows: Vec<(String, String, i64)> =
        db(db(s.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))))?.collect())?;
    rows.into_iter()
        .map(|(id, location, revision)| {
            Ok(RegisteredRepository {
                id: id_of(id)?,
                location,
                revision,
            })
        })
        .collect()
}

macro_rules! authority_read {
    ($t:ty) => {
        impl AuthorityRead for $t {
            fn principal(&self, actor: &ActorId) -> Result<Option<PrincipalBinding>, StorageError> {
                principal(self.conn, actor)
            }
            fn authority_grant(&self, id: &Id) -> Result<Option<AuthorityGrant>, StorageError> {
                grant(self.conn, id)
            }
            fn registered_repositories(&self) -> Result<Vec<RegisteredRepository>, StorageError> {
                repositories(self.conn)
            }
        }
    };
}
authority_read!(Snapshot<'_>);
authority_read!(Unit<'_>);

fn usable(g: &AuthorityGrant, actor: &ActorId, now: i64) -> bool {
    g.usable_by(actor, now)
}

impl AuthorityLedger for Unit<'_> {
    fn bind_principal(
        &mut self,
        b: NewPrincipalBinding,
        expected_revision: Option<i64>,
    ) -> Result<PrincipalBinding, StorageError> {
        if actor_kind(self.conn, &b.actor)? != Some(b.kind) {
            return Err(StorageError::NotFound {
                kind: "actor",
                id: b.actor.0.as_str().to_owned(),
            });
        }
        if actor_kind(self.conn, &b.bound_by)?.is_none() {
            return Err(StorageError::NotFound {
                kind: "actor",
                id: b.bound_by.0.as_str().to_owned(),
            });
        }
        if let Some(families) = &b.tool_families
            && (families.len() > 64 || families.iter().any(|f| f.is_empty() || f.len() > 64))
        {
            return Err(invalid(
                "tool families must be 0..=64 names of 1..=64 bytes",
            ));
        }
        let tools = b
            .tool_families
            .as_ref()
            .map(|t| serde_json::to_string(t).expect("names"));
        let role = b.role.map(AgentRole::as_str);
        let project = b.project_id.as_ref().map(|p| p.0.as_str());
        let current = principal(self.conn, &b.actor)?;
        match (current, expected_revision) {
            (None, None) => {
                db(db(self.conn.prepare_cached(
                    "INSERT INTO principal_authority(actor_id,role,project_id,tool_families_json,state,revision,bound_by,updated_at) VALUES(?1,?2,?3,?4,'Active',1,?5,?6)",
                ))?
                .execute(params![
                    b.actor.0.as_str(),
                    role,
                    project,
                    tools,
                    b.bound_by.0.as_str(),
                    self.now
                ]))?;
            }
            (Some(_), None) => {
                return Err(StorageError::Duplicate {
                    kind: "principal_authority",
                    id: b.actor.0.as_str().to_owned(),
                });
            }
            (None, Some(_)) => {
                return Err(StorageError::NotFound {
                    kind: "principal_authority",
                    id: b.actor.0.as_str().to_owned(),
                });
            }
            (Some(c), Some(expected)) => {
                if c.revision != expected {
                    return Err(conflict(
                        "principal_authority",
                        b.actor.0.as_str(),
                        expected,
                        c.revision,
                    ));
                }
                db(db(self.conn.prepare_cached(
                    "UPDATE principal_authority SET role=?1,project_id=?2,tool_families_json=?3,state='Active',revision=revision+1,bound_by=?4,updated_at=?5 WHERE actor_id=?6 AND revision=?7",
                ))?
                .execute(params![
                    role,
                    project,
                    tools,
                    b.bound_by.0.as_str(),
                    self.now,
                    b.actor.0.as_str(),
                    expected
                ]))?;
            }
        }
        principal(self.conn, &b.actor)?
            .ok_or_else(|| corrupt("binding vanished inside its transaction"))
    }

    fn revoke_principal(
        &mut self,
        actor: &ActorId,
        expected_revision: i64,
    ) -> Result<PrincipalBinding, StorageError> {
        let c = principal(self.conn, actor)?.ok_or_else(|| StorageError::NotFound {
            kind: "principal_authority",
            id: actor.0.as_str().to_owned(),
        })?;
        if c.revision != expected_revision {
            return Err(conflict(
                "principal_authority",
                actor.0.as_str(),
                expected_revision,
                c.revision,
            ));
        }
        db(db(self.conn.prepare_cached(
            "UPDATE principal_authority SET state='Revoked',revision=revision+1,updated_at=?1 WHERE actor_id=?2 AND revision=?3",
        ))?
        .execute(params![self.now, actor.0.as_str(), expected_revision]))?;
        principal(self.conn, actor)?
            .ok_or_else(|| corrupt("binding vanished inside its transaction"))
    }

    fn issue_grant(&mut self, g: NewAuthorityGrant) -> Result<AuthorityGrant, StorageError> {
        if !g.binding.is_object() {
            return Err(invalid("grant binding must be a JSON object"));
        }
        if g.uses.is_some_and(|u| u <= 0) {
            return Err(invalid("grant uses must be positive"));
        }
        for actor in [&g.actor, &g.issued_by] {
            if actor_kind(self.conn, actor)?.is_none() {
                return Err(StorageError::NotFound {
                    kind: "actor",
                    id: actor.0.as_str().to_owned(),
                });
            }
        }
        if grant(self.conn, &g.id)?.is_some() {
            return Err(StorageError::Duplicate {
                kind: "authority_grant",
                id: g.id.as_str().to_owned(),
            });
        }
        db(db(self.conn.prepare_cached(
            "INSERT INTO authority_grants(id,actor_id,kind,issued_by,revision,state,binding_json,expires_at,uses_remaining,created_at) VALUES(?1,?2,?3,?4,1,'Active',?5,?6,?7,?8)",
        ))?
        .execute(params![
            g.id.as_str(),
            g.actor.0.as_str(),
            g.kind.as_str(),
            g.issued_by.0.as_str(),
            Value::to_string(&g.binding),
            g.expires_at_ms,
            g.uses,
            self.now
        ]))?;
        grant(self.conn, &g.id)?.ok_or_else(|| corrupt("grant vanished inside its transaction"))
    }

    fn revoke_grant(
        &mut self,
        id: &Id,
        expected_revision: i64,
        revoked_by: &ActorId,
    ) -> Result<AuthorityGrant, StorageError> {
        let g = grant(self.conn, id)?.ok_or_else(|| StorageError::NotFound {
            kind: "authority_grant",
            id: id.as_str().to_owned(),
        })?;
        if g.revision != expected_revision {
            return Err(conflict(
                "authority_grant",
                id.as_str(),
                expected_revision,
                g.revision,
            ));
        }
        if g.state != GrantState::Active {
            return Err(invalid("only an active grant can be revoked"));
        }
        db(db(self.conn.prepare_cached(
            "UPDATE authority_grants SET state='Revoked',revoked_by=?1,revision=revision+1 WHERE id=?2 AND revision=?3",
        ))?
        .execute(params![revoked_by.0.as_str(), id.as_str(), expected_revision]))?;
        grant(self.conn, id)?.ok_or_else(|| corrupt("grant vanished inside its transaction"))
    }

    fn consume_grant(
        &mut self,
        use_: &GrantUse,
        actor: &ActorId,
    ) -> Result<AuthorityGrant, StorageError> {
        let g = grant(self.conn, &use_.grant_id)?.ok_or_else(|| StorageError::NotFound {
            kind: "authority_grant",
            id: use_.grant_id.as_str().to_owned(),
        })?;
        if !usable(&g, actor, self.now) {
            return Err(StorageError::ScopeDenied);
        }
        if g.revision != use_.expected_revision {
            return Err(conflict(
                "authority_grant",
                g.id.as_str(),
                use_.expected_revision,
                g.revision,
            ));
        }
        let remaining = g.uses_remaining.map(|u| u - 1);
        let state = if remaining == Some(0) {
            "Consumed"
        } else {
            "Active"
        };
        // Compare-and-set on the revision authority resolved.
        let n = db(db(self.conn.prepare_cached(
            "UPDATE authority_grants SET uses_remaining=?1,state=?2,revision=revision+1 WHERE id=?3 AND revision=?4 AND state='Active'",
        ))?
        .execute(params![remaining, state, g.id.as_str(), g.revision]))?;
        if n != 1 {
            return Err(corrupt("grant changed inside its transaction"));
        }
        grant(self.conn, &g.id)?.ok_or_else(|| corrupt("grant vanished inside its transaction"))
    }
}
