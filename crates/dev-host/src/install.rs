//! `dev-host install --version X`: installs a released dev-host binary at a pinned version.
//!
//! Releases are tagged `dev-host-v<X>` in this repository. Each GitHub Release carries one
//! binary per supported platform, named `dev-host-<X>-<triple>[.exe]`, and beside it
//! `<asset>.sha256` holding `<sha256 hex>  <asset>`. Install:
//!
//! 1. downloads the asset and its checksum over HTTPS with the system `curl` (macOS and
//!    Windows 10+ both ship it, so the tool needs no HTTP or TLS crates and still builds with
//!    plain cargo) into a staging folder inside the install folder;
//! 2. refuses the binary unless its SHA-256 matches the published checksum;
//! 3. runs the staged binary with `--version` and refuses it unless it prints `dev-host <X>`;
//! 4. renames it over the installed binary, so the install folder never holds a partial file.
//!    Windows cannot overwrite a running exe but can rename it, so the old exe is first moved
//!    aside to `dev-host.exe.old` and removed when nothing still runs it.
//!
//! The install folder is `$HOME/.local/bin` on macOS and `%LOCALAPPDATA%\dev-host\bin` on
//! Windows (both on the system drive). Other platforms have no published binary: build with
//! `cargo build -p dev-host`.

use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

use crate::Vars;

/// The repository whose GitHub Releases carry the dev-host binaries.
pub const RELEASES: &str = "https://github.com/SceneWorks/DevWorks/releases/download";

/// The Rust target triple of the binary published for this platform, if one is published.
pub fn platform_triple() -> Option<&'static str> {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("aarch64-apple-darwin")
    } else if cfg!(all(windows, target_arch = "x86_64")) {
        Some("x86_64-pc-windows-msvc")
    } else {
        None
    }
}

/// Refuses a version that is not plain `MAJOR.MINOR.PATCH[-PRE]`; it is spliced into a URL.
pub fn validate_version(version: &str) -> Result<(), String> {
    let (core, pre) = version.split_once('-').unwrap_or((version, ""));
    let parts: Vec<&str> = core.split('.').collect();
    let core_ok = parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    let pre_ok = !version.contains('-')
        || (!pre.is_empty()
            && pre
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-'));
    if core_ok && pre_ok {
        Ok(())
    } else {
        Err(format!(
            "version {version:?} is not MAJOR.MINOR.PATCH (for example 0.1.0)"
        ))
    }
}

/// The release asset name for `version` on `triple`.
pub fn asset_name(version: &str, triple: &str) -> String {
    let suffix = if triple.contains("windows") {
        ".exe"
    } else {
        ""
    };
    format!("dev-host-{version}-{triple}{suffix}")
}

/// The download URL of `asset` in the release for `version`.
pub fn asset_url(version: &str, asset: &str) -> String {
    format!("{RELEASES}/dev-host-v{version}/{asset}")
}

/// The installed binary's file name on the platform `triple` names.
fn binary_name(triple: &str) -> &'static str {
    if triple.contains("windows") {
        "dev-host.exe"
    } else {
        "dev-host"
    }
}

/// The install folder for `triple`: `$HOME/.local/bin` on macOS,
/// `%LOCALAPPDATA%\dev-host\bin` on Windows.
pub fn install_dir(vars: &Vars, triple: &str) -> Result<PathBuf, String> {
    if triple.contains("windows") {
        let base = vars
            .get("LOCALAPPDATA")
            .ok_or("LOCALAPPDATA is not set; cannot find the install folder")?;
        Ok(Path::new(base).join("dev-host").join("bin"))
    } else {
        let home = vars
            .get("HOME")
            .ok_or("HOME is not set; cannot find the install folder")?;
        Ok(Path::new(home).join(".local").join("bin"))
    }
}

/// The SHA-256 of a file, as lowercase hex.
pub fn file_sha256(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    Ok(Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Checks `actual` (lowercase hex) against a published checksum file for `asset`: one line
/// `<hex>  <name>` (a leading `*` on the name is the binary-mode marker) or a bare `<hex>`.
pub fn verify_checksum(checksum_file: &str, asset: &str, actual: &str) -> Result<(), String> {
    let mut fields = checksum_file.split_whitespace();
    let expected = fields.next().unwrap_or_default().to_ascii_lowercase();
    if expected.len() != 64 || !expected.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!(
            "the checksum file for {asset} does not start with a SHA-256"
        ));
    }
    if let Some(name) = fields.next()
        && name.trim_start_matches('*') != asset
    {
        return Err(format!(
            "the checksum file names {name}, not {asset}; refusing the download"
        ));
    }
    if expected == actual {
        Ok(())
    } else {
        Err(format!(
            "checksum mismatch for {asset}: published {expected}, downloaded {actual}; \
             refusing the download"
        ))
    }
}

/// Checks a binary's `--version` output against the requested version.
pub fn verify_version_output(stdout: &str, version: &str) -> Result<(), String> {
    let got = stdout.trim();
    if got == format!("dev-host {version}") {
        Ok(())
    } else {
        Err(format!(
            "the downloaded binary reports {got:?}, not \"dev-host {version}\"; refusing it"
        ))
    }
}

/// Downloads `url` to `dest` over HTTPS only, failing on any HTTP error status.
pub fn curl_fetch(url: &str, dest: &Path) -> Result<(), String> {
    let output = Command::new("curl")
        .args([
            "--fail",
            "--location",
            "--silent",
            "--show-error",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--retry",
            "3",
            "--output",
        ])
        .arg(dest)
        .arg(url)
        .output()
        .map_err(|e| format!("cannot run curl (needed to download {url}): {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "download of {url} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// What an install did.
#[derive(Debug)]
pub struct Installed {
    pub path: PathBuf,
    /// The previous binary moved aside but still in use (Windows), to be removed later.
    pub leftover: Option<PathBuf>,
}

/// Installs `version` of the `triple` binary into `dir`, fetching each URL with `fetch`.
pub fn install(
    version: &str,
    triple: &str,
    dir: &Path,
    fetch: &dyn Fn(&str, &Path) -> Result<(), String>,
) -> Result<Installed, String> {
    validate_version(version)?;
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let staging = dir.join(format!(".dev-host-install-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir(&staging)
        .map_err(|e| format!("cannot create {}: {e}", staging.display()))?;
    let result = stage_and_place(version, triple, dir, &staging, fetch);
    let _ = std::fs::remove_dir_all(&staging);
    result
}

fn stage_and_place(
    version: &str,
    triple: &str,
    dir: &Path,
    staging: &Path,
    fetch: &dyn Fn(&str, &Path) -> Result<(), String>,
) -> Result<Installed, String> {
    let asset = asset_name(version, triple);
    let staged = staging.join(binary_name(triple));
    let checksum = staging.join(format!("{asset}.sha256"));
    fetch(&asset_url(version, &format!("{asset}.sha256")), &checksum)?;
    fetch(&asset_url(version, &asset), &staged)?;
    let checksum_text = std::fs::read_to_string(&checksum)
        .map_err(|e| format!("cannot read {}: {e}", checksum.display()))?;
    verify_checksum(&checksum_text, &asset, &file_sha256(&staged)?)?;
    make_executable(&staged)?;
    let output = Command::new(&staged)
        .arg("--version")
        .output()
        .map_err(|e| format!("cannot run the downloaded binary: {e}"))?;
    verify_version_output(&String::from_utf8_lossy(&output.stdout), version)?;
    place(&staged, &dir.join(binary_name(triple)))
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("cannot mark {} executable: {e}", path.display()))
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<(), String> {
    Ok(())
}

/// Moves the verified `staged` binary to `dest` with a rename. Where `dest` cannot be
/// replaced in place (Windows, while it runs), the old binary is renamed aside first and put
/// back if the final rename fails.
pub fn place(staged: &Path, dest: &Path) -> Result<Installed, String> {
    let rename_err = |e: std::io::Error| {
        format!(
            "cannot move {} to {}: {e}",
            staged.display(),
            dest.display()
        )
    };
    if !cfg!(windows) || !dest.exists() {
        std::fs::rename(staged, dest).map_err(rename_err)?;
        return Ok(Installed {
            path: dest.to_owned(),
            leftover: None,
        });
    }
    let mut old = dest.as_os_str().to_owned();
    old.push(".old");
    let old = PathBuf::from(old);
    let _ = std::fs::remove_file(&old);
    std::fs::rename(dest, &old).map_err(|e| {
        format!(
            "cannot move the installed {} aside to replace it: {e}; close programs using it \
             and retry",
            dest.display()
        )
    })?;
    if let Err(e) = std::fs::rename(staged, dest) {
        let _ = std::fs::rename(&old, dest);
        return Err(rename_err(e));
    }
    let leftover = std::fs::remove_file(&old).err().map(|_| old);
    Ok(Installed {
        path: dest.to_owned(),
        leftover,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::Scratch;

    #[test]
    fn versions_are_plain_semver() {
        for ok in ["0.1.0", "12.0.3", "1.2.3-rc.1"] {
            assert!(validate_version(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "1.2",
            "v1.2.3",
            "1.2.3/../x",
            "1.2.3-",
            "1..3",
            "1.2.3?x=1",
        ] {
            assert!(validate_version(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn asset_names_and_urls() {
        assert_eq!(
            asset_name("0.2.0", "aarch64-apple-darwin"),
            "dev-host-0.2.0-aarch64-apple-darwin"
        );
        assert_eq!(
            asset_name("0.2.0", "x86_64-pc-windows-msvc"),
            "dev-host-0.2.0-x86_64-pc-windows-msvc.exe"
        );
        assert_eq!(
            asset_url("0.2.0", "a"),
            format!("{RELEASES}/dev-host-v0.2.0/a")
        );
    }

    #[test]
    fn install_dirs_follow_the_platform() {
        let vars: Vars = [
            ("HOME", "/h"),
            ("LOCALAPPDATA", r"C:\Users\u\AppData\Local"),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            install_dir(&vars, "aarch64-apple-darwin").unwrap(),
            Path::new("/h").join(".local").join("bin")
        );
        assert_eq!(
            install_dir(&vars, "x86_64-pc-windows-msvc").unwrap(),
            Path::new(r"C:\Users\u\AppData\Local")
                .join("dev-host")
                .join("bin")
        );
        assert!(install_dir(&Vars::default(), "aarch64-apple-darwin").is_err());
        assert!(install_dir(&Vars::default(), "x86_64-pc-windows-msvc").is_err());
    }

    const HEX: &str = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";

    #[test]
    fn checksum_mismatch_is_refused() {
        assert!(verify_checksum(&format!("{HEX}  a\n"), "a", HEX).is_ok());
        assert!(verify_checksum(&format!("{HEX} *a\n"), "a", HEX).is_ok());
        assert!(verify_checksum(&HEX.to_uppercase(), "a", HEX).is_ok());
        let other = HEX.replace('9', "8");
        let err = verify_checksum(&format!("{other}  a\n"), "a", HEX).unwrap_err();
        assert!(err.contains("checksum mismatch"), "{err}");
        assert!(verify_checksum(&format!("{HEX}  b\n"), "a", HEX).is_err());
        assert!(verify_checksum("not found", "a", HEX).is_err());
        assert!(verify_checksum("", "a", HEX).is_err());
    }

    #[test]
    fn version_mismatch_is_refused() {
        assert!(verify_version_output("dev-host 0.2.0\n", "0.2.0").is_ok());
        assert!(verify_version_output("dev-host 0.1.0\n", "0.2.0").is_err());
        assert!(verify_version_output("dev-host 0.2.0-rc.1\n", "0.2.0").is_err());
        assert!(verify_version_output("", "0.2.0").is_err());
    }

    #[test]
    fn file_sha256_is_lowercase_hex() {
        let s = Scratch::new("sha");
        let f = s.path().join("x");
        std::fs::write(&f, "test").unwrap();
        assert_eq!(file_sha256(&f).unwrap(), HEX);
    }

    /// A fake release: `fetch` copies files from `release` by asset name.
    #[cfg(unix)]
    fn fake_release(
        release: &Path,
        version_printed: &str,
        published_sha: Option<&str>,
    ) -> (String, String) {
        let triple = "aarch64-apple-darwin";
        let asset = asset_name("9.9.9", triple);
        let bin = release.join(&asset);
        std::fs::write(
            &bin,
            format!("#!/bin/sh\necho 'dev-host {version_printed}'\n"),
        )
        .unwrap();
        let sha = published_sha
            .map(str::to_owned)
            .unwrap_or_else(|| file_sha256(&bin).unwrap());
        std::fs::write(
            release.join(format!("{asset}.sha256")),
            format!("{sha}  {asset}\n"),
        )
        .unwrap();
        (triple.to_owned(), asset)
    }

    #[cfg(unix)]
    fn run_install(release: &Path, dir: &Path, triple: &str) -> Result<Installed, String> {
        let fetch = |url: &str, dest: &Path| {
            let name = url.rsplit('/').next().unwrap();
            std::fs::copy(release.join(name), dest)
                .map(|_| ())
                .map_err(|e| e.to_string())
        };
        install("9.9.9", triple, dir, &fetch)
    }

    #[cfg(unix)]
    #[test]
    fn install_places_a_verified_binary_and_replaces_the_old_one() {
        let s = Scratch::new("install-ok");
        let release = s.path().join("release");
        let dir = s.path().join("bin");
        std::fs::create_dir_all(&release).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("dev-host"), "old").unwrap();
        let (triple, _) = fake_release(&release, "9.9.9", None);
        let installed = run_install(&release, &dir, &triple).unwrap();
        assert_eq!(installed.path, dir.join("dev-host"));
        assert!(installed.leftover.is_none());
        let out = Command::new(&installed.path)
            .arg("--version")
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "dev-host 9.9.9"
        );
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(
            names,
            vec![std::ffi::OsString::from("dev-host")],
            "staging left behind"
        );
    }

    #[cfg(unix)]
    #[test]
    fn install_refuses_checksum_or_version_mismatch_and_keeps_the_old_binary() {
        for (printed, sha) in [("9.9.9", Some(HEX)), ("9.9.8", None)] {
            let s = Scratch::new("install-bad");
            let release = s.path().join("release");
            let dir = s.path().join("bin");
            std::fs::create_dir_all(&release).unwrap();
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("dev-host"), "old").unwrap();
            let (triple, _) = fake_release(&release, printed, sha);
            let err = run_install(&release, &dir, &triple).unwrap_err();
            let expected = if sha.is_some() {
                "checksum mismatch"
            } else {
                "reports"
            };
            assert!(err.contains(expected), "{err}");
            assert_eq!(
                std::fs::read_to_string(dir.join("dev-host")).unwrap(),
                "old"
            );
            assert_eq!(
                std::fs::read_dir(&dir).unwrap().count(),
                1,
                "staging left behind"
            );
        }
    }

    #[test]
    fn a_failed_download_installs_nothing() {
        let s = Scratch::new("install-404");
        let dir = s.path().join("bin");
        let fetch = |_: &str, _: &Path| Err("HTTP 404".to_owned());
        let err = install("9.9.9", "aarch64-apple-darwin", &dir, &fetch).unwrap_err();
        assert!(err.contains("404"));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
    }
}
