//! Pure-Rust native backend. Entry points are confined to the owned document worker.
mod text;

use super::raster::{PngError, encode_png};
use devworks_core::{
    fingerprint::sha256_hex,
    services::documents::{
        extraction::{ExtractionControl, ExtractionError, normalize_text},
        pdf::{
            INPUT_LIMIT, NORMALIZED_LIMIT, PAGE_LIMIT, PIXEL_LIMIT, PdfFailure, PdfOperation,
            PdfPageText, PdfRaster, PdfReply, PdfResult, PdfTextCoverage,
        },
    },
};
use hayro::{
    PixmapSettings, RenderCache, RenderSettings, hayro_interpret::InterpreterSettings,
    hayro_syntax::Pdf, vello_cpu::color::palette::css::WHITE,
};
use lopdf::{Document, LoadOptions};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

const STREAM_LIMIT: usize = 16 * 1024 * 1024;
const PAGE_WIDTH: f32 = 800.0;

pub fn process(
    bytes: Vec<u8>,
    operation: PdfOperation,
    control: &ExtractionControl<'_>,
) -> PdfReply {
    let source_digest = sha256_hex(&bytes);
    let result =
        extract(bytes, operation, control).unwrap_or_else(|error| PdfResult::Failed { error });
    PdfReply {
        source_digest,
        result,
    }
}

fn extract(
    bytes: Vec<u8>,
    operation: PdfOperation,
    control: &ExtractionControl<'_>,
) -> Result<PdfResult, PdfFailure> {
    check(control)?;
    if bytes.len() > INPUT_LIMIT {
        return Err(PdfFailure::InputLimit);
    }
    let doc = Document::load_mem_with_options(
        &bytes,
        LoadOptions {
            strict: true,
            max_decompressed_size: Some(STREAM_LIMIT),
            ..LoadOptions::default()
        },
    )
    .map_err(|error| parse_error(&error))?;
    check(control)?;
    if doc.is_encrypted() || doc.was_encrypted() {
        return Err(PdfFailure::Encrypted);
    }
    // Stop the traverse once it crosses our page budget.
    let count = doc.page_iter().take(PAGE_LIMIT as usize + 1).count();
    if count == 0 {
        return Err(PdfFailure::Malformed);
    }
    if count > PAGE_LIMIT as usize {
        return Err(PdfFailure::PageLimit);
    }
    match operation {
        PdfOperation::ReadPages => {
            read_pages(&doc, count, control).map(|pages| PdfResult::Pages { pages })
        }
        PdfOperation::RenderPage(page) if page == 0 || page as usize > count => {
            Err(PdfFailure::PageNotFound)
        }
        PdfOperation::RenderPage(page) => {
            let page_id = doc
                .page_iter()
                .nth(page as usize - 1)
                .ok_or(PdfFailure::PageNotFound)?;
            let decoded = doc
                .get_page_content_with_limit(page_id, STREAM_LIMIT)
                .map_err(|error| parse_error(&error))?;
            let content = lopdf::content::Content::decode_strict(&decoded)
                .map_err(|error| parse_error(&error))?;
            let no_marks = content.operations.is_empty()
                && doc
                    .get_page_annotations(page_id)
                    .is_ok_and(|annotations| annotations.is_empty());
            drop(doc);
            render_page(bytes, page, count, no_marks, control)
                .map(|raster| PdfResult::Raster { raster })
        }
    }
}

fn read_pages(
    doc: &Document,
    count: usize,
    control: &ExtractionControl<'_>,
) -> Result<Vec<PdfPageText>, PdfFailure> {
    let mut text_bytes = NORMALIZED_LIMIT;
    let mut result = Vec::with_capacity(count);
    for (index, id) in doc.page_iter().enumerate() {
        check(control)?;
        let page = index as u32 + 1;
        let content = text::page(doc, id, text_bytes, control);
        check(control)?;
        let mut coverage = PdfTextCoverage::Text;
        let mut draws_images = false;
        let text = match content {
            Ok(text::Page {
                text,
                partial,
                draws_images: images,
            }) => {
                draws_images = images;
                let text = normalize_text(text.trim_end_matches('\n'), text_bytes, control)
                    .map_err(control_error)?;
                if partial {
                    coverage = PdfTextCoverage::Partial;
                    text
                } else if text.trim().is_empty() {
                    coverage = PdfTextCoverage::NeedsRaster;
                    String::new()
                } else {
                    text
                }
            }
            Err(
                error @ (PdfFailure::StreamLimit
                | PdfFailure::OutputLimit
                | PdfFailure::Cancelled
                | PdfFailure::Deadline),
            ) => return Err(error),
            Err(_) => {
                coverage = PdfTextCoverage::Partial;
                String::new()
            }
        };
        // Interactive form values and annotations have their own semantics and never become
        // assertions from text extraction. Signal partial page coverage when they are present.
        if doc.get_page_annotations(id).is_ok_and(|annotations| {
            annotations.iter().any(|annotation| {
                annotation
                    .get(b"Subtype")
                    .and_then(lopdf::Object::as_name)
                    .is_ok_and(|name| name == b"Widget")
            })
        }) {
            coverage = PdfTextCoverage::Partial;
        }
        text_bytes -= text.len();
        result.push(PdfPageText {
            page,
            text,
            coverage,
            draws_images,
        });
    }
    check(control)?;
    Ok(result)
}

fn render_page(
    bytes: Vec<u8>,
    page: u32,
    count: usize,
    no_marks: bool,
    control: &ExtractionControl<'_>,
) -> Result<PdfRaster, PdfFailure> {
    check(control)?;
    let pdf = Pdf::new(bytes).map_err(|_| PdfFailure::RenderUnsupported)?;
    if pdf.pages().len() != count {
        return Err(PdfFailure::RenderUnsupported);
    }
    let source = &pdf.pages()[page as usize - 1];
    let (width, height) = source.render_dimensions();
    let area = f64::from(width) * f64::from(height);
    if !area.is_finite() || width <= 0.0 || height <= 0.0 {
        return Err(PdfFailure::RenderLimit);
    }
    let side_scale = PAGE_WIDTH / width.max(height / 4.0);
    let area_scale = (PIXEL_LIMIT as f64 / area).sqrt() as f32;
    let scale = side_scale.min(area_scale);
    let scaled = ((width * scale) as u32, (height * scale) as u32);
    if scaled.0 == 0 || scaled.1 == 0 || u64::from(scaled.0) * u64::from(scaled.1) > PIXEL_LIMIT {
        return Err(PdfFailure::RenderLimit);
    }
    let incomplete = Arc::new(AtomicBool::new(false));
    let reported = incomplete.clone();
    let settings = InterpreterSettings {
        warning_sink: Arc::new(move |_| reported.store(true, Ordering::Relaxed)),
        ..InterpreterSettings::default()
    };
    let pixmap = hayro::render(
        source,
        &RenderCache::new(),
        &settings,
        &RenderSettings::default(),
        &PixmapSettings {
            x_scale: scale,
            y_scale: scale,
            bg_color: WHITE,
        },
    );
    check(control)?;
    if incomplete.load(Ordering::Relaxed) {
        return Err(PdfFailure::RenderUnsupported);
    }
    if (u32::from(pixmap.width()), u32::from(pixmap.height())) != scaled {
        return Err(PdfFailure::RenderLimit);
    }
    let pixels = pixmap.data_as_u8_slice();
    let blank = pixels
        .as_chunks::<4>()
        .0
        .iter()
        .all(|pixel| *pixel == [255, 255, 255, 255]);
    let png = if blank {
        if !no_marks {
            return Err(PdfFailure::RenderUnsupported);
        }
        Vec::new()
    } else {
        encode_png(pixels, scaled.0, scaled.1).map_err(|err| match err {
            PngError::Limit => PdfFailure::RenderLimit,
            PngError::Encoding => PdfFailure::RenderUnsupported,
        })?
    };
    check(control)?;
    Ok(PdfRaster {
        page,
        width: scaled.0,
        height: scaled.1,
        png,
    })
}

fn check(control: &ExtractionControl<'_>) -> Result<(), PdfFailure> {
    control.check().map_err(control_error)
}

fn control_error(error: ExtractionError) -> PdfFailure {
    match error {
        ExtractionError::Cancelled => PdfFailure::Cancelled,
        ExtractionError::Deadline => PdfFailure::Deadline,
        _ => PdfFailure::OutputLimit,
    }
}

fn parse_error(error: &lopdf::Error) -> PdfFailure {
    match error {
        lopdf::Error::InvalidPassword | lopdf::Error::Decryption(_) => PdfFailure::Encrypted,
        lopdf::Error::Decompress(
            lopdf::DecompressError::MemoryLimitExceeded { .. }
            | lopdf::DecompressError::TooManyFilters { .. },
        ) => PdfFailure::StreamLimit,
        _ => PdfFailure::Malformed,
    }
}
