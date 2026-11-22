//! Native desktop extraction composition over the owned PDF worker and Word adapter.
use devworks_adapters::process::NativeProcessHost;
use devworks_core::{
    fingerprint::sha256_hex,
    protocol::{
        Counter, Id,
        provider::{ArtifactRef, Digest},
    },
    services::documents::{
        extraction::{ExtractionControl, ExtractionInput},
        indexing::DocumentProcessor,
        vision::VisionTarget,
    },
};
use devworks_desktop::document_extraction::NativeDocumentProcessor;
use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

const MIXED: &[u8] = include_bytes!("fixtures/pdf/mixed.pdf");
const ENCRYPTED: &[u8] = include_bytes!("fixtures/pdf/encrypted.pdf");

fn exact(bytes: &[u8]) -> ArtifactRef {
    ArtifactRef {
        artifact_id: Id::new("document").unwrap(),
        version_id: Id::new("original").unwrap(),
        sha256: Digest::try_from(sha256_hex(bytes)).unwrap(),
        byte_length: Counter::new(bytes.len() as i64).unwrap(),
        media_type: "application/pdf".into(),
    }
}

#[test]
fn document_index_processor_preserves_pdf_and_word_source_locators() {
    use devworks_core::protocol::data::{CitationSource, CoverageState, Locator};
    let desktop = std::env::var_os("DEVWORKS_PDF_PACKAGED_DESKTOP")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_devworks-desktop")));
    assert!(
        desktop.is_file(),
        "missing desktop executable at {}",
        desktop.display()
    );
    let processor =
        NativeDocumentProcessor::beside_desktop(&desktop, Arc::new(NativeProcessHost)).unwrap();
    let revision = processor.revision();
    let control = ExtractionControl {
        deadline: Instant::now() + Duration::from_secs(10),
        cancelled: &|| false,
    };
    let reference = exact(MIXED);
    let pdf = processor
        .extract(
            ExtractionInput {
                version: &reference,
                revision: &revision,
                bytes: MIXED,
            },
            &control,
        )
        .unwrap();
    assert!(matches!(pdf.coverage.state, CoverageState::Partial));
    assert!(
        pdf.coverage
            .gaps
            .contains(&Id::new("PdfPageNeedsRaster").unwrap())
    );
    assert_eq!(pdf.blocks.len(), 1);
    assert!(matches!(
        pdf.blocks[0].citation.locator,
        Locator::Pdf { page: 1 }
    ));
    assert!(matches!(
        pdf.vision_targets.as_slice(),
        [VisionTarget::Pdf { page: 2 }]
    ));
    let mut rendered = 0;
    processor
        .render_vision(
            ExtractionInput {
                version: &reference,
                revision: &revision,
                bytes: MIXED,
            },
            &pdf.vision_targets,
            &control,
            &mut |target, raster| {
                let raster = raster.expect("actual PDF page raster");
                assert!(matches!(target, VisionTarget::Pdf { page: 2 }));
                assert!(raster.validate(target));
                assert!(!raster.png.is_empty());
                rendered += 1;
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(rendered, 1);
    let encrypted = processor
        .extract(
            ExtractionInput {
                version: &exact(ENCRYPTED),
                revision: &revision,
                bytes: ENCRYPTED,
            },
            &control,
        )
        .err();
    assert_eq!(encrypted, Some(Id::new("PdfEncrypted").unwrap()));

    let word_bytes = include_bytes!("../../../docs/planning/devworks-requirements.docx");
    let mut source = exact(word_bytes);
    source.media_type =
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document".into();
    let word = processor
        .extract(
            ExtractionInput {
                version: &source,
                revision: &revision,
                bytes: word_bytes,
            },
            &control,
        )
        .unwrap();
    assert!(word.blocks.len() > 500);
    assert!(word.blocks.iter().all(|block| matches!(
        &block.citation.source,
        CitationSource::Managed { version, extraction_revision }
            if version.sha256 == source.sha256 && extraction_revision == &revision
    ) && matches!(
        block.citation.locator,
        Locator::Docx { .. }
    )));
}

#[test]
fn without_pdf_worker_word_still_renders_supported_image_only_placements() {
    use devworks_core::protocol::data::{CoverageState, Locator};
    let installation = tempfile::tempdir().unwrap();
    let desktop = installation.path().join("devworks-desktop");
    let processor =
        NativeDocumentProcessor::beside_desktop(&desktop, Arc::new(NativeProcessHost)).unwrap();
    let revision = processor.revision();
    let control = ExtractionControl {
        deadline: Instant::now() + Duration::from_secs(10),
        cancelled: &|| false,
    };
    let missing = processor
        .extract(
            ExtractionInput {
                version: &exact(MIXED),
                revision: &revision,
                bytes: MIXED,
            },
            &control,
        )
        .err();
    assert_eq!(missing, Some(Id::new("PdfUnavailable").unwrap()));

    // The same embedded PNG occurs beside direct text, by itself, and cropped by half.
    let bytes = include_bytes!("fixtures/docx/mixed-vision.docx");
    let mut reference = exact(bytes);
    reference.media_type =
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document".into();
    let input = ExtractionInput {
        version: &reference,
        revision: &revision,
        bytes,
    };
    let indexed = processor.extract(input, &control).unwrap();
    assert_eq!(indexed.blocks.len(), 1);
    assert_eq!(indexed.blocks[0].text, "With visible text");
    assert!(matches!(indexed.coverage.state, CoverageState::Partial));
    assert!(matches!(
        indexed.vision_targets.as_slice(),
        [VisionTarget::Word {
            locator: Locator::Docx { paragraph: 2, .. },
            placement: 2,
            ..
        }]
    ));
    let mut captured = false;
    processor
        .render_vision(
            ExtractionInput {
                version: &reference,
                revision: &revision,
                bytes,
            },
            &indexed.vision_targets,
            &control,
            &mut |target, raster| {
                let raster = raster.expect("actual Word raster");
                assert!(raster.validate(target));
                assert_eq!((raster.width, raster.height), (2, 1));
                captured = true;
                Ok(())
            },
        )
        .unwrap();
    assert!(captured);
}
