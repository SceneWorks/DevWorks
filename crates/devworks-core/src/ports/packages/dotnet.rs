//! Identity of the installed runtime and compiler references used by the managed .NET pack.
use super::RuntimeExecutable;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

pub const PACKAGE: &str = "devworks-dotnet-semantic";
pub const SDK_RUNTIME: &str = "dotnet-semantic-sdk";
pub const SDK_UNAVAILABLE: &str = "dotnet-sdk-unavailable";

pub fn source_path(path: &str) -> bool {
    path.len() <= 240
        && !path.contains(['\\', ':', '\0'])
        && path.split('/').all(|part| {
            !matches!(part, "" | "." | "..")
                && [".git", "bin", "obj", "node_modules", "vendor"]
                    .iter()
                    .all(|name| !part.eq_ignore_ascii_case(name))
        })
}

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

fn arm64_macho(path: &Path) -> Option<()> {
    const ARM64: u32 = 0x0100_000c;
    let mut file = std::fs::File::open(path).ok()?;
    let mut header = [0; 8];
    file.read_exact(&mut header).ok()?;
    let magic = u32::from_be_bytes(header[..4].try_into().ok()?);
    let field = header[4..].try_into().ok()?;
    if magic == 0xcffa_edfe {
        return (u32::from_le_bytes(field) == ARM64).then_some(());
    }
    let width = match magic {
        0xcafe_babe => 20,
        0xcafe_babf => 32,
        _ => return None,
    };
    let count = u32::from_be_bytes(field);
    if !(1..=32).contains(&count) {
        return None;
    }
    let mut entry = [0; 32];
    for _ in 0..count {
        file.read_exact(&mut entry[..width]).ok()?;
        if u32::from_be_bytes(entry[..4].try_into().ok()?) == ARM64 {
            return Some(());
        }
    }
    None
}

impl Runtime {
    /// Read only the canonical installation layout. Runtime selection is independent of any
    /// repository global.json and uses the most recent installed stable compatible versions.
    pub fn detect(executable: &RuntimeExecutable) -> Result<Self, &'static str> {
        if !cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            return Err("unsupported-dotnet-pack-target");
        }
        if !executable.program.is_absolute()
            || executable
                .program
                .file_name()
                .is_none_or(|name| name != "dotnet")
            || !executable.program.is_file()
        {
            return Err("dotnet-sdk-layout-unavailable");
        }
        let root = executable
            .program
            .parent()
            .ok_or("dotnet-sdk-layout-unavailable")?;
        let sdk = versions(&root.join("sdk"))
            .ok_or("dotnet-sdk-layout-unavailable")?
            .into_iter()
            .rev()
            .find(|((major, minor, patch), _)| {
                (*major, *minor) == (10, 0) && (200..300).contains(patch)
            })
            .ok_or("dotnet-sdk-version-incompatible")?
            .1;
        let runtime = versions(&root.join("shared/Microsoft.NETCore.App"))
            .ok_or("dotnet-runtime-unavailable")?
            .into_iter()
            .rev()
            .find(|((major, minor, _), _)| (*major, *minor) == (10, 0))
            .ok_or("dotnet-runtime-unavailable")?
            .1;
        let (_, fxr) = versions(&root.join("host/fxr"))
            .and_then(|mut host| host.pop_last())
            .ok_or("dotnet-runtime-unavailable")?;
        for binary in [
            &executable.program,
            &fxr.join("libhostfxr.dylib"),
            &runtime.join("libcoreclr.dylib"),
        ] {
            arm64_macho(binary).ok_or("dotnet-native-runtime-incompatible")?;
        }
        let mut witnesses = [
            sdk.join("dotnet.dll"),
            fxr.join("libhostfxr.dylib"),
            runtime.join("libhostpolicy.dylib"),
            runtime.join("libcoreclr.dylib"),
            runtime.join("Microsoft.NETCore.App.deps.json"),
        ]
        .iter()
        .map(|path| witness(root, path))
        .collect::<Option<Vec<_>>>()
        .ok_or("dotnet-runtime-unavailable")?;
        let mut references = BTreeMap::new();
        let mut reference_identities = BTreeMap::new();
        for framework in ["Microsoft.NETCore.App", "Microsoft.AspNetCore.App"] {
            let pack = root.join("packs").join(format!("{framework}.Ref"));
            if !pack.exists() {
                continue;
            }
            for ((major, minor, _), directory) in versions(&pack)
                .ok_or("dotnet-reference-pack-unavailable")?
                .into_iter()
                .rev()
            {
                let target = format!("net{major}.{minor}");
                let key = format!("{framework}/{target}");
                let refs = directory.join("ref").join(target);
                if refs.is_dir() && !references.contains_key(&key) {
                    let refs = refs
                        .canonicalize()
                        .map_err(|_| "dotnet-reference-pack-unavailable")?;
                    if !refs.starts_with(&pack) {
                        return Err("dotnet-reference-pack-unavailable");
                    }
                    witnesses.push(
                        witness(root, &directory.join("data/FrameworkList.xml"))
                            .ok_or("dotnet-reference-pack-unavailable")?,
                    );
                    let identity =
                        reference_identity(&refs).ok_or("dotnet-reference-pack-unavailable")?;
                    reference_identities.insert(key.clone(), identity);
                    references.insert(key, refs);
                }
            }
        }
        if !references.contains_key("Microsoft.NETCore.App/net10.0") {
            return Err("dotnet-reference-pack-unavailable");
        }
        Ok(Self {
            executable: executable.clone(),
            sdk_version: leaf(&sdk).ok_or("dotnet-sdk-layout-unavailable")?,
            runtime_version: leaf(&runtime).ok_or("dotnet-runtime-unavailable")?,
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
            && Self::detect(&self.executable).as_ref() == Ok(self)
    }
}
