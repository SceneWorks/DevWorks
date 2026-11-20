use crate::source::{File, Source};
use devworks_adapters::index::manifests::{
    SwiftDependency, SwiftPackageLayout, SwiftTargetLayout, swift_package_layout,
};
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
    pub target_name: String,
    pub files: Vec<String>,
    pub swift_version: String,
    pub deployment: Option<String>,
    pub defines: Vec<String>,
    pub executable: bool,
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
        if within(file, &join(root, exclude)?) {
            return Ok(false);
        }
    }
    if let Some(sources) = &target.sources {
        for source in sources {
            if within(file, &join(root, source)?) {
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
    pub(crate) fn build(source: &Source) -> Result<Self, &'static str> {
        if source.files.iter().any(|file| {
            file.language == "swift-context"
                && file
                    .unit
                    .rsplit('/')
                    .next()
                    .is_some_and(|name| name.starts_with("Package@swift-"))
        }) {
            return Err(UNSUPPORTED);
        }
        let packages = source
            .files
            .iter()
            .filter(|file| is_manifest(file))
            .map(|file| Ok((file.unit.clone(), swift_package_layout(&file.text)?)))
            .collect::<Result<BTreeMap<_, _>, &'static str>>()?;
        let xcode: BTreeSet<_> = source
            .files
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
                let root = join(parent(manifest), &target.path)?;
                let mut files = Vec::new();
                let mut mixed = false;
                for file in &source.files {
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
                    files.push(file.unit.clone());
                }
                modules.push(Module {
                    name,
                    id: format!("{manifest}#target:{}", target.name),
                    manifest: Some(manifest.clone()),
                    target_name: target.name.clone(),
                    executable: target.kind == "executableTarget"
                        || files
                            .iter()
                            .any(|path| path.rsplit('/').next() == Some("main.swift")),
                    files,
                    swift_version: package.swift_version.clone(),
                    deployment: package.macos_version.clone(),
                    defines: target.defines.clone(),
                    testing: target.kind == "testTarget",
                    dependencies: BTreeSet::new(),
                    missing_dependency: mixed,
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
        for file in source
            .files
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
                executable: loose
                    .iter()
                    .any(|path| path.rsplit('/').next() == Some("main.swift")),
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
