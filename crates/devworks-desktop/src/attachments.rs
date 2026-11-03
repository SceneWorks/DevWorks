//! Composer attachments (UI-08/09): files from the dialog, drag-and-drop or paste become durable
//! workspace artifacts tagged with their conversation, through a user-ingress upload ticket and
//! `artifacts.save` — the artifact service hashes, verifies and publishes the bytes. Previews are
//! decoded off the UI thread; content is never claimed to be analyzed.
use crate::backend::{Backend, TaskOutput};
use crate::ops;
use eframe::egui::ColorImage;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};

/// Largest file a preview is decoded from (the artifact service enforces the storage limit).
pub const PREVIEW_MAX_BYTES: u64 = 32 << 20;
/// Longest edge of a decoded preview.
pub const PREVIEW_EDGE: u32 = 160;

/// The media type recorded for `path`, by extension (the artifact service stores the bytes
/// as given; nothing here inspects content).
pub fn media_type(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "txt" | "log" => "text/plain",
        "md" | "markdown" => "text/markdown",
        "json" => "application/json",
        "csv" => "text/csv",
        "pdf" => "application/pdf",
        "rs" | "py" | "ts" | "js" | "swift" | "cs" | "go" | "java" | "toml" | "yaml" | "yml" => {
            "text/plain"
        }
        _ => "application/octet-stream",
    }
}

/// Whether a preview can be decoded for this media type in this build.
pub fn previewable(media_type: &str) -> bool {
    matches!(media_type, "image/png" | "image/jpeg")
}

/// Opens `path` for reading only if it is a regular file, without blocking on or following
/// into anything else: the name is checked without following links, the open itself neither
/// follows a final link nor waits for a FIFO writer (Unix `O_NOFOLLOW | O_NONBLOCK`; Windows
/// `FILE_FLAG_OPEN_REPARSE_POINT`), and the opened handle is checked again, so a FIFO, device,
/// socket, directory or link swapped in between is refused before a byte is read.
pub fn open_regular(path: &Path) -> std::io::Result<(std::fs::File, u64)> {
    let not_regular =
        || std::io::Error::new(std::io::ErrorKind::InvalidInput, "not a regular file");
    if !std::fs::symlink_metadata(path)?.file_type().is_file() {
        return Err(not_regular());
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_FLAG_OPEN_REPARSE_POINT: open a link itself, never its target.
        options.custom_flags(0x0020_0000);
    }
    let file = options.open(path)?;
    let meta = file.metadata()?;
    if !meta.file_type().is_file() {
        return Err(not_regular());
    }
    Ok((file, meta.len()))
}

/// SHA-256 and length of a regular file of at most `max` bytes.
fn sha256_file(path: &Path, max: u64) -> Result<(String, u64), String> {
    let (file, len) = open_regular(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::InvalidInput => "only regular files can be attached".to_owned(),
        _ => format!("could not read the file: {e}"),
    })?;
    let too_large = || format!("larger than the attachment limit of {max} bytes");
    if len > max {
        return Err(too_large());
    }
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut n = 0u64;
    // One byte past the limit detects a file that grew since it was checked.
    let mut f = file.take(max.saturating_add(1));
    loop {
        let read = f
            .read(&mut buf)
            .map_err(|e| format!("could not read the file: {e}"))?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
        n += read as u64;
    }
    if n > max {
        return Err(too_large());
    }
    let digest = hasher.finalize();
    Ok((digest.iter().map(|b| format!("{b:02x}")).collect(), n))
}

/// The configured largest artifact file (`storage.artifactMaxFileBytes`), read through
/// `settings.read_effective`; the catalog default when it cannot be read. The artifact service
/// enforces the same limit again at save.
pub fn max_attachment_bytes(backend: &dyn Backend) -> u64 {
    use devworks_core::settings::catalog::{ARTIFACT_MAX_FILE_KEY, descriptor};
    let frame = ops::query_frame(
        backend,
        "settings.read_effective",
        serde_json::json!({"scope": {"kind": "Workspace"}, "keys": [ARTIFACT_MAX_FILE_KEY],
            "page": {"limit": 1}}),
    );
    ops::settings_entries(backend.query("settings.read_effective", &frame))
        .ok()
        .and_then(|(_, e)| e.into_iter().next()?.value.value.0.as_u64())
        .or_else(|| descriptor(ARTIFACT_MAX_FILE_KEY)?.default_value().as_u64())
        .unwrap_or(64 << 20)
}

/// Stores `path` as a workspace artifact for `session`: mint the user's upload ticket, hash the
/// file, `artifacts.save` it with the conversation tag. Removes `remove_after` (a materialized
/// paste) once the service holds its own copy.
pub fn store(
    backend: &dyn Backend,
    path: &Path,
    name: &str,
    media_type: &str,
    session: &str,
    remove_after: bool,
) -> Result<TaskOutput, String> {
    let result = (|| {
        let (digest, bytes) = sha256_file(path, max_attachment_bytes(backend))?;
        let ticket = backend.mint_workspace_upload(path)?;
        let tags: Vec<String> = ops::conversation_tag(session).into_iter().collect();
        let frame = ops::save_workspace_artifact(
            backend,
            ticket.as_str(),
            name,
            media_type,
            &digest,
            bytes,
            &tags,
        );
        Ok(TaskOutput::Stored(
            backend.command("artifacts.save", &frame),
        ))
    })();
    if remove_after {
        let _ = std::fs::remove_file(path);
    }
    result
}

/// Decodes a bounded preview of an image file.
pub fn preview(path: &Path) -> Result<TaskOutput, String> {
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if meta.len() > PREVIEW_MAX_BYTES {
        return Err("too large to preview".into());
    }
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    preview_bytes(&bytes)
}

/// Decodes the preview of an attachment already stored as an artifact (a draft restored after
/// a restart): `artifacts.read` issues a principal-bound stream handle, and the artifact
/// service's verified stream supplies the bytes. The picked file is never consulted.
pub fn preview_stored(
    backend: &dyn Backend,
    artifact: &devworks_core::protocol::provider::ArtifactRef,
) -> Result<TaskOutput, String> {
    use devworks_core::protocol::records::ArtifactReadContent;
    if artifact.byte_length.get() as u64 > PREVIEW_MAX_BYTES {
        return Err("too large to preview".into());
    }
    let content =
        ops::read_content(backend.query("artifacts.read", &ops::artifact_read(backend, artifact)))
            .map_err(|e| e.describe())?;
    let bytes = match content {
        ArtifactReadContent::Binary { stream_id, .. } => {
            backend.read_stream(&stream_id, PREVIEW_MAX_BYTES)?
        }
        ArtifactReadContent::Text { content, .. } => content.0.into_bytes(),
    };
    preview_bytes(&bytes)
}

fn preview_bytes(bytes: &[u8]) -> Result<TaskOutput, String> {
    let image = image::load_from_memory(bytes).map_err(|e| format!("no preview: {e}"))?;
    let thumb = if image.width() > PREVIEW_EDGE || image.height() > PREVIEW_EDGE {
        image.thumbnail(PREVIEW_EDGE, PREVIEW_EDGE).to_rgba8()
    } else {
        image.to_rgba8()
    };
    Ok(TaskOutput::Preview {
        size: [thumb.width() as usize, thumb.height() as usize],
        rgba: thumb.into_raw(),
    })
}

/// Writes a pasted image to `dir` as PNG so it can be attached like a picked file.
pub fn materialize_paste(image: &ColorImage, dir: &Path, name: &str) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let path = dir.join(name);
    let [w, h] = image.size;
    let rgba: Vec<u8> = image
        .pixels
        .iter()
        .flat_map(|c| c.to_srgba_unmultiplied())
        .collect();
    let buffer = image::RgbaImage::from_raw(w as u32, h as u32, rgba)
        .ok_or_else(|| "the pasted image has no pixels".to_owned())?;
    buffer
        .save_with_format(&path, image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    Ok(path)
}

/// A pasted text that names an existing local file (a copied file path or `file://` URL).
pub fn pasted_path(text: &str) -> Option<PathBuf> {
    let t = text.trim();
    if t.contains('\n') {
        return None;
    }
    let t = t.strip_prefix("file://").unwrap_or(t);
    let p = PathBuf::from(t);
    (p.is_absolute() && p.is_file()).then_some(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_types_follow_extensions_and_only_decodable_images_preview() {
        assert_eq!(media_type(Path::new("a/B.PNG")), "image/png");
        assert_eq!(media_type(Path::new("notes.md")), "text/markdown");
        assert_eq!(media_type(Path::new("blob")), "application/octet-stream");
        assert!(previewable("image/jpeg"));
        assert!(
            !previewable("image/svg+xml"),
            "no SVG rasterizer in this build"
        );
    }

    #[test]
    fn pasted_text_attaches_only_existing_absolute_files() {
        let dir = std::env::temp_dir().join(format!("devworks-paste-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a.txt");
        std::fs::write(&f, b"x").unwrap();
        assert_eq!(pasted_path(&f.display().to_string()), Some(f.clone()));
        assert_eq!(pasted_path("relative.txt"), None);
        assert_eq!(pasted_path("two\nlines"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_regular_files_within_the_limit_are_hashed() {
        let dir = std::env::temp_dir().join(format!("devworks-regular-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.txt");
        std::fs::write(&file, b"abcdef").unwrap();
        assert_eq!(sha256_file(&file, 6).unwrap().1, 6);
        assert!(
            sha256_file(&file, 5)
                .unwrap_err()
                .contains("attachment limit")
        );
        assert!(sha256_file(&dir, 100).unwrap_err().contains("regular"));
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&file, dir.join("link")).unwrap();
            assert!(
                sha256_file(&dir.join("link"), 100)
                    .unwrap_err()
                    .contains("regular")
            );
            // A FIFO without a writer would block a plain open forever.
            let fifo = dir.join("fifo");
            assert!(
                std::process::Command::new("mkfifo")
                    .arg(&fifo)
                    .status()
                    .unwrap()
                    .success()
            );
            let (tx, rx) = std::sync::mpsc::channel();
            let f = fifo.clone();
            std::thread::spawn(move || {
                let _ = tx.send(sha256_file(&f, 100));
            });
            match rx.recv_timeout(std::time::Duration::from_secs(10)) {
                Ok(r) => assert!(r.unwrap_err().contains("regular")),
                Err(_) => {
                    // Release the blocked reader before failing (failure-only bound).
                    let _ = std::fs::OpenOptions::new().write(true).open(&fifo);
                    panic!("hashing a FIFO blocked");
                }
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_pasted_image_round_trips_through_png() {
        let dir = std::env::temp_dir().join(format!("devworks-paste-img-{}", std::process::id()));
        let img = ColorImage::new([3, 2], vec![eframe::egui::Color32::RED; 6]);
        let p = materialize_paste(&img, &dir, "p.png").unwrap();
        match preview(&p).unwrap() {
            TaskOutput::Preview { size, rgba } => {
                assert_eq!(size, [3, 2]);
                assert_eq!(&rgba[..4], &[255, 0, 0, 255]);
            }
            TaskOutput::Stored(_) => panic!("expected a preview"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
