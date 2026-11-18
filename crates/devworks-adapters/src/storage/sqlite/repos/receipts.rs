//! The command ledger: receipts and their replayable replies.
use super::*;

pub(super) fn receipt_reply(
    conn: &Connection,
    identity: &ReceiptIdentity,
) -> Result<Option<ReceiptReply>, StorageError> {
    let row: Option<(String, String, String, String, i64)> = db(stmt(
        conn,
        "SELECT command_id,artifact_id,version_id,schema_json,published_at FROM receipt_replies WHERE actor_id=?1 AND scope_key=?2 AND idempotency_key=?3",
    )?
    .query_row(
        params![
            identity.actor_id().0.as_str(),
            identity.scope_key().as_str(),
            identity.request_key().0.as_str()
        ],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
    )
    .optional())?;
    let Some((command, artifact, version, schema, at)) = row else {
        return Ok(None);
    };
    Ok(Some(ReceiptReply {
        command_id: CommandId(id_of(command)?),
        artifact_id: id_of(artifact)?,
        version_id: id_of(version)?,
        schema: json_of(&schema)?,
        published_at_ms: at,
    }))
}

pub(super) fn lookup_receipt(
    conn: &Connection,
    identity: &ReceiptIdentity,
) -> Result<Option<LedgerEntry>, StorageError> {
    let row: Option<(String, String, String)> = db(stmt(conn,
            "SELECT command_kind,payload_digest,result_json FROM command_receipts WHERE actor_id=?1 AND scope_key=?2 AND idempotency_key=?3",
        )?
        .query_row(
            params![
                identity.actor_id().0.as_str(),
                identity.scope_key().as_str(),
                identity.request_key().0.as_str()
            ],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional())?;
    let Some((operation, digest, result)) = row else {
        return Ok(None);
    };
    let stored = json_of(&result)?;
    let contract = stored["contract"].as_str().unwrap_or_default().to_owned();
    let schema_revision = SchemaRevision(
        stored["schemaRevision"]
            .as_u64()
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(0),
    );
    let fingerprint = Fingerprint::from_hex(&digest);
    let incompatible = |reason: String| LedgerEntry::Incompatible {
        operation: operation.clone(),
        contract: contract.clone(),
        schema_revision,
        fingerprint: fingerprint.clone(),
        reason,
    };
    if contract != CONTRACT {
        return Ok(Some(incompatible(format!(
            "receipt recorded under contract {contract:?}"
        ))));
    }
    let Some(fp) = fingerprint.clone() else {
        return Ok(Some(incompatible(
            "receipt digest is not a fingerprint".into(),
        )));
    };
    if let Some(purged) = stored.get("purged") {
        let outcome = match serde_json::from_value::<
            devworks_core::protocol::records::PurgedReceiptOutcome,
        >(purged.clone())
        {
            Ok(outcome) => outcome,
            Err(_) => return Ok(Some(incompatible("purged receipt does not decode".into()))),
        };
        use devworks_core::protocol::Check;
        if outcome.check().is_err()
            || outcome.request_digest.as_str() != fp.as_str()
            || stored.get("receipt").is_some()
        {
            return Ok(Some(incompatible("invalid purged receipt fence".into())));
        }
        return Ok(Some(LedgerEntry::Purged(
            devworks_core::ports::StoredPurgedReceipt {
                operation,
                contract,
                schema_revision,
                fingerprint: fp,
                outcome,
            },
        )));
    }
    match serde_json::from_value::<Receipt<DomainDecision>>(stored["receipt"].clone()) {
        Ok(receipt) => Ok(Some(LedgerEntry::Current(StoredReceipt {
            operation,
            contract,
            schema_revision,
            fingerprint: fp,
            receipt,
        }))),
        Err(e) => Ok(Some(incompatible(format!("receipt does not decode: {e}")))),
    }
}

impl CommandLedger for Unit<'_> {
    fn record(
        &mut self,
        identity: &ReceiptIdentity,
        receipt: &StoredReceipt,
    ) -> Result<(), StorageError> {
        if lookup_receipt(self.conn, identity)?.is_some() {
            return Err(StorageError::Duplicate {
                kind: "command_receipt",
                id: identity.request_key().0.as_str().to_owned(),
            });
        }
        let op = receipt.receipt.operation_id.as_ref().map(|o| o.0.as_str());
        require(self.conn, "operations", "operation", op)?;
        let body = json!({
            "contract": receipt.contract,
            "schemaRevision": receipt.schema_revision.0,
            "receipt": receipt.receipt,
        });
        db(stmt(self.conn,
            "INSERT INTO command_receipts(actor_id,scope_key,idempotency_key,command_kind,payload_digest,operation_id,result_json,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
        )?
        .execute(params![
            identity.actor_id().0.as_str(),
            identity.scope_key().as_str(),
            identity.request_key().0.as_str(),
            receipt.operation,
            receipt.fingerprint.as_str(),
            op,
            text(&body),
            self.now
        ]))?;
        Ok(())
    }

    fn record_reply(
        &mut self,
        identity: &ReceiptIdentity,
        reply: &ReceiptReply,
    ) -> Result<(), StorageError> {
        object("reply schema", &reply.schema)?;
        if lookup_receipt(self.conn, identity)?.is_none() {
            return Err(StorageError::NotFound {
                kind: "command_receipt",
                id: identity.request_key().0.as_str().to_owned(),
            });
        }
        let duplicate = receipt_reply(self.conn, identity)?.is_some()
            || db(stmt(
                self.conn,
                "SELECT 1 FROM receipt_replies WHERE command_id=?1 OR artifact_id=?2",
            )?
            .exists(params![
                reply.command_id.0.as_str(),
                reply.artifact_id.as_str()
            ]))?;
        if duplicate {
            return Err(StorageError::Duplicate {
                kind: "receipt_reply",
                id: reply.command_id.0.as_str().to_owned(),
            });
        }
        let belongs = db(stmt(
            self.conn,
            "SELECT 1 FROM artifact_versions WHERE id=?1 AND artifact_id=?2",
        )?
        .exists(params![
            reply.version_id.as_str(),
            reply.artifact_id.as_str()
        ]))?;
        if !belongs {
            return Err(StorageError::NotFound {
                kind: "artifact_version",
                id: reply.version_id.as_str().to_owned(),
            });
        }
        db(stmt(self.conn,
            "INSERT INTO receipt_replies(actor_id,scope_key,idempotency_key,command_id,artifact_id,version_id,schema_json,published_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
        )?
        .execute(params![
            identity.actor_id().0.as_str(),
            identity.scope_key().as_str(),
            identity.request_key().0.as_str(),
            reply.command_id.0.as_str(),
            reply.artifact_id.as_str(),
            reply.version_id.as_str(),
            text(&reply.schema),
            reply.published_at_ms
        ]))?;
        Ok(())
    }
}
