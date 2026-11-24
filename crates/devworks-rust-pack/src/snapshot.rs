use devworks_core::fingerprint::sha256_hex;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

const MAX_SOURCE_FILES: usize = 4096;
const MAX_SOURCE_BYTES: usize = 4 * 1024 * 1024;
const MAX_CACHE_BYTES: u64 = 128 * 1024 * 1024;
// Default crates.io sparse index for the pinned Cargo toolchain.
const CRATES_IO_CACHE: &str = "index.crates.io-1949cf8c6b5b557f";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Source {
    repository_id: String,
    commit: String,
    capture_digest: Option<String>,
    source_mode: SourceMode,
    files: Vec<File>,
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
    text: String,
}

#[derive(Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Language {
    Rust,
    Cargo,
}

impl Source {
    pub(crate) fn parse(bytes: &[u8]) -> Result<Self, &'static str> {
        let value = devworks_core::framing::parse(bytes).map_err(|_| "invalid-source")?;
        let source: Self = serde_json::from_value(value).map_err(|_| "invalid-source")?;
        let _ = source.source_mode;
        if source.repository_id.is_empty()
            || source.commit.len() != 40 && source.commit.len() != 64
            || !source.commit.bytes().all(|byte| byte.is_ascii_hexdigit())
            || source.capture_digest.as_ref().is_some_and(|digest| {
                digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
            || source.files.is_empty()
            || source.files.len() > MAX_SOURCE_FILES
        {
            return Err("invalid-source");
        }
        let mut aliases = BTreeSet::new();
        let mut total = 0usize;
        for file in &source.files {
            total = total.saturating_add(file.text.len());
            if !relative_file(&file.unit)
                || !aliases.insert(file.unit.to_lowercase())
                || sha256_hex(file.text.as_bytes()) != file.sha256
            {
                return Err("invalid-source");
            }
            let name = file.unit.rsplit('/').next().unwrap_or_default();
            if !match file.language {
                Language::Rust => name.ends_with(".rs"),
                Language::Cargo => matches!(name, "Cargo.toml" | "Cargo.lock"),
            } {
                return Err("invalid-source");
            }
        }
        if total > MAX_SOURCE_BYTES {
            return Err("source-budget");
        }
        Ok(source)
    }

    pub(crate) fn rust_files(&self) -> BTreeMap<&str, &str> {
        self.files
            .iter()
            .filter(|file| file.language == Language::Rust)
            .map(|file| (file.unit.as_str(), file.text.as_str()))
            .collect()
    }

    pub(crate) fn materialize(&self, destination: &Path) -> Result<Vec<PathBuf>, &'static str> {
        let mut projects = Vec::new();
        for file in &self.files {
            let relative = Path::new(&file.unit);
            if relative.file_name().and_then(|name| name.to_str()) == Some("Cargo.toml") {
                let directory = relative.parent().ok_or("invalid-manifest")?;
                let validated = toml::from_str(&file.text)
                    .map_err(|_| "invalid-manifest")
                    .and_then(|value| verify_manifest(directory, &value));
                if let Err(error) = validated {
                    if directory.as_os_str().is_empty() {
                        return Err(error);
                    }
                    // Nested test fixtures can use invalid Cargo syntax intentionally. If a
                    // real workspace depends on this manifest, Cargo will report it missing.
                    continue;
                }
                projects.push(directory.to_path_buf());
            }
            let path = destination.join(relative);
            let parent = path.parent().ok_or("invalid-source")?;
            std::fs::create_dir_all(parent).map_err(|_| "scratch-write")?;
            use std::io::Write;
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .and_then(|mut output| output.write_all(file.text.as_bytes()))
                .map_err(|_| "scratch-write")?;
        }
        projects.sort();
        if projects.is_empty() {
            Err("cargo-manifest-missing")
        } else {
            Ok(projects)
        }
    }

    pub(crate) fn copy_cached_dependencies(
        &self,
        workspace: &Path,
        original_home: &Path,
        cargo_home: &Path,
    ) -> Result<bool, &'static str> {
        let mut dependencies = BTreeSet::new();
        let lock_path = workspace.join("Cargo.lock");
        let Some(lock) = self
            .files
            .iter()
            .find(|file| Path::new(&file.unit) == lock_path)
        else {
            return Ok(false);
        };
        let lock: Lockfile = toml::from_str(&lock.text).map_err(|_| "invalid-cargo-lock")?;
        for package in lock.package {
            match package.source.as_deref() {
                None => {}
                Some("registry+https://github.com/rust-lang/crates.io-index") => {
                    if !package
                        .name
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
                        || package.name.is_empty()
                        || package.name.len() > 64
                        || !valid_version(&package.version)
                    {
                        return Err("invalid-cargo-lock");
                    }
                    dependencies.insert((package.name, package.version));
                }
                Some(_) => return Err("unsupported-offline-dependency"),
            }
        }
        if dependencies.is_empty() {
            return Ok(true);
        }
        let registry = original_home.join(".cargo/registry");
        let mut copied = 0;
        let mut indices = BTreeSet::new();
        for (name, version) in dependencies {
            let index = PathBuf::from("index")
                .join(CRATES_IO_CACHE)
                .join(".cache")
                .join(sparse_path(&name.to_ascii_lowercase()));
            if indices.insert(index.clone()) {
                copy_regular(&registry, &cargo_home.join("registry"), &index, &mut copied)?;
            }
            let archive = PathBuf::from("cache")
                .join(CRATES_IO_CACHE)
                .join(format!("{name}-{version}.crate"));
            copy_regular(
                &registry,
                &cargo_home.join("registry"),
                &archive,
                &mut copied,
            )?;
        }
        copy_regular(
            &registry,
            &cargo_home.join("registry"),
            &PathBuf::from("index")
                .join(CRATES_IO_CACHE)
                .join("config.json"),
            &mut copied,
        )?;
        Ok(true)
    }
}

#[derive(Deserialize)]
struct Lockfile {
    #[serde(default)]
    package: Vec<LockedPackage>,
}

#[derive(Deserialize)]
struct LockedPackage {
    name: String,
    version: String,
    source: Option<String>,
}

fn valid_version(version: &str) -> bool {
    !version.is_empty()
        && version.len() <= 128
        && version
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'+' | b'-'))
}

fn sparse_path(name: &str) -> PathBuf {
    let prefix = match name.len() {
        1 => "1".into(),
        2 => "2".into(),
        3 => format!("3/{}", &name[..1]),
        _ => format!("{}/{}", &name[..2], &name[2..4]),
    };
    PathBuf::from(prefix).join(name)
}

fn copy_regular(
    from: &Path,
    to: &Path,
    relative: &Path,
    total: &mut u64,
) -> Result<(), &'static str> {
    use std::io::Read;
    let base = from
        .canonicalize()
        .map_err(|_| "offline-dependency-missing")?;
    let input = base.join(relative);
    let canonical = input
        .canonicalize()
        .map_err(|_| "offline-dependency-missing")?;
    if canonical != input {
        return Err("unsafe-cargo-cache");
    }
    let file = std::fs::File::open(input).map_err(|_| "offline-dependency-missing")?;
    if !file
        .metadata()
        .map_err(|_| "offline-dependency-missing")?
        .is_file()
    {
        return Err("unsafe-cargo-cache");
    }
    let mut bytes = Vec::new();
    let remaining = MAX_CACHE_BYTES.saturating_sub(*total);
    file.take(remaining + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "offline-dependency-missing")?;
    if bytes.len() as u64 > remaining {
        return Err("offline-cache-budget");
    }
    *total += bytes.len() as u64;
    let path = to.join(relative);
    std::fs::create_dir_all(path.parent().ok_or("scratch-write")?).map_err(|_| "scratch-write")?;
    std::fs::write(path, bytes).map_err(|_| "scratch-write")
}

fn relative_file(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 240
        && !path.contains(['\\', ':', '\0'])
        && path.split('/').all(|part| !matches!(part, "" | "." | ".."))
}

fn contained(directory: &Path, route: &str) -> bool {
    if route.is_empty() || route.contains(['\\', ':', '\0']) {
        return false;
    }
    let mut depth = directory.components().count();
    Path::new(route)
        .components()
        .all(|component| match component {
            Component::Normal(_) => {
                depth += 1;
                true
            }
            Component::ParentDir if depth > 0 => {
                depth -= 1;
                true
            }
            Component::CurDir => true,
            _ => false,
        })
}

fn verify_manifest(directory: &Path, value: &toml::Value) -> Result<(), &'static str> {
    if let Some(workspace) = value.get("workspace") {
        for key in ["members", "exclude", "default-members"] {
            if workspace
                .get(key)
                .and_then(toml::Value::as_array)
                .is_some_and(|items| {
                    items.iter().any(|item| {
                        item.as_str()
                            .is_some_and(|route| !contained(directory, route))
                    })
                })
            {
                return Err("cargo-path-outside-source");
            }
        }
    }
    verify_manifest_routes(directory, value)
}

fn verify_manifest_routes(directory: &Path, value: &toml::Value) -> Result<(), &'static str> {
    match value {
        toml::Value::Table(table) => {
            for (key, nested) in table {
                if let Some(route) = nested.as_str()
                    && matches!(
                        key.as_str(),
                        "path" | "workspace" | "build" | "readme" | "license-file"
                    )
                    && !contained(directory, route)
                {
                    return Err("cargo-path-outside-source");
                }
                if key != "metadata" {
                    verify_manifest_routes(directory, nested)?;
                }
            }
        }
        toml::Value::Array(values) => {
            for value in values {
                verify_manifest_routes(directory, value)?;
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn workspace_paths_and_package_exclusion_globs_have_distinct_rules() {
        let package = toml::from_str("[package]\nexclude=['/ci']").unwrap();
        assert_eq!(verify_manifest(Path::new(""), &package), Ok(()));
        let workspace = toml::from_str("[workspace]\nmembers=['/outside']").unwrap();
        assert_eq!(
            verify_manifest(Path::new(""), &workspace),
            Err("cargo-path-outside-source")
        );
    }

    #[test]
    fn locked_cache_uses_the_pinned_sparse_index_and_original_archive_name() {
        let home = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let registry = home.path().join(".cargo/registry");
        std::fs::create_dir_all(registry.join("index/index.crates.io-6f17d22bba15001f")).unwrap();
        let index = registry.join("index").join(CRATES_IO_CACHE);
        let cache = registry.join("cache").join(CRATES_IO_CACHE);
        std::fs::create_dir_all(index.join(".cache/in/fl")).unwrap();
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(index.join("config.json"), b"index config").unwrap();
        std::fs::write(index.join(".cache/in/fl/inflector"), b"index entry").unwrap();
        std::fs::write(cache.join("Inflector-0.11.4.crate"), b"archive fixture").unwrap();
        let text = "[[package]]\nname='Inflector'\nversion='0.11.4'\nsource='registry+https://github.com/rust-lang/crates.io-index'";
        let source = Source::parse(
            serde_json::json!({
                "repositoryId": "repo", "commit": "a".repeat(40), "captureDigest": null,
                "sourceMode": "Repository", "files": [{
                    "unit": "Cargo.lock", "language": "cargo", "text": text,
                    "sha256": sha256_hex(text.as_bytes())
                }]
            })
            .to_string()
            .as_bytes(),
        )
        .unwrap();
        assert!(
            source
                .copy_cached_dependencies(Path::new(""), home.path(), output.path())
                .unwrap()
        );
        let archive = output
            .path()
            .join("registry/cache")
            .join(CRATES_IO_CACHE)
            .join("Inflector-0.11.4.crate");
        assert_eq!(std::fs::read(archive).unwrap(), b"archive fixture");
        assert!(
            !source
                .copy_cached_dependencies(Path::new("nested"), home.path(), output.path())
                .unwrap()
        );
    }

    #[test]
    fn cargo_sibling_dependency_stays_in_snapshot() {
        let manifest: toml::Value =
            toml::from_str("[dependencies]\nother={path='../other'}").unwrap();
        assert_eq!(verify_manifest_routes(Path::new("app"), &manifest), Ok(()));
        assert_eq!(
            verify_manifest_routes(Path::new(""), &manifest),
            Err("cargo-path-outside-source")
        );
        assert!(!relative_file("../lib.rs"));
        assert!(!relative_file("C:/lib.rs"));
    }
}
