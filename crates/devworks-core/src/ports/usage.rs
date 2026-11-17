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

impl CostClassification {
    pub const ALL: [Self; 6] = [
        Self::ReportedCharge,
        Self::ConfiguredEstimate,
        Self::OfficialEstimate,
        Self::SubscriptionApiEquivalent,
        Self::LocalFree,
        Self::Untracked,
    ];
    /// The stored and wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReportedCharge => "ReportedCharge",
            Self::ConfiguredEstimate => "ConfiguredEstimate",
            Self::OfficialEstimate => "OfficialEstimate",
            Self::SubscriptionApiEquivalent => "SubscriptionApiEquivalent",
            Self::LocalFree => "LocalFree",
            Self::Untracked => "Untracked",
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.as_str() == value)
    }
    /// The report `priceKind` and `priceOrigin` wire names. This is the one mapping every usage
    /// report uses; an attempt with no balance yet has no classification and reports
    /// [`UNKNOWN_PRICE`] instead.
    pub fn price(self) -> (&'static str, &'static str) {
        match self {
            Self::ReportedCharge => ("Reported", "ProviderReported"),
            Self::ConfiguredEstimate => ("Estimated", "UserConfigured"),
            Self::OfficialEstimate | Self::SubscriptionApiEquivalent => {
                ("Estimated", "OfficialSnapshot")
            }
            Self::LocalFree => ("Free", "LocalFree"),
            Self::Untracked => ("Untracked", "Untracked"),
        }
    }
    /// Estimates priced from a tariff rather than reported by the provider.
    pub fn is_estimate(self) -> bool {
        matches!(
            self,
            Self::OfficialEstimate | Self::ConfiguredEstimate | Self::SubscriptionApiEquivalent
        )
    }
}

/// `priceKind` and `priceOrigin` for an attempt that has no balance, so no classification.
pub const UNKNOWN_PRICE: (&str, &str) = ("Unknown", "Untracked");

/// The route/profile binding resolved by the host before attempt admission.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
    /// The official source the host actually consulted. Some with no snapshot records its
    /// absence; None means an override/local/custom path bypassed official publication.
    /// Old immutable quotes omit this field and retain their original representation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publication_provider: Option<OfficialPriceProvider>,
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

/// Upper bound on the tariff rows of one official catalog, before and after derivation.
pub const MAX_PUBLISHED_TARIFFS: usize = 20_000;

/// Pricing modifiers an official source declares in prose. The source adapter only reports
/// whether each declaration is present; the derived tier and regional rows are core rules.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DeclaredPriceModifiers {
    /// Cache multipliers stack with the Batch service modifier.
    pub batch_cache_multipliers_stack: bool,
    /// Cache multipliers apply on top of the Fast service price.
    pub fast_cache_multipliers_apply: bool,
    /// A 1.1x multiplier applies to requests pinned to a processing region.
    pub regional_multiplier: bool,
}

/// Fully parsed public source, staged outside a write transaction before conditional
/// publication. Snapshot IDs/revisions are assigned by the storage owner at publication.
/// `tariffs` holds the rows the source publishes; derived rows are added by
/// `services::prices::derivation` before validation.
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
    pub modifiers: DeclaredPriceModifiers,
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

#[cfg(test)]
mod quote_compatibility {
    use super::PriceQuote;
    use serde_json::json;
    #[test]
    fn old_immutable_quote_json_does_not_gain_a_publication_witness() {
        let old = json!({"classification":"Untracked","connection_revision":2,"snapshot_id":null,"price_revision":null,"override_revision":1,"currency":"USD","rates":null,"tariffs":[],"untracked_reason":"official_catalog_unavailable"});
        let quote: PriceQuote = serde_json::from_value(old.clone()).unwrap();
        assert!(quote.publication_provider.is_none());
        assert_eq!(serde_json::to_value(quote).unwrap(), old);
    }
}

#[cfg(test)]
mod classification_mapping {
    use super::{CostClassification as C, UNKNOWN_PRICE};

    /// Both usage reports (attempt rows and historical observations) use this one mapping.
    #[test]
    fn each_classification_has_one_stored_name_and_one_price_pair() {
        let table = [
            (
                C::ReportedCharge,
                "ReportedCharge",
                ("Reported", "ProviderReported"),
            ),
            (
                C::ConfiguredEstimate,
                "ConfiguredEstimate",
                ("Estimated", "UserConfigured"),
            ),
            (
                C::OfficialEstimate,
                "OfficialEstimate",
                ("Estimated", "OfficialSnapshot"),
            ),
            (
                C::SubscriptionApiEquivalent,
                "SubscriptionApiEquivalent",
                ("Estimated", "OfficialSnapshot"),
            ),
            (C::LocalFree, "LocalFree", ("Free", "LocalFree")),
            (C::Untracked, "Untracked", ("Untracked", "Untracked")),
        ];
        assert_eq!(table.len(), C::ALL.len());
        for (class, name, price) in table {
            assert_eq!(class.as_str(), name);
            assert_eq!(C::parse(name), Some(class));
            assert_eq!(class.price(), price);
        }
        assert_eq!(C::parse("Unknown"), None);
        // Only an attempt that has no balance is reported as Unknown.
        assert_eq!(UNKNOWN_PRICE, ("Unknown", "Untracked"));
    }
}
