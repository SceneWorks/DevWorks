//! Identity of the installed runtime and compiler references used by the managed .NET pack.
use super::RuntimeExecutable;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const PACKAGE: &str = "devworks-dotnet-semantic";
pub const SDK_RUNTIME: &str = "dotnet-semantic-sdk";
pub const SDK_UNAVAILABLE: &str = "dotnet-sdk-unavailable";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Runtime {
    pub executable: RuntimeExecutable,
    pub sdk_version: String,
    pub runtime_version: String,
    pub runtime_directory: PathBuf,
    pub references: BTreeMap<String, PathBuf>,
    pub reference_identities: BTreeMap<String, String>,
    pub witnesses: Vec<RuntimeExecutable>,
}

type Version = (u32, u32, u32);

fn versions(directory: &Path) -> Option<BTreeMap<Version, PathBuf>> {
    let mut found = BTreeMap::new();
    for (i, entry) in std::fs::read_dir(directory).ok()?.enumerate() {
        if i >= 128 {
            return None;
        }
        let entry = entry.ok()?;
        if !entry.file_type().ok()?.is_dir() {
            continue;
        }
        if let Some(version) = entry.file_name().to_str().and_then(version) {
            found.insert(version, entry.path());
        }
    }
    Some(found)
}

fn version(name: &str) -> Option<Version> {
    let mut parts = name.split('.').map(str::parse::<u32>);
    let value = (
        parts.next()?.ok()?,
        parts.next()?.ok()?,
        parts.next()?.ok()?,
    );
    (parts.next().is_none() && name == format!("{}.{}.{}", value.0, value.1, value.2))
        .then_some(value)
}

fn leaf(path: &Path) -> Option<String> {
    path.file_name()?.to_str().map(str::to_owned)
}

fn witness(root: &Path, path: &Path) -> Option<RuntimeExecutable> {
    let asset = RuntimeExecutable::inspect(path).ok()?;
    asset.program.starts_with(root).then_some(asset)
}

fn reference_identity(directory: &Path) -> Option<String> {
    let mut assets = BTreeMap::new();
    for (i, entry) in std::fs::read_dir(directory).ok()?.enumerate() {
        if i >= 1024 {
            return None;
        }
        let entry = entry.ok()?;
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "dll") {
            if !entry.file_type().ok()?.is_file() {
                return None;
            }
            assets.insert(leaf(&path)?, witness(directory, &path)?.identity);
        }
    }
    if assets.is_empty() {
        return None;
    }
    let bytes = serde_json::to_vec(&assets).ok()?;
    Some(crate::fingerprint::sha256_hex(&bytes))
}

impl Runtime {
    /// Read only the canonical installation layout. Runtime selection is independent of any
    /// repository global.json and uses the most recent installed stable compatible versions.
    pub fn detect(executable: &RuntimeExecutable) -> Option<Self> {
        if !cfg!(all(target_os = "macos", target_arch = "aarch64"))
            || !executable.program.is_absolute()
            || executable.program.file_name()? != "dotnet"
            || !executable.program.is_file()
        {
            return None;
        }
        let root = executable.program.parent()?;
        let sdk = versions(&root.join("sdk"))?
            .into_iter()
            .rev()
            .find(|((major, minor, patch), _)| {
                (*major, *minor) == (10, 0) && (200..300).contains(patch)
            })?
            .1;
        let runtime = versions(&root.join("shared/Microsoft.NETCore.App"))?
            .into_iter()
            .rev()
            .find(|((major, minor, _), _)| (*major, *minor) == (10, 0))?
            .1;
        let (_, fxr) = versions(&root.join("host/fxr"))?.pop_last()?;
        let mut witnesses = [
            sdk.join("dotnet.dll"),
            fxr.join("libhostfxr.dylib"),
            runtime.join("libhostpolicy.dylib"),
            runtime.join("libcoreclr.dylib"),
            runtime.join("Microsoft.NETCore.App.deps.json"),
        ]
        .iter()
        .map(|path| witness(root, path))
        .collect::<Option<Vec<_>>>()?;
        let mut references = BTreeMap::new();
        let mut reference_identities = BTreeMap::new();
        for framework in ["Microsoft.NETCore.App", "Microsoft.AspNetCore.App"] {
            let pack = root.join("packs").join(format!("{framework}.Ref"));
            if !pack.exists() {
                continue;
            }
            for ((major, minor, _), directory) in versions(&pack)?.into_iter().rev() {
                let target = format!("net{major}.{minor}");
                let key = format!("{framework}/{target}");
                let refs = directory.join("ref").join(target);
                if refs.is_dir() && !references.contains_key(&key) {
                    let refs = refs.canonicalize().ok()?;
                    if !refs.starts_with(&pack) {
                        return None;
                    }
                    witnesses.push(witness(root, &directory.join("data/FrameworkList.xml"))?);
                    let identity = reference_identity(&refs)?;
                    reference_identities.insert(key.clone(), identity);
                    references.insert(key, refs);
                }
            }
        }
        if !references.contains_key("Microsoft.NETCore.App/net10.0") {
            return None;
        }
        Some(Self {
            executable: executable.clone(),
            sdk_version: leaf(&sdk)?,
            runtime_version: leaf(&runtime)?,
            runtime_directory: runtime,
            references,
            reference_identities,
            witnesses,
        })
    }

    /// The helper rechecks the complete observation before starting the child and before
    /// returning its result. The host compares its current observation at promotion too.
    pub fn current(&self) -> bool {
        RuntimeExecutable::inspect(&self.executable.program)
            .ok()
            .as_ref()
            == Some(&self.executable)
            && Self::detect(&self.executable).as_ref() == Some(self)
    }
}
