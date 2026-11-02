//! Deterministic storage qualification/measurement only; never invokes inference.
pub mod mapping;
pub mod workload;
pub fn probe_path() -> std::path::PathBuf {
    std::env::args_os()
        .nth(1)
        .map(std::path::PathBuf::from)
        .expect("Pass a disposable database path")
}

pub fn fresh_probe_path() -> anyhow::Result<std::path::PathBuf> {
    let path = probe_path();
    anyhow::ensure!(
        !path.exists(),
        "Refusing to reuse an existing qualification database: {}",
        path.display()
    );
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(path)
}
