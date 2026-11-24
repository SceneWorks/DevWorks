use devworks_core::fingerprint::sha256_hex;
use devworks_core::ports::packages::RuntimeExecutable;
use serde::Deserialize;
use std::collections::BTreeSet;
use std::path::Path;

const MAX_FILES: usize = 4096;
const MAX_BYTES: usize = 4 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Source {
    pub repository_id: String,
    pub runtime: Runtime,
    commit: String,
    capture_digest: Option<String>,
    source_mode: SourceMode,
    files: Vec<File>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Runtime {
    pub version: String,
    pub executable: RuntimeExecutable,
}

#[derive(Deserialize)]
enum SourceMode {
    Repository,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    unit: String,
    language: Language,
    sha256: String,
    text: Option<String>,
}

#[derive(Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum Language {
    #[serde(rename = "typescript")]
    TypeScript,
    #[serde(rename = "javascript")]
    JavaScript,
    Tsx,
    #[serde(rename = "typescript-context")]
    TypeScriptContext,
}

fn hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

fn source_path(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 240
        && !key.contains(['\\', ':', '\0'])
        && key
            .split('/')
            .all(|part| !matches!(part, "" | "." | ".." | "node_modules" | ".git"))
}

fn matches_language(path: &str, language: &Language) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    let suffix = name
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    match language {
        Language::TypeScript => matches!(suffix.as_str(), "ts" | "mts" | "cts"),
        Language::JavaScript => matches!(suffix.as_str(), "js" | "jsx" | "mjs" | "cjs"),
        Language::Tsx => suffix == "tsx",
        Language::TypeScriptContext => {
            matches!(
                name,
                "package.json" | "angular.json" | "pnpm-workspace.yaml"
            ) || suffix == "html"
                || name.starts_with("next.config.")
                || name.starts_with("vite.config.")
                || (name.starts_with("tsconfig") || name.starts_with("jsconfig"))
                    && suffix == "json"
        }
    }
}

impl Source {
    pub(crate) fn parse(bytes: &[u8]) -> Result<Self, &'static str> {
        let value =
            devworks_core::framing::parse(bytes).map_err(|_| "invalid-typescript-source")?;
        let source: Self =
            serde_json::from_value(value).map_err(|_| "invalid-typescript-source")?;
        let SourceMode::Repository = source.source_mode;
        if source.repository_id.is_empty()
            || source.repository_id.len() > 128
            || !hex(&source.commit, 40) && !hex(&source.commit, 64)
            || source
                .capture_digest
                .as_deref()
                .is_some_and(|digest| !hex(digest, 64))
            || source.files.is_empty()
            || source.files.len() > MAX_FILES
        {
            return Err("invalid-typescript-source");
        }
        let mut paths = BTreeSet::new();
        let mut total = 0usize;
        for file in &source.files {
            let valid_contents = match &file.text {
                Some(text) => {
                    total = total.saturating_add(text.len());
                    sha256_hex(text.as_bytes()) == file.sha256
                }
                None => file.language == Language::TypeScriptContext && hex(&file.sha256, 64),
            };
            if !source_path(&file.unit)
                || !matches_language(&file.unit, &file.language)
                || !paths.insert(file.unit.to_lowercase())
                || !valid_contents
            {
                return Err("invalid-typescript-source");
            }
        }
        if total > MAX_BYTES {
            return Err("typescript-source-budget");
        }
        Ok(source)
    }

    pub(crate) fn materialize(&self, root: &Path) -> Result<Selection<'_>, &'static str> {
        let mut included = Vec::with_capacity(self.files.len());
        let mut readable = Vec::with_capacity(self.files.len());
        for source in &self.files {
            included.push(source.unit.as_str());
            let Some(text) = &source.text else { continue };
            use std::io::Write;
            let path = root.join(&source.unit);
            let parent = path.parent().ok_or("invalid-typescript-source")?;
            std::fs::create_dir_all(parent).map_err(|_| "typescript-scratch-write")?;
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .and_then(|mut file| file.write_all(text.as_bytes()))
                .map_err(|_| "typescript-scratch-write")?;
            readable.push(source.unit.as_str());
        }
        Ok(Selection { included, readable })
    }
}

pub(crate) struct Selection<'a> {
    pub included: Vec<&'a str>,
    pub readable: Vec<&'a str>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn captured_paths_and_repository_mode_are_required() {
        let body = "export const a = 1;";
        let request = json!({
            "repositoryId":"repo","commit":"a".repeat(40),"captureDigest":null,
            "sourceMode":"Repository","runtime":{"version":"24.15.0",
                "executable":{"program":"node","identity":"b".repeat(64)}},
            "files":[{"unit":"ok.ts","language":"typescript","sha256":sha256_hex(body.as_bytes()),"text":body}]
        });
        let parse = |value: &serde_json::Value| Source::parse(value.to_string().as_bytes());
        assert!(parse(&request).is_ok());
        let mut batch = request.clone();
        batch["sourceMode"] = json!("Batch");
        assert!(matches!(parse(&batch), Err("invalid-typescript-source")));
        for invalid in ["../ok.ts", "node_modules/ok.ts", "src/../ok.ts"] {
            let mut value = request.clone();
            value["files"][0]["unit"] = json!(invalid);
            assert!(matches!(parse(&value), Err("invalid-typescript-source")));
        }
    }
}
