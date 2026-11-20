//! Qualified Apple Swift compiler, native IndexStore reader and installed SDK inputs.
use super::RuntimeExecutable;
use crate::fingerprint::sha256_hex;
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::{Path, PathBuf};

pub const PACKAGE: &str = "devworks-swift-semantic";
pub const SDK_RUNTIME: &str = "swift-semantic-sdk";
pub const VERSION: &str = "6.4.2";
const SDK_NAME: &str = "macosx27.2";
const COMPILER_TAG: &str = "swiftlang-6.4.2.1.9";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Runtime {
    pub frontend: RuntimeExecutable,
    pub index_store: RuntimeExecutable,
    pub target: String,
    pub sdk: PathBuf,
    pub resource_directory: PathBuf,
    pub frameworks: PathBuf,
    pub platform_libraries: PathBuf,
    pub plugins: PathBuf,
    pub testing_plugins: PathBuf,
    pub in_process_server: PathBuf,
    pub platform_plugins: PathBuf,
    pub platform_server: PathBuf,
    pub sdk_settings: String,
    pub files: Vec<RuntimeExecutable>,
    pub directories: Vec<Directory>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Directory {
    pub path: PathBuf,
    pub identity: String,
}

const UNAVAILABLE: &str = "swift-xcode-assets-unavailable";

fn regular(root: &Path, path: &Path) -> Result<RuntimeExecutable, &'static str> {
    let asset = RuntimeExecutable::inspect(path).map_err(|_| UNAVAILABLE)?;
    (asset.program.starts_with(root)
        && asset
            .program
            .to_str()
            .is_some_and(|path| !path.contains(['#', '\n', '\r'])))
    .then_some(asset)
    .ok_or(UNAVAILABLE)
}

fn directory(root: &Path, path: &Path) -> Result<Directory, &'static str> {
    let path = path.canonicalize().map_err(|_| UNAVAILABLE)?;
    if !path.starts_with(root)
        || path
            .to_str()
            .is_none_or(|path| path.contains(['#', '\n', '\r']))
    {
        return Err(UNAVAILABLE);
    }
    let meta = std::fs::metadata(&path).map_err(|_| UNAVAILABLE)?;
    if !meta.is_dir() {
        return Err(UNAVAILABLE);
    }
    #[cfg(unix)]
    let state = {
        use std::os::unix::fs::MetadataExt;
        format!(
            "{}:{}:{}:{}:{}:{}",
            meta.dev(),
            meta.ino(),
            meta.mtime(),
            meta.mtime_nsec(),
            meta.ctime(),
            meta.ctime_nsec()
        )
    };
    #[cfg(not(unix))]
    let state = format!("{:?}:{:?}", meta.created().ok(), meta.modified().ok());
    Ok(Directory {
        path,
        identity: sha256_hex(state.as_bytes()),
    })
}

fn sdk_settings(sdk: &Path) -> Result<(String, RuntimeExecutable), &'static str> {
    let asset = regular(sdk, &sdk.join("SDKSettings.json"))?;
    let mut contents = Vec::new();
    std::fs::File::open(&asset.program)
        .and_then(|file| file.take(1024 * 1024 + 1).read_to_end(&mut contents))
        .map_err(|_| UNAVAILABLE)?;
    if contents.len() > 1024 * 1024 {
        return Err(UNAVAILABLE);
    }
    let json = crate::framing::parse(&contents).map_err(|_| UNAVAILABLE)?;
    if json["CanonicalName"] != SDK_NAME || json["Version"] != "27.2" {
        return Err("swift-sdk-version-unqualified");
    }
    Ok((sha256_hex(&contents), asset))
}

impl Runtime {
    /// Resolve the qualified installation selected by the system developer-tool lookup.
    pub fn detect(frontend: &Path, sdk: &Path, target_info: &str) -> Result<Self, &'static str> {
        if !cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            return Err("unsupported-swift-pack-target");
        }
        let report = crate::framing::parse(target_info.as_bytes())
            .map_err(|_| "swift-target-info-invalid")?;
        if report["swiftCompilerTag"] != COMPILER_TAG
            || !report["compilerVersion"]
                .as_str()
                .is_some_and(|v| v.starts_with("Apple Swift version 6.4.2 ("))
        {
            return Err("swift-toolchain-version-unqualified");
        }
        let target = report["target"]["triple"]
            .as_str()
            .filter(|target| target.starts_with("arm64-apple-macosx") && target.len() <= 48)
            .filter(|target| {
                target["arm64-apple-macosx".len()..]
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || byte == b'.')
            })
            .ok_or("unsupported-swift-pack-target")?
            .to_owned();
        Self::installation(frontend, sdk, target)
    }

    fn installation(frontend: &Path, sdk: &Path, target: String) -> Result<Self, &'static str> {
        let frontend = RuntimeExecutable::inspect(frontend).map_err(|_| UNAVAILABLE)?;
        let developer = frontend.program.ancestors().nth(5).ok_or(UNAVAILABLE)?;
        let expected = developer.join("Toolchains/XcodeDefault.xctoolchain/usr");
        if frontend.program != expected.join("bin/swift-frontend")
            || developer.file_name().is_none_or(|name| name != "Developer")
            || !developer
                .parent()
                .is_some_and(|parent| parent.ends_with("Contents"))
        {
            return Err("swift-full-xcode-required");
        }
        let platform = developer.join("Platforms/MacOSX.platform/Developer");
        let sdk = directory(&platform.join("SDKs"), sdk)?;
        let resources = directory(&expected, &expected.join("lib/swift"))?;
        let frameworks = directory(&platform, &platform.join("Library/Frameworks"))?;
        let libraries = directory(&platform, &platform.join("usr/lib"))?;
        let plugins = directory(&resources.path, &resources.path.join("host/plugins"))?;
        let testing = directory(&plugins.path, &plugins.path.join("testing"))?;
        let platform_plugins = directory(&platform, &platform.join("usr/lib/swift/host/plugins"))?;
        let in_process = regular(
            &resources.path,
            &resources.path.join("host/libSwiftInProcPluginServer.dylib"),
        )?;
        let platform_server = regular(&platform, &platform.join("usr/bin/swift-plugin-server"))?;
        let index_store = regular(&expected, &expected.join("lib/libIndexStore.dylib"))?;
        let (sdk_settings, settings_file) = sdk_settings(&sdk.path)?;
        let mut files = vec![settings_file, in_process.clone(), platform_server.clone()];
        for folder in [&plugins, &testing, &platform_plugins] {
            for (i, entry) in std::fs::read_dir(&folder.path)
                .map_err(|_| UNAVAILABLE)?
                .enumerate()
            {
                if i >= 128 {
                    return Err(UNAVAILABLE);
                }
                let path = entry.map_err(|_| UNAVAILABLE)?.path();
                if path.extension().is_some_and(|value| value == "dylib") {
                    files.push(regular(&folder.path, &path)?);
                }
            }
        }
        files.sort_by(|left, right| left.program.cmp(&right.program));
        if !files
            .iter()
            .any(|file| file.program.ends_with("libObservationMacros.dylib"))
            || !files
                .iter()
                .any(|file| file.program.ends_with("libTestingMacros.dylib"))
        {
            return Err(UNAVAILABLE);
        }
        Ok(Self {
            frontend,
            index_store,
            target,
            sdk: sdk.path.clone(),
            resource_directory: resources.path.clone(),
            frameworks: frameworks.path.clone(),
            platform_libraries: libraries.path.clone(),
            plugins: plugins.path.clone(),
            testing_plugins: testing.path.clone(),
            in_process_server: in_process.program,
            platform_plugins: platform_plugins.path.clone(),
            platform_server: platform_server.program,
            sdk_settings,
            files,
            directories: vec![
                sdk,
                resources,
                frameworks,
                libraries,
                plugins,
                testing,
                platform_plugins,
            ],
        })
    }

    pub fn current(&self) -> bool {
        Self::installation(&self.frontend.program, &self.sdk, self.target.clone()).as_ref()
            == Ok(self)
    }
}

/// In-tree non-Swift inputs mark an unsupported mixed-language or custom module target.
pub fn foreign_input(path: &str) -> bool {
    path.rsplit_once('.').is_some_and(|(_, suffix)| {
        [
            "c",
            "cc",
            "cpp",
            "cxx",
            "m",
            "mm",
            "h",
            "hpp",
            "modulemap",
            "swiftinterface",
        ]
        .contains(&suffix.to_ascii_lowercase().as_str())
    })
}

/// Every path is relative to the captured layer. Swift's filelists use newline delimiters.
pub fn source_path(path: &str) -> bool {
    path.len() <= 240
        && !path.contains(['\\', ':', '\0', '\n', '\r'])
        && path.split('/').all(|part| {
            !matches!(part, "" | "." | "..")
                && [
                    ".git",
                    ".build",
                    ".swiftpm",
                    "build",
                    "deriveddata",
                    "node_modules",
                    "vendor",
                ]
                .iter()
                .all(|name| !part.eq_ignore_ascii_case(name))
        })
}
