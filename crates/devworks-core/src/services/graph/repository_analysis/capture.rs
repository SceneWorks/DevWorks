//! Common bounded reads of source captured by an exact index view.
use super::super::{budget::Budget, model};
use super::selection;
use crate::ports::storage::{ReadSnapshot, StoredUnit};
use crate::services::artifacts::{ArtifactService, lifecycle::ExactReadRefusal};
use crate::services::indexing::views::ViewLayers;
use crate::services::kernel::Failure;

const MAX_SOURCE_BYTES_PER_REPOSITORY: usize = 64 * 1024 * 1024;

#[derive(Default)]
pub(super) struct CapturedBytes {
    retained: usize,
}

impl CapturedBytes {
    pub(super) fn source(
        &mut self,
        read: &dyn ReadSnapshot,
        artifacts: &ArtifactService,
        layers: &ViewLayers,
        path: &str,
        budget: &mut Budget,
    ) -> Result<Result<(StoredUnit, String), &'static str>, Failure> {
        let Some((unit, capture)) = model::capture(read, layers, path)? else {
            return Ok(Err("CapturedSourceUnavailable"));
        };
        let count = usize::try_from(capture.byte_count).map_err(|_| selection::unavailable())?;
        if self.retained.saturating_add(count) > MAX_SOURCE_BYTES_PER_REPOSITORY {
            return Ok(Err("CapturedSourceByteLimit"));
        }
        self.retained += count;
        let mut bytes = Vec::with_capacity(count);
        match artifacts.scan_index_source(read, &capture, |segment| {
            bytes.extend_from_slice(segment);
            budget.visit()
        }) {
            Ok(()) => {}
            Err(ExactReadRefusal::Storage(error)) => return Err(error.into()),
            Err(_) => return Ok(Err("CapturedSourceUnreadable")),
        }
        Ok(String::from_utf8(bytes)
            .map(|text| (unit, text))
            .map_err(|_| "CapturedSourceEncoding"))
    }
}
