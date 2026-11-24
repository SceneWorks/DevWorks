//! Public trust material is copied at configuration, never read with another actor's identity.
use super::SchemaService;
use crate::{
    context::InvocationContext,
    gateway::AdmittedCommand,
    operations::{OperationInput, schemas as ops},
    ports::{Preparation, Prepared, storage::*},
    protocol::{Check, DomainErrorCode, records::SchemaSourceAction},
    services::kernel::{Failure, Refusal},
};

pub(super) enum PreparedCertificate {
    Material(StoredSchemaCertificate),
    Invalid,
}
fn invalid() -> StorageError {
    StorageError::InvalidRecord("expected bounded public certificate PEM blocks".into())
}
fn pem(bytes: &[u8]) -> Result<(), StorageError> {
    if bytes.is_empty() || bytes.len() > 1048576 {
        return Err(invalid());
    }
    let mut text = std::str::from_utf8(bytes)
        .map_err(|_| invalid())?
        .trim_start_matches('\u{feff}')
        .trim();
    let mut count = 0;
    while !text.is_empty() {
        text = text
            .strip_prefix("-----BEGIN CERTIFICATE-----")
            .ok_or_else(invalid)?;
        let end = text.find("-----END CERTIFICATE-----").ok_or_else(invalid)?;
        let body = &text[..end];
        if body.trim().is_empty()
            || !body.bytes().all(|b| {
                b.is_ascii_alphanumeric()
                    || matches!(b, b'+' | b'/' | b'=')
                    || b.is_ascii_whitespace()
            })
        {
            return Err(invalid());
        }
        text = text[end + "-----END CERTIFICATE-----".len()..].trim();
        count += 1;
        if count > 256 {
            return Err(invalid());
        }
    }
    if count == 0 {
        return Err(invalid());
    }
    Ok(())
}
/// The configured certificate travels with the selected source, never with a caller-supplied
/// replacement or an artifact looked up under a different principal.
pub(super) fn for_source(
    read: &dyn SchemaRead,
    source: &StoredSchemaSource,
) -> Result<Option<Vec<u8>>, StorageError> {
    let selected = source
        .connection
        .config
        .as_ref()
        .and_then(|config| config.ca_certificate.as_ref());
    let material = read.schema_certificate(&source.connection.id)?;
    match (selected, material) {
        (None, None) => Ok(None),
        (Some(reference), Some(material)) if same(&material.reference, reference) => {
            Ok(Some(material.pem))
        }
        _ => Err(StorageError::ScopeDenied),
    }
}

impl SchemaService {
    pub(super) fn prepare_certificate(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        env: &Preparation<'_>,
    ) -> Result<Option<Prepared>, StorageError> {
        let OperationInput::Schemas(ops::Input::Configure(input)) = command.input() else {
            return Ok(None);
        };
        if matches!(input.action, Some(SchemaSourceAction::Remove)) {
            return Ok(None);
        }
        let Some(reference) = &input.config.ca_certificate else {
            return Ok(None);
        };
        // Gateway preparation precedes receipt lookup even for a currently-authorized retry.
        // An already decided request must replay without reopening its original artifact.
        let retained = env.read(|r| {
            if r.receipts().lookup(command.identity())?.is_some() {
                return Ok(true);
            }
            let Some(id) = &input.connection_id else {
                return Ok(false);
            };
            let Some(source) = r.schemas().schema_source(id)? else {
                return Ok(false);
            };
            if source.owner_id != *ctx.principal() || source.deleted {
                return Err(StorageError::ScopeDenied);
            }
            Ok(r.schemas()
                .schema_certificate(id)?
                .is_some_and(|m| same(&m.reference, reference)))
        })?;
        if retained {
            return Ok(None);
        }
        if env.stopped(self.clock.now_ms().get()) {
            return Err(StorageError::Cancelled);
        }
        let runtime = self.runtime.as_ref().ok_or(StorageError::ScopeDenied)?;
        let (bytes, _) =
            match runtime
                .artifacts
                .read_exact(runtime.store.as_ref(), ctx, reference, 1048576)
            {
                Ok(value) => value,
                Err(crate::services::artifacts::lifecycle::ExactReadRefusal::TooLarge {
                    ..
                }) => return Ok(Some(Prepared::new(PreparedCertificate::Invalid))),
                Err(_) => return Err(StorageError::ScopeDenied),
            };
        if pem(&bytes).is_err() {
            return Ok(Some(Prepared::new(PreparedCertificate::Invalid)));
        }
        if env.stopped(self.clock.now_ms().get()) {
            return Err(StorageError::Cancelled);
        }
        Ok(Some(Prepared::new(PreparedCertificate::Material(
            StoredSchemaCertificate {
                reference: reference.clone(),
                pem: bytes,
            },
        ))))
    }
}
fn same(
    a: &crate::protocol::provider::ArtifactRef,
    b: &crate::protocol::provider::ArtifactRef,
) -> bool {
    a.artifact_id == b.artifact_id
        && a.version_id == b.version_id
        && a.sha256 == b.sha256
        && a.byte_length == b.byte_length
        && a.media_type == b.media_type
}
pub(super) fn configuration(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    input: &ops::ConfigureInput,
    prepared: Option<&PreparedCertificate>,
) -> Result<Option<StoredSchemaCertificate>, Failure> {
    if matches!(input.action, Some(SchemaSourceAction::Remove)) {
        return Ok(None);
    }
    let Some(reference) = &input.config.ca_certificate else {
        return Ok(None);
    };
    reference.check().map_err(|_| invalid())?;
    if let Some(id) = &input.connection_id
        && let Some(material) = tx.schemas().schema_certificate(id)?
        && same(&material.reference, reference)
    {
        return Ok(Some(material));
    }
    let material = match prepared.ok_or(StorageError::ScopeDenied)? {
        PreparedCertificate::Material(material) => material,
        PreparedCertificate::Invalid => {
            return Err(Refusal::new(
                DomainErrorCode::InvalidArgument,
                "schema_ca_certificate",
                None,
            )
            .into());
        }
    };
    if !same(&material.reference, reference) {
        return Err(StorageError::ScopeDenied.into());
    }
    if !crate::services::artifacts::exact_reference_readable_in(tx, ctx, reference)? {
        return Err(Refusal::new(
            DomainErrorCode::SourceChanged,
            "schema_ca_certificate",
            None,
        )
        .into());
    }
    pem(&material.pem)?;
    Ok(Some(material.clone()))
}
