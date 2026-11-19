use devworks_core::fingerprint::sha256_hex;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

const MAX_SOURCE_FILES: usize = 4096;
const MAX_SOURCE_BYTES: usize = 4 * 1024 * 1024;
const MAX_CACHE_BYTES: u64 = 128 * 1024 * 1024;

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
                let value: toml::Value =
                    toml::from_str(&file.text).map_err(|_| "invalid-manifest")?;
                verify_manifest_routes(directory, &value)?;
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
        let mut roots: Vec<PathBuf> = Vec::new();
        for project in projects {
            if !roots.iter().any(|parent| project.starts_with(parent)) {
                roots.push(project);
            }
        }
        if roots.is_empty() {
            Err("cargo-manifest-missing")
        } else {
            Ok(roots)
        }
    }

    pub(crate) fn copy_cached_dependencies(
        &self,
        original_home: &Path,
        cargo_home: &Path,
    ) -> Result<(), &'static str> {
        let mut dependencies = BTreeSet::new();
        for lock in self.files.iter().filter(|file| {
            file.language == Language::Cargo && file.unit.rsplit('/').next() == Some("Cargo.lock")
        }) {
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
        }
        if dependencies.is_empty() {
            return Ok(());
        }
        let registry = original_home.join(".cargo/registry");
        let indices =
            std::fs::read_dir(registry.join("index")).map_err(|_| "offline-dependency-missing")?;
        let expected = indices
            .filter_map(Result::ok)
            .find_map(|entry| {
                let name = entry.file_name().into_string().ok()?;
                (name.starts_with("index.crates.io-") && entry.file_type().ok()?.is_dir())
                    .then_some(name)
            })
            .ok_or("offline-dependency-missing")?;
        let mut copied = 0;
        let mut indices = BTreeSet::new();
        for (name, version) in dependencies {
            let name = name.to_ascii_lowercase();
            let index = PathBuf::from("index")
                .join(&expected)
                .join(".cache")
                .join(sparse_path(&name));
            if indices.insert(index.clone()) {
                copy_regular(&registry, &cargo_home.join("registry"), &index, &mut copied)?;
            }
            let archive = PathBuf::from("cache")
                .join(&expected)
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
            &PathBuf::from("index").join(expected).join("config.json"),
            &mut copied,
        )
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
                if matches!(key.as_str(), "members" | "exclude" | "default-members")
                    && nested.as_array().is_some_and(|items| {
                        items.iter().any(|item| {
                            item.as_str()
                                .is_some_and(|route| !contained(directory, route))
                        })
                    })
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
