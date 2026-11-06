//! Registered transport identity is distinct from an Auto route's unresolved serving identity.
use devworks_core::ports::{
    storage::{StorageError, StoredConnection, StoredRouteModel},
    usage::PriceQuery,
};

pub(super) fn registered_upstream(
    connection: &StoredConnection,
    model: Option<&StoredRouteModel>,
    query: &PriceQuery,
) -> Result<String, StorageError> {
    if connection.id != query.connection_id || connection.profile != query.endpoint_profile {
        return Err(StorageError::ScopeDenied);
    }
    let Some(model) = model else {
        // Embeddings have an explicitly qualified discovery/space registration instead of a
        // conversational catalog row; its exact upstream is checked by capabilities().
        return if connection.profile == "Embedding" {
            query
                .provider_model_id
                .clone()
                .ok_or(StorageError::ScopeDenied)
        } else {
            Err(StorageError::ScopeDenied)
        };
    };
    if model.id != query.catalog_model_id
        || !model.enabled
        || model.endpoint_profile != connection.profile
        || model
            .connection_id
            .as_ref()
            .is_some_and(|id| id != &connection.id)
    {
        return Err(StorageError::ScopeDenied);
    }
    let unresolved = connection.profile == "OpenRouterChat"
        && devworks_core::ports::router::is_open_router_auto(&model.upstream_model_id);
    if (unresolved && query.provider_model_id.is_some())
        || (!unresolved && query.provider_model_id.as_ref() != Some(&model.upstream_model_id))
    {
        return Err(StorageError::ScopeDenied);
    }
    Ok(model.upstream_model_id.clone())
}
