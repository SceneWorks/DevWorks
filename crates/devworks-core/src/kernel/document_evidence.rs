//! Document passages and vision progress the storage port records for the document services.
use crate::protocol::{Id, data::Citation, data::Locator, provider::Digest};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Heading {
    pub level: u8,
    pub title: String,
}

#[derive(Debug, Clone)]
pub struct ExtractedBlock {
    pub citation: Citation,
    pub headings: Arc<[Heading]>,
    pub text: String,
}

/// Native image-only evidence that is stable across re-open and extraction retries.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum VisionTarget {
    /// The PDF worker reports this one-based page as needing a raster.
    Pdf { page: u32 },
    /// A supported embedded rendition in an active, image-only Word story paragraph.
    Word {
        locator: Locator,
        image_part: String,
        placement: u32,
        original_digest: Digest,
        raster_digest: Digest,
    },
}

/// The routed result keeps the exact producer, model and output artifact. Its text becomes
/// searchable only in a separately published immutable extraction.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VisionEvidence {
    pub operation: crate::protocol::OperationId,
    pub request: Id,
    pub attempt: Id,
    pub model: Id,
    pub connection: Id,
    pub upstream_model: String,
    pub output: crate::protocol::provider::ArtifactRef,
}

/// Durable progress for one exact occurrence from its direct extraction.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VisionUnit {
    pub extraction: Id,
    pub target: Id,
    pub origin: Id,
    pub raster: Option<crate::protocol::provider::ArtifactRef>,
    pub material: Option<VisionMaterial>,
    pub outcome: VisionOutcome,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum VisionOutcome {
    Ready,
    Blank,
    Unknown,
    Failed { diagnostic: Id },
    Complete { evidence: Box<VisionEvidence> },
}

/// The exact rendered image and prompt archive are published before native admission.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VisionMaterial {
    pub context: crate::protocol::provider::ArtifactRef,
    pub operation: crate::protocol::OperationId,
}
