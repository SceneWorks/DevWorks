//! The package handler ledger: delivery processing records.
use super::*;

pub(super) fn package_state(
    conn: &Connection,
    package: &str,
) -> Result<Option<PackageState>, StorageError> {
    let raw: Option<(String, i64, i64, String)> = db(stmt(
        conn,
        "SELECT package_id,generation,revision,state_json FROM package_handler_state WHERE package_id=?1",
    )?
    .query_row([package], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
    })
    .optional())?;
    raw.map(|(package_id, generation, revision, state)| {
        Ok(PackageState {
            package_id,
            generation,
            revision,
            state: json_of(&state)?,
        })
    })
    .transpose()
}

pub(super) fn processing(
    conn: &Connection,
    package: &str,
    subscription: &Id,
    event: &EventId,
) -> Result<Option<ProcessingRecord>, StorageError> {
    type Raw = (String, String, i64, i64, i64, String);
    let raw: Option<Raw> = db(stmt(
        conn,
        "SELECT receipt_id,payload_digest,state_generation,state_revision,\
            checkpoint,intents_json FROM package_handler_commits WHERE \
            package_id=?1 AND subscription_id=?2 AND event_id=?3",
    )?
    .query_row(
        params![package, subscription.as_str(), event.0.as_str()],
        |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        },
    )
    .optional())?;
    raw.map(
        |(receipt, digest, generation, revision, checkpoint, intents)| {
            Ok(ProcessingRecord {
                package_id: package.to_owned(),
                subscription_id: subscription.clone(),
                event_id: event.clone(),
                receipt_id: id_of(receipt)?,
                payload_digest: digest,
                state_generation: generation,
                state_revision: revision,
                checkpoint,
                intents: json_of(&intents)?,
            })
        },
    )
    .transpose()
}

/// Opaque conflict target for a package (package IDs are qualified names, not opaque IDs).
pub(super) fn sha256_id(package_id: &str) -> String {
    devworks_core::fingerprint::sha256_hex(package_id.as_bytes())
}

impl PackageHandlerLedger for Unit<'_> {
    fn compare_and_set_state(
        &mut self,
        package_id: &str,
        generation: i64,
        expected_revision: i64,
        state: Value,
    ) -> Result<PackageState, StorageError> {
        bounded("package id", package_id, 256)?;
        if generation <= 0 || expected_revision < 0 {
            return Err(invalid("package state generation/revision bounds"));
        }
        let current = package_state(self.conn, package_id)?;
        if let Some(c) = &current
            && c.generation != generation
        {
            return Err(StorageError::LeaseLost {
                kind: "package_state",
                id: package_id.to_owned(),
            });
        }
        let current_revision = current.as_ref().map_or(0, |c| c.revision);
        if current_revision != expected_revision {
            return Err(conflict(
                RevisionDimension::Registry,
                "package_state",
                Id::new(sha256_id(package_id))
                    .map_err(|_| invalid("package id"))?
                    .as_str(),
                expected_revision.max(1),
                current_revision.max(1),
            ));
        }
        db(stmt(
            self.conn,
            "INSERT INTO package_handler_state(package_id,generation,revision,\
                state_json) VALUES(?1,?2,1,?3) ON CONFLICT(package_id) DO UPDATE SET \
                revision=revision+1,state_json=excluded.state_json WHERE revision=?4 \
                AND generation=?2",
        )?
        .execute(params![
            package_id,
            generation,
            text(&state),
            expected_revision
        ]))?;
        package_state(self.conn, package_id)?
            .ok_or_else(|| corrupt("package state vanished inside its transaction"))
    }

    fn record_processing(&mut self, r: &ProcessingRecord) -> Result<(), StorageError> {
        bounded("package id", &r.package_id, 256)?;
        bounded("payload digest", &r.payload_digest, 128)?;
        require(self.conn, "events", "event", Some(r.event_id.0.as_str()))?;
        let duplicate = processing(self.conn, &r.package_id, &r.subscription_id, &r.event_id)?
            .is_some()
            || exists(
                self.conn,
                "SELECT 1 FROM package_handler_commits WHERE receipt_id=?1",
                r.receipt_id.as_str(),
            )?;
        if duplicate {
            return Err(StorageError::Duplicate {
                kind: "package_processing",
                id: format!(
                    "{}/{}/{}",
                    r.package_id,
                    r.subscription_id.as_str(),
                    r.event_id.0.as_str()
                ),
            });
        }
        db(stmt(
            self.conn,
            "INSERT INTO package_handler_commits(package_id,subscription_id,event_id,\
                receipt_id,payload_digest,state_generation,state_revision,checkpoint,\
                intents_json) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        )?
        .execute(params![
            r.package_id,
            r.subscription_id.as_str(),
            r.event_id.0.as_str(),
            r.receipt_id.as_str(),
            r.payload_digest,
            r.state_generation,
            r.state_revision,
            r.checkpoint,
            text(&r.intents)
        ]))?;
        Ok(())
    }
}
