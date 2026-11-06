//! Host-owned tariff selection and public price-source fetching. Quotes are immutable attempt
//! inputs, not permission to dispatch. No request, codec frame or remote reply can choose a
//! feature budget owner, declare a connection free, or rewrite a prior quote.
use super::storage::{ReadSnapshot, StorageError};
use crate::protocol::Id;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CostClassification {
    ReportedCharge,
    ConfiguredEstimate,
    OfficialEstimate,
    SubscriptionApiEquivalent,
    LocalFree,
    Untracked,
}

/// The route/profile binding resolved by the host before attempt admission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PriceQuery {
    pub catalog_model_id: Id,
    pub provider_model_id: Option<String>,
    pub connection_id: Id,
    pub endpoint_profile: String,
    pub access_mode: String,
    pub service_tier: Option<String>,
    pub input_tokens: Option<i64>,
    pub cache_write_ttl_ms: Option<i64>,
    pub processing_region: Option<String>,
    pub required_modalities: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheWriteRate {
    pub ttl_ms: Option<i64>,
    pub usd_per_million: String,
}

/// Convenience projection for token accounting. Missing quantities/rates remain missing;
/// `cache_writes` preserves distinct TTLs instead of choosing a silent universal multiplier.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageRates {
    pub input_usd_per_million: Option<String>,
    pub output_usd_per_million: Option<String>,
    pub cache_read_usd_per_million: Option<String>,
    pub cache_write_usd_per_million: Option<String>,
    pub cache_writes: Vec<CacheWriteRate>,
}

/// One exact published rate. Amount is a decimal USD string per `units` of `unit`; neither
/// floating-point balances nor an absent rate represented by zero are accepted conventions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishedTariff {
    pub upstream_model_id: String,
    pub service_tier: String,
    pub context_lower_exclusive: Option<i64>,
    pub context_upper_inclusive: Option<i64>,
    pub modality: String,
    pub charge_kind: String,
    pub cache_ttl_seconds: Option<i64>,
    pub unit: String,
    pub units: i64,
    pub amount_usd: String,
    pub region: Option<String>,
    pub endpoint_profile: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriceQuote {
    pub classification: CostClassification,
    /// Current connection/billing policy observed by quotation; admission checks it again.
    pub connection_revision: Option<i64>,
    pub snapshot_id: Option<Id>,
    pub price_revision: Option<i64>,
    pub override_revision: Option<i64>,
    pub currency: String,
    pub rates: Option<UsageRates>,
    /// Applicable exact unit/modality rates, retained with the admitted attempt. Settlement
    /// must not look up today's catalog or infer a modality/TTL that was never observed.
    pub tariffs: Vec<PublishedTariff>,
    pub untracked_reason: Option<String>,
}

pub trait PriceResolver: Send + Sync {
    fn quote(
        &self,
        read: &dyn ReadSnapshot,
        query: &PriceQuery,
    ) -> Result<PriceQuote, StorageError>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OfficialPriceProvider {
    OpenAi,
    Anthropic,
}
impl OfficialPriceProvider {
    pub fn id(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Anthropic => "anthropic",
        }
    }
    pub fn source_url(self) -> &'static str {
        match self {
            Self::OpenAi => "https://developers.openai.com/api/docs/pricing",
            Self::Anthropic => "https://platform.claude.com/docs/en/about-claude/pricing",
        }
    }
}

/// Fully parsed public source, staged outside a write transaction before conditional
/// publication. Snapshot IDs/revisions are assigned by the storage owner at publication.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FetchedPriceCatalog {
    pub provider: OfficialPriceProvider,
    pub source_url: String,
    pub source_digest: String,
    pub parser_version: String,
    pub fetched_at_ms: i64,
    pub verified_at_ms: i64,
    pub effective_date: Option<String>,
    pub tariffs: Vec<PublishedTariff>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PriceFetchError {
    Transport(String),
    SourceChanged(String),
    InvalidRate(String),
    Cancelled,
}

/// Public, credential-free HTTP maintenance; implementations enforce official source identity,
/// finite response/time bounds and no redirects. It never performs an inference request.
pub trait PriceSource: Send + Sync {
    fn fetch(
        &self,
        provider: OfficialPriceProvider,
        now_ms: i64,
    ) -> Result<FetchedPriceCatalog, PriceFetchError>;
}
