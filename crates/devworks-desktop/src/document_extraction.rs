//! Native extractors the desktop can compose into the durable document service. The original
//! artifact is opened by the service, and the PDF subprocess receives only exact source bytes.
use devworks_adapters::documents::docx::DocxExtractor;
use devworks_core::{
    ports::process::ProcessHost,
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
    },
};
use devworks_engine::documents::pdf::PdfWorker;
use std::{collections::BTreeSet, fmt::Debug, path::Path, sync::Arc};

pub struct NativeDocumentProcessor {
    pdf: PdfWorker,
    word: DocxExtractor,
    text: TextExtractor,
}

impl NativeDocumentProcessor {
    pub fn beside_desktop(
        desktop: &Path,
        process: Arc<dyn ProcessHost>,
    ) -> Result<Self, PdfFailure> {
        Ok(Self {
            pdf: PdfWorker::beside_desktop(desktop, process)?,
            word: DocxExtractor::default(),
            text: TextExtractor::default(),
        })
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
            .run(request, PdfOperation::ReadPages, control)
            .map_err(|error| diagnostic("Pdf", error))?;
        let PdfResult::Pages { pages } = result else {
            return Err(diagnostic("Pdf", PdfFailure::InvalidRequest));
        };
        let limits = ExtractionLimits::default();
        let mut blocks = Vec::new();
        let mut gaps = BTreeSet::new();
        let mut remaining = limits.normalized_bytes;
        for page in pages {
            control.check().map_err(|error| diagnostic("Pdf", error))?;
            match page.coverage {
                PdfTextCoverage::NeedsRaster => {
                    gaps.insert("PdfPageNeedsRaster");
                }
                PdfTextCoverage::Partial => {
                    gaps.insert("PdfLayoutPartial");
                }
                PdfTextCoverage::Text => {}
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
        ))
    }
}

impl DocumentProcessor for NativeDocumentProcessor {
    fn revision(&self) -> Revision {
        Revision::new(1).expect("document extraction revision")
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
                if word
                    .images
                    .iter()
                    .any(|image| !image.vision_placements.is_empty())
                {
                    gaps.push(Id::new("WordImageNeedsVision").expect("static Word gap"));
                }
                if gaps.is_empty() && word.searchable_blocks().next().is_none() {
                    return Err(Id::new("WordEmpty").expect("static Word diagnostic"));
                }
                Ok(output(word.searchable_blocks().cloned().collect(), gaps))
            }
            _ => {
                let blocks = self
                    .text
                    .extract(input, control)
                    .map_err(|error| diagnostic("Text", error))?;
                Ok(output(blocks, vec![]))
            }
        }
    }
}

fn diagnostic(prefix: &str, reason: impl Debug) -> Id {
    let diagnostic = format!("{prefix}{reason:?}").replace(['(', ')'], "");
    Id::new(diagnostic).expect("fixed native document diagnostic")
}

fn output(blocks: Vec<ExtractedBlock>, gaps: Vec<Id>) -> DocumentExtraction {
    let state = if gaps.is_empty() {
        CoverageState::Complete
    } else {
        CoverageState::Partial
    };
    DocumentExtraction {
        blocks,
        coverage: Coverage {
            state,
            gaps,
            more_gaps: false,
        },
    }
}
