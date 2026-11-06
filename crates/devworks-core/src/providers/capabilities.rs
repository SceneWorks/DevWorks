//! Host-declared support for one exact model/profile/access binding; unknown never satisfies a need.
use crate::protocol::provider::NativeBinding;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Support {
    Supported,
    Unsupported,
    Unknown,
}
impl Support {
    pub fn supported(self) -> bool {
        self == Self::Supported
    }
}
#[derive(Clone, Debug)]
pub struct Capabilities {
    pub binding: NativeBinding,
    pub images: Support,
    pub tools: Support,
    pub parallel_tools: Support,
    pub structured_output: Support,
    pub embeddings: Support,
    pub sampling: Support,
    pub top_k: Support,
    pub stop: Support,
    pub thinking: Support,
    pub prompt_cache_key: Support,
    pub prompt_cache_retention: Support,
    pub cache_breakpoints: Support,
    pub efforts: Vec<String>,
}
