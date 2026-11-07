//! Composer attachments (UI-08/09): files from the dialog, drag-and-drop or paste become durable
//! workspace artifacts tagged with their conversation, through a user-ingress upload ticket and
//! `artifacts.save` — the artifact service hashes, verifies and publishes the bytes. Previews are
//! decoded off the UI thread; content is never claimed to be analyzed.
use crate::backend::{Backend, TaskOutput, resolve_command};
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

pub use crate::files::open_regular;

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

/// Prepares `path`'s workspace artifact for `session`: hash the file, mint the user's upload
/// ticket and build the exact `artifacts.save` frame (with its request key and the
/// conversation tag). Nothing is admitted yet: the shell keeps the frame in the attachment's
/// state before submitting it, so a close at any point can still confirm that same request.
pub fn prepare(
    backend: &dyn Backend,
    path: &Path,
    name: &str,
    media_type: &str,
    session: &str,
) -> Result<TaskOutput, String> {
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
    Ok(TaskOutput::Prepared { frame })
}

/// Prepares storing `path` as the user's workspace artifact (the same upload ticket, digest
/// check and `artifacts.save` frame as an attachment, tagged `tag` instead of a conversation):
/// a Knowledge import's source. The shell holds the frame before admitting it with [`save`],
/// so an unknown outcome is confirmed by resubmitting that identical frame (never a new one,
/// which could store the file twice), in this launch or the next.
pub fn prepare_file(
    backend: &dyn Backend,
    path: &Path,
    name: &str,
    media_type: &str,
    tag: &str,
) -> Result<TaskOutput, String> {
    let (digest, bytes) = sha256_file(path, max_attachment_bytes(backend))?;
    let ticket = backend.mint_workspace_upload(path)?;
    let frame = ops::save_workspace_artifact(
        backend,
        ticket.as_str(),
        name,
        media_type,
        &digest,
        bytes,
        &[tag.to_owned()],
    );
    Ok(TaskOutput::Prepared { frame })
}

/// Admits an attachment's prepared `artifacts.save` frame, or resubmits it unchanged (same
/// request key) to confirm an unknown outcome. `owned` is a file the shell created (a pasted
/// image): it is removed once the artifact service holds its copy, never before.
pub fn save(
    backend: &dyn Backend,
    frame: Vec<u8>,
    owned: Option<&Path>,
) -> Result<TaskOutput, String> {
    let outcome = resolve_command(backend, "artifacts.save", &frame);
    // A recorded refusal (over the quota, say) stored nothing: the paste is kept.
    if let Some(p) = owned
        && ops::saved_artifact(outcome.clone()).is_ok()
    {
        remove_owned(p, &backend.scratch_dir());
    }
    Ok(TaskOutput::Stored { outcome, frame })
}

/// Removes a file the shell created, and only when it is plainly one: a regular file (not a
/// symbolic link) directly in `scratch`, named without `..`. Ownership itself is recorded on
/// the attachment when the shell creates the file; this is defence in depth. Returns whether
/// it removed the file.
pub fn remove_owned(path: &Path, scratch: &Path) -> bool {
    use std::path::Component;
    if path.components().any(|c| {
        !matches!(
            c,
            Component::Normal(_) | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return false;
    }
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if !meta.file_type().is_file() {
        return false;
    }
    let (Some(parent), Ok(scratch)) = (path.parent(), std::fs::canonicalize(scratch)) else {
        return false;
    };
    if std::fs::canonicalize(parent).ok() != Some(scratch) {
        return false;
    }
    std::fs::remove_file(path).is_ok()
}

/// Decodes a bounded preview of an image file.
pub fn preview(path: &Path) -> Result<TaskOutput, String> {
    let bytes =
        crate::files::read_regular(path, PREVIEW_MAX_BYTES).map_err(|e| match e.kind() {
            std::io::ErrorKind::InvalidInput => "not a regular file".to_owned(),
            std::io::ErrorKind::FileTooLarge => "too large to preview".to_owned(),
            _ => e.to_string(),
        })?;
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

/// Encode first; native custody records the actual session scope before creating a file.
pub fn encode_paste(image: &ColorImage) -> Result<Vec<u8>, String> {
    let [w, h] = image.size;
    let rgba: Vec<u8> = image
        .pixels
        .iter()
        .flat_map(|c| c.to_srgba_unmultiplied())
        .collect();
    let buffer = image::RgbaImage::from_raw(w as u32, h as u32, rgba)
        .ok_or_else(|| "the pasted image has no pixels".to_owned())?;
    let mut encoded = std::io::Cursor::new(Vec::new());
    buffer
        .write_to(&mut encoded, image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    Ok(encoded.into_inner())
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

    #[cfg(unix)]
    #[test]
    fn only_a_plain_file_directly_in_the_scratch_directory_is_removed() {
        let dir = std::env::temp_dir().join(format!("devworks-owned-{}", std::process::id()));
        let scratch = dir.join("staging-ui");
        std::fs::create_dir_all(&scratch).unwrap();
        let outside = dir.join("notes.txt");
        std::fs::write(&outside, b"user").unwrap();
        // `..` lexically inside the scratch directory, really beside it.
        assert!(!remove_owned(&scratch.join("../notes.txt"), &scratch));
        // A symbolic link in the scratch directory to a user's file.
        let link = scratch.join("link.txt");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        assert!(!remove_owned(&link, &scratch));
        assert!(outside.exists() && std::fs::symlink_metadata(&link).is_ok());
        // A nested directory's file is not directly in it.
        std::fs::create_dir_all(scratch.join("nested")).unwrap();
        std::fs::write(scratch.join("nested/x.png"), b"x").unwrap();
        assert!(!remove_owned(&scratch.join("nested/x.png"), &scratch));
        // Any `..` is refused, even one that resolves back into it.
        let back = scratch.join("back.png");
        std::fs::write(&back, b"png").unwrap();
        assert!(!remove_owned(&scratch.join("nested/../back.png"), &scratch));
        assert!(back.exists());
        // The shell's own paste is.
        let paste = scratch.join("pasted.png");
        std::fs::write(&paste, b"png").unwrap();
        assert!(remove_owned(&paste, &scratch));
        assert!(!paste.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

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
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("p.png");
        std::fs::write(&p, encode_paste(&img).unwrap()).unwrap();
        match preview(&p).unwrap() {
            TaskOutput::Preview { size, rgba } => {
                assert_eq!(size, [3, 2]);
                assert_eq!(&rgba[..4], &[255, 0, 0, 255]);
            }
            TaskOutput::Stored { .. }
            | TaskOutput::Prepared { .. }
            | TaskOutput::SettingSecret { .. }
            | TaskOutput::PackagePrepared { .. }
            | TaskOutput::Refreshed
            | TaskOutput::TextPreview(_)
            | TaskOutput::Convention(_) => {
                panic!("expected a preview")
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
