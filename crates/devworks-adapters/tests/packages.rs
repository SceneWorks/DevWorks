//! Package bundle inspection and install directories: nothing of a bundle is trusted or written
//! before its container, paths, strict manifest, digests and signature verify; installed
//! versions are published atomically, re-verified byte for byte and collected safely.
use devworks_adapters::packages::{
    BundleIndexVerifier, BundleInspector, JsonSchemaValidator, NativeIndexTransport,
    NativePackageFiles, host_target_triple, key_fingerprint, safe_path, write_bundle, write_index,
};
use devworks_core::fingerprint::sha256_hex;
use devworks_core::ports::packages::{
    IndexSource, IndexTransport, IndexVerifier, InspectError, PackageFileError, PackageFiles,
    PackageInspector, SchemaValidator,
};
use devworks_core::ports::storage::Cancellation;
use devworks_core::ports::web::{HopRequest, HopResponse, HopTransport, NetFault, Resolver};
use ed25519_dalek::SigningKey;
use serde_json::{Value, json};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn manifest(files: &[(&str, &[u8])], extra: impl FnOnce(&mut Value)) -> Vec<u8> {
    let mut m = json!({
        "packageId": "demo-pack", "version": "1.0.0", "publisherId": "acme",
        "protocolMin": {"major": 1, "minor": 0}, "protocolMax": {"major": 1, "minor": 0},
        "minimumHostVersion": "0.1.0", "dependencies": [], "runtimes": [], "schemaAssets": [],
        "requestedLimits": {"maxConcurrent": 1, "queueCapacity": 4, "maxMemoryBytes": "1048576", "streamCreditBytes": 4096},
        "targets": [{"targetTriple": host_target_triple(), "entrypoint": "bin/helper"}],
        "files": files.iter().map(|(p, b)| json!({"path": p, "sha256": sha256_hex(b), "byteLength": b.len().to_string()})).collect::<Vec<_>>(),
        "stateSchema": {"schemaId": "demo-pack/state", "revision": "1", "sha256": "a".repeat(64)},
        "configurationSchema": {"schemaId": "demo-pack/config", "revision": "1", "sha256": "b".repeat(64)},
        "helpFile": "help.md",
        "requestedPermissions": [{"kind": "Network", "endpointId": "api"}, {"kind": "WorktreeRead"}, {"kind": "WorktreeRead"}],
        "contributions": [{"kind": "Semantic", "meta": {"id": "demo-pack/index",
            "inputSchema": {"schemaId": "demo-pack/in", "revision": "1", "sha256": "c".repeat(64)},
            "outputSchema": {"schemaId": "demo-pack/out", "revision": "1", "sha256": "d".repeat(64)},
            "helpTopic": "index", "required": true, "cancellation": "Cooperative"},
            "languages": ["csharp"], "format": "Scip"}]
    });
    extra(&mut m);
    m.to_string().into_bytes()
}
const HELPER: &[u8] = b"#!/bin/sh\nexit 0\n";
const HELP: &[u8] = b"# Demo\n";
fn files() -> Vec<(&'static str, &'static [u8])> {
    vec![("bin/helper", HELPER), ("help.md", HELP)]
}
fn signed(m: &[u8], f: &[(&str, &[u8])]) -> Vec<u8> {
    write_bundle(m, f, Some(&SigningKey::from_bytes(&[7; 32])))
}
fn refuse(b: &[u8]) -> InspectError {
    BundleInspector.inspect(b).expect_err("refused")
}

#[test]
fn a_signed_bundle_inspects_to_its_verified_facts() {
    let m = manifest(&files(), |_| {});
    let got = BundleInspector.inspect(&signed(&m, &files())).unwrap();
    let i = &got.inspection;
    assert_eq!(i.manifest_digest, sha256_hex(&m));
    assert_eq!(
        i.signer_key.as_deref(),
        Some(key_fingerprint(&SigningKey::from_bytes(&[7; 32]).verifying_key()).as_str())
    );
    // The access envelope is normalized, sorted and unique.
    assert_eq!(i.access, vec!["network:api", "worktree.read"]);
    assert_eq!(i.kind(), "LanguagePack");
    assert_eq!(i.languages(), vec!["csharp"]);
    assert!(
        got.files
            .iter()
            .find(|f| f.path == "bin/helper")
            .unwrap()
            .executable
    );
    assert!(
        !got.files
            .iter()
            .find(|f| f.path == "help.md")
            .unwrap()
            .executable
    );
    // Unsigned content is inspected as unsigned (never as signed).
    let unsigned = BundleInspector
        .inspect(&write_bundle(&m, &files(), None))
        .unwrap();
    assert!(unsigned.inspection.signer_key.is_none());
}

#[test]
fn containers_paths_digests_and_signatures_are_checked_before_anything_is_trusted() {
    let m = manifest(&files(), |_| {});
    // Container: magic, truncation, trailing data.
    assert!(matches!(
        refuse(b"not a bundle at all"),
        InspectError::Format(_)
    ));
    let good = signed(&m, &files());
    assert!(matches!(
        refuse(&good[..good.len() - 1]),
        InspectError::Format(_)
    ));
    let mut trailing = good.clone();
    trailing.push(0);
    assert!(matches!(refuse(&trailing), InspectError::Format(_)));
    // A file the manifest does not list, a missing file, a modified file, a duplicate entry.
    let mut more = files();
    more.push(("extra.txt", b"x"));
    assert!(matches!(
        refuse(&signed(&m, &more)),
        InspectError::Format(_)
    ));
    assert!(matches!(
        refuse(&signed(&m, &files()[..1])),
        InspectError::Integrity(_)
    ));
    assert!(matches!(
        refuse(&signed(
            &m,
            &[("bin/helper", b"changed"), ("help.md", HELP)]
        )),
        InspectError::Integrity(_)
    ));
    let dup = [("bin/helper", HELPER), ("help.md", HELP), ("HELP.md", HELP)];
    assert!(matches!(
        refuse(&signed(&m, &dup)),
        InspectError::Format(_) | InspectError::UnsafePath(_)
    ));
    // Unsafe archive paths are refused even when the manifest names them.
    for bad in [
        "../escape",
        "/abs",
        "a/./b",
        "dir\\file",
        "c:file",
        "con.txt",
        "trail.",
        "nul",
    ] {
        assert!(!safe_path(bad), "{bad}");
        let m = manifest(
            &[("bin/helper", HELPER), ("help.md", HELP), (bad, b"x")],
            |_| {},
        );
        let e = refuse(&signed(
            &m,
            &[("bin/helper", HELPER), ("help.md", HELP), (bad, b"x")],
        ));
        assert!(
            matches!(e, InspectError::UnsafePath(_) | InspectError::Manifest(_)),
            "{bad}: {e:?}"
        );
    }
    // Signatures: one over another manifest, a wrong algorithm, a malformed key.
    let other = manifest(&files(), |m| m["version"] = json!("1.0.1"));
    let forged = splice(&write_bundle(&m, &files(), None), &signed(&other, &files()));
    assert!(matches!(refuse(&forged), InspectError::Signature(_)));
    let block = json!({"algorithm": "rsa", "publicKey": "00", "signature": "00"}).to_string();
    assert!(matches!(
        refuse(&with_signature(&m, &files(), &block)),
        InspectError::Signature(_)
    ));
    let block = json!({"algorithm": "ed25519", "publicKey": "zz", "signature": "00"}).to_string();
    assert!(matches!(
        refuse(&with_signature(&m, &files(), &block)),
        InspectError::Signature(_)
    ));
    // The manifest is decoded strictly: duplicate members and unknown fields are refused.
    let text = String::from_utf8(m.clone()).unwrap();
    let duplicated = text.replacen(
        "\"version\":\"1.0.0\"",
        "\"version\":\"1.0.0\",\"version\":\"9.9.9\"",
        1,
    );
    assert_ne!(duplicated, text);
    assert!(matches!(
        refuse(&signed(duplicated.as_bytes(), &files())),
        InspectError::Manifest(_)
    ));
    let unknown = manifest(&files(), |m| m["installScript"] = json!("rm -rf /"));
    assert!(matches!(
        refuse(&signed(&unknown, &files())),
        InspectError::Manifest(_)
    ));
    // Structural manifest rules (an entrypoint that is not a listed file).
    let no_entry = manifest(&files(), |m| {
        m["targets"][0]["entrypoint"] = json!("bin/missing")
    });
    assert!(matches!(
        refuse(&signed(&no_entry, &files())),
        InspectError::Manifest(_)
    ));
}

/// Replaces the signature block of `bundle` with the one of `donor`.
fn splice(bundle: &[u8], donor: &[u8]) -> Vec<u8> {
    let at = |b: &[u8]| 12 + u32::from_le_bytes(b[8..12].try_into().unwrap()) as usize;
    let len = |b: &[u8], a: usize| 4 + u32::from_le_bytes(b[a..a + 4].try_into().unwrap()) as usize;
    let (a, da) = (at(bundle), at(donor));
    let mut out = bundle[..a].to_vec();
    out.extend_from_slice(&donor[da..da + len(donor, da)]);
    out.extend_from_slice(&bundle[a + len(bundle, a)..]);
    out
}
fn with_signature(m: &[u8], f: &[(&str, &[u8])], block: &str) -> Vec<u8> {
    let mut donor = Vec::new();
    donor.extend_from_slice(&write_bundle(b"{}", &[], None)[..8]);
    donor.extend_from_slice(&2u32.to_le_bytes());
    donor.extend_from_slice(b"{}");
    donor.extend_from_slice(&(block.len() as u32).to_le_bytes());
    donor.extend_from_slice(block.as_bytes());
    splice(&write_bundle(m, f, None), &donor)
}

fn root() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let r = std::env::temp_dir().join(format!(
        "devworks-package-files-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&r);
    r
}

#[test]
fn install_directories_publish_atomically_and_detect_modification() {
    let r = root();
    let store = NativePackageFiles::new(r.clone());
    let m = manifest(&files(), |_| {});
    let b = BundleInspector.inspect(&signed(&m, &files())).unwrap();
    let (pkg, digest) = (
        b.inspection.package_id.clone(),
        b.inspection.manifest_digest.clone(),
    );
    store
        .publish(&pkg, &digest, &b.files, &b.inspection.files)
        .unwrap();
    // Idempotent: publishing the same contents again keeps the verified directory.
    store
        .publish(&pkg, &digest, &b.files, &b.inspection.files)
        .unwrap();
    store.verify(&pkg, &digest, &b.inspection.files).unwrap();
    let guide = b
        .inspection
        .files
        .iter()
        .find(|file| file.path == b.inspection.help_file)
        .unwrap();
    let guide_bytes = &b
        .files
        .iter()
        .find(|file| file.path == guide.path)
        .unwrap()
        .bytes;
    assert_eq!(
        store
            .read_verified(&pkg, &digest, guide, guide.byte_length)
            .unwrap(),
        *guide_bytes
    );
    assert!(
        store
            .read_verified(&pkg, &digest, guide, guide.byte_length - 1)
            .is_err()
    );
    assert_eq!(
        store.installed().unwrap(),
        vec![(pkg.clone(), digest.clone())]
    );
    assert!(store.idle_for(&pkg, &digest).unwrap() < Duration::from_secs(60));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(store.path(&pkg, &digest, "bin/helper"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0o111);
    }
    // A changed byte and an added file are both modifications of signed content.
    std::fs::write(store.path(&pkg, &digest, "help.md"), b"# Tampered\n").unwrap();
    assert!(matches!(
        store.read_verified(&pkg, &digest, guide, 1024),
        Err(PackageFileError::Mismatch(_))
    ));
    assert!(matches!(
        store.verify(&pkg, &digest, &b.inspection.files),
        Err(PackageFileError::Mismatch(_))
    ));
    // Republishing replaces the damaged copy with the verified bytes.
    store
        .publish(&pkg, &digest, &b.files, &b.inspection.files)
        .unwrap();
    store.verify(&pkg, &digest, &b.inspection.files).unwrap();
    #[cfg(unix)]
    {
        // Even identical signed bytes through an external symlink must not be followed.
        let external = r.with_extension("outside-guide");
        std::fs::write(&external, guide_bytes).unwrap();
        let help = store.path(&pkg, &digest, &guide.path);
        std::fs::remove_file(&help).unwrap();
        std::os::unix::fs::symlink(&external, &help).unwrap();
        assert!(store.read_verified(&pkg, &digest, guide, 1024).is_err());
        std::fs::remove_file(&help).unwrap();
        std::fs::write(&help, guide_bytes).unwrap();
        std::fs::remove_file(&external).unwrap();
    }
    std::fs::write(store.path(&pkg, &digest, "dropped.so"), b"x").unwrap();
    assert!(matches!(
        store.verify(&pkg, &digest, &b.inspection.files),
        Err(PackageFileError::Mismatch(_))
    ));
    // A file that does not hash to its manifest entry is never published.
    let mut wrong = b.files.clone();
    wrong[0].bytes = b"other".to_vec();
    assert!(
        store
            .publish(&pkg, &"e".repeat(64), &wrong, &b.inspection.files)
            .is_err()
    );
    assert!(!store.directory(&pkg, &"e".repeat(64)).exists());
    // Removal, and staging leftovers: in-progress ones are kept, abandoned ones removed.
    store.remove(&pkg, &digest).unwrap();
    assert!(store.installed().unwrap().is_empty());
    std::fs::create_dir_all(r.join(".staging").join("interrupted")).unwrap();
    assert_eq!(
        store
            .clean_staging(Some(Duration::from_secs(3600)))
            .unwrap(),
        0
    );
    assert_eq!(store.clean_staging(None).unwrap(), 1);
    let _ = std::fs::remove_dir_all(&r);
}

// ---------------------------------------------------------------------------------------------
// Signed publisher indexes, the local transport and configuration schema validation

fn index_entry(bundle: &str) -> Value {
    json!({"packageId": "acme-pack", "version": "1.2.0", "manifestDigest": "a".repeat(64),
        "bundleDigest": "b".repeat(64), "bundleBytes": 10, "targets": [host_target_triple()],
        "kind": "HarnessPackage", "languages": [], "bundle": bundle})
}
fn index(entries: Vec<Value>) -> Value {
    json!({"publisherId": "acme", "generatedAtMs": 5, "entries": entries})
}
fn hex_key(k: &SigningKey) -> String {
    k.verifying_key()
        .as_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn source(location: &str, k: &SigningKey) -> IndexSource {
    IndexSource {
        location: location.into(),
        public_key: hex_key(k),
    }
}

#[test]
fn an_index_verifies_only_under_its_sources_key_and_resolves_safe_bundle_locations() {
    let (k, other) = (
        SigningKey::from_bytes(&[3; 32]),
        SigningKey::from_bytes(&[4; 32]),
    );
    let v = BundleIndexVerifier;
    // An absolute directory on this platform (a drive letter on Windows).
    let base = if cfg!(windows) {
        PathBuf::from(r"C:\srv\devworks")
    } else {
        PathBuf::from("/srv/devworks")
    };
    let dir = source(&base.display().to_string(), &k);
    let doc = write_index(&index(vec![index_entry("packs/acme-pack-1.2.0.dwpkg")]), &k);
    let ok = v.verify(&dir, &doc).unwrap();
    assert_eq!((ok.publisher_id.as_str(), ok.generated_at_ms), ("acme", 5));
    assert_eq!(ok.key_fingerprint, key_fingerprint(&k.verifying_key()));
    // The platform's separator throughout.
    assert_eq!(
        ok.entries[0].bundle,
        base.join("packs")
            .join("acme-pack-1.2.0.dwpkg")
            .display()
            .to_string()
    );
    assert_eq!(
        v.index_location(&dir).unwrap(),
        base.join("devworks-index.json").display().to_string()
    );
    // A directory that is not absolute on this platform is refused (`/srv` has no drive on
    // Windows; `srv` is relative everywhere).
    assert!(v.index_location(&source("srv/devworks", &k)).is_err());
    if cfg!(windows) {
        assert!(v.index_location(&source("/srv/devworks", &k)).is_err());
    }

    // Signed by another (valid) key than the source trusts.
    let foreign = write_index(&index(vec![index_entry("a.dwpkg")]), &other);
    assert!(
        v.verify(&dir, &foreign)
            .unwrap_err()
            .contains("does not trust")
    );
    // A tampered body no longer verifies.
    let tampered = String::from_utf8(doc.clone())
        .unwrap()
        .replace("1.2.0", "9.9.9");
    assert!(
        v.verify(&dir, tampered.as_bytes())
            .unwrap_err()
            .contains("does not verify")
    );
    // Traversal, absolute paths and other schemes are refused as bundle locations.
    for bad in [
        "../x.dwpkg",
        "packs/../../x.dwpkg",
        "/etc/passwd",
        r"..\x.dwpkg",
        r"C:\x.dwpkg",
        "C:x.dwpkg",
        r"\\server\share\x.dwpkg",
        "http://example.test/x.dwpkg",
    ] {
        let d = write_index(&index(vec![index_entry(bad)]), &k);
        assert!(
            v.verify(&dir, &d)
                .unwrap_err()
                .contains("unsafe bundle location"),
            "{bad}"
        );
    }
    // Malformed entries name the entry.
    let mut e = index_entry("a.dwpkg");
    e["version"] = json!("latest");
    let err = v
        .verify(&dir, &write_index(&index(vec![e]), &k))
        .unwrap_err();
    assert!(err.contains("entry 1: invalid version"), "{err}");

    // URL sources: https only; relative bundles resolve against the index URL.
    let url = source("https://packs.example.test/acme/index.json", &k);
    assert_eq!(v.index_location(&url).unwrap(), url.location);
    let doc = write_index(&index(vec![index_entry("acme-pack.dwpkg")]), &k);
    let r = v.verify(&url, &doc).unwrap();
    assert_eq!(
        r.entries[0].bundle,
        "https://packs.example.test/acme/acme-pack.dwpkg"
    );
    assert!(
        v.index_location(&source("http://packs.example.test/i.json", &k))
            .is_err()
    );
    assert!(v.index_location(&source("relative/dir", &k)).is_err());
    assert!(v.verify(&url, b"{not json").is_err());
}

#[test]
fn the_local_transport_reads_bounded_regular_files_only() {
    let r = root();
    std::fs::create_dir_all(&r).unwrap();
    let file = r.join("devworks-index.json");
    std::fs::write(&file, b"0123456789").unwrap();
    let t = NativeIndexTransport::default();
    let at = file.display().to_string();
    assert_eq!(t.fetch(&at, 10).unwrap(), b"0123456789");
    assert!(t.fetch(&at, 9).unwrap_err().contains("larger than"));
    let dir = r.display().to_string();
    assert!(
        t.fetch(&dir, 100)
            .unwrap_err()
            .contains("not a regular file")
    );
    assert!(
        t.fetch(&r.join("missing").display().to_string(), 100)
            .is_err()
    );
    let _ = std::fs::remove_dir_all(&r);
}

/// Names that resolve to fixed addresses (no network).
struct Names;
impl Resolver for Names {
    fn resolve(
        &self,
        host: &str,
        _: u16,
        _: Instant,
        _: &[Cancellation],
    ) -> Result<Vec<IpAddr>, NetFault> {
        let ip = match host {
            "packs.example.test" => "93.184.216.34",
            "internal.example.test" => "10.0.0.5",
            "mixed.example.test" => {
                return Ok(vec![
                    "93.184.216.35".parse().unwrap(),
                    "192.168.1.9".parse().unwrap(),
                ]);
            }
            _ => {
                return Err(NetFault::NotSent {
                    reason: "unknown".into(),
                    timed_out: false,
                });
            }
        };
        Ok(vec![ip.parse().unwrap()])
    }
}

/// Answers by address: the public index redirects to `redirect_to`; any other address serves
/// `index`. Records every address a hop connected to.
struct Scripted {
    redirect_to: String,
    connected: Mutex<Vec<SocketAddr>>,
}
impl HopTransport for Scripted {
    fn get(&self, request: &HopRequest<'_>) -> Result<HopResponse, NetFault> {
        self.connected.lock().unwrap().push(request.addr);
        let (status, headers, body): (u16, Vec<(String, String)>, &[u8]) = if request
            .addr
            .ip()
            .to_string()
            == "93.184.216.34"
            && request.target == "/index.json"
        {
            (
                302,
                vec![("Location".into(), self.redirect_to.clone())],
                b"",
            )
        } else {
            (200, Vec::new(), b"index")
        };
        Ok(HopResponse {
            status,
            headers,
            body: Box::new(std::io::Cursor::new(body.to_vec())),
        })
    }
}

fn scripted(redirect_to: &str, allow_private: bool) -> (Arc<Scripted>, NativeIndexTransport) {
    let wire = Arc::new(Scripted {
        redirect_to: redirect_to.to_owned(),
        connected: Mutex::new(Vec::new()),
    });
    let t = NativeIndexTransport::new(
        Arc::new(Names),
        wire.clone(),
        Arc::new(move || allow_private),
    );
    (wire, t)
}

#[test]
fn package_downloads_refuse_a_redirect_to_a_private_address() {
    const INDEX: &str = "https://packs.example.test/index.json";
    // A redirect to a name that resolves privately, and to private, loopback and metadata
    // literals: refused before any connection to that address.
    for to in [
        "https://internal.example.test/index.json",
        "https://10.1.2.3/index.json",
        "https://127.0.0.1/index.json",
        "https://169.254.169.254/latest/meta-data",
        "https://mixed.example.test/index.json",
    ] {
        let (wire, t) = scripted(to, false);
        let e = t.fetch(INDEX, 100).unwrap_err();
        assert!(e.contains("refused"), "{to}: {e}");
        assert_eq!(
            *wire.connected.lock().unwrap(),
            vec!["93.184.216.34:443".parse::<SocketAddr>().unwrap()],
            "{to}"
        );
    }
    // A redirect to plain http is refused too.
    let (_, t) = scripted("http://packs.example.test/other.json", true);
    assert!(t.fetch(INDEX, 100).unwrap_err().contains("only https"));
    // The web.fetch private-network setting allows private and loopback ranges, never
    // link-local metadata.
    let (wire, t) = scripted("https://internal.example.test/index.json", true);
    assert_eq!(t.fetch(INDEX, 100).unwrap(), b"index");
    assert_eq!(wire.connected.lock().unwrap().len(), 2);
    let (_, t) = scripted("https://169.254.169.254/latest/meta-data", true);
    assert!(t.fetch(INDEX, 100).unwrap_err().contains("Metadata"));
    // A public redirect is followed, and the body is still bounded.
    let (_, t) = scripted("/bundles/index.json", false);
    assert_eq!(t.fetch(INDEX, 100).unwrap(), b"index");
    assert!(t.fetch(INDEX, 4).unwrap_err().contains("larger than"));
}

#[test]
fn package_downloads_never_connect_to_a_private_first_hop() {
    // A listener on loopback: an https download of it must be refused before connecting.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let t = NativeIndexTransport::default();
    let e = t
        .fetch(
            &format!("https://127.0.0.1:{port}/devworks-index.json"),
            100,
        )
        .unwrap_err();
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        listener.accept().is_err(),
        "connected to a loopback index: {e}"
    );
    assert!(e.contains("Loopback"), "{e}");
}

#[test]
fn configuration_values_are_validated_with_field_pointers() {
    let schema = json!({"type": "object", "additionalProperties": false,
        "properties": {"threads": {"type": "integer", "minimum": 1}, "name": {"type": "string"}}});
    let v = JsonSchemaValidator;
    assert!(
        v.validate(&schema, &json!({"threads": 2, "name": "x"}))
            .is_ok()
    );
    let errors = v
        .validate(&schema, &json!({"threads": 0, "name": 3}))
        .unwrap_err();
    let mut pointers: Vec<_> = errors.iter().map(|e| e.pointer.as_str()).collect();
    pointers.sort();
    assert_eq!(pointers, ["/name", "/threads"]);
    // An invalid schema is reported, not accepted.
    let bad = v.validate(&json!({"type": 7}), &json!({})).unwrap_err();
    assert!(
        bad[0].message.contains("schema is invalid"),
        "{}",
        bad[0].message
    );
    // References are never resolved (no network, no file system).
    let remote = json!({"$ref": "https://example.test/schema.json"});
    assert!(v.validate(&remote, &json!({})).is_err());
}

#[test]
fn case_aliases_cannot_pass_the_exact_lowercase_contribution_namespace() {
    for name in ["Foo", "FOO", "foo"] {
        let m = manifest(&files(), |m| {
            m["packageId"] = json!(name);
            m["contributions"][0]["meta"]["id"] = json!(format!("{name}/index"));
        });
        let result = BundleInspector.inspect(&signed(&m, &files()));
        if name == "foo" {
            assert_eq!(result.unwrap().inspection.package_id, "foo");
        } else {
            assert!(matches!(result, Err(InspectError::Manifest(_))));
        }
    }
    // Keeping a lowercase contribution while changing only package ID also fails validation.
    let m = manifest(&files(), |m| m["packageId"] = json!("Demo-pack"));
    assert!(matches!(
        BundleInspector.inspect(&signed(&m, &files())),
        Err(InspectError::Manifest(_))
    ));
}

#[path = "packages/streaming.rs"]
mod streaming;
