//! Retained linker rules the name linker applies to calls (DW-023):
//!
//! - [`call_target_allowed`]: the retained `CallLanguages.IsCallTarget`/`CanCall`. A call binds
//!   to a definition of its own language family; across a family boundary only a bare call to a
//!   free function the foreign-function boundary really links by name: Swift and Rust call C and
//!   Objective-C functions, Rust reaches C++ only through C linkage (`extern "C"`), Swift's C++
//!   interop calls any C++ function, and Swift and C call a Rust function only when it exports a
//!   C ABI under its own name. A type is a call target only as a constructor call within its own
//!   family; a file of an unmapped extension is not filtered by family.
//! - [`SwiftScopes`]: the retained `SwiftModuleScopes` (`IndexingPipeline.SwiftModuleScope.cs`)
//!   and `PreferVisibleSwiftCandidates`: which Swift module each Swift file belongs to and which
//!   in-repository modules it sees through its imports (re-exports included), so a Swift call
//!   several definitions share binds to the one its file can see, and a receiver type path
//!   several modules declare names the visible one.
use std::collections::{HashMap, HashSet};

/// The retained `CallLanguages.FamilyByExtension`.
pub(crate) fn call_family(path: &str) -> Option<&'static str> {
    let ext = path
        .rsplit('/')
        .next()?
        .rsplit_once('.')?
        .1
        .to_ascii_lowercase();
    Some(match ext.as_str() {
        "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hxx" | "hh" | "m" | "mm" => "c",
        "swift" => "swift",
        "rs" => "rust",
        "py" | "pyw" | "pyi" => "python",
        "ts" | "tsx" | "mts" | "cts" | "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "cs" | "fs" | "vb" => "dotnet",
        "java" | "kt" | "kts" | "scala" => "jvm",
        "go" => "go",
        "rb" => "ruby",
        "php" => "php",
        "sh" | "bash" | "zsh" => "shell",
        "cfm" | "cfc" => "cfml",
        "sql" => "sql",
        _ => return None,
    })
}

fn is_cpp_file(path: &str) -> bool {
    let ext = path
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    matches!(
        ext.as_str(),
        "cc" | "cpp" | "cxx" | "hpp" | "hxx" | "hh" | "mm"
    )
}

/// The retained `CallLanguages.CallableLabels`.
const CALLABLE_KINDS: [&str; 5] = [
    "Function",
    "Method",
    "Constructor",
    "Delegate",
    "ExternalSymbol",
];
/// The retained `CallLanguages.ConstructibleTypeLabels`.
const CONSTRUCTIBLE_KINDS: [&str; 7] = [
    "Class",
    "Struct",
    "Record",
    "Enum",
    "Component",
    "Service",
    "Job",
];

/// A call target candidate as the family rules read it.
pub(crate) struct CallTarget<'a> {
    pub unit: &'a str,
    pub kind: &'a str,
    /// The `abi` property: recorded only for a function exported to foreign callers by name.
    pub abi: Option<&'a str>,
}

/// The retained `CallLanguages.IsCallTarget(callerFamily, candidate, isBareCall)`.
pub(crate) fn call_target_allowed(caller_unit: &str, c: &CallTarget<'_>, bare: bool) -> bool {
    let callable = CALLABLE_KINDS.contains(&c.kind);
    if !callable && !CONSTRUCTIBLE_KINDS.contains(&c.kind) {
        return false;
    }
    let (caller, candidate) = (call_family(caller_unit), call_family(c.unit));
    let (Some(caller), Some(candidate)) = (caller, candidate) else {
        return true;
    };
    if caller == candidate {
        return true;
    }
    bare && c.kind == "Function" && can_call(caller, candidate, c)
}

/// The retained `CallLanguages.CanCall` across two different families.
fn can_call(caller: &str, candidate: &str, c: &CallTarget<'_>) -> bool {
    let exports_c_abi = c.abi.is_some_and(|a| !a.is_empty() && a != "Rust");
    match candidate {
        "c" => caller == "swift" || (caller == "rust" && (!is_cpp_file(c.unit) || exports_c_abi)),
        "rust" => caller == "swift" || (caller == "c" && exports_c_abi),
        _ => false,
    }
}

// ---------------------------------------------------------------------------------------------
// Swift module scopes

/// A Swift module provider (a SwiftPM target or an Xcode native target) as the index records it.
#[derive(Clone, Debug, Default)]
pub(crate) struct SwiftModule {
    /// The node's qualified name (its identity among providers).
    pub qualified_name: String,
    /// The module an `import` names (`module_name`, else the node name).
    pub module_name: String,
    pub xcode: bool,
    pub manifest_path: String,
    pub project_directory: Option<String>,
    pub source_path: Option<String>,
    /// `sources:` subpaths, comma-separated.
    pub source_paths: Option<String>,
    pub package_key: Option<String>,
}

/// One Swift module import of a file (`import M`, `@_exported import M`).
#[derive(Clone, Debug)]
pub(crate) struct SwiftImport {
    pub file: String,
    /// The imported path as written (`Darwin.C`): its top-level module is imported.
    pub module: String,
    pub exported: bool,
}

fn trim_slashes(p: &str) -> String {
    p.trim_matches('/').to_owned()
}

/// The modules of every Swift file, and what each file sees through its imports.
#[derive(Debug, Default)]
pub(crate) struct SwiftScopes {
    roots: Vec<(String, String, bool)>,
    imports_by_file: HashMap<String, HashSet<String>>,
    reexports_by_scope: HashMap<String, HashSet<String>>,
}

impl SwiftScopes {
    pub(crate) fn new(modules: &[SwiftModule], imports: &[SwiftImport]) -> Self {
        let mut s = Self::default();
        // Roots of each module; SwiftPM modules sharing a root are one scope (union by root).
        let mut rooted: Vec<(&SwiftModule, Vec<String>)> = Vec::new();
        for m in modules {
            let roots: Vec<String> = if m.xcode {
                m.project_directory
                    .iter()
                    .map(|d| trim_slashes(d))
                    .collect()
            } else if let Some(sources) = m.source_paths.as_deref().filter(|x| !x.is_empty()) {
                sources
                    .split(',')
                    .filter(|x| !x.is_empty())
                    .map(trim_slashes)
                    .collect()
            } else {
                m.source_path.iter().map(|d| trim_slashes(d)).collect()
            };
            let mut distinct = Vec::new();
            for r in roots {
                if !distinct.contains(&r) {
                    distinct.push(r);
                }
            }
            if !distinct.is_empty() {
                rooted.push((m, distinct));
            }
        }
        let key_of = |m: &SwiftModule| {
            if m.xcode {
                format!("xcode:{}", m.manifest_path)
            } else {
                m.qualified_name.clone()
            }
        };
        let mut parent: HashMap<String, String> = HashMap::new();
        fn find(parent: &HashMap<String, String>, mut k: String) -> String {
            while let Some(next) = parent.get(&k).filter(|n| **n != k) {
                k = next.clone();
            }
            k
        }
        let mut by_root: Vec<(String, Vec<String>)> = Vec::new();
        for (m, roots) in rooted.iter().filter(|(m, _)| !m.xcode) {
            for r in roots {
                let key = key_of(m);
                match by_root.iter_mut().find(|(root, _)| root == r) {
                    Some((_, keys)) => {
                        if !keys.contains(&key) {
                            keys.push(key);
                        }
                    }
                    None => by_root.push((r.clone(), vec![key])),
                }
            }
        }
        for (_, keys) in &by_root {
            for k in keys.iter().skip(1) {
                let (a, b) = (find(&parent, keys[0].clone()), find(&parent, k.clone()));
                if a != b {
                    if a < b {
                        parent.insert(b, a);
                    } else {
                        parent.insert(a, b);
                    }
                }
            }
        }
        let mut scope_by_module: HashMap<String, String> = HashMap::new();
        for (m, roots) in &rooted {
            let scope = find(&parent, key_of(m));
            scope_by_module.insert(m.qualified_name.clone(), scope.clone());
            for r in roots {
                s.roots.push((r.clone(), scope.clone(), m.xcode));
            }
        }
        for import in imports {
            let name = import.module.split('.').next().unwrap_or(&import.module);
            for provider in nearest_targets(modules, name, &import.file) {
                let Some(scope) = scope_by_module.get(&provider.qualified_name) else {
                    continue;
                };
                s.imports_by_file
                    .entry(import.file.clone())
                    .or_default()
                    .insert(scope.clone());
                if !import.exported {
                    continue;
                }
                if let Some(exporter) = s.scope_of(&import.file)
                    && exporter != scope
                {
                    let exporter = exporter.to_owned();
                    s.reexports_by_scope
                        .entry(exporter)
                        .or_default()
                        .insert(scope.clone());
                }
            }
        }
        s
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }

    /// The module scope a repository-relative Swift file belongs to: the module whose source
    /// root is its nearest ancestor (or the file itself); a SwiftPM target wins over an Xcode
    /// project at the same depth.
    pub(crate) fn scope_of(&self, file: &str) -> Option<&str> {
        let mut best: Option<(usize, bool, &str)> = None;
        for (root, scope, xcode) in &self.roots {
            if !root.is_empty() && file != root && !file.starts_with(&format!("{root}/")) {
                continue;
            }
            let depth = if root.is_empty() {
                0
            } else {
                root.matches('/').count() + 1
            };
            let better = match best {
                None => true,
                Some((d, best_xcode, _)) => depth > d || (depth == d && best_xcode && !xcode),
            };
            if better {
                best = Some((depth, *xcode, scope.as_str()));
            }
        }
        best.map(|(_, _, s)| s)
    }

    /// The scopes a file sees through its imports and, transitively, their re-exports.
    pub(crate) fn imported_scopes(&self, file: &str) -> HashSet<String> {
        let mut visible = HashSet::new();
        let mut pending: Vec<String> = self
            .imports_by_file
            .get(file)
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default();
        while let Some(scope) = pending.pop() {
            if !visible.insert(scope.clone()) {
                continue;
            }
            if let Some(re) = self.reexports_by_scope.get(&scope) {
                pending.extend(re.iter().cloned());
            }
        }
        visible
    }

    /// Whether a Swift file (in `caller_scope`) sees a definition declared in `candidate_file`:
    /// in its own module, in a module it sees through its imports, or in no module.
    pub(crate) fn visible(
        &self,
        caller_scope: Option<&str>,
        caller_file: &str,
        candidate_file: &str,
    ) -> bool {
        let Some(caller_scope) = caller_scope else {
            return true;
        };
        match self.scope_of(candidate_file) {
            None => true,
            Some(s) if s == caller_scope => true,
            Some(s) => self.imported_scopes(caller_file).contains(s),
        }
    }

    /// The retained `PreferVisibleSwiftCandidates`: the single Swift candidate of the caller's own
    /// module, else the single Swift candidate of the modules the caller's file imports; otherwise
    /// every candidate (still ambiguous). `unit_of` names each candidate's file.
    pub(crate) fn prefer<T>(
        &self,
        caller_file: &str,
        candidates: Vec<T>,
        unit_of: impl Fn(&T) -> &str,
    ) -> Vec<T> {
        let Some(caller_scope) = self.scope_of(caller_file) else {
            return candidates;
        };
        let swift: Vec<(usize, Option<&str>)> = candidates
            .iter()
            .enumerate()
            .filter(|(_, c)| call_family(unit_of(c)) == Some("swift"))
            .map(|(i, c)| (i, self.scope_of(unit_of(c))))
            .collect();
        let own: Vec<usize> = swift
            .iter()
            .filter(|(_, s)| *s == Some(caller_scope))
            .map(|(i, _)| *i)
            .collect();
        let pick = match own.len() {
            1 => Some(own[0]),
            0 => {
                let imported = self.imported_scopes(caller_file);
                let visible: Vec<usize> = swift
                    .iter()
                    .filter(|(_, s)| s.is_some_and(|s| imported.contains(s)))
                    .map(|(i, _)| *i)
                    .collect();
                (visible.len() == 1).then(|| visible[0])
            }
            _ => None,
        };
        match pick {
            Some(i) => candidates.into_iter().nth(i).into_iter().collect(),
            None => candidates,
        }
    }
}

/// The retained `SwiftModuleResolver.Resolve` restricted to targets (`matched_by: target`, the
/// only matches module scopes read): the targets named `module` whose manifest directory is the
/// nearest ancestor of `file` (all of them when none is), SwiftPM before Xcode at the same depth,
/// one per package.
fn nearest_targets<'m>(
    modules: &'m [SwiftModule],
    module: &str,
    file: &str,
) -> Vec<&'m SwiftModule> {
    let mut ranked: Vec<(&SwiftModule, i64)> = Vec::new();
    for m in modules.iter().filter(|m| m.module_name == module) {
        if ranked
            .iter()
            .any(|(r, _)| r.qualified_name == m.qualified_name)
        {
            continue;
        }
        let dir = match &m.project_directory {
            Some(d) => d.clone(),
            None => super::paths::parent(&m.manifest_path).to_owned(),
        };
        let depth = if dir.is_empty() {
            0
        } else if file.starts_with(&format!("{dir}/")) {
            dir.matches('/').count() as i64 + 1
        } else {
            -1
        };
        ranked.push((m, depth));
    }
    let Some(best) = ranked.iter().map(|(_, d)| *d).max() else {
        return Vec::new();
    };
    let mut nearest: Vec<&SwiftModule> = ranked
        .into_iter()
        .filter(|(_, d)| *d == best)
        .map(|(m, _)| m)
        .collect();
    nearest.sort_by(|a, b| a.qualified_name.cmp(&b.qualified_name));
    if nearest.iter().any(|m| !m.xcode) {
        nearest.retain(|m| !m.xcode);
    }
    let mut seen: Vec<&str> = Vec::new();
    nearest.retain(|m| {
        let key = m.package_key.as_deref().unwrap_or(&m.qualified_name);
        if seen.contains(&key) {
            false
        } else {
            seen.push(key);
            true
        }
    });
    nearest
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(qn: &str, name: &str, manifest: &str, source: &str) -> SwiftModule {
        SwiftModule {
            qualified_name: qn.into(),
            module_name: name.into(),
            manifest_path: manifest.into(),
            source_path: Some(source.into()),
            ..Default::default()
        }
    }

    #[test]
    fn calls_cross_language_families_only_at_real_ffi_boundaries() {
        let f = |unit: &'static str, kind: &'static str, abi: Option<&'static str>| CallTarget {
            unit,
            kind,
            abi,
        };
        // Same family: callables and constructed types.
        assert!(call_target_allowed(
            "a.rs",
            &f("b.rs", "Method", None),
            false
        ));
        assert!(call_target_allowed(
            "a.rs",
            &f("b.rs", "Struct", None),
            true
        ));
        // Never a module, file or namespace.
        assert!(!call_target_allowed(
            "a.rs",
            &f("b.rs", "Module", None),
            true
        ));
        // Rust reaches C, and C++ only through C linkage; only a bare call to a free function.
        assert!(call_target_allowed(
            "a.rs",
            &f("lib.c", "Function", None),
            true
        ));
        assert!(!call_target_allowed(
            "a.rs",
            &f("lib.c", "Function", None),
            false
        ));
        assert!(!call_target_allowed(
            "a.rs",
            &f("lib.cpp", "Function", None),
            true
        ));
        assert!(call_target_allowed(
            "a.rs",
            &f("lib.cpp", "Function", Some("C")),
            true
        ));
        assert!(!call_target_allowed(
            "a.rs",
            &f("lib.cpp", "Method", Some("C")),
            true
        ));
        assert!(!call_target_allowed(
            "a.rs",
            &f("lib.c", "Struct", None),
            true
        ));
        // Swift calls any C/C++ function and any Rust function; C a Rust one only with a C ABI.
        assert!(call_target_allowed(
            "A.swift",
            &f("lib.cpp", "Function", None),
            true
        ));
        assert!(call_target_allowed(
            "A.swift",
            &f("lib.rs", "Function", None),
            true
        ));
        assert!(!call_target_allowed(
            "main.c",
            &f("lib.rs", "Function", None),
            true
        ));
        assert!(call_target_allowed(
            "main.c",
            &f("lib.rs", "Function", Some("C")),
            true
        ));
        assert!(!call_target_allowed(
            "main.c",
            &f("lib.rs", "Function", Some("Rust")),
            true
        ));
        // Other families never cross; an unmapped extension is not filtered.
        assert!(!call_target_allowed(
            "a.py",
            &f("b.go", "Function", None),
            true
        ));
        assert!(call_target_allowed(
            "a.tf",
            &f("b.go", "Function", None),
            true
        ));
    }

    #[test]
    fn swift_files_see_their_module_and_its_imports_with_reexports() {
        let modules = vec![
            target(
                "r:Package.swift#target:App",
                "App",
                "Package.swift",
                "Sources/App",
            ),
            target(
                "r:Package.swift#target:Core",
                "Core",
                "Package.swift",
                "Sources/Core",
            ),
            target(
                "r:Package.swift#target:Util",
                "Util",
                "Package.swift",
                "Sources/Util",
            ),
            target(
                "r:Vendor/Package.swift#target:App",
                "App",
                "Vendor/Package.swift",
                "Vendor/Sources/App",
            ),
        ];
        let imports = vec![
            SwiftImport {
                file: "Sources/App/main.swift".into(),
                module: "Core".into(),
                exported: false,
            },
            SwiftImport {
                file: "Sources/Core/Exports.swift".into(),
                module: "Util".into(),
                exported: true,
            },
        ];
        let s = SwiftScopes::new(&modules, &imports);
        let app = s.scope_of("Sources/App/main.swift").map(str::to_owned);
        assert_eq!(app.as_deref(), Some("r:Package.swift#target:App"));
        assert_eq!(s.scope_of("README.swift"), None);
        let seen = s.imported_scopes("Sources/App/main.swift");
        assert!(seen.contains("r:Package.swift#target:Core"));
        assert!(
            seen.contains("r:Package.swift#target:Util"),
            "a re-export is seen"
        );
        assert!(s.visible(
            app.as_deref(),
            "Sources/App/main.swift",
            "Sources/Util/U.swift"
        ));
        assert!(!s.visible(
            app.as_deref(),
            "Sources/App/main.swift",
            "Vendor/Sources/App/V.swift"
        ));
        // A tie: the caller's own module wins, else the single imported one.
        let pick = s.prefer(
            "Sources/App/main.swift",
            vec!["Vendor/Sources/App/V.swift", "Sources/App/A.swift"],
            |c| c,
        );
        assert_eq!(pick, vec!["Sources/App/A.swift"]);
        let pick = s.prefer(
            "Sources/App/main.swift",
            vec!["Vendor/Sources/App/V.swift", "Sources/Core/C.swift"],
            |c| c,
        );
        assert_eq!(pick, vec!["Sources/Core/C.swift"]);
        let pick = s.prefer(
            "Sources/App/main.swift",
            vec!["Sources/App/A.swift", "Sources/App/B.swift"],
            |c| c,
        );
        assert_eq!(pick.len(), 2, "two in its own module stay ambiguous");
        // SwiftPM targets sharing a root are one scope (named by its least key, whatever the
        // declaration order).
        let shared = SwiftScopes::new(
            &[
                target("r:P.swift#target:B", "B", "P.swift", "Src"),
                target("r:P.swift#target:A", "A", "P.swift", "Src"),
            ],
            &[],
        );
        assert_eq!(shared.scope_of("Src/x.swift"), Some("r:P.swift#target:A"));
    }
}
