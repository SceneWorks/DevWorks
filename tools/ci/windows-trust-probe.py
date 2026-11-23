from pathlib import Path
import os

p = Path(os.environ["RUNNER_TEMP"]) / "trust-probe"
(p / "src").mkdir(parents=True)
(p / "Cargo.toml").write_text("""
[package]
name = "trust-probe"
version = "0.0.0"
edition = "2024"
[dependencies]
windows = { version = "=0.62.2", features = ["Win32_Foundation", "Win32_Security_WinTrust", "Win32_Security_Cryptography_Catalog", "Win32_Security_Cryptography_Sip"] }
sha2 = "=0.11.1"
""")
source = Path("crates/devworks-desktop/src/app_updates/windows_trust.rs").read_text().split("#[cfg(test)]")[0]
source = source.replace("pub(super) fn", "pub fn")
source = source.replace("use devworks_core::fingerprint::sha256_hex;", """
use sha2::{Digest, Sha256};
fn sha256_hex(bytes: &[u8]) -> String { format!("{:x}", Sha256::digest(bytes)) }
""")
source = source.replace("fn verify_profile(path: &Path, profile: &str) -> Result<(), String> {", """
fn verify_profile(path: &Path, profile: &str, flags: u32, initial: u32, use_ex: bool) -> Result<(), String> {
""")
source = source.replace("verify_profile(path, profile)", "verify_profile(path, profile, WSS_VERIFY_SPECIFIC | WSS_GET_SECONDARY_SIG_COUNT, u32::MAX, true)")
source = source.replace("dwFlags: WSS_VERIFY_SPECIFIC | WSS_GET_SECONDARY_SIG_COUNT,", "dwFlags: flags,")
source = source.replace("u32::MAX,", "initial,", 0) if False else source.replace("Sigs: u32::MAX,", "Sigs: initial,").replace("SigIndex: u32::MAX,", "SigIndex: initial,")
source = source.replace("let status = WinVerifyTrustEx(HWND::default(), &mut action, &mut data);", """
let status = if use_ex { WinVerifyTrustEx(HWND::default(), &mut action, &mut data) }
    else { WinVerifyTrust(HWND::default(), &mut action, (&mut data as *mut WINTRUST_DATA).cast()) };
if status == 0 {
    println!("flags={flags} initial={initial} ex={use_ex} secondary={} index={} identity={:?}", signatures.cSecondarySigs,
         signatures.dwVerifiedSigIndex, verified_identity(data.hWVTStateData, profile));
}
""")
source = source.replace("let _ = WinVerifyTrustEx(HWND::default(), &mut action, &mut data);", """
if use_ex { WinVerifyTrustEx(HWND::default(), &mut action, &mut data); }
else { WinVerifyTrust(HWND::default(), &mut action, (&mut data as *mut WINTRUST_DATA).cast()); }
""")
source = source.replace("if !certificate_hash_matches(root, AZURE_ROOT_SHA256)", """
println!("root_ok={} profile_ok={} signer_digest_sha256={}", certificate_hash_matches(root, AZURE_ROOT_SHA256),
    has_profile_eku(leaf, profile), oid_matches(PCSTR(info.HashAlgorithm.pszObjId.0), szOID_NIST_sha256.as_bytes()));
if !certificate_hash_matches(root, AZURE_ROOT_SHA256)
""")
source = source.replace("if signer.csCounterSigners != 1", """
println!("counter_count={}", signer.csCounterSigners);
if signer.csCounterSigners != 1
""")
(p / "src/main.rs").write_text(source + r"""
fn main() {
    let fixtures = std::path::PathBuf::from(std::env::var_os("DEVWORKS_WINDOWS_TRUST_FIXTURES").unwrap());
    for name in ["older.exe", "newer.exe"] {
        let path = fixtures.join(name);
        println!("{name}");
        for use_ex in [true, false] {
            for flags in [WSS_VERIFY_SPECIFIC | WSS_GET_SECONDARY_SIG_COUNT, WSS_GET_SECONDARY_SIG_COUNT, WSS_VERIFY_SPECIFIC] {
                for initial in [u32::MAX, 0] {
                    if let Err(error) = verify_profile(&path, configured_profile().unwrap(), flags, initial, use_ex) {
                        println!("flags={flags} initial={initial} ex={use_ex}: {error}");
                    }
                }
            }
        }
    }
}
""")
