use crate::source::File;
use devworks_adapters::index::manifests::{
    SwiftDependency, SwiftPackageLayout, SwiftTargetLayout, swift_has_main_attribute,
    swift_package_layout,
};
use devworks_core::fingerprint::sha256_hex;
use devworks_core::ports::packages::swift::{foreign_input, source_path};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

const UNSUPPORTED: &str = "swift-project-unsupported";

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Module {
    pub name: String,
    pub id: String,
    pub manifest: Option<String>,
    pub package_name: Option<String>,
    pub target_name: String,
    pub files: Vec<String>,
    pub swift_version: String,
    pub deployment: Option<String>,
    pub defines: Vec<String>,
    pub parse_as_library: bool,
    pub mixed: bool,
    pub testing: bool,
    pub dependencies: BTreeSet<usize>,
    pub missing_dependency: bool,
}

pub(crate) struct Workspace {
    pub modules: Vec<Module>,
    pub gaps: BTreeMap<String, String>,
}

fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(parent, _)| parent)
}

fn within(file: &str, root: &str) -> bool {
    root.is_empty()
        || file == root
        || file
            .strip_prefix(root)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn join(base: &str, path: &str) -> Result<String, &'static str> {
    if path.starts_with('/') || path.contains(['\\', ':', '\0', '\n', '\r']) {
        return Err(UNSUPPORTED);
    }
    let mut parts: Vec<_> = base.split('/').filter(|part| !part.is_empty()).collect();
    for part in path.split('/') {
        match part {
            "." | "" => {}
            ".." => {
                parts.pop().ok_or(UNSUPPORTED)?;
            }
            _ => parts.push(part),
        }
    }
    let result = parts.join("/");
    if !result.is_empty() && !source_path(&result) {
        return Err(UNSUPPORTED);
    }
    Ok(result)
}

fn compiler_name(name: &str) -> Result<String, &'static str> {
    if name.len() > 120 || name.is_empty() || name.contains(['/', '\\', ':']) {
        return Err(UNSUPPORTED);
    }
    let mut clean: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if clean.as_bytes()[0].is_ascii_digit() {
        clean.insert(0, '_');
    }
    if [
        "Swift",
        "SwiftOnoneSupport",
        "SwiftShims",
        "Foundation",
        "Testing",
        "XCTest",
        "Observation",
    ]
    .contains(&clean.as_str())
    {
        return Err(UNSUPPORTED);
    }
    Ok(clean)
}

fn swift(file: &File) -> bool {
    file.language == "swift"
}

fn is_manifest(file: &File) -> bool {
    file.language == "swift-context" && file.unit.rsplit('/').next() == Some("Package.swift")
}

fn source_matches(
    target: &SwiftTargetLayout,
    root: &str,
    file: &str,
) -> Result<bool, &'static str> {
    if !within(file, root) {
        return Ok(false);
    }
    for exclude in &target.exclude {
        let exclude = join(root, exclude)?;
        if !within(&exclude, root) {
            return Err(UNSUPPORTED);
        }
        if within(file, &exclude) {
            return Ok(false);
        }
    }
    if let Some(sources) = &target.sources {
        for source in sources {
            let source = join(root, source)?;
            if !within(&source, root) {
                return Err(UNSUPPORTED);
            }
            if within(file, &source) {
                return Ok(true);
            }
        }
        return Ok(false);
    }
    Ok(true)
}

fn target_dependencies<'a>(
    dep: &'a SwiftDependency,
    home: &'a str,
    package: &'a SwiftPackageLayout,
    packages: &'a BTreeMap<String, SwiftPackageLayout>,
) -> Result<Option<(String, Vec<&'a str>)>, &'static str> {
    let SwiftDependency::Product { name, package: id } = dep else {
        let SwiftDependency::Target(name) = dep else {
            unreachable!()
        };
        return Ok(Some((home.to_owned(), vec![name])));
    };
    let Some(path) = package.packages.get(&id.to_ascii_lowercase()) else {
        return Ok(None);
    };
    let directory = join(parent(home), path)?;
    let manifest = join(&directory, "Package.swift")?;
    let Some(layout) = packages.get(&manifest) else {
        return Ok(None);
    };
    Ok(layout
        .products
        .get(name)
        .map(|targets| (manifest, targets.iter().map(String::as_str).collect())))
}

impl Workspace {
    pub(crate) fn build(files_in_capture: &[File]) -> Result<Self, &'static str> {
        if files_in_capture.iter().any(|file| {
            file.language == "swift-context"
                && file
                    .unit
                    .rsplit('/')
                    .next()
                    .is_some_and(|name| name.starts_with("Package@swift-"))
        }) {
            return Err(UNSUPPORTED);
        }
        let packages = files_in_capture
            .iter()
            .filter(|file| is_manifest(file))
            .map(|file| Ok((file.unit.clone(), swift_package_layout(&file.text)?)))
            .collect::<Result<BTreeMap<_, _>, &'static str>>()?;
        let xcode: BTreeSet<_> = files_in_capture
            .iter()
            .filter(|file| {
                file.language == "swift-context" && file.unit.ends_with("project.pbxproj")
            })
            .map(|file| {
                let directory = parent(&file.unit);
                if directory.ends_with(".xcodeproj") {
                    parent(directory)
                } else {
                    directory
                }
            })
            .collect();
        let mut modules = Vec::new();
        let mut names = BTreeSet::new();
        let mut mapping = BTreeMap::new();
        let mut owners = BTreeSet::new();
        for (manifest, package) in &packages {
            for target in &package.targets {
                let name = compiler_name(&target.name)?;
                if !names.insert(name.clone())
                    || modules.len() >= 64
                    || mapping
                        .insert((manifest.clone(), target.name.clone()), modules.len())
                        .is_some()
                {
                    return Err(UNSUPPORTED);
                }
                let package_root = parent(manifest);
                let root = join(package_root, &target.path)?;
                if !within(&root, package_root) {
                    return Err(UNSUPPORTED);
                }
                let mut files = Vec::new();
                let mut mixed = false;
                let mut has_main_attribute = false;
                for file in files_in_capture {
                    if !source_matches(target, &root, &file.unit)? {
                        continue;
                    }
                    if !swift(file) {
                        mixed |= foreign_input(&file.unit);
                        continue;
                    }
                    if !owners.insert(file.unit.clone()) {
                        return Err(UNSUPPORTED);
                    }
                    has_main_attribute |= swift_has_main_attribute(&file.text);
                    files.push(file.unit.clone());
                }
                let executable = target.kind != "testTarget"
                    && (target.kind == "executableTarget"
                        || files
                            .iter()
                            .any(|path| path.rsplit('/').next() == Some("main.swift")));
                let package_name = (package.tools_version >= (5, 9)).then(|| {
                    // Stable across checkouts. Distinct package roots never share package access.
                    format!("devworks_{}", sha256_hex(manifest.as_bytes()))
                });
                modules.push(Module {
                    name,
                    id: format!("{manifest}#target:{}", target.name),
                    manifest: Some(manifest.clone()),
                    target_name: target.name.clone(),
                    parse_as_library: !executable
                        || (package.tools_version >= (5, 5)
                            && files.len() == 1
                            && has_main_attribute),
                    package_name,
                    mixed,
                    files,
                    swift_version: package.swift_version.clone(),
                    // SwiftPM uses the installed SDK's minimum for the qualified toolchain.
                    deployment: Some(
                        package
                            .macos_version
                            .clone()
                            .unwrap_or_else(|| "12.0".into()),
                    ),
                    defines: target.defines.clone(),
                    testing: target.kind == "testTarget",
                    dependencies: BTreeSet::new(),
                    missing_dependency: false,
                });
            }
        }
        for (manifest, package) in &packages {
            for target in &package.targets {
                let index = mapping[&(manifest.clone(), target.name.clone())];
                let module = &mut modules[index];
                for dependency in &target.dependencies {
                    if let Some((home, targets)) =
                        target_dependencies(dependency, manifest, package, &packages)?
                    {
                        for name in targets {
                            if let Some(index) = mapping.get(&(home.clone(), name.to_owned())) {
                                module.dependencies.insert(*index);
                            } else {
                                module.missing_dependency = true;
                            }
                        }
                    } else {
                        module.missing_dependency = true;
                    }
                }
            }
        }
        let mut gaps = BTreeMap::new();
        let mut loose = Vec::new();
        for file in files_in_capture
            .iter()
            .filter(|file| swift(file) && !owners.contains(&file.unit))
        {
            let reason = if xcode.iter().any(|dir| within(&file.unit, dir)) {
                "swift-xcode-project-unsupported"
            } else if packages
                .keys()
                .any(|manifest| within(&file.unit, parent(manifest)))
            {
                "swift-source-excluded"
            } else {
                loose.push(file.unit.clone());
                continue;
            };
            gaps.insert(file.unit.clone(), reason.into());
        }
        if !loose.is_empty() {
            let name = "DevWorksLooseSwift";
            if names.contains(name) || modules.len() >= 64 {
                return Err(UNSUPPORTED);
            }
            modules.push(Module {
                name: name.into(),
                id: "loose".into(),
                manifest: None,
                target_name: name.into(),
                parse_as_library: !loose
                    .iter()
                    .any(|path| path.rsplit('/').next() == Some("main.swift")),
                package_name: None,
                mixed: false,
                files: loose,
                swift_version: "6".into(),
                deployment: None,
                defines: Vec::new(),
                testing: false,
                dependencies: BTreeSet::new(),
                missing_dependency: false,
            });
        }
        Ok(Self { modules, gaps })
    }

    pub(crate) fn order(&self) -> Result<Vec<usize>, &'static str> {
        let mut output = Vec::new();
        let mut visited = vec![0; self.modules.len()];
        for index in 0..self.modules.len() {
            self.visit(index, &mut visited, &mut output)?;
        }
        Ok(output)
    }

    fn visit(
        &self,
        index: usize,
        visited: &mut [u8],
        output: &mut Vec<usize>,
    ) -> Result<(), &'static str> {
        match visited[index] {
            1 => return Err(UNSUPPORTED),
            2 => return Ok(()),
            _ => {}
        }
        visited[index] = 1;
        for &dependency in &self.modules[index].dependencies {
            self.visit(dependency, visited, output)?;
        }
        visited[index] = 2;
        output.push(index);
        Ok(())
    }

    pub(crate) fn closure(&self, index: usize) -> BTreeSet<usize> {
        let mut included = BTreeSet::new();
        let mut pending: Vec<_> = self.modules[index].dependencies.iter().copied().collect();
        while let Some(next) = pending.pop() {
            if included.insert(next) {
                pending.extend(self.modules[next].dependencies.iter().copied());
            }
        }
        included
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::fixture_file;

    fn manifest(path: &str, declaration: &str) -> File {
        fixture_file(
            path,
            "swift-context",
            &format!(
                "// swift-tools-version:6.0\nimport PackageDescription\nlet package = {declaration}"
            ),
        )
    }

    #[test]
    fn swift_workspace_binds_nested_products_and_reports_unselected_sources() {
        let files = [
            manifest(
                "Root/Package.swift",
                r#"Package(name: "Root",
                dependencies: [.package(path: "../Dep")],
                targets: [.executableTarget(name: "Tool", dependencies: [.product(name: "Core", package: "dep")], exclude: ["Omit.swift"])])"#,
            ),
            manifest(
                "Dep/Package.swift",
                r#"Package(name: "Dependency",
                products: [.library(name: "Core", targets: ["Build"])], targets: [.target(name: "Build")])"#,
            ),
            fixture_file(
                "Root/Sources/Tool/Tool.swift",
                "swift",
                "@main struct Tool {}",
            ),
            fixture_file("Root/Sources/Tool/Omit.swift", "swift", "struct Omitted {}"),
            fixture_file(
                "Dep/Sources/Build/Build.swift",
                "swift",
                "public struct Build {}",
            ),
            fixture_file("Other.swift", "swift", "let detached = 0"),
            fixture_file("IDE/Project.xcodeproj/project.pbxproj", "swift-context", ""),
            fixture_file("IDE/Sources/Tool.swift", "swift", "let xcode = 0"),
        ];
        let work = Workspace::build(&files).unwrap();
        let ordered: Vec<_> = work
            .order()
            .unwrap()
            .into_iter()
            .map(|id| work.modules[id].name.as_str())
            .collect();
        assert_eq!(ordered, ["Build", "Tool", "DevWorksLooseSwift"]);
        assert!(work.modules[1].parse_as_library);
        assert_ne!(work.modules[0].package_name, work.modules[1].package_name);
        assert_eq!(work.modules[1].deployment.as_deref(), Some("12.0"));
        assert_eq!(
            work.gaps["Root/Sources/Tool/Omit.swift"],
            "swift-source-excluded"
        );
        assert_eq!(
            work.gaps["IDE/Sources/Tool.swift"],
            "swift-xcode-project-unsupported"
        );
    }

    #[test]
    fn swift_workspace_rejects_ambiguous_escaping_and_cyclic_layouts() {
        for targets in [
            r#".target(name: "A", path: "Src"), .target(name: "A", path: "Other")"#,
            r#".target(name: "A", path: "Src"), .target(name: "B", path: "Src")"#,
            r#".target(name: "A", path: "../outside")"#,
            r#".target(name: "A", dependencies: ["B"]), .target(name: "B", dependencies: ["A"])"#,
        ] {
            let files = [
                manifest(
                    "Package.swift",
                    &format!(r#"Package(name: "P", targets: [{targets}])"#),
                ),
                fixture_file("Src/Owned.swift", "swift", "struct Owned {}"),
            ];
            assert_eq!(
                Workspace::build(&files).and_then(|workspace| workspace.order()),
                Err(UNSUPPORTED),
                "{targets}"
            );
        }
        assert!(
            Workspace::build(&[fixture_file("Package@swift-6.swift", "swift-context", "")])
                .is_err()
        );
    }
}
