//! Build an inspected unsigned development bundle using the actual native bundle owner.
use devworks_adapters::packages::{BundleInspector, host_target_triple, write_bundle};
use devworks_core::{fingerprint::sha256_hex, ports::packages::PackageInspector};
use serde_json::{Value, json};
use std::path::Path;
fn build(helper: &[u8]) -> Result<Vec<u8>, String> {
    let data = json!({"type":"object"});
    let config = json!({"type":"object","properties":{},"additionalProperties":false});
    let state = config.clone();
    let schema_ref = |name: &str, schema: &Value| {
        json!({
            "schemaId":format!("demo-echo/{name}"),"revision":"1",
            "sha256":sha256_hex(devworks_core::jcs::canonicalize(schema).unwrap().as_bytes())
        })
    };
    let data_ref = schema_ref("data", &data);
    let config_ref = schema_ref("config", &config);
    let state_ref = schema_ref("state", &state);
    let entrypoint = if cfg!(windows) {
        "bin/demo-echo.exe"
    } else {
        "bin/demo-echo"
    };
    let files = [
        (entrypoint, helper.to_vec()),
        (
            "help.md",
            b"# Echo\nReturns the exact input artifact. No host mutation or inference.\n".to_vec(),
        ),
        ("schemas/data.json", serde_json::to_vec(&data).unwrap()),
        ("schemas/config.json", serde_json::to_vec(&config).unwrap()),
        ("schemas/state.json", serde_json::to_vec(&state).unwrap()),
    ];
    let manifest = json!({
        "packageId":"demo-echo","version":"1.0.0","publisherId":"local-development",
        "protocolMin":{"major":1,"minor":0},"protocolMax":{"major":1,"minor":0},
        "minimumHostVersion":"0.1.0","dependencies":[],"runtimes":[],
        "schemaAssets":[{"reference":data_ref,"path":"schemas/data.json"},{"reference":config_ref,"path":"schemas/config.json"},{"reference":state_ref,"path":"schemas/state.json"}],
        "requestedLimits":{"maxConcurrent":1,"queueCapacity":4,"maxMemoryBytes":"67108864","streamCreditBytes":4096},
        "targets":[{"targetTriple":host_target_triple(),"entrypoint":entrypoint}],
        "files":files.iter().map(|(path,bytes)|json!({"path":path,"sha256":sha256_hex(bytes),"byteLength":bytes.len().to_string()})).collect::<Vec<_>>(),
        "stateSchema":state_ref,"configurationSchema":config_ref,"helpFile":"help.md",
        "requestedPermissions":[],
        "contributions":[{"kind":"Tool","meta":{"id":"demo-echo/echo","inputSchema":data_ref,"outputSchema":data_ref,"helpTopic":"echo","required":true,"cancellation":"Cooperative"},"requestedEffect":"ReadCandidate"}]
    });
    let borrowed = files
        .iter()
        .map(|(path, bytes)| (*path, bytes.as_slice()))
        .collect::<Vec<_>>();
    let bundle = write_bundle(&serde_json::to_vec(&manifest).unwrap(), &borrowed, None);
    BundleInspector
        .inspect(&bundle)
        .map_err(|error| format!("native bundle inspection refused: {error:?}"))?;
    Ok(bundle)
}
fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.len() != 2 {
        eprintln!("usage: build-bundle <native-helper-path> <output.dwpkg>");
        std::process::exit(2);
    }
    let result = std::fs::read(Path::new(&args[0]))
        .map_err(|error| error.to_string())
        .and_then(|helper| build(&helper))
        .and_then(|bundle| std::fs::write(&args[1], bundle).map_err(|error| error.to_string()));
    if let Err(error) = result {
        eprintln!("{error}");
        std::process::exit(1);
    }
    println!(
        "Built {}. Import and inspect it before exact human approval.",
        args[1]
    );
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn manifest_files_schemas_and_help_pass_the_real_native_inspector() {
        let bundle = build(b"development helper fixture").unwrap();
        let inspected = BundleInspector.inspect(&bundle).unwrap();
        assert_eq!(inspected.inspection.package_id, "demo-echo");
        assert!(inspected.inspection.signer_key.is_none());
        assert_eq!(inspected.files.len(), 5);
    }
}
