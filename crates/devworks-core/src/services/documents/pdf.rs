//! The private PDF worker protocol. It carries immutable PDF bytes and bounded native evidence.
//! Only the host can authorize a source version or publish results.
use serde::{Deserialize, Serialize};

pub const INPUT_LIMIT: usize = 32 * 1024 * 1024;
pub const NORMALIZED_LIMIT: usize = 16 * 1024 * 1024;
pub const PAGE_LIMIT: u32 = 512;
pub const PIXEL_LIMIT: u64 = 2_000_000;
pub const RASTER_LIMIT: usize = 2 * 1024 * 1024;
pub const REQUEST_MAGIC: [u8; 4] = *b"DWP1";
pub const READY_MAGIC: [u8; 4] = *b"DWR1";
pub const WORKER_NAME: &str = "devworks-pdf-worker";
pub const WORKER_ARGUMENT: &str = "--pdf-private-v1";
pub const WORKER_SECONDS: u64 = 30;
pub const WORKER_MEMORY: usize = 512 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PdfOperation {
    ReadPages,
    RenderPage(u32),
}

impl PdfOperation {
    pub fn number(self) -> u32 {
        match self {
            Self::ReadPages => 0,
            Self::RenderPage(page) => page,
        }
    }

    pub fn from_number(page: u32) -> Result<Self, PdfFailure> {
        match page {
            0 => Ok(Self::ReadPages),
            1..=PAGE_LIMIT => Ok(Self::RenderPage(page)),
            _ => Err(PdfFailure::PageNotFound),
        }
    }

    pub fn wire_limit(self) -> usize {
        match self {
            Self::ReadPages => 6 * NORMALIZED_LIMIT + PAGE_LIMIT as usize * 256,
            Self::RenderPage(_) => 4 * RASTER_LIMIT + 4096,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PdfFailure {
    Cancelled,
    Deadline,
    InputLimit,
    OutputLimit,
    SourceMismatch,
    InvalidRequest,
    InvalidResponse,
    Unavailable,
    Busy,
    Crashed,
    CleanupUnconfirmed,
    Malformed,
    Encrypted,
    PageLimit,
    PageNotFound,
    StreamLimit,
    MemoryLimit,
    RenderLimit,
    RenderUnsupported,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PdfTextCoverage {
    Text,
    NeedsRaster,
    Partial,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PdfPageText {
    pub page: u32,
    pub text: String,
    pub coverage: PdfTextCoverage,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PdfRaster {
    pub page: u32,
    pub width: u32,
    pub height: u32,
    /// A zero-length raster is a verified blank page.
    pub png: Vec<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PdfResult {
    Pages { pages: Vec<PdfPageText> },
    Raster { raster: PdfRaster },
    Failed { error: PdfFailure },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PdfReply {
    pub source_digest: String,
    pub result: PdfResult,
}

impl PdfReply {
    pub fn validate(&self, source_digest: &str, operation: PdfOperation) -> Result<(), PdfFailure> {
        if self.source_digest != source_digest {
            return Err(PdfFailure::SourceMismatch);
        }
        let valid = match (&self.result, operation) {
            (PdfResult::Pages { pages }, PdfOperation::ReadPages) => {
                !pages.is_empty()
                    && pages.len() <= PAGE_LIMIT as usize
                    && pages.iter().enumerate().all(|(index, page)| {
                        page.page == index as u32 + 1
                            && match page.coverage {
                                PdfTextCoverage::Text => !page.text.trim().is_empty(),
                                PdfTextCoverage::NeedsRaster => page.text.is_empty(),
                                PdfTextCoverage::Partial => true,
                            }
                    })
                    && pages.iter().map(|page| page.text.len()).sum::<usize>() <= NORMALIZED_LIMIT
            }
            (PdfResult::Raster { raster }, PdfOperation::RenderPage(page)) => {
                raster.page == page
                    && raster.width != 0
                    && raster.height != 0
                    && u64::from(raster.width) * u64::from(raster.height) <= PIXEL_LIMIT
                    && raster.png.len() <= RASTER_LIMIT
                    && (raster.png.is_empty() || raster.png.starts_with(b"\x89PNG\r\n\x1a\n"))
            }
            (PdfResult::Failed { .. }, _) => true,
            _ => false,
        };
        valid.then_some(()).ok_or(PdfFailure::InvalidResponse)
    }
}
