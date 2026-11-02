use pdfium_render::prelude::*;
use serde_json::{Value, json};
use std::path::Path;
pub fn run(lib: &str, fixtures: &str, out: &str) -> Result<Value, Box<dyn std::error::Error>> {
    let pdfium = Pdfium::new(Pdfium::bind_to_library(
        Path::new(lib).join("lib/libpdfium.dylib"),
    )?);
    let root = Path::new(fixtures);
    let output = Path::new(out);
    std::fs::create_dir_all(output)?;
    let mut checks = Vec::new();
    let doc = pdfium.load_pdf_from_file(&root.join("mixed.pdf"), None)?;
    if doc.pages().len() != 2 {
        return Err("page count".into());
    }
    checks.push("native_pdf_page_count");
    let first = doc.pages().get(0)?;
    if !first.text()?.all().contains("DevWorks PDF fixture") {
        return Err("text extraction".into());
    }
    checks.push("page_bound_text_extraction");
    let second = doc.pages().get(1)?;
    if !second.text()?.all().trim().is_empty() {
        return Err("scan invented text".into());
    }
    checks.push("image_only_page_reports_no_extracted_text");
    let bitmap = second.render_with_config(&PdfRenderConfig::new().set_target_width(800))?;
    let im = bitmap.as_image()?;
    if im.width() != 800 || im.height() == 0 {
        return Err("render dimensions".into());
    }
    im.save(output.join("scan-page.png"))?;
    checks.push("native_scan_page_raster_for_routed_vision");
    if pdfium
        .load_pdf_from_file(&root.join("encrypted.pdf"), None)
        .is_ok()
    {
        return Err("encryption bypass".into());
    }
    checks.push("encrypted_pdf_requires_password");
    if pdfium
        .load_pdf_from_file(&root.join("encrypted.pdf"), Some("fixture-pass"))
        .is_err()
    {
        return Err("password open".into());
    }
    checks.push("explicit_password_opens_owned_fixture");
    if pdfium
        .load_pdf_from_file(&root.join("broken.pdf"), None)
        .is_ok()
    {
        return Err("broken accepted".into());
    }
    checks.push("malformed_pdf_returns_error");
    if Pdfium::bind_to_library(root.join("missing-library")).is_ok() {
        return Err("unexpected library".into());
    }
    checks.push("explicit_missing_library_does_not_fall_back_to_system");
    Ok(
        json!({"host":"macOS arm64","pdfiumBuild":"7881","pdfiumRender":"0.9.5","passed":checks.len(),"checks":checks,"renderWidth":im.width(),"renderHeight":im.height(),"noInference":true,"limits":["Narrow synthetic PDF fixtures; no hostile corpus, parser confinement, target packaging, full reading order/fonts/forms or vision-model execution.","The downloaded unsigned experiment asset is not approved shipping provenance."]}),
    )
}
