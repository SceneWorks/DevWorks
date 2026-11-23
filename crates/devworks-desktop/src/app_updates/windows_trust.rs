//! Windows owner trust for the configured Azure Artifact Signing Public Trust profile.

use devworks_core::fingerprint::sha256_hex;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use windows::Win32::Foundation::{HANDLE, HWND};
use windows::Win32::Security::Cryptography::{
    CERT_CONTEXT, CERT_FIND_EXT_ONLY_ENHKEY_USAGE_FLAG, CTL_USAGE, CertGetEnhancedKeyUsage,
    szOID_NIST_sha256, szOID_RFC3161_counterSign, szOID_RFC3161v21_counterSign,
    szOID_RSA_counterSign,
};
use windows::Win32::Security::WinTrust::*;
use windows::core::{PCSTR, PCWSTR};

const PROFILE_PREFIX: &str = "1.3.6.1.4.1.311.97.";
const PUBLIC_TRUST_OID: &[u8] = b"1.3.6.1.4.1.311.97.1.0";
const CODE_SIGNING_OID: &[u8] = b"1.3.6.1.5.5.7.3.3";
// Microsoft Identity Verification Root Certificate Authority 2020, DER SHA-256.
const AZURE_ROOT_SHA256: &str = "5367f20c7ade0e2bca790915056d086b720c33c1fa2a2661acf787e3292e1270";

pub(super) fn configured_profile() -> Option<&'static str> {
    option_env!("DEVWORKS_WINDOWS_SIGNING_PROFILE_OID").filter(|value| is_public_profile_oid(value))
}

fn is_public_profile_oid(value: &str) -> bool {
    let Some(suffix) = value.strip_prefix(PROFILE_PREFIX) else {
        return false;
    };
    let mut parts = suffix.split('.');
    (0..4).all(|_| {
        parts.next().is_some_and(|part| {
            !part.is_empty()
                && part.bytes().all(|byte| byte.is_ascii_digit())
                && (part.len() == 1 || !part.starts_with('0'))
                && part.parse::<u32>().is_ok()
        })
    }) && parts.next().is_none()
}

pub(super) fn verify(path: &Path) -> Result<(), String> {
    let profile = configured_profile().ok_or("The app has no pinned Windows signing profile.")?;
    verify_profile(path, profile)
}

fn verify_profile(path: &Path, profile: &str) -> Result<(), String> {
    let path: Vec<_> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;
    let mut file = WINTRUST_FILE_INFO {
        cbStruct: std::mem::size_of::<WINTRUST_FILE_INFO>() as u32,
        pcwszFilePath: PCWSTR(path.as_ptr()),
        ..Default::default()
    };
    let mut signatures = WINTRUST_SIGNATURE_SETTINGS {
        cbStruct: std::mem::size_of::<WINTRUST_SIGNATURE_SETTINGS>() as u32,
        dwFlags: WSS_VERIFY_SPECIFIC | WSS_GET_SECONDARY_SIG_COUNT,
        cSecondarySigs: u32::MAX,
        dwVerifiedSigIndex: u32::MAX,
        ..Default::default()
    };
    let mut data = WINTRUST_DATA {
        cbStruct: std::mem::size_of::<WINTRUST_DATA>() as u32,
        dwUIChoice: WTD_UI_NONE,
        fdwRevocationChecks: WTD_REVOKE_WHOLECHAIN,
        dwUnionChoice: WTD_CHOICE_FILE,
        Anonymous: WINTRUST_DATA_0 { pFile: &mut file },
        dwStateAction: WTD_STATEACTION_VERIFY,
        dwProvFlags: WTD_REVOCATION_CHECK_CHAIN_EXCLUDE_ROOT | WTD_CACHE_ONLY_URL_RETRIEVAL,
        pSignatureSettings: &mut signatures,
        ..Default::default()
    };

    // SAFETY: the file path and structures outlive the WinTrust state. Inspect the
    // provider pointers only after successful verification and before CLOSE.
    unsafe {
        let status = WinVerifyTrustEx(HWND::default(), &mut action, &mut data);
        let result = if status != 0 {
            Err(format!(
                "Windows could not verify the embedded Authenticode signature ({status:#010x})."
            ))
        } else if signatures.cSecondarySigs != 0 || signatures.dwVerifiedSigIndex != 0 {
            Err("Expected one verified embedded Windows signature.".into())
        } else {
            verified_identity(data.hWVTStateData, profile).map_err(str::to_owned)
        };
        data.dwStateAction = WTD_STATEACTION_CLOSE;
        let _ = WinVerifyTrustEx(HWND::default(), &mut action, &mut data);
        result
    }
}

/// Inspect only the exact signer and countersigner from a successful native file verification.
///
/// SAFETY: `state` must be an open, successful WinTrust verification state for signature zero.
unsafe fn verified_identity(state: HANDLE, profile: &str) -> Result<(), &'static str> {
    const IDENTITY: &str =
        "The Windows executable does not match the DevWorks platform signing identity.";
    const TIMESTAMP: &str = "The Windows executable has no verified RFC 3161 timestamp.";

    // SAFETY: look up entries in the caller's live WinTrust state. Windows owns the
    // arrays and their certificate and OID buffers until it closes the state.
    unsafe {
        let provider = WTHelperProvDataFromStateData(state);
        if provider.as_ref().is_none_or(|value| value.csSigners != 1) {
            return Err(IDENTITY);
        }
        let signer_ptr = WTHelperGetProvSignerFromChain(provider, 0, false, 0);
        let signer = signer_ptr
            .as_ref()
            .filter(|value| value.dwError == 0 && value.csCertChain >= 2)
            .ok_or(IDENTITY)?;
        let info = signer.psSigner.as_ref().ok_or(IDENTITY)?;
        if !oid_matches(
            PCSTR(info.HashAlgorithm.pszObjId.0),
            szOID_NIST_sha256.as_bytes(),
        ) {
            return Err(IDENTITY);
        }

        let leaf = WTHelperGetProvCertFromChain(signer_ptr, 0)
            .as_ref()
            .and_then(|value| value.pCert.as_ref())
            .ok_or(IDENTITY)?;
        let root = WTHelperGetProvCertFromChain(signer_ptr, signer.csCertChain - 1)
            .as_ref()
            .and_then(|value| value.pCert.as_ref())
            .ok_or(IDENTITY)?;
        if !certificate_hash_matches(root, AZURE_ROOT_SHA256) || !has_profile_eku(leaf, profile) {
            return Err(IDENTITY);
        }

        let attributes = &info.UnauthAttrs;
        if attributes.rgAttr.is_null() || !(1..=64).contains(&attributes.cAttr) {
            return Err(TIMESTAMP);
        }
        let attributes = std::slice::from_raw_parts(attributes.rgAttr, attributes.cAttr as usize);
        let rfc3161 = szOID_RFC3161_counterSign.as_bytes();
        let stamp_oids = [
            rfc3161,
            szOID_RFC3161v21_counterSign.as_bytes(),
            szOID_RSA_counterSign.as_bytes(),
        ];
        let mut stamps = attributes.iter().filter(|attr| {
            stamp_oids
                .iter()
                .any(|oid| oid_matches(PCSTR(attr.pszObjId.0), oid))
        });
        if stamps.next().is_none_or(|stamp| {
            !oid_matches(PCSTR(stamp.pszObjId.0), rfc3161)
                || stamp.cValue != 1
                || stamp.rgValue.is_null()
        }) || stamps.next().is_some()
        {
            return Err(TIMESTAMP);
        }
        if signer.csCounterSigners != 1
            || WTHelperGetProvSignerFromChain(provider, 0, true, 0)
                .as_ref()
                .is_none_or(|timestamp| {
                    timestamp.dwError != 0
                        || timestamp.csCertChain == 0
                        || timestamp.pChainContext.is_null()
                })
        {
            return Err(TIMESTAMP);
        }
        Ok(())
    }
}

/// SAFETY: Windows owns the certificate and its DER buffer for the open verify state.
unsafe fn certificate_hash_matches(cert: &CERT_CONTEXT, expected: &str) -> bool {
    !cert.pbCertEncoded.is_null() && (1..=65536).contains(&cert.cbCertEncoded) && {
        // SAFETY: the provider supplies cbCertEncoded bytes at pbCertEncoded.
        let bytes =
            unsafe { std::slice::from_raw_parts(cert.pbCertEncoded, cert.cbCertEncoded as usize) };
        sha256_hex(bytes) == expected
    }
}

/// SAFETY: the certificate is live for the call and was returned by the trust provider.
unsafe fn has_profile_eku(cert: &CERT_CONTEXT, profile: &str) -> bool {
    // SAFETY: CryptoAPI reports the required size first, then writes identifiers within
    // the caller-owned, pointer-aligned buffer. Read only the signed extension.
    unsafe {
        let mut size = 0;
        if CertGetEnhancedKeyUsage(cert, CERT_FIND_EXT_ONLY_ENHKEY_USAGE_FLAG, None, &mut size)
            .is_err()
            || !(std::mem::size_of::<CTL_USAGE>() as u32..=65536).contains(&size)
        {
            return false;
        }
        let mut buffer = vec![0usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
        let usage = buffer.as_mut_ptr().cast::<CTL_USAGE>();
        if CertGetEnhancedKeyUsage(
            cert,
            CERT_FIND_EXT_ONLY_ENHKEY_USAGE_FLAG,
            Some(usage),
            &mut size,
        )
        .is_err()
        {
            return false;
        }
        let usage = &*usage;
        if usage.rgpszUsageIdentifier.is_null() || !(1..=64).contains(&usage.cUsageIdentifier) {
            return false;
        }
        let identifiers =
            std::slice::from_raw_parts(usage.rgpszUsageIdentifier, usage.cUsageIdentifier as usize);
        [profile.as_bytes(), PUBLIC_TRUST_OID, CODE_SIGNING_OID]
            .into_iter()
            .all(|expected| {
                identifiers
                    .iter()
                    .any(|identifier| oid_matches(PCSTR(identifier.0), expected))
            })
    }
}

/// SAFETY: the pointer is null or points to a NUL-terminated WinTrust/CryptoAPI string.
unsafe fn oid_matches(actual: PCSTR, expected: &[u8]) -> bool {
    // SAFETY: a non-null OID comes from the live native verification/decoding state.
    !actual.is_null() && unsafe { actual.as_bytes() == expected }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, OpenOptions};
    use std::io::{Read, Seek, SeekFrom, Write};

    #[test]
    fn configuration_requires_a_canonical_azure_profile_oid() {
        assert!(is_public_profile_oid(
            "1.3.6.1.4.1.311.97.840169298.24257481.644551716.202800253"
        ));
        for invalid in [
            "",
            "1.3.6.1.4.1.311.97.1.0",
            "1.3.6.1.4.1.311.97.1.2.3.04",
            "1.3.6.1.4.1.311.97.1.2.3.4294967296",
            "1.3.6.1.4.1.311.97.1.2.3.+4",
            "1.3.6.1.4.1.311.97.1.2.3.4.5",
        ] {
            assert!(!is_public_profile_oid(invalid), "{invalid}");
        }
    }

    #[test]
    #[ignore = "requires two genuine Azure-signed fixtures from the qualification workflow"]
    fn real_rotated_profiles_and_modified_image() {
        let fixtures = std::env::var_os("DEVWORKS_WINDOWS_TRUST_FIXTURES")
            .map(std::path::PathBuf::from)
            .expect("Native test requires two exact public SceneWorks release fixtures");
        for name in ["older.exe", "newer.exe"] {
            verify(&fixtures.join(name)).unwrap_or_else(|error| panic!("{name}: {error}"));
        }
        let alternate = "1.3.6.1.4.1.311.97.840169298.24257481.644551716.202800254";
        assert!(is_public_profile_oid(alternate));
        let otherwise_trusted = fixtures.join("older.exe");
        let error = verify_profile(&otherwise_trusted, alternate).unwrap_err();
        assert!(error.contains("signing identity"), "{error}");

        let modified = tempfile::tempdir().unwrap();
        let image = modified.path().join("modified.exe");
        fs::copy(otherwise_trusted, &image).unwrap();
        let mut handle = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&image)
            .unwrap();
        // The DOS stub is covered by Authenticode and lies before the mutable PE checksum.
        handle.seek(SeekFrom::Start(64)).unwrap();
        let mut byte = [0];
        handle.read_exact(&mut byte).unwrap();
        byte[0] ^= 1;
        handle.seek(SeekFrom::Start(64)).unwrap();
        handle.write_all(&byte).unwrap();
        drop(handle);
        let error = verify(&image).unwrap_err();
        assert!(error.contains("could not verify"), "{error}");
    }

    #[test]
    #[ignore = "requires the private, same-run signed updater fixtures"]
    fn current_leaf_requires_a_verified_timestamp() {
        let fixtures = std::env::var_os("DEVWORKS_WINDOWS_TRUST_FIXTURES")
            .map(std::path::PathBuf::from)
            .expect("Native test requires exact same-run signed fixtures");
        verify(&fixtures.join("genuine.exe")).unwrap();
        let error = verify(&fixtures.join("no-timestamp.exe")).unwrap_err();
        // The unexpired signer and executable must pass WinTrust. The product
        // check still refuses the absent timestamp on the verified signer.
        assert!(error.contains("no verified RFC 3161 timestamp"), "{error}");
        let error = verify(&fixtures.join("bad-timestamp.exe")).unwrap_err();
        assert!(
            error.contains("no verified RFC 3161 timestamp")
                || error.contains("could not verify the embedded Authenticode signature"),
            "{error}"
        );
    }

    #[test]
    #[ignore = "requires the disposable Windows runner's lookalike publisher fixture"]
    fn the_profile_requires_the_microsoft_root_even_when_windows_trusts_the_publisher() {
        let path = std::env::var_os("DEVWORKS_WINDOWS_LOOKALIKE")
            .map(std::path::PathBuf::from)
            .expect("Native test requires a locally signed lookalike fixture");
        let trust_removed = std::env::var_os("DEVWORKS_WINDOWS_LOOKALIKE_UNTRUSTED").is_some();
        let error = verify(&path).unwrap_err();
        let expected = if trust_removed {
            "could not verify the embedded Authenticode signature"
        } else {
            "does not match the DevWorks platform signing identity"
        };
        assert!(error.contains(expected), "{error}");
    }

    #[test]
    #[ignore = "only for the fresh native Windows qualification job"]
    fn managed_entry_does_not_fall_through_with_invalid_configuration() {
        const PROBE: &str = "DEVWORKS_TEST_WINDOWS_ENTRY_PROBE";
        match std::env::var(PROBE).as_deref() {
            Ok("launcher") => {
                let error = crate::app_updates::run_internal()
                    .expect("the known-folder launcher must be managed")
                    .unwrap_err();
                assert!(error.contains("no qualified signing owner"), "{error}");
                return;
            }
            Ok("versioned") => {
                let Err(error) = crate::app_updates::enter_application() else {
                    panic!("the versioned executable fell through without update ownership");
                };
                assert!(error.contains("no qualified signing owner"), "{error}");
                return;
            }
            Ok("development") => {
                assert!(crate::app_updates::run_internal().is_none());
                let startup = crate::app_updates::enter_application().unwrap();
                assert!(startup.gate.is_none() && startup.candidate.is_none());
                return;
            }
            _ => {}
        }

        use std::process::Command;
        let root = super::super::windows_per_user_root().unwrap();
        let programs = root.parent().unwrap();
        fs::create_dir_all(programs).unwrap();
        let installed = tempfile::Builder::new()
            .prefix("DevWorks")
            .rand_bytes(0)
            .tempdir_in(programs)
            .expect("the qualification job must start with no managed DevWorks installation");
        assert_eq!(
            installed.path().canonicalize().unwrap(),
            root.canonicalize().unwrap()
        );
        let dev = std::env::current_exe().unwrap();
        let launcher = installed.path().join("DevWorks.exe");
        let versioned = installed
            .path()
            .join("versions")
            .join("0".repeat(64))
            .join("devworks-desktop.exe");
        fs::create_dir_all(versioned.parent().unwrap()).unwrap();
        fs::copy(&dev, &launcher).unwrap();
        fs::copy(&dev, &versioned).unwrap();
        let other = tempfile::tempdir().unwrap();
        let test = "app_updates::windows_trust::tests::managed_entry_does_not_fall_through_with_invalid_configuration";
        for (entry, mode) in [
            (launcher, "launcher"),
            (versioned, "versioned"),
            (dev, "development"),
        ] {
            for (label, value) in [
                ("original", std::env::var_os("LOCALAPPDATA")),
                ("unset", None),
                ("relative", Some("relative".into())),
                ("elsewhere", Some(other.path().into())),
            ] {
                let mut command = Command::new(&entry);
                command.args([test, "--ignored", "--exact", "--nocapture"]);
                command.env(PROBE, mode);
                if let Some(value) = value {
                    command.env("LOCALAPPDATA", value);
                } else {
                    command.env_remove("LOCALAPPDATA");
                }
                let output = command.output().unwrap();
                let stdout = String::from_utf8_lossy(&output.stdout);
                assert!(
                    output.status.success() && stdout.contains("1 passed"),
                    "{mode} under {label} LOCALAPPDATA: {stdout}\n{}",
                    String::from_utf8_lossy(&output.stderr),
                );
            }
        }
    }
}
