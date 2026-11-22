//! What the host derived an artifact from, as a typed value. The host stamps `derived.kind` into
//! the provenance of artifacts it produces for its own services (Help topics, skill and role
//! versions, convention texts, embedding inputs). The artifact use cases leave exactly those
//! artifacts alone, and they decide that from [`DerivedKind`], not from a provenance string.
use crate::ports::storage::StoredArtifact;
use crate::protocol::ActorId;
use serde_json::Value;

/// The `derived.kind` values the host writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DerivedKind {
    HelpTopic,
    Instruction,
    ConventionContent,
    ConventionResource,
    ConventionImport,
    EmbeddingProbeInput,
    ConventionEmbeddingInput,
    DocumentEmbeddingInput,
    DocumentEmbeddingOutput,
    DocumentVisionInput,
    DocumentVisionOutput,
    ProviderNativeContinuation,
}

impl DerivedKind {
    /// The stored name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HelpTopic => "HelpTopic",
            Self::Instruction => "Instruction",
            Self::ConventionContent => "ConventionContent",
            Self::ConventionResource => "ConventionResource",
            Self::ConventionImport => "ConventionImport",
            Self::EmbeddingProbeInput => "EmbeddingProbeInput",
            Self::ConventionEmbeddingInput => "ConventionEmbeddingInput",
            Self::DocumentEmbeddingInput => "DocumentEmbeddingInput",
            Self::DocumentEmbeddingOutput => "DocumentEmbeddingOutput",
            Self::DocumentVisionInput => "DocumentVisionInput",
            Self::DocumentVisionOutput => "DocumentVisionOutput",
            Self::ProviderNativeContinuation => "ProviderNativeContinuation",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        [
            Self::HelpTopic,
            Self::Instruction,
            Self::ConventionContent,
            Self::ConventionResource,
            Self::ConventionImport,
            Self::EmbeddingProbeInput,
            Self::ConventionEmbeddingInput,
            Self::DocumentEmbeddingInput,
            Self::DocumentEmbeddingOutput,
            Self::DocumentVisionInput,
            Self::DocumentVisionOutput,
            Self::ProviderNativeContinuation,
        ]
        .into_iter()
        .find(|k| k.as_str() == name)
    }

    /// The kind a provenance document records, whoever produced it.
    pub fn of(provenance: &Value) -> Option<Self> {
        Self::parse(provenance["derived"]["kind"].as_str()?)
    }

    /// Whether `provenance` records `self`.
    pub fn is_in(self, provenance: &Value) -> bool {
        Self::of(provenance) == Some(self)
    }

    /// The kind of an artifact the trusted host itself produced (`host` is the host actor).
    pub fn of_host_artifact(artifact: &StoredArtifact, host: &ActorId) -> Option<Self> {
        let p = &artifact.provenance;
        (p["principalKind"] == "Host" && p["producedBy"].as_str() == Some(host.0.as_str()))
            .then(|| Self::of(p))
            .flatten()
    }

    /// A Help topic or a skill/role instruction version: the exact evidence its owner's history
    /// refers to.
    pub fn is_help_evidence(self) -> bool {
        matches!(self, Self::HelpTopic | Self::Instruction)
    }

    /// Native inference material tied to an exact managed document version.
    pub fn is_document_custody(self) -> bool {
        matches!(
            self,
            Self::DocumentEmbeddingInput
                | Self::DocumentEmbeddingOutput
                | Self::DocumentVisionInput
                | Self::DocumentVisionOutput
        )
    }

    /// A convention derivative (text, resource or embedding input).
    pub fn is_convention_derivative(self) -> bool {
        matches!(
            self,
            Self::ConventionContent
                | Self::ConventionResource
                | Self::EmbeddingProbeInput
                | Self::ConventionEmbeddingInput
        )
    }
}

#[cfg(test)]
mod tests {
    use super::DerivedKind;

    /// SQL filters and other raw readers name these kinds as literals; they must match the enum.
    #[test]
    fn stored_names_are_stable() {
        for (kind, name) in [
            (DerivedKind::HelpTopic, "HelpTopic"),
            (DerivedKind::Instruction, "Instruction"),
            (
                DerivedKind::ConventionEmbeddingInput,
                "ConventionEmbeddingInput",
            ),
            (
                DerivedKind::ProviderNativeContinuation,
                "ProviderNativeContinuation",
            ),
        ] {
            assert_eq!(kind.as_str(), name);
            assert_eq!(DerivedKind::parse(name), Some(kind));
        }
    }
}
