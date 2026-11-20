use devworks_core::fingerprint::sha256_hex;
use devworks_core::ports::packages::dotnet::Runtime;
use serde::Deserialize;
use std::collections::BTreeSet;

const MAX_FILES: usize = 4096;
const MAX_BYTES: usize = 4 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Source {
    repository_id: String,
    commit: String,
    capture_digest: Option<String>,
    source_mode: SourceMode,
    files: Vec<File>,
    pub runtime: Runtime,
}

#[derive(Deserialize)]
enum SourceMode {
    Repository,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    unit: String,
    language: String,
    sha256: String,
    text: String,
}

fn hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

fn valid_file(file: &File) -> bool {
    let path = &file.unit;
    if path.len() > 240
        || path.contains(['\\', ':', '\0'])
        || path.split('/').any(|part| {
            matches!(part, "" | "." | "..")
                || [".git", "bin", "obj", "node_modules"]
                    .iter()
                    .any(|name| part.eq_ignore_ascii_case(name))
        })
        || file.sha256 != sha256_hex(file.text.as_bytes())
    {
        return false;
    }
    let name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    match file.language.as_str() {
        "csharp" => name.ends_with(".cs"),
        "visual-basic" => name.ends_with(".vb"),
        "dotnet-context" => {
            name.ends_with(".csproj")
                || name.ends_with(".vbproj")
                || matches!(
                    name.as_str(),
                    "directory.build.props"
                        | "directory.build.targets"
                        | "directory.packages.props"
                )
        }
        _ => false,
    }
}

impl Source {
    pub(crate) fn parse(bytes: &[u8]) -> Result<Self, &'static str> {
        let invalid = "invalid-dotnet-source";
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
            .all(|file| valid_file(file) && paths.insert(&file.unit))
            || !source
                .files
                .iter()
                .any(|file| file.language != "dotnet-context")
        {
            return Err(invalid);
        }
        Ok(source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn source_capture_must_be_unique_and_bound_to_the_literal_bytes() {
        let first = json!({
            "unit":"src/Client.vb", "language":"visual-basic",
            "text":"Public Class Example\nEnd Class", "sha256":sha256_hex(b"Public Class Example\nEnd Class"),
        });
        let mut capture = json!({
            "repositoryId":"fixture", "commit":"c".repeat(40), "captureDigest":null,
            "sourceMode":"Repository", "files":[first],
            "runtime":{
                "executable":{"program":"dotnet", "identity":"a".repeat(64)},
                "sdkVersion":"10.0.202", "runtimeVersion":"10.0.6", "runtimeDirectory":"runtime",
                "references":{}, "referenceIdentities":{}, "witnesses":[],
            },
        });
        let parse = |value: &serde_json::Value| Source::parse(&serde_json::to_vec(value).unwrap());
        assert!(parse(&capture).is_ok());
        capture["files"] = json!([first, first]);
        assert!(parse(&capture).is_err());
        capture["files"] = json!([first]);
        capture["files"][0]["unit"] = json!("src/../Client.vb");
        assert!(parse(&capture).is_err());
        capture["files"][0]["unit"] = first["unit"].clone();
        capture["files"][0]["text"] = json!("unbound content");
        assert!(parse(&capture).is_err());
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[test]
    fn runtime_binding_uses_the_latest_compatible_assets_and_detects_replacement() {
        use devworks_core::ports::packages::RuntimeExecutable;
        let staging = tempfile::tempdir().unwrap();
        let write = |name: &str, value: &str| {
            let path = staging.path().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, value).unwrap();
        };
        for path in [
            "dotnet",
            "sdk/10.0.201/dotnet.dll",
            "sdk/10.0.202/dotnet.dll",
            "sdk/10.0.203-preview/dotnet.dll",
            "sdk/11.0.100/dotnet.dll",
            "host/fxr/10.0.6/libhostfxr.dylib",
            "shared/Microsoft.NETCore.App/10.0.6/libhostpolicy.dylib",
            "shared/Microsoft.NETCore.App/10.0.6/libcoreclr.dylib",
            "shared/Microsoft.NETCore.App/10.0.6/Microsoft.NETCore.App.deps.json",
            "packs/Microsoft.NETCore.App.Ref/10.0.5/ref/net10.0/System.Runtime.dll",
            "packs/Microsoft.NETCore.App.Ref/10.0.5/data/FrameworkList.xml",
            "packs/Microsoft.NETCore.App.Ref/10.0.6/ref/net10.0/System.Runtime.dll",
            "packs/Microsoft.NETCore.App.Ref/10.0.6/data/FrameworkList.xml",
        ] {
            write(path, "fixture");
        }
        let executable = RuntimeExecutable::inspect(&staging.path().join("dotnet")).unwrap();
        let prior = Runtime::detect(&executable).unwrap();
        assert_eq!(prior.sdk_version, "10.0.202");
        assert_eq!(prior.runtime_version, "10.0.6");
        assert!(prior.references["Microsoft.NETCore.App/net10.0"].ends_with("10.0.6/ref/net10.0"));
        assert!(prior.current());
        write("sdk/10.0.204/dotnet.dll", "compatible patch");
        assert!(!prior.current());
        let refreshed = Runtime::detect(&executable).unwrap();
        assert_eq!(refreshed.sdk_version, "10.0.204");
        assert!(refreshed.current());
        write(
            "packs/Microsoft.NETCore.App.Ref/10.0.6/ref/net10.0/System.Runtime.dll",
            "replacement assembly",
        );
        assert!(!refreshed.current());
    }
}
