//! Language names are retained metadata; support always follows the actual registered parser.
use crate::ports::indexing::{LanguageMode, StructuralParser};

pub(super) fn describe(
    parser: Option<&dyn StructuralParser>,
    path: &str,
) -> (Option<String>, bool) {
    if let Some(language) = parser.and_then(|p| p.language_of(path)) {
        return (
            Some(language.name.to_owned()),
            language.mode == LanguageMode::Structural,
        );
    }
    let file = path.rsplit('/').next().unwrap_or(path);
    let extension = file
        .rsplit_once('.')
        .filter(|(prefix, _)| !prefix.is_empty())
        .map(|(_, ext)| ext.to_ascii_lowercase());
    let name = match extension.as_deref().unwrap_or("") {
        "cs" => Some("C#"),
        "ts" | "tsx" | "mts" | "cts" => Some("TypeScript"),
        "js" | "jsx" | "mjs" | "cjs" => Some("JavaScript"),
        "rs" => Some("Rust"),
        "swift" => Some("Swift"),
        "py" | "pyw" | "pyi" => Some("Python"),
        "go" => Some("Go"),
        "java" => Some("Java"),
        "rb" => Some("Ruby"),
        "php" => Some("PHP"),
        "sh" | "bash" | "zsh" => Some("Bash"),
        "c" | "h" => Some("C"),
        "cpp" | "cc" | "cxx" | "hpp" | "hxx" | "hh" => Some("C++"),
        "sql" => Some("SQL"),
        "tf" | "tfvars" => Some("Terraform"),
        "cfm" | "cfc" => Some("ColdFusion"),
        "yml" | "yaml" => Some("YAML"),
        "m" => Some("Objective-C"),
        "mm" => Some("Objective-C++"),
        "kt" | "kts" => Some("Kotlin"),
        "scala" | "sc" => Some("Scala"),
        "dart" => Some("Dart"),
        "fs" | "fsx" | "fsi" => Some("F#"),
        "vb" => Some("Visual Basic"),
        "groovy" => Some("Groovy"),
        "lua" => Some("Lua"),
        "pl" | "pm" => Some("Perl"),
        "r" => Some("R"),
        "ex" | "exs" => Some("Elixir"),
        "erl" | "hrl" => Some("Erlang"),
        "hs" => Some("Haskell"),
        "ml" | "mli" => Some("OCaml"),
        "clj" | "cljs" => Some("Clojure"),
        "jl" => Some("Julia"),
        "zig" => Some("Zig"),
        "nim" => Some("Nim"),
        "cr" => Some("Crystal"),
        "ps1" | "psm1" => Some("PowerShell"),
        "vue" => Some("Vue"),
        "svelte" => Some("Svelte"),
        "cshtml" | "razor" => Some("Razor"),
        "sol" => Some("Solidity"),
        "f90" | "f" => Some("Fortran"),
        "asm" | "s" => Some("Assembly"),
        "metal" => Some("Metal"),
        "cu" => Some("CUDA"),
        "hcl" => Some("HCL"),
        _ => None,
    };
    (name.map(str::to_owned), name.is_none())
}
