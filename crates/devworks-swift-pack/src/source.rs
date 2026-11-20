use devworks_core::fingerprint::sha256_hex;
use devworks_core::ports::packages::swift::{Runtime, foreign_input, source_path};
use serde::Deserialize;
use std::collections::BTreeSet;

const MAX_FILES: usize = 4096;
const MAX_BYTES: usize = 4 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Source {
    pub repository_id: String,
    commit: String,
    capture_digest: Option<String>,
    source_mode: SourceMode,
    pub files: Vec<File>,
    pub runtime: Runtime,
}

#[derive(Deserialize)]
enum SourceMode {
    Repository,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct File {
    pub unit: String,
    pub language: String,
    pub sha256: String,
    pub text: String,
}

fn hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

fn valid_file(file: &File) -> bool {
    let path = &file.unit;
    if !source_path(path) || file.sha256 != sha256_hex(file.text.as_bytes()) {
        return false;
    }
    let name = path.rsplit('/').next().unwrap_or(path);
    let context = matches!(name, "Package.swift" | "project.pbxproj")
        || (name.starts_with("Package@swift-") && name.ends_with(".swift"))
        || foreign_input(name);
    match file.language.as_str() {
        "swift" => !context && name.to_ascii_lowercase().ends_with(".swift"),
        "swift-context" => context,
        _ => false,
    }
}

impl Source {
    pub(crate) fn parse(bytes: &[u8]) -> Result<Self, &'static str> {
        let invalid = "invalid-swift-source";
        let value = devworks_core::framing::parse(bytes).map_err(|_| invalid)?;
        if value.get("captureDigest").is_none() {
            return Err(invalid);
        }
        let source: Self = serde_json::from_value(value).map_err(|_| invalid)?;
        let SourceMode::Repository = source.source_mode;
        if source.repository_id.is_empty()
            || source.repository_id.len() > 128
            || !(hex(&source.commit, 40) || hex(&source.commit, 64))
            || source
                .capture_digest
                .as_ref()
                .is_some_and(|value| !hex(value, 64))
            || source.files.is_empty()
            || source.files.len() > MAX_FILES
            || source
                .files
                .iter()
                .map(|file| file.text.len())
                .sum::<usize>()
                > MAX_BYTES
        {
            return Err(invalid);
        }
        let mut paths = BTreeSet::new();
        if !source
            .files
            .iter()
            .all(|file| valid_file(file) && paths.insert(file.unit.to_lowercase()))
            || !source.files.iter().any(|file| file.language == "swift")
        {
            return Err(invalid);
        }
        Ok(source)
    }
}
