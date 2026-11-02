//! Narrow native DOCX feasibility probe. Not a Word renderer or app adapter.
use quick_xml::{Reader, events::Event};
use serde_json::json;
use std::{collections::HashSet, fs::File, io::Read, path::Path};

fn extract(path: &Path) -> Result<serde_json::Value, String> {
    if path
        .extension()
        .and_then(|x| x.to_str())
        .map(|x| x.to_ascii_lowercase())
        .as_deref()
        != Some("docx")
    {
        return Err("UnsupportedFormat".into());
    }
    let file = File::open(path).map_err(|e| e.to_string())?;
    if file.metadata().map_err(|e| e.to_string())?.len() > 50 * 1024 * 1024 {
        return Err("ArchiveTooLarge".into());
    }
    let mut archive = zip::ZipArchive::new(file).map_err(|e| format!("InvalidPackage:{e}"))?;
    if archive.len() > 2048 {
        return Err("TooManyParts".into());
    }
    let mut names = HashSet::new();
    let mut declared_total = 0u64;
    for i in 0..archive.len() {
        let entry = archive.by_index(i).map_err(|e| e.to_string())?;
        let name = entry.name().map_err(|e| e.to_string())?;
        if entry.enclosed_name().is_none() || name.contains('\\') {
            return Err("UnsafePartPath".into());
        }
        if !names.insert(name.to_ascii_lowercase()) {
            return Err("DuplicatePart".into());
        }
        declared_total = declared_total
            .checked_add(entry.size())
            .ok_or("PartSizeOverflow")?;
        if declared_total > 32 * 1024 * 1024 {
            return Err("ExpandedPackageTooLarge".into());
        }
    }
    if !names.contains("[content_types].xml") || !names.contains("word/document.xml") {
        return Err("MissingRequiredPart".into());
    }
    let part = archive
        .by_name("word/document.xml")
        .map_err(|e| e.to_string())?;
    let mut xml = Vec::new();
    part.take(8 * 1024 * 1024 + 1)
        .read_to_end(&mut xml)
        .map_err(|e| e.to_string())?;
    if xml.len() > 8 * 1024 * 1024 {
        return Err("MainPartTooLarge".into());
    }
    let mut reader = Reader::from_reader(xml.as_slice());
    reader.config_mut().check_end_names = true;
    let mut depth = 0usize;
    let mut in_text = false;
    let mut paragraph = String::new();
    let mut paragraphs = Vec::new();
    let mut table_cells = 0usize;
    loop {
        match reader
            .read_event()
            .map_err(|e| format!("MalformedXml:{e}"))?
        {
            Event::Start(e) => {
                depth += 1;
                if depth > 128 {
                    return Err("XmlDepthExceeded".into());
                }
                match e.local_name().as_ref() {
                    "p" => paragraph.clear(),
                    "t" => in_text = true,
                    "tc" => table_cells += 1,
                    _ => {}
                }
            }
            Event::Text(e) if in_text => {
                let decoded = e.xml10_content();
                paragraph
                    .push_str(&quick_xml::escape::unescape(&decoded).map_err(|e| e.to_string())?);
            }
            Event::GeneralRef(e) if in_text => {
                let name = e.xml10_content();
                let reference = format!("&{name};");
                paragraph
                    .push_str(&quick_xml::escape::unescape(&reference).map_err(|e| e.to_string())?);
            }
            Event::Empty(e) => match e.local_name().as_ref() {
                "tab" => paragraph.push('\t'),
                "br" => paragraph.push('\n'),
                _ => {}
            },
            Event::End(e) => {
                if e.local_name().as_ref() == "t" {
                    in_text = false;
                }
                if e.local_name().as_ref() == "p" {
                    paragraphs.push(json!({"locator":format!("word/document.xml#paragraph/{}", paragraphs.len()+1),"text":paragraph}));
                    paragraph = String::new();
                }
                depth = depth.checked_sub(1).ok_or("UnbalancedXml")?;
            }
            Event::DocType(_) => return Err("DtdForbidden".into()),
            Event::Eof => break,
            _ => {}
        }
    }
    if depth != 0 {
        return Err("UnclosedXml".into());
    }
    Ok(
        json!({"status":"ExtractedProbeBodyOnly","paragraphs":paragraphs,"tableCells":table_cells,
        "scope":"Main-body runs and table paragraphs only. No rendering, auxiliary stories, revisions or application publication."}),
    )
}

fn main() {
    let path = std::env::args_os()
        .nth(1)
        .expect("Usage: devworks-docx-probe PATH");
    match extract(Path::new(&path)) {
        Ok(value) => println!("{value}"),
        Err(reason) => {
            println!("{}", json!({"status":"Rejected","reason":reason}));
            std::process::exit(2);
        }
    }
}
