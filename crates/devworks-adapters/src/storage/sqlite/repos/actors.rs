//! The actor registry.
use super::*;

pub(crate) fn actor_kind(
    conn: &Connection,
    actor: &ActorId,
) -> Result<Option<PrincipalKind>, StorageError> {
    let kind: Option<String> = db(stmt(conn, "SELECT kind FROM actors WHERE id=?1")?
        .query_row([actor.0.as_str()], |r| r.get(0))
        .optional())?;
    kind.as_deref().map(parse_principal).transpose()
}

pub(super) fn actor_profile(
    conn: &Connection,
    actor: &ActorId,
) -> Result<Option<StoredActor>, StorageError> {
    let row: Option<(String, String)> = db(stmt(
        conn,
        "SELECT kind,display_name FROM actors WHERE id=?1",
    )?
    .query_row([actor.0.as_str()], |r| Ok((r.get(0)?, r.get(1)?)))
    .optional())?;
    row.map(|(kind, display_name)| {
        Ok(StoredActor {
            kind: parse_principal(&kind)?,
            display_name,
        })
    })
    .transpose()
}

impl ActorRegistry for Unit<'_> {
    fn register(
        &mut self,
        actor: &ActorId,
        kind: PrincipalKind,
        display_name: &str,
    ) -> Result<(), StorageError> {
        bounded("display name", display_name, 256)?;
        match actor_kind(self.conn, actor)? {
            Some(k) if k == kind => Ok(()),
            Some(_) => Err(StorageError::Duplicate {
                kind: "actor",
                id: actor.0.as_str().to_owned(),
            }),
            None => {
                db(stmt(
                    self.conn,
                    "INSERT INTO actors(id,kind,display_name) VALUES(?1,?2,?3)",
                )?
                .execute(params![
                    actor.0.as_str(),
                    principal_name(kind),
                    display_name
                ]))?;
                Ok(())
            }
        }
    }
}
