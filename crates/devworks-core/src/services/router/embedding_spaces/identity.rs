use crate::fingerprint::sha256_hex;
use crate::ports::storage::{StorageError, StoredConnection};
use crate::protocol::Id;
use crate::protocol::data::{EmbeddingSpace, Metric};
use crate::protocol::provider::Digest;
use crate::providers::capabilities::EmbeddingDimensions;
use serde_json::json;

/// Deterministic semantic identity; this value carries no qualification or dispatch authority.
#[derive(Clone, Debug)]
pub struct EmbeddingIdentity {
    pub model_digest: String,
    pub configuration_digest: String,
    pub encoder_config: serde_json::Value,
    pub space: EmbeddingSpace,
}

/// Names, tariffs, credential refreshes and inventory timestamps do not change vector meaning.
/// Account, endpoint, actual upstream model and encoder configuration do.
pub fn embedding_identity(
    connection: &StoredConnection,
    upstream_model: &str,
    dimensions: u32,
    mode: &EmbeddingDimensions,
    metric: Metric,
) -> Result<EmbeddingIdentity, StorageError> {
    let invalid = || StorageError::InvalidRecord("invalid embedding identity".into());
    if connection.profile != "Embedding"
        || connection.account_revision < 1
        || connection.endpoint.is_empty()
        || upstream_model.trim().is_empty()
        || upstream_model.len() > 4096
        || dimensions == 0
        || dimensions > 65536
    {
        return Err(invalid());
    }
    let mode = match mode {
        EmbeddingDimensions::Fixed(expected) if *expected == dimensions => "Native",
        EmbeddingDimensions::Parameter => "Parameter",
        _ => return Err(invalid()),
    };
    let model = json!({
        "version":1,"connectionId":connection.id,"endpointProfile":"Embedding",
        "endpoint":connection.endpoint,"accountRevision":connection.account_revision.to_string(),
        "accountDigest":connection.account_digest,"upstreamModelId":upstream_model,
    });
    let model_digest = sha256_hex(
        crate::jcs::canonicalize(&model)
            .map_err(|_| invalid())?
            .as_bytes(),
    );
    let config = json!({
        "version":1,"modelDigest":model_digest,"dimensions":dimensions,
        "dimensionMode":mode,"metric":metric,"elementType":"Float32","encodingFormat":"float",
    });
    let configuration_digest = sha256_hex(
        crate::jcs::canonicalize(&config)
            .map_err(|_| invalid())?
            .as_bytes(),
    );
    let space = EmbeddingSpace {
        space_id: Id::new(format!("embspace-{configuration_digest}")).map_err(|_| invalid())?,
        model_digest: Digest(model_digest.clone()),
        dimensions,
        metric,
    };
    Ok(EmbeddingIdentity {
        model_digest,
        configuration_digest,
        encoder_config: config,
        space,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connection() -> StoredConnection {
        StoredConnection {
            id: Id::new("embedding-connection").unwrap(),
            name: "Embeddings".into(),
            profile: "Embedding".into(),
            endpoint: "http://127.0.0.1:1234/v1".into(),
            enabled: true,
            local_free: false,
            open_router: None,
            credential_ref: None,
            credential_revision: 1,
            credential_epoch: None,
            lifecycle: "Connected".into(),
            state_reason: None,
            account_revision: 1,
            refresh_operation_id: None,
            account_digest: Some("account-a".into()),
            access_expires_at_ms: None,
            risk: None,
            discovered: None,
            discovered_at_ms: None,
            healthy: None,
            checked_at_ms: None,
            revision: 1,
            created_at_ms: 1,
            updated_at_ms: 1,
        }
    }
    fn identity(connection: &StoredConnection) -> EmbeddingIdentity {
        embedding_identity(
            connection,
            "actual-upstream",
            3,
            &EmbeddingDimensions::Fixed(3),
            Metric::Cosine,
        )
        .unwrap()
    }

    #[test]
    fn cosmetic_billing_and_refresh_edits_preserve_semantic_identity() {
        let before = connection();
        let mut after = before.clone();
        after.name = "Renamed".into();
        after.local_free = true;
        after.credential_revision += 1;
        after.discovered_at_ms = Some(100);
        after.checked_at_ms = Some(100);
        after.revision += 1;
        after.updated_at_ms = 100;
        assert_eq!(
            identity(&before).space.space_id,
            identity(&after).space.space_id
        );
    }

    #[test]
    fn encoder_and_provider_identity_changes_never_relabel_old_vectors() {
        let original = connection();
        let old = identity(&original).space.space_id;
        for changed in [
            {
                let mut c = original.clone();
                c.endpoint.push_str("/other");
                c
            },
            {
                let mut c = original.clone();
                c.account_revision += 1;
                c
            },
            {
                let mut c = original.clone();
                c.account_digest = Some("other".into());
                c
            },
        ] {
            assert_ne!(old, identity(&changed).space.space_id);
        }
        for (model, dimensions, mode, metric) in [
            (
                "another-model",
                3,
                EmbeddingDimensions::Fixed(3),
                Metric::Cosine,
            ),
            (
                "actual-upstream",
                4,
                EmbeddingDimensions::Fixed(4),
                Metric::Cosine,
            ),
            (
                "actual-upstream",
                3,
                EmbeddingDimensions::Parameter,
                Metric::Cosine,
            ),
            (
                "actual-upstream",
                3,
                EmbeddingDimensions::Fixed(3),
                Metric::L2,
            ),
        ] {
            assert_ne!(
                old,
                embedding_identity(&original, model, dimensions, &mode, metric)
                    .unwrap()
                    .space
                    .space_id
            );
        }
        assert!(
            embedding_identity(
                &original,
                "actual-upstream",
                3,
                &EmbeddingDimensions::Unknown,
                Metric::Cosine
            )
            .is_err()
        );
        assert!(
            embedding_identity(
                &original,
                "actual-upstream",
                4,
                &EmbeddingDimensions::Fixed(3),
                Metric::Cosine
            )
            .is_err()
        );
    }
}
