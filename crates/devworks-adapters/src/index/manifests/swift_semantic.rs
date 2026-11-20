//! Static SwiftPM inputs for compiler indexing. The structural parser keeps its permissive
//! extraction rules; compilation uses only the manifest shapes modeled here.
use super::swiftpm::{closing, label, literal, scan, split_top};
use std::collections::BTreeMap;

const UNSUPPORTED: &str = "swift-project-unsupported";

#[derive(Debug)]
pub struct SwiftPackageLayout {
    pub swift_version: String,
    pub macos_version: Option<String>,
    pub packages: BTreeMap<String, String>,
    pub products: BTreeMap<String, Vec<String>>,
    pub targets: Vec<SwiftTargetLayout>,
}

#[derive(Debug)]
pub struct SwiftTargetLayout {
    pub kind: String,
    pub name: String,
    pub path: String,
    pub sources: Option<Vec<String>>,
    pub exclude: Vec<String>,
    pub defines: Vec<String>,
    pub dependencies: Vec<SwiftDependency>,
}

#[derive(Debug)]
pub enum SwiftDependency {
    Target(String),
    Product { name: String, package: String },
}

#[derive(Clone, Copy)]
struct Expr<'a> {
    code: &'a str,
    masked: &'a [u8],
}

type Args<'a> = BTreeMap<String, Expr<'a>>;

impl<'a> Expr<'a> {
    fn trimmed(self) -> Self {
        let start = self
            .masked
            .iter()
            .take_while(|b| b.is_ascii_whitespace())
            .count();
        let end = self
            .masked
            .iter()
            .rposition(|b| !b.is_ascii_whitespace())
            .map_or(start, |i| i + 1);
        self.slice(start, end)
    }

    fn slice(self, start: usize, end: usize) -> Self {
        Self {
            code: &self.code[start..end],
            masked: &self.masked[start..end],
        }
    }

    fn items(self, open: usize) -> Result<Vec<Self>, &'static str> {
        let end = closing(self.masked, open).ok_or(UNSUPPORTED)?;
        if end + 1 != self.masked.len() {
            return Err(UNSUPPORTED);
        }
        Ok(split_top(self.masked, open + 1, end)
            .into_iter()
            .map(|(a, b)| self.slice(a, b).trimmed())
            .filter(|item| !item.masked.is_empty())
            .collect())
    }

    fn array(self) -> Result<Vec<Self>, &'static str> {
        let expr = self.trimmed();
        if expr.masked.first() != Some(&b'[') {
            return Err(UNSUPPORTED);
        }
        expr.items(0)
    }

    fn call(self) -> Result<(&'a str, Vec<Self>), &'static str> {
        let expr = self.trimmed();
        let open = expr
            .masked
            .iter()
            .position(|b| *b == b'(')
            .ok_or(UNSUPPORTED)?;
        let name = expr.code[..open].trim();
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.') {
            return Err(UNSUPPORTED);
        }
        Ok((name, expr.items(open)?))
    }

    fn string(self) -> Result<String, &'static str> {
        literal(self.code)
            .filter(|text| !text.is_empty() && !text.contains(['\0', '\n', '\r']))
            .ok_or(UNSUPPORTED)
    }
}

fn arguments<'a>(items: Vec<Expr<'a>>, allowed: &[&str]) -> Result<Args<'a>, &'static str> {
    let mut result = BTreeMap::new();
    for item in items {
        let (label, offset) = label(item.masked).ok_or(UNSUPPORTED)?;
        if !allowed.contains(&label.as_str())
            || result
                .insert(label, item.slice(offset, item.code.len()))
                .is_some()
        {
            return Err(UNSUPPORTED);
        }
    }
    Ok(result)
}

fn list<'a>(args: &Args<'a>, key: &str) -> Result<Vec<Expr<'a>>, &'static str> {
    args.get(key).map_or(Ok(Vec::new()), |expr| expr.array())
}

fn text(args: &Args<'_>, key: &str) -> Result<String, &'static str> {
    args.get(key).ok_or(UNSUPPORTED)?.string()
}

fn strings(args: &Args<'_>, key: &str) -> Result<Vec<String>, &'static str> {
    list(args, key)?.into_iter().map(Expr::string).collect()
}

fn name_and_kind(name: &str) -> &str {
    name.rsplit('.').next().unwrap_or(name)
}

fn target(expr: Expr<'_>) -> Result<SwiftTargetLayout, &'static str> {
    let (kind, fields) = expr.call()?;
    let kind = name_and_kind(kind);
    if !["target", "executableTarget", "testTarget"].contains(&kind) {
        return Err(UNSUPPORTED);
    }
    let fields = arguments(
        fields,
        &[
            "name",
            "path",
            "sources",
            "exclude",
            "dependencies",
            "swiftSettings",
        ],
    )?;
    let name = text(&fields, "name")?;
    let path = fields
        .get("path")
        .map(|value| value.string())
        .transpose()?
        .unwrap_or_else(|| {
            let folder = if kind == "testTarget" {
                "Tests"
            } else {
                "Sources"
            };
            format!("{folder}/{name}")
        });
    let sources = fields
        .contains_key("sources")
        .then(|| strings(&fields, "sources"))
        .transpose()?;
    let mut dependencies = Vec::new();
    for dependency in list(&fields, "dependencies")? {
        if let Ok(name) = dependency.string() {
            dependencies.push(SwiftDependency::Target(name));
            continue;
        }
        let (kind, values) = dependency.call()?;
        let values = arguments(values, &["name", "package"])?;
        let name = text(&values, "name")?;
        dependencies.push(match name_and_kind(kind) {
            "target" | "byName" if !values.contains_key("package") => SwiftDependency::Target(name),
            "product" => SwiftDependency::Product {
                name,
                package: text(&values, "package")?,
            },
            _ => return Err(UNSUPPORTED),
        });
    }
    let mut defines = Vec::new();
    for define in list(&fields, "swiftSettings")? {
        let (kind, values) = define.call()?;
        let [value] = values.as_slice() else {
            return Err(UNSUPPORTED);
        };
        if name_and_kind(kind) != "define" {
            return Err(UNSUPPORTED);
        }
        let name = value.string()?;
        if !name
            .as_bytes()
            .first()
            .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_')
            || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            return Err(UNSUPPORTED);
        }
        defines.push(name);
    }
    Ok(SwiftTargetLayout {
        name,
        kind: kind.to_owned(),
        path,
        sources,
        exclude: strings(&fields, "exclude")?,
        defines,
        dependencies,
    })
}

pub fn swift_package_layout(content: &str) -> Result<SwiftPackageLayout, &'static str> {
    let tool_line = content.lines().next().ok_or(UNSUPPORTED)?.trim_start();
    let version = tool_line
        .strip_prefix("//")
        .ok_or(UNSUPPORTED)?
        .trim_start()
        .strip_prefix("swift-tools-version:")
        .ok_or(UNSUPPORTED)?
        .trim();
    let major = version
        .split('.')
        .next()
        .filter(|major| matches!(*major, "5" | "6"))
        .ok_or(UNSUPPORTED)?;
    let (code, masked) = scan(content);
    let code = std::str::from_utf8(&code).map_err(|_| UNSUPPORTED)?;
    let visible = std::str::from_utf8(&masked).map_err(|_| UNSUPPORTED)?;
    let start = visible
        .find("Package(")
        .or_else(|| visible.find("Package ("))
        .ok_or(UNSUPPORTED)?;
    let prefix = visible[..start].trim();
    let (declaration, assignment) = prefix.split_once('=').ok_or(UNSUPPORTED)?;
    if declaration.split_whitespace().collect::<Vec<_>>()
        != ["import", "PackageDescription", "let", "package"]
        || !assignment.trim().is_empty()
    {
        return Err(UNSUPPORTED);
    }
    let (name, fields) = Expr {
        code: &code[start..],
        masked: &masked[start..],
    }
    .call()?;
    if name != "Package" {
        return Err(UNSUPPORTED);
    }
    let fields = arguments(
        fields,
        &[
            "name",
            "platforms",
            "products",
            "dependencies",
            "targets",
            "swiftLanguageModes",
            "swiftLanguageVersions",
            "defaultLocalization",
        ],
    )?;
    text(&fields, "name")?;
    let language = if fields.contains_key("swiftLanguageModes") {
        "swiftLanguageModes"
    } else {
        "swiftLanguageVersions"
    };
    let choices = list(&fields, language)?;
    let swift_version = match choices.as_slice() {
        [] => major,
        [mode] => match mode.code.trim() {
            ".v5" => "5",
            ".v6" => "6",
            _ => return Err(UNSUPPORTED),
        },
        _ => return Err(UNSUPPORTED),
    }
    .to_owned();
    let mut macos_version = None;
    for platform in list(&fields, "platforms")? {
        let (kind, values) = platform.call()?;
        let [argument] = values.as_slice() else {
            return Err(UNSUPPORTED);
        };
        if name_and_kind(kind) == "macOS" {
            let value = match argument.code.trim().strip_prefix(".v") {
                Some(version) => version.replace('_', "."),
                None => argument.string()?,
            };
            let parts: Vec<_> = value.split('.').collect();
            if !(1..=3).contains(&parts.len())
                || parts.iter().any(|part| part.parse::<u16>().is_err())
                || macos_version.replace(value).is_some()
            {
                return Err(UNSUPPORTED);
            }
        }
    }

    let mut packages = BTreeMap::new();
    for dependency in list(&fields, "dependencies")? {
        let (kind, values) = dependency.call()?;
        if name_and_kind(kind) != "package" {
            return Err(UNSUPPORTED);
        }
        // External declarations are graph metadata. Never resolve them into compiler paths.
        if !values
            .iter()
            .any(|value| label(value.masked).is_some_and(|(label, _)| label == "path"))
        {
            continue;
        }
        let values = arguments(values, &["name", "path"])?;
        let path = text(&values, "path")?;
        let name = values
            .get("name")
            .map(|value| value.string())
            .transpose()?
            .unwrap_or_else(|| {
                path.trim_end_matches('/')
                    .rsplit('/')
                    .next()
                    .unwrap_or("")
                    .to_owned()
            });
        if name.is_empty() || packages.insert(name.to_ascii_lowercase(), path).is_some() {
            return Err(UNSUPPORTED);
        }
    }

    let mut products = BTreeMap::new();
    for product in list(&fields, "products")? {
        let (kind, values) = product.call()?;
        if !["library", "executable"].contains(&name_and_kind(kind)) {
            return Err(UNSUPPORTED);
        }
        let values = arguments(values, &["name", "targets", "type"])?;
        let targets = strings(&values, "targets")?;
        if targets.is_empty() || products.insert(text(&values, "name")?, targets).is_some() {
            return Err(UNSUPPORTED);
        }
    }
    let targets = list(&fields, "targets")?
        .into_iter()
        .map(target)
        .collect::<Result<Vec<_>, _>>()?;
    if targets.is_empty() {
        return Err(UNSUPPORTED);
    }
    Ok(SwiftPackageLayout {
        swift_version,
        macos_version,
        packages,
        products,
        targets,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PREFIX: &str = "// swift-tools-version:6.0\nimport PackageDescription\nlet package = ";

    #[test]
    fn literal_swiftpm_layout_retains_project_inputs() {
        let manifest = format!(
            "{PREFIX}{}",
            r#"Package(name: "Layout", platforms: [.macOS(.v14)],
            products: [.library(name: "API", targets: ["Core"])],
            dependencies: [.package(name: "Extras", path: "../Extras"), .package(url: "https://example.invalid/pkg", from: "1.0.0")],
            targets: [
                .target(name: "Core", dependencies: [.product(name: "Support", package: "Extras")],
                        path: "Src", sources: ["Feature"], exclude: ["Generated.swift"], swiftSettings: [.define("FEATURE")]),
                .testTarget(name: "CoreTests", dependencies: [.target(name: "Core")])
            ])"#
        );
        let layout = swift_package_layout(&manifest).unwrap();
        assert_eq!(layout.swift_version, "6");
        assert_eq!(layout.macos_version.as_deref(), Some("14"));
        assert_eq!(layout.packages["extras"], "../Extras");
        assert_eq!(layout.products["API"], ["Core"]);
        assert_eq!(layout.targets[0].path, "Src");
        assert_eq!(layout.targets[0].sources.as_deref().unwrap(), ["Feature"]);
        assert_eq!(layout.targets[0].exclude, ["Generated.swift"]);
        assert_eq!(layout.targets[0].defines, ["FEATURE"]);
        assert_eq!(layout.targets[1].path, "Tests/CoreTests");
        assert!(matches!(&layout.targets[0].dependencies[0],
            SwiftDependency::Product { name, package } if name == "Support" && package == "Extras"));
    }

    #[test]
    fn unsupported_manifest_expressions_are_explicit() {
        for body in [
            "Package(name: \"Pkg\", targets: makeTargets())",
            "Package(name: \"Pkg\", targets: [.target(name: \"Core\", path: selectedPath)])",
            "Package(name: \"Pkg\", targets: [.target(name: \"Core\", swiftSettings: [.unsafeFlags([\"-I\", \"other\"])])])",
            "Package(name: \"Pkg\", targets: [.testTarget(name: \"Test\", plugins: [.plugin(name: \"BuildCode\")])])",
            "Package(name: \"Pkg\", targets: [.target(name: \"Core\")]); package.targets = []",
        ] {
            assert!(
                swift_package_layout(&format!("{PREFIX}{body}")).is_err(),
                "{body}"
            );
        }
    }
}
