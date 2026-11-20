use super::{CONTRIBUTION, PACKAGE};
use devworks_adapters::packages::{BundleInspector, host_target_triple, write_bundle};
use devworks_core::{fingerprint::sha256_hex, ports::packages::PackageInspector};
use ed25519_dalek::SigningKey;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const MANIFEST_VERSION: &str = "1.0.0";
const ENTRYPOINT: &str = "bin/devworks-typescript-pack";
const EMPTY_SCHEMA: &[u8] = br#"{"type":"object","additionalProperties":false}"#;
const SOURCE: &[u8] = include_bytes!("../assets/source.schema.json");
const INDEX: &[u8] = include_bytes!("../assets/index.schema.json");
const HELP: &[u8] = include_bytes!("../assets/help.md");

fn schema(name: &str, bytes: &[u8]) -> Result<(Value, Vec<u8>), String> {
    let value: Value = serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
    let canonical =
        devworks_core::jcs::canonicalize(&value).map_err(|error| format!("{error:?}"))?;
    let reference = json!({"schemaId":format!("{PACKAGE}/{name}"),"revision":"1",
        "sha256":sha256_hex(canonical.as_bytes())});
    Ok((reference, canonical.into_bytes()))
}

fn build(helper: Vec<u8>, assets: &Path, key: Option<&SigningKey>) -> Result<Vec<u8>, String> {
    let (source, source_bytes) = schema("source", SOURCE)?;
    let (index, index_bytes) = schema("index-result", INDEX)?;
    let (empty, empty_bytes) = schema("empty", EMPTY_SCHEMA)?;
    let mut files = BTreeMap::from([
        (ENTRYPOINT.to_owned(), helper),
        ("help.md".to_owned(), HELP.to_vec()),
        ("schemas/source.json".to_owned(), source_bytes),
        ("schemas/index.json".to_owned(), index_bytes),
        ("schemas/empty.json".to_owned(), empty_bytes),
    ]);
    append_assets(assets, assets, &mut files)?;
    if !files.contains_key("extractor/dist/devworks.js") {
        return Err("The staged compiler entrypoint is missing.".into());
    }
    let manifest = json!({
        "packageId":PACKAGE, "publisherId":"devworks", "version":MANIFEST_VERSION,
        "protocolMin":{"major":1,"minor":0},"protocolMax":{"major":1,"minor":0},
        "minimumHostVersion":"0.1.0","dependencies":[],
        "runtimes":[{"runtimeId":"node","versionRequirement":">=24, <25","required":true}],
        "schemaAssets":[{"reference":source,"path":"schemas/source.json"},
            {"reference":index,"path":"schemas/index.json"},{"reference":empty,"path":"schemas/empty.json"}],
        "requestedLimits":{"maxConcurrent":1,"queueCapacity":2,"maxMemoryBytes":"2147483648","streamCreditBytes":262144},
        "targets":[{"targetTriple":host_target_triple(),"entrypoint":ENTRYPOINT}],
        "files":files.iter().map(|(name,bytes)|json!({"path":name,
            "sha256":sha256_hex(bytes),"byteLength":bytes.len().to_string()})).collect::<Vec<_>>(),
        "stateSchema":empty,"configurationSchema":empty,"helpFile":"help.md",
        "requestedPermissions":[{"kind":"WorktreeRead"},{"kind":"Process","runtimeId":"node"}],
        "contributions":[{"kind":"Semantic","sourceMode":"Repository","languages":["typescript","javascript","tsx"],"format":"Native",
            "meta":{"id":CONTRIBUTION,"inputSchema":source,"outputSchema":index,
                "helpTopic":"index","required":true,"cancellation":"Cooperative"}}]
    });
    let manifest = serde_json::to_vec(&manifest).map_err(|error| error.to_string())?;
    let packed: Vec<_> = files
        .iter()
        .map(|(name, bytes)| (name.as_str(), bytes.as_slice()))
        .collect();
    let bundle = write_bundle(&manifest, &packed, key);
    BundleInspector
        .inspect(&bundle)
        .map_err(|error| format!("native bundle inspection: {error:?}"))?;
    Ok(bundle)
}

fn append_assets(
    root: &Path,
    path: &Path,
    files: &mut BTreeMap<String, Vec<u8>>,
) -> Result<(), String> {
    for child in std::fs::read_dir(path).map_err(|error| error.to_string())? {
        let child = child.map_err(|error| error.to_string())?;
        let kind = child.file_type().map_err(|error| error.to_string())?;
        let path = child.path();
        if kind.is_dir() {
            append_assets(root, &path, files)?;
        } else if kind.is_file() {
            let relative = path.strip_prefix(root).map_err(|error| error.to_string())?;
            let relative = relative
                .to_str()
                .ok_or("The compiler asset path is unsupported.")?;
            files.insert(
                format!("extractor/{relative}"),
                std::fs::read(path).map_err(|error| error.to_string())?,
            );
        } else {
            return Err("Compiler assets must be regular files.".into());
        }
    }
    Ok(())
}

pub(crate) fn build_from_args() -> Result<(), String> {
    if !cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        return Err("The release recipe is available for macOS arm64.".into());
    }
    let args: Vec<_> = std::env::args_os().skip(2).map(PathBuf::from).collect();
    let [output, assets, keys @ ..] = args.as_slice() else {
        return Err(
            "Usage: devworks-rust-pack --bundle <output> <staged-assets> [signing-key-file]".into(),
        );
    };
    if keys.len() > 1 {
        return Err(
            "Provide the output path, staged compiler assets and at most one signing key file."
                .into(),
        );
    }
    let key = keys
        .first()
        .map(|key| {
            let mut bytes = Vec::new();
            std::fs::File::open(key)
                .and_then(|file| file.take(33).read_to_end(&mut bytes))
                .map_err(|_| "The signing key could not be read.")?;
            let key = <[u8; 32]>::try_from(bytes.as_slice())
                .map_err(|_| "The signing key must contain 32 bytes.")?;
            Ok::<_, &str>(SigningKey::from_bytes(&key))
        })
        .transpose()?;
    let exe = std::env::current_exe().map_err(|error| error.to_string())?;
    let helper = std::fs::read(exe).map_err(|error| error.to_string())?;
    let bytes = build(helper, assets, key.as_ref())?;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)
        .and_then(|mut file| file.write_all(&bytes))
        .map_err(|error| error.to_string())?;
    println!("{}", sha256_hex(&bytes));
    Ok(())
}
