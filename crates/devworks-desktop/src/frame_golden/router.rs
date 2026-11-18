//! Sample frames of the provider connection, catalog model and embedding space `router.configure`
//! actions.
use super::{Silent, from};
use crate::connections_screen::{
    ConnectionAction, ModelEdit, NewModel, connection_action, model_action, new_model_action,
    risk_action, sign_in,
};
use crate::embedding_spaces_screen::create_action;
use crate::ops;
use devworks_core::protocol::records::{
    AccountAction, OpenRouterConstraints, RouteCatalogModel, RouterConfigAction,
};
use devworks_core::protocol::{Id, Revision};
use serde_json::json;

pub fn frames() -> Vec<(&'static str, Vec<u8>)> {
    let b = &Silent;
    let configure = |action: RouterConfigAction| ops::router_configure(b, action);
    let id: Id = from(json!("connection-a"));
    let revision: Revision = from(json!("3"));
    let target = Some(("connection-a", "3"));
    let local = "http://127.0.0.1:11434/v1";
    let with_key = |action: Option<RouterConfigAction>, key: &str| {
        let mut action = action.expect("an action");
        if let RouterConfigAction::Connection { credential_ref, .. } = &mut action {
            *credential_ref = Some(from(json!(key)));
        }
        action
    };
    let constraints: OpenRouterConstraints = from(json!({"allowedModels":["a/one","b/two"],
        "excludedModels":[],"costTier":"high",
        "provider":{"order":["Provider A"],"allowFallbacks":false}}));
    let sign_in_action =
        |action: AccountAction, flow: Option<&Id>| match sign_in(&id, action, flow, "label") {
            ConnectionAction::Configure { action, .. } => action,
            _ => unreachable!("sign_in configures"),
        };
    let new_model = |context: &str| {
        let mut n = NewModel::default();
        n.id = "model-a".into();
        n.name = "Model A".into();
        n.upstream = "vendor/model-a".into();
        n.context = context.into();
        n.efforts = [false, true, false, true, false, false];
        n.vision = true;
        n
    };
    let catalog: RouteCatalogModel = from(json!({"id":"model-a","displayName":"Model A",
        "endpointProfile":"CompatibleChat","upstreamModelId":"vendor/model-a","enabled":true,
        "tierMemberships":["High"],"efforts":["Low","High"],"imageInput":true,"tools":false,
        "revision":"1","available":true}));
    vec![
        (
            "connection_add",
            configure(
                connection_action(
                    None,
                    "Local",
                    "CompatibleChat",
                    local,
                    None,
                    Some(true),
                    None,
                )
                .unwrap(),
            ),
        ),
        (
            "connection_add_with_key",
            configure(with_key(
                connection_action(
                    None,
                    "Cloud",
                    "AnthropicMessages",
                    "https://api.anthropic.com",
                    None,
                    Some(false),
                    None,
                ),
                "secret-a",
            )),
        ),
        (
            "connection_edit",
            configure(
                connection_action(target, "Local", "CompatibleChat", local, None, None, None)
                    .unwrap(),
            ),
        ),
        (
            "connection_edit_local_free",
            configure(
                connection_action(
                    target,
                    "Local",
                    "CompatibleChat",
                    local,
                    None,
                    Some(false),
                    None,
                )
                .unwrap(),
            ),
        ),
        (
            "connection_edit_open_router",
            configure(with_key(
                connection_action(
                    target,
                    "Router",
                    "OpenRouterChat",
                    "https://openrouter.ai/api/v1",
                    None,
                    None,
                    Some(constraints),
                ),
                "secret-b",
            )),
        ),
        (
            "connection_turn_off",
            configure(
                connection_action(
                    target,
                    "Local",
                    "CompatibleChat",
                    local,
                    Some(false),
                    None,
                    None,
                )
                .unwrap(),
            ),
        ),
        (
            "connection_remove",
            configure(RouterConfigAction::RemoveConnection {
                connection_id: id.clone(),
                expected_revision: revision.clone(),
            }),
        ),
        (
            "connection_risk_accept",
            configure(risk_action(&id, &revision, true)),
        ),
        (
            "connection_risk_withdraw",
            configure(risk_action(&id, &revision, false)),
        ),
        (
            "connection_sign_in_begin",
            configure(sign_in_action(AccountAction::BeginSignIn, None)),
        ),
        (
            "connection_sign_in_complete",
            configure(sign_in_action(
                AccountAction::CompleteSignIn,
                Some(&from(json!("flow-a"))),
            )),
        ),
        (
            "model_add",
            configure(new_model_action(&new_model(""), "CompatibleChat", None).unwrap()),
        ),
        (
            "model_add_bound",
            configure(
                new_model_action(&new_model("128000"), "CompatibleChat", Some("connection-a"))
                    .unwrap(),
            ),
        ),
        (
            "model_edit",
            configure(
                model_action(
                    &catalog,
                    &ModelEdit {
                        enabled: false,
                        context: "64000".into(),
                        connection: "connection-a".into(),
                    },
                )
                .unwrap(),
            ),
        ),
        (
            "embedding_create",
            configure(
                create_action(
                    " Space A ",
                    "connection-a",
                    "3",
                    "embed-1",
                    1536,
                    true,
                    true,
                )
                .unwrap(),
            ),
        ),
        (
            "embedding_create_native",
            configure(
                create_action("Space B", "connection-a", "3", "embed-1", 768, false, false)
                    .unwrap(),
            ),
        ),
        (
            "embedding_clear",
            configure(RouterConfigAction::ClearEmbeddingSpace {
                expected_selection_revision: from(json!("5")),
            }),
        ),
        (
            "embedding_qualify",
            configure(RouterConfigAction::QualifyEmbeddingSpace {
                candidate_id: from(json!("candidate-a")),
                expected_revision: from(json!("2")),
            }),
        ),
        (
            "embedding_select",
            configure(RouterConfigAction::SelectEmbeddingSpace {
                candidate_id: from(json!("candidate-a")),
                expected_revision: from(json!("2")),
                expected_selection_revision: from(json!("5")),
            }),
        ),
    ]
}
