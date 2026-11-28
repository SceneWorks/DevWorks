//! Native extractors the desktop can compose into the durable document service. The original
//! artifact is opened by the service, and the PDF subprocess receives only exact source bytes.
use devworks_adapters::documents::docx::{DocxExtractor, WordDocument, WordImage};
use devworks_core::{
    ports::{process::ProcessHost, storage::StorageError},
    protocol::{
        Id, Revision,
        data::{Coverage, CoverageState, Locator},
    },
    services::documents::{
        extraction::{
            DocumentExtractor, ExtractedBlock, ExtractionControl, ExtractionInput,
            ExtractionLimits, TextExtractor, normalize_text,
        },
        indexing::{DocumentExtraction, DocumentProcessor},
        pdf::{PdfFailure, PdfOperation, PdfResult, PdfTextCoverage},
        vision::{MAX_TARGETS, RasterCapture, VisionRaster, VisionTarget},
    },
};
use devworks_engine::documents::pdf::PdfWorker;
use std::{collections::BTreeSet, fmt::Debug, path::Path, sync::Arc};

pub struct NativeDocumentProcessor {
    pdf: Option<Arc<PdfWorker>>,
    word: DocxExtractor,
    text: TextExtractor,
}

impl NativeDocumentProcessor {
    pub fn beside_desktop(
        desktop: &Path,
        process: Arc<dyn ProcessHost>,
    ) -> Result<Self, PdfFailure> {
        Ok(Self::new(Some(Arc::new(PdfWorker::beside_desktop(
            desktop, process,
        )?))))
    }

    pub(crate) fn new(pdf: Option<Arc<PdfWorker>>) -> Self {
        Self {
            pdf,
            word: DocxExtractor::default(),
            text: TextExtractor::default(),
        }
    }

    fn pdf(
        &self,
        input: ExtractionInput<'_>,
        control: &ExtractionControl<'_>,
    ) -> Result<DocumentExtraction, Id> {
        // The private protocol returns owned pages. Reuse the original binding for exact locators.
        let request = ExtractionInput { ..input };
        let result = self
            .pdf
            .as_ref()
            .ok_or_else(|| diagnostic("Pdf", PdfFailure::Unavailable))?
            .run(request, PdfOperation::ReadPages, control)
            .map_err(|error| diagnostic("Pdf", error))?;
        let PdfResult::Pages { pages } = result else {
            return Err(diagnostic("Pdf", PdfFailure::InvalidRequest));
        };
        let limits = ExtractionLimits::default();
        let mut blocks = Vec::new();
        let mut gaps = BTreeSet::new();
        let mut vision_targets = Vec::new();
        let mut remaining = limits.normalized_bytes;
        for page in pages {
            control.check().map_err(|error| diagnostic("Pdf", error))?;
            if page.coverage == PdfTextCoverage::Partial {
                gaps.insert("PdfLayoutPartial");
            }
            if page.coverage == PdfTextCoverage::NeedsRaster || page.draws_images {
                gaps.insert("PdfPageNeedsRaster");
                vision_targets.push(VisionTarget::Pdf { page: page.page });
            }
            let body = normalize_text(&page.text, remaining, control)
                .map_err(|error| diagnostic("Pdf", error))?;
            remaining -= body.len();
            let mut tail = body.as_str();
            while !tail.is_empty() {
                if blocks.len() >= limits.blocks {
                    return Err(diagnostic("Pdf", PdfFailure::OutputLimit));
                }
                let end = tail.floor_char_boundary(limits.chunk_bytes.min(tail.len()));
                let (text, rest) = tail.split_at(end);
                blocks.push(ExtractedBlock::new(
                    &input,
                    "devworks-pdf-v1",
                    Locator::Pdf { page: page.page },
                    blocks.len(),
                    Arc::from([]),
                    text.to_owned(),
                ));
                tail = rest;
            }
        }
        Ok(output(
            blocks,
            gaps.into_iter()
                .map(|gap| Id::new(gap).expect("static PDF gap"))
                .collect(),
            vision_targets,
        ))
    }
}

impl DocumentProcessor for NativeDocumentProcessor {
    fn revision(&self) -> Revision {
        Revision::new(2).expect("document extraction revision")
    }

    fn extract(
        &self,
        input: ExtractionInput<'_>,
        control: &ExtractionControl<'_>,
    ) -> Result<DocumentExtraction, Id> {
        let media = input
            .version
            .media_type
            .split(';')
            .next()
            .unwrap_or("")
            .trim();
        match media.to_ascii_lowercase().as_str() {
            "application/pdf" => self.pdf(input, control),
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => {
                let word = self
                    .word
                    .extract(input, control)
                    .map_err(|error| diagnostic("Word", error))?;
                let mut gaps: Vec<_> = word
                    .coverage_issues
                    .iter()
                    .map(|issue| diagnostic("Word", issue))
                    .collect();
                let images = word_vision_targets(&word)?;
                if images.len() > MAX_TARGETS {
                    gaps.push(Id::new("WordVisionTargetLimit").expect("static Word gap"));
                }
                let vision_targets = images
                    .into_iter()
                    .take(MAX_TARGETS)
                    .map(|(target, _)| target)
                    .collect::<Vec<_>>();
                if !vision_targets.is_empty() {
                    gaps.push(Id::new("WordImageNeedsVision").expect("static Word gap"));
                }
                if gaps.is_empty() && word.searchable_blocks().next().is_none() {
                    return Err(Id::new("WordEmpty").expect("static Word diagnostic"));
                }
                Ok(output(
                    word.searchable_blocks().cloned().collect(),
                    gaps,
                    vision_targets,
                ))
            }
            _ => {
                let blocks = self
                    .text
                    .extract(input, control)
                    .map_err(|error| diagnostic("Text", error))?;
                Ok(output(blocks, vec![], vec![]))
            }
        }
    }
    fn render_vision(
        &self,
        input: ExtractionInput<'_>,
        targets: &[VisionTarget],
        control: &ExtractionControl<'_>,
        capture: &mut RasterCapture<'_>,
    ) -> Result<(), StorageError> {
        let word = targets
            .iter()
            .any(|target| matches!(target, VisionTarget::Word { .. }))
            .then(|| {
                self.word
                    .extract(ExtractionInput { ..input }, control)
                    .map_err(|error| diagnostic("Word", error))
            });
        let images = word
            .as_ref()
            .and_then(|result| result.as_ref().ok())
            .map(word_vision_targets)
            .transpose()
            .map_err(|error| StorageError::InvalidRecord(error.as_str().into()))?;
        let images: std::collections::HashMap<_, _> = images
            .into_iter()
            .flatten()
            .map(|(target, image)| (target.id(), image))
            .collect();
        for target in targets {
            if (control.cancelled)() {
                return Err(StorageError::Cancelled);
            }
            if let Err(error) = control.check() {
                capture(target, Err(diagnostic("VisionRaster", error)))?;
                continue;
            }
            let raster = match target {
                VisionTarget::Pdf { page } => self
                    .pdf
                    .as_ref()
                    .ok_or(PdfFailure::Unavailable)
                    .and_then(|worker| {
                        worker.run(
                            ExtractionInput { ..input },
                            PdfOperation::RenderPage(*page),
                            control,
                        )
                    })
                    .and_then(|result| match result {
                        PdfResult::Raster { raster } => Ok(VisionRaster {
                            png: raster.png,
                            width: raster.width,
                            height: raster.height,
                        }),
                        _ => Err(PdfFailure::InvalidResponse),
                    })
                    .map_err(|error| diagnostic("Pdf", error)),
                VisionTarget::Word { .. } => images
                    .get(&target.id())
                    .map(|image| VisionRaster {
                        png: image.payload.as_ref().to_vec(),
                        width: image.width,
                        height: image.height,
                    })
                    .ok_or_else(|| {
                        word.as_ref()
                            .and_then(|result| result.as_ref().err().cloned())
                            .unwrap_or_else(|| {
                                Id::new("WordImageUnavailable").expect("static Word gap")
                            })
                    }),
            };
            capture(target, raster)?;
        }
        Ok(())
    }
}

fn word_vision_targets(document: &WordDocument) -> Result<Vec<(VisionTarget, &WordImage)>, Id> {
    let mut targets = Vec::new();
    for image in &document.images {
        let original_digest: devworks_core::protocol::provider::Digest = image
            .original_digest
            .clone()
            .try_into()
            .map_err(|_| Id::new("WordImageDigest").expect("static Word gap"))?;
        let raster_digest: devworks_core::protocol::provider::Digest = image
            .payload_digest
            .clone()
            .try_into()
            .map_err(|_| Id::new("WordImageDigest").expect("static Word gap"))?;
        for placement in &image.vision_placements {
            if let Some(locator) = placement.source.locator() {
                targets.push((
                    VisionTarget::Word {
                        locator,
                        image_part: image.part.clone(),
                        placement: placement.ordinal,
                        original_digest: original_digest.clone(),
                        raster_digest: raster_digest.clone(),
                    },
                    image,
                ));
            }
        }
    }
    Ok(targets)
}

fn diagnostic(prefix: &str, reason: impl Debug) -> Id {
    let diagnostic = format!("{prefix}{reason:?}").replace(['(', ')'], "");
    Id::new(diagnostic).expect("fixed native document diagnostic")
}

fn output(
    blocks: Vec<ExtractedBlock>,
    gaps: Vec<Id>,
    vision_targets: Vec<VisionTarget>,
) -> DocumentExtraction {
    let state = if gaps.is_empty() {
        CoverageState::Complete
    } else {
        CoverageState::Partial
    };
    DocumentExtraction {
        blocks,
        vision_targets,
        coverage: Coverage {
            state,
            gaps,
            more_gaps: false,
        },
    }
}
