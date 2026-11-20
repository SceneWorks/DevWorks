use devworks_core::fingerprint::sha256_hex;
use devworks_core::ports::packages::swift::{Runtime, foreign_input, source_path};
use serde::Deserialize;
use std::collections::BTreeSet;

const MAX_FILES: usize = 4096;
const MAX_BYTES: usize = 4 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Source {
    repository_id: String,
    pub repository_name: String,
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
    let marker = name == "project.pbxproj" || foreign_input(name);
    let context = name == "Package.swift"
        || (name.starts_with("Package@swift-") && name.ends_with(".swift"))
        || marker;
    if marker && !file.text.is_empty() {
        return false;
    }
    match file.language.as_str() {
        "swift" => !context && name.to_ascii_lowercase().ends_with(".swift"),
        "swift-context" => context,
        _ => false,
    }
}

fn valid_files(files: &[File]) -> bool {
    if files.len() > MAX_FILES
        || files.iter().map(|file| file.text.len()).sum::<usize>() > MAX_BYTES
    {
        return false;
    }
    let mut paths = BTreeSet::new();
    files
        .iter()
        .all(|file| valid_file(file) && paths.insert(file.unit.to_lowercase()))
        && files.iter().any(|file| file.language == "swift")
}

#[cfg(test)]
pub(crate) fn fixture_file(unit: &str, language: &str, text: &str) -> File {
    File {
        unit: unit.into(),
        language: language.into(),
        sha256: sha256_hex(text.as_bytes()),
        text: text.into(),
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
            || source.repository_name.is_empty()
            || source.repository_name.chars().count() > 512
            || source.repository_name.contains('\0')
            || !(hex(&source.commit, 40) || hex(&source.commit, 64))
            || source
                .capture_digest
                .as_ref()
                .is_some_and(|value| !hex(value, 64))
            || !valid_files(&source.files)
        {
            return Err(invalid);
        }
        Ok(source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swift_source_requires_bounded_and_distinct_captured_inputs() {
        let swift = || fixture_file("Sources/Build/main.swift", "swift", "let number = 7");
        let marker = fixture_file("Sources/Build/file.h", "swift-context", "");
        assert!(valid_files(&[swift(), marker]));
        let changed = File {
            text: "let number = 8".into(),
            ..swift()
        };
        assert!(!valid_files(&[changed]));
        let alias = File {
            unit: "Sources/build/main.swift".into(),
            ..swift()
        };
        assert!(!valid_files(&[swift(), alias]));
        let invalid = fixture_file("Sources/Build/file.h", "swift", "");
        assert!(!valid_files(&[swift(), invalid]));
        let foreign = fixture_file("Sources/Build/file.h", "swift-context", "source bytes");
        assert!(!valid_files(&[swift(), foreign]));
        let too_large = fixture_file("Other.swift", "swift", &"a".repeat(MAX_BYTES + 1));
        assert!(!valid_files(&[too_large]));
        let too_many: Vec<_> = (0..=MAX_FILES)
            .map(|index| fixture_file(&format!("{index}.swift"), "swift", ""))
            .collect();
        assert!(!valid_files(&too_many));
    }
}
