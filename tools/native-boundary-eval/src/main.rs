mod browser;
mod decision;
mod guards;
mod pdf;
mod ui;
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let report = match args.get(1).map(String::as_str) {
        Some("ui") => ui::run()?,
        Some("decision") => decision::run()?,
        Some("guards") => guards::run(&args[2])?,
        Some("pdf") => pdf::run(&args[2], &args[3], &args[4])?,
        Some("browser") => browser::run(&args[2], &args[3])?,
        _ => {
            return Err(
                "ui | pdf PDFIUM_DIR FIXTURES OUTPUT | browser CHROMIUM OWNED_TEMP_DIR".into(),
            );
        }
    };
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
