//! Runs the same owned native worker that ships beside the desktop binary.
use devworks_adapters::process::NativeProcessHost;
use devworks_core::{
    fingerprint::sha256_hex,
    ports::process::{NativeIdentity, Probe, ProcessError, ProcessHost, ProcessSpec, ProcessTree},
    protocol::{
        Counter, Id, Revision,
        provider::{ArtifactRef, Digest},
    },
    services::documents::{
        extraction::{ExtractionControl, ExtractionInput},
        pdf::{INPUT_LIMIT, PdfFailure, PdfOperation, PdfResult, PdfTextCoverage},
    },
};
use devworks_engine::documents::pdf::PdfWorker;
use std::{
    path::PathBuf,
    process::Command,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const MIXED: &[u8] = include_bytes!("fixtures/pdf/mixed.pdf");
const ENCRYPTED: &[u8] = include_bytes!("fixtures/pdf/encrypted.pdf");
const BROKEN: &[u8] = include_bytes!("fixtures/pdf/broken.pdf");
const PDFIUM_SCAN: &[u8] = include_bytes!("fixtures/pdf/pdfium-7881-scan.png");

fn application() -> PathBuf {
    std::env::var_os("DEVWORKS_PDF_PACKAGED_DESKTOP")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_devworks-desktop")))
}

fn worker() -> PdfWorker {
    PdfWorker::beside_desktop(&application(), Arc::new(NativeProcessHost)).unwrap()
}

fn exact(bytes: &[u8]) -> ArtifactRef {
    ArtifactRef {
        artifact_id: Id::new("document").unwrap(),
        version_id: Id::new("pdf_original").unwrap(),
        sha256: Digest::try_from(sha256_hex(bytes)).unwrap(),
        byte_length: Counter::new(bytes.len() as i64).unwrap(),
        media_type: "application/pdf".into(),
    }
}

fn run(worker: &PdfWorker, bytes: &[u8], command: PdfOperation) -> Result<PdfResult, PdfFailure> {
    worker.run(
        ExtractionInput {
            version: &exact(bytes),
            revision: &Revision::new(1).unwrap(),
            bytes,
        },
        command,
        &ExtractionControl {
            deadline: Instant::now() + Duration::from_secs(10),
            cancelled: &|| false,
        },
    )
}

#[test]
fn ad65_native_pdf_parity() {
    let worker = worker();
    let PdfResult::Pages { pages } = run(&worker, MIXED, PdfOperation::ReadPages).unwrap() else {
        panic!("expected two page records");
    };
    assert_eq!(pages.len(), 2);
    assert_eq!(pages[0].page, 1);
    assert_eq!(
        pages[0].text.trim(),
        "DevWorks PDF fixture: native page-bound text."
    );
    assert_eq!(pages[0].coverage, PdfTextCoverage::Text);
    assert_eq!(pages[1].page, 2);
    assert_eq!(pages[1].text, "");
    assert_eq!(pages[1].coverage, PdfTextCoverage::NeedsRaster);

    let PdfResult::Raster { raster } = run(&worker, MIXED, PdfOperation::RenderPage(2)).unwrap()
    else {
        panic!("expected page-two raster");
    };
    assert_eq!((raster.page, raster.width, raster.height), (2, 800, 1131));
    let observed = image::load_from_memory(&raster.png).unwrap().into_luma8();
    let reference = image::load_from_memory(PDFIUM_SCAN).unwrap().into_luma8();
    assert_eq!(observed.dimensions(), reference.dimensions());
    let mut squared_error = 0f64;
    for (&reference, &observed) in reference.as_raw().iter().zip(observed.as_raw()) {
        let delta = f64::from(reference) - f64::from(observed);
        squared_error += delta * delta;
    }
    let (recall, reference_dark) = dark_pixel_agreement(&reference, &observed);
    let (precision, observed_dark) = dark_pixel_agreement(&observed, &reference);
    let rmse = (squared_error / observed.as_raw().len() as f64).sqrt();
    eprintln!(
        "AD65 native scan: one-pixel agreement {recall:.6}/{precision:.6}, luma RMSE {rmse:.3}, {} PNG bytes",
        raster.png.len()
    );
    // Require agreement in both directions so shifted/missing marks and invented marks fail.
    assert!(recall > 0.999 && precision > 0.999);
    assert!(reference_dark > 100 && reference_dark.abs_diff(observed_dark) * 100 < reference_dark);
    assert!(rmse < 10.0);

    assert_eq!(
        run(&worker, ENCRYPTED, PdfOperation::ReadPages).unwrap_err(),
        PdfFailure::Encrypted
    );
    assert_eq!(
        run(&worker, BROKEN, PdfOperation::ReadPages).unwrap_err(),
        PdfFailure::Malformed
    );
    assert_eq!(
        run(&worker, MIXED, PdfOperation::RenderPage(3)).unwrap_err(),
        PdfFailure::PageNotFound
    );
}

fn dark_pixel_agreement(reference: &image::GrayImage, observed: &image::GrayImage) -> (f64, usize) {
    let mut total = 0;
    let mut matched = 0;
    for (x, y, color) in reference.enumerate_pixels() {
        if color.0[0] >= 128 {
            continue;
        }
        total += 1;
        let agrees = (y.saturating_sub(1)..=(y + 1).min(observed.height() - 1)).any(|row| {
            (x.saturating_sub(1)..=(x + 1).min(observed.width() - 1))
                .any(|column| observed.get_pixel(column, row).0[0] < 128)
        });
        matched += usize::from(agrees);
    }
    (matched as f64 / total as f64, total)
}

#[test]
fn installed_sidecar_has_an_enforced_allocation_budget() {
    let desktop = application();
    let name = format!("devworks-pdf-worker{}", std::env::consts::EXE_SUFFIX);
    let worker_path = desktop.parent().unwrap().join(name);
    let handled = Command::new(worker_path)
        .arg("--check-pdf-worker-memory")
        .status()
        .unwrap();
    assert!(
        handled.success(),
        "the actual worker must refuse an oversized allocation"
    );
}

#[test]
fn source_identity_size_and_missing_worker_fail_before_publication() {
    let worker = worker();
    let bad = exact(ENCRYPTED);
    let control = ExtractionControl {
        deadline: Instant::now() + Duration::from_secs(10),
        cancelled: &|| false,
    };
    let error = worker
        .run(
            ExtractionInput {
                version: &bad,
                revision: &Revision::new(1).unwrap(),
                bytes: MIXED,
            },
            PdfOperation::ReadPages,
            &control,
        )
        .unwrap_err();
    assert_eq!(error, PdfFailure::SourceMismatch);
    let too_large = vec![0; INPUT_LIMIT + 1];
    assert_eq!(
        run(&worker, &too_large, PdfOperation::ReadPages).unwrap_err(),
        PdfFailure::InputLimit
    );
    let isolated = tempfile::tempdir().unwrap();
    let absent = PdfWorker::beside_desktop(
        &isolated.path().join("devworks-desktop"),
        Arc::new(NativeProcessHost),
    )
    .unwrap();
    assert_eq!(
        run(&absent, MIXED, PdfOperation::ReadPages).unwrap_err(),
        PdfFailure::Unavailable
    );
}

struct FaultHost {
    mode: &'static str,
    child: Mutex<Option<NativeIdentity>>,
}

impl ProcessHost for FaultHost {
    fn platform(&self) -> &'static str {
        NativeProcessHost.platform()
    }
    fn environment(&self) -> Vec<(String, String)> {
        NativeProcessHost.environment()
    }
    fn spawn(&self, spec: &ProcessSpec) -> Result<Box<dyn ProcessTree>, ProcessError> {
        let mut spec = spec.clone();
        spec.args[0] = self.mode.into();
        let child = NativeProcessHost.spawn(&spec)?;
        *self.child.lock().unwrap() = Some(child.identity().clone());
        Ok(child)
    }
    fn current_identity(&self) -> Option<NativeIdentity> {
        NativeProcessHost.current_identity()
    }
    fn probe(&self, identity: &NativeIdentity) -> Probe {
        NativeProcessHost.probe(identity)
    }
    fn terminate(&self, identity: &NativeIdentity) -> Probe {
        NativeProcessHost.terminate(identity)
    }
}

#[test]
fn native_crash_timeout_and_cancel_reap_the_worker() {
    // Faults are test-only, on the same native tree and pipe. The packaged binary tested by the
    // parity case is never asked to provide either fault mode.
    for (mode, expected) in [
        ("--test-pdf-crash", PdfFailure::Crashed),
        ("--test-pdf-stall", PdfFailure::Deadline),
        ("--test-pdf-stall", PdfFailure::Cancelled),
    ] {
        let host = Arc::new(FaultHost {
            mode,
            child: Mutex::new(None),
        });
        let test_desktop = PathBuf::from(env!("CARGO_BIN_EXE_devworks-desktop"));
        let worker = PdfWorker::beside_desktop(&test_desktop, host.clone()).unwrap();
        let cancelled =
            || expected == PdfFailure::Cancelled && host.child.lock().unwrap().is_some();
        let timeout = if expected == PdfFailure::Deadline {
            500
        } else {
            10_000
        };
        let control = ExtractionControl {
            deadline: Instant::now() + Duration::from_millis(timeout),
            cancelled: &cancelled,
        };
        let source = exact(MIXED);
        let error = worker
            .run(
                ExtractionInput {
                    version: &source,
                    revision: &Revision::new(1).unwrap(),
                    bytes: MIXED,
                },
                PdfOperation::ReadPages,
                &control,
            )
            .unwrap_err();
        assert_eq!(error, expected);
        let child = host
            .child
            .lock()
            .unwrap()
            .clone()
            .expect("native child was launched");
        assert_eq!(host.probe(&child), Probe::Gone);
    }
}

fn authored(objects: &[Vec<u8>]) -> Vec<u8> {
    use std::io::Write;
    let mut pdf = b"%PDF-1.7\n%\xc4\xd5\xe6\xf7\n".to_vec();
    let mut offsets = Vec::new();
    for (index, object) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        writeln!(pdf, "{} 0 obj", index + 1).unwrap();
        pdf.extend_from_slice(object);
        pdf.extend_from_slice(b"\nendobj\n");
    }
    let xref = pdf.len();
    writeln!(pdf, "xref\n0 {}", offsets.len() + 1).unwrap();
    pdf.extend_from_slice(b"0000000000 65535 f \n");
    for offset in &offsets {
        writeln!(pdf, "{offset:010} 00000 n ").unwrap();
    }
    writeln!(
        pdf,
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF",
        offsets.len() + 1
    )
    .unwrap();
    pdf
}

fn stream(content: &[u8]) -> Vec<u8> {
    let mut object = format!("<< /Length {} >>\nstream\n", content.len()).into_bytes();
    object.extend_from_slice(content);
    object.extend_from_slice(b"\nendstream");
    object
}

#[test]
fn native_pdf_font_rotation_blank_page_and_form_coverage() {
    let objects = [
        b"<< /Type /Catalog /Pages 2 0 R /AcroForm << /Fields [9 0 R] >> >>".to_vec(),
        b"<< /Type /Pages /Count 3 /Kids [3 0 R 4 0 R 5 0 R] >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 400 600] /Rotate 90 /Resources << /Font << /F1 6 0 R >> >> /Contents 7 0 R >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 400 600] /Resources << >> >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 400 600] /Resources << /Font << /F1 6 0 R >> >> /Contents 8 0 R /Annots [9 0 R] >>".to_vec(),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>".to_vec(),
        stream(br"BT /F1 18 Tf 20 500 Td (Caf\351) Tj 100 -35 Td (second column) Tj ET"),
        stream(br"BT /F1 14 Tf 20 500 Td (page content) Tj ET"),
        b"<< /Type /Annot /Subtype /Widget /FT /Tx /T (test) /V (form value) /Rect [20 100 140 124] /P 5 0 R >>".to_vec(),
    ];
    let data = authored(&objects);
    let worker = worker();
    let PdfResult::Pages { pages } = run(&worker, &data, PdfOperation::ReadPages).unwrap() else {
        panic!("expected page evidence");
    };
    assert_eq!(pages.len(), 3);
    assert!(pages[0].text.contains("Café") && pages[0].text.contains("second column"));
    assert_eq!(pages[0].coverage, PdfTextCoverage::Text);
    assert_eq!(pages[1].coverage, PdfTextCoverage::NeedsRaster);
    assert_eq!(pages[2].coverage, PdfTextCoverage::Partial);
    assert!(pages[2].text.contains("page content") && !pages[2].text.contains("form value"));
    let PdfResult::Raster { raster } = run(&worker, &data, PdfOperation::RenderPage(1)).unwrap()
    else {
        panic!("expected rotated native raster");
    };
    assert_eq!((raster.page, raster.width, raster.height), (1, 800, 533));
    assert!(
        !raster.png.is_empty(),
        "standard fallback font produced visible marks"
    );
    let PdfResult::Raster { raster } = run(&worker, &data, PdfOperation::RenderPage(2)).unwrap()
    else {
        panic!("expected verified blank native raster");
    };
    assert_eq!(raster.page, 2);
    assert!(raster.png.is_empty());
}

#[test]
fn native_page_tree_bound_returns_a_typed_failure() {
    let mut objects = vec![b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), Vec::new()];
    let count = 513;
    let kids = (3..3 + count)
        .map(|id| format!("{id} 0 R "))
        .collect::<String>();
    objects[1] = format!("<< /Type /Pages /Count {count} /Kids [{kids}] >>").into_bytes();
    objects.extend((0..count).map(|_| {
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 400 600] /Resources << >> >>".to_vec()
    }));
    assert_eq!(
        run(&worker(), &authored(&objects), PdfOperation::ReadPages).unwrap_err(),
        PdfFailure::PageLimit
    );
}
