//! Catalog `addons.*` lifecycle command envelope and decision projection adopted from the
//! recorded candidates (tools/contracts-eval/src/package.rs). Package manifests and RPC frames
//! are public package wire DTOs and live in `devworks-extension-protocol`.
use super::provider::{ArtifactRef, Digest};
use super::shared::*;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum AddonCommand {
    Install {
        bundle: ArtifactRef,
        expected_registry_revision: Revision,
    },
    Update {
        package_id: Id,
        bundle: ArtifactRef,
        expected_registry_revision: Revision,
    },
    Enable {
        package_id: Id,
        expected_registry_revision: Revision,
    },
    Disable {
        package_id: Id,
        expected_registry_revision: Revision,
    },
    Rollback {
        package_id: Id,
        target_manifest_digest: Digest,
        expected_registry_revision: Revision,
        expected_state_generation: Revision,
    },
    Uninstall {
        package_id: Id,
        expected_registry_revision: Revision,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AddonCommandRequest {
    pub protocol: Protocol,
    pub request_key: RequestKey,
    pub command: AddonCommand,
}
impl AddonCommandRequest {
    /// The candidate had no typed check, so a bundle reference whose media type exceeded the
    /// schema's 128-character bound decoded successfully. Apply the shared reference rule.
    pub fn validate(&self) -> Result<(), &'static str> {
        match &self.command {
            AddonCommand::Install { bundle, .. } | AddonCommand::Update { bundle, .. } => {
                bundle.validate()
            }
            _ => Ok(()),
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum PackageWait {
    TrustApproval,
    ExactVersionApproval,
    AccessApproval,
    Compatibility,
    Quiescence,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum PackageDecision {
    Staged {
        package_id: Id,
        manifest_digest: Digest,
        waits: Vec<PackageWait>,
    },
    Activated {
        package_id: Id,
        manifest_digest: Digest,
        registry_revision: Revision,
        state_generation: Revision,
    },
    Disabled {
        package_id: Id,
        registry_revision: Revision,
    },
    Uninstalled {
        package_id: Id,
        registry_revision: Revision,
    },
    RevisionConflict {
        revision_kind: RevisionKind,
        expected: Revision,
        current: Revision,
    },
    Rejected {
        code: Id,
    },
}
pub type AddonCommandReply = CommandReply<PackageDecision>;
