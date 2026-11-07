use std::{env, fs, path::PathBuf};

fn main() {
    let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("Cargo manifest directory"))
        .join("../../docs/help");
    let catalog = root.join("catalog.json");
    println!("cargo:rerun-if-changed={}", catalog.display());
    println!("cargo:rerun-if-changed={}", root.join("topics").display());
    let mut files: Vec<_> = fs::read_dir(root.join("topics"))
        .expect("bundled Help topics")
        .map(|entry| entry.expect("Help directory entry").path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "md"))
        .collect();
    files.sort();
    let mut source = format!("pub const CATALOG: &str = include_str!({:?});\n", catalog);
    source.push_str("pub const FILES: &[(&str, &str)] = &[\n");
    for path in files {
        println!("cargo:rerun-if-changed={}", path.display());
        let name = format!(
            "topics/{}",
            path.file_name().expect("topic name").to_string_lossy()
        );
        source.push_str(&format!("({name:?}, include_str!({path:?})),\n"));
    }
    source.push_str("];\n");
    fs::write(
        PathBuf::from(env::var_os("OUT_DIR").expect("Cargo output directory"))
            .join("help_bundle.rs"),
        source,
    )
    .expect("write bundled Help includes");
}
