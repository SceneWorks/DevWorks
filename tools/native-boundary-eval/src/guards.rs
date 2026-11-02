//! Narrow native verified-read feasibility. No arbitrary process/sandbox guarantee.
use serde_json::{Value, json};
use std::{
    ffi::CString,
    fs::{File, OpenOptions},
    io::Read,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{OpenOptionsExt, symlink},
    },
    path::{Component, Path},
};
fn open_beneath(root: &File, path: &Path) -> Result<File, String> {
    let components: Vec<_> = path.components().collect();
    if components.is_empty()
        || components
            .iter()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err("path must contain only relative normal components".into());
    }
    let mut parent = root.try_clone().map_err(|_| "root handle")?;
    for (i, c) in components.iter().enumerate() {
        let name = CString::new(c.as_os_str().as_encoded_bytes()).map_err(|_| "NUL path")?;
        let directory = i + 1 < components.len();
        // SAFETY: owned live directory fd, NUL-terminated component, no caller-controlled flags.
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY
                    | libc::O_CLOEXEC
                    | libc::O_NOFOLLOW
                    | libc::O_NONBLOCK
                    | if directory { libc::O_DIRECTORY } else { 0 },
            )
        };
        if fd < 0 {
            return Err("unavailable or symlink denied".into());
        }
        // SAFETY: openat returned a fresh fd; this File is its only owner.
        let file = unsafe { File::from_raw_fd(fd) };
        if !directory && !file.metadata().map_err(|_| "metadata")?.is_file() {
            return Err("regular file required".into());
        }
        parent = file;
    }
    Ok(parent)
}
fn materialize(
    mut file: File,
    current_revision: u64,
    expected_revision: u64,
) -> Result<Vec<u8>, String> {
    if current_revision != expected_revision {
        return Err("scope revoked".into());
    }
    let mut bytes = Vec::new();
    file.by_ref()
        .take(8193)
        .read_to_end(&mut bytes)
        .map_err(|_| "read")?;
    if bytes.len() > 8192 {
        return Err("read bound".into());
    }
    Ok(bytes)
}
pub fn run(owned: &str) -> Result<Value, Box<dyn std::error::Error>> {
    let base = Path::new(owned);
    if std::fs::read_dir(base)?.next().is_some() {
        return Err("owned guard fixture must start empty".into());
    }
    let root = base.join("root");
    let outside = base.join("outside");
    std::fs::create_dir(&root)?;
    std::fs::create_dir(&outside)?;
    std::fs::create_dir(root.join("src"))?;
    std::fs::write(root.join("src/allowed.txt"), b"allowed")?;
    std::fs::write(outside.join("secret.txt"), b"private fixture")?;
    let handle = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(&root)?;
    let mut checks = Vec::new();
    if materialize(open_beneath(&handle, Path::new("src/allowed.txt"))?, 1, 1)? != b"allowed" {
        return Err("allowed read".into());
    }
    checks.push("read_relative_regular_file_through_registered_root_handle");
    if open_beneath(&handle, Path::new("../outside/secret.txt")).is_ok()
        || open_beneath(&handle, &outside.join("secret.txt")).is_ok()
    {
        return Err("path escape".into());
    }
    checks.push("absolute_and_parent_paths_rejected");
    symlink(outside.join("secret.txt"), root.join("final-link"))?;
    symlink(&outside, root.join("parent-link"))?;
    if open_beneath(&handle, Path::new("final-link")).is_ok()
        || open_beneath(&handle, Path::new("parent-link/secret.txt")).is_ok()
    {
        return Err("symlink escape".into());
    }
    checks.push("final_and_parent_symlink_escape_rejected_by_native_openat");
    let original = open_beneath(&handle, Path::new("src/allowed.txt"))?;
    std::fs::rename(root.join("src/allowed.txt"), root.join("src/original.txt"))?;
    symlink(outside.join("secret.txt"), root.join("src/allowed.txt"))?;
    if materialize(original, 1, 1)? != b"allowed"
        || open_beneath(&handle, Path::new("src/allowed.txt")).is_ok()
    {
        return Err("stable descriptor target".into());
    }
    checks.push("opened_descriptor_keeps_original_bytes_after_path_substitution");
    if materialize(open_beneath(&handle, Path::new("src/original.txt"))?, 2, 1).is_ok() {
        return Err("revoked read published".into());
    }
    checks.push("changed_scope_revision_denies_materialization");
    std::fs::write(root.join("large"), vec![0u8; 8193])?;
    if materialize(open_beneath(&handle, Path::new("large"))?, 1, 1).is_ok() {
        return Err("unbounded read".into());
    }
    checks.push("read_byte_bound_enforced");
    Ok(
        json!({"host":"macOS arm64","passed":checks.len(),"checks":checks,"noInference":true,"ownedFixturesOnly":true,"limits":["Known native read adapter subset; policy/revision arguments are fixture values, not a production authority service.","No arbitrary executable confinement, destructive deletion, hard-link/mount policy, Windows/Linux semantics or native secret store is qualified.","Real publication needs a defined authority linearization point and current scope validation, not this fixture function alone."]}),
    )
}
