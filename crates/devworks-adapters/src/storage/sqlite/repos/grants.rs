//! Resume grants: issue, consume and revoke.
use super::*;

pub(super) const GRANT_COLUMNS: &str =
    "id,actor_id,root_id,revision,state,authority_source,binding_json,expires_at,uses_remaining";

pub(super) fn grant(conn: &Connection, id: &Id) -> Result<Option<GrantRecord>, StorageError> {
    type Raw = (
        String,
        String,
        String,
        i64,
        String,
        String,
        String,
        Option<i64>,
        Option<i64>,
    );
    let raw: Option<Raw> = db(stmt(
        conn,
        &format!("SELECT {GRANT_COLUMNS} FROM resume_grants WHERE id=?1"),
    )?
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
        ))
    })
    .optional())?;
    raw.map(
        |(id, actor, root, revision, state, source, binding, expires, uses)| {
            Ok(GrantRecord {
                id: id_of(id)?,
                actor: ActorId(id_of(actor)?),
                root_id: RootId(id_of(root)?),
                revision,
                active: state == "Active",
                revoked: state == "Revoked",
                authority_source: source,
                binding: json_of(&binding)?,
                expires_at_ms: expires,
                uses_remaining: uses,
            })
        },
    )
    .transpose()
}

impl GrantLedger for Unit<'_> {
    fn issue(&mut self, g: NewGrant) -> Result<GrantRecord, StorageError> {
        bounded("authority source", &g.authority_source, 256)?;
        object("grant binding", &g.binding)?;
        if g.uses.is_some_and(|u| u <= 0) {
            return Err(invalid("grant uses must be positive"));
        }
        if actor_kind(self.conn, &g.actor)?.is_none() {
            return Err(StorageError::NotFound {
                kind: "actor",
                id: g.actor.0.as_str().to_owned(),
            });
        }
        require(self.conn, "work_items", "root", Some(g.root_id.0.as_str()))?;
        if grant(self.conn, &g.id)?.is_some() {
            return Err(StorageError::Duplicate {
                kind: "grant",
                id: g.id.as_str().to_owned(),
            });
        }
        db(stmt(self.conn,
            "INSERT INTO resume_grants(id,actor_id,root_id,revision,state,authority_source,binding_json,expires_at,uses_remaining) VALUES(?1,?2,?3,1,'Active',?4,?5,?6,?7)",
        )?
        .execute(params![
            g.id.as_str(),
            g.actor.0.as_str(),
            g.root_id.0.as_str(),
            g.authority_source,
            text(&g.binding),
            g.expires_at_ms,
            g.uses
        ]))?;
        grant(self.conn, &g.id)?.ok_or_else(|| corrupt("grant vanished inside its transaction"))
    }

    fn consume(&mut self, use_: &GrantUse, actor: &ActorId) -> Result<GrantRecord, StorageError> {
        let g = grant(self.conn, &use_.grant_id)?.ok_or_else(|| StorageError::NotFound {
            kind: "grant",
            id: use_.grant_id.as_str().to_owned(),
        })?;
        if &g.actor != actor
            || !g.active
            || g.expires_at_ms.is_some_and(|t| t <= self.now)
            || g.uses_remaining.is_some_and(|u| u <= 0)
        {
            return Err(StorageError::ScopeDenied);
        }
        if g.revision != use_.expected_revision {
            return Err(conflict_strict(
                RevisionDimension::Authority,
                "grant",
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
        let n = db(stmt(self.conn,
            "UPDATE resume_grants SET uses_remaining=?1,state=?2,revision=revision+1 WHERE id=?3 AND revision=?4",
        )?
        .execute(params![remaining, state, g.id.as_str(), g.revision]))?;
        if n != 1 {
            return Err(corrupt("grant changed inside its transaction"));
        }
        grant(self.conn, &g.id)?.ok_or_else(|| corrupt("grant vanished inside its transaction"))
    }

    fn revoke(&mut self, id: &Id, expected_revision: i64) -> Result<GrantRecord, StorageError> {
        let g = grant(self.conn, id)?.ok_or_else(|| StorageError::NotFound {
            kind: "grant",
            id: id.as_str().to_owned(),
        })?;
        if g.revision != expected_revision {
            return Err(conflict_strict(
                RevisionDimension::Authority,
                "grant",
                g.id.as_str(),
                expected_revision,
                g.revision,
            ));
        }
        let n = db(stmt(
            self.conn,
            "UPDATE resume_grants SET state='Revoked',revision=revision+1 WHERE id=?1 AND revision=?2",
        )?
        .execute(params![g.id.as_str(), g.revision]))?;
        if n != 1 {
            return Err(corrupt("grant changed inside its transaction"));
        }
        grant(self.conn, &g.id)?.ok_or_else(|| corrupt("grant vanished inside its transaction"))
    }
}
