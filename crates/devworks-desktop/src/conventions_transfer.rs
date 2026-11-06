//! File staging and lossless transfers for the native convention editor. Gateway publication
//! remains in the client's exact-frame command lifecycle, including after an unknown outcome.
use crate::{
    backend::Backend,
    conventions_ops as cop,
    conventions_screen::{Draft, Scope},
    ops,
};
use devworks_core::protocol::{provider::ArtifactRef, records::*};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

#[derive(Clone, Serialize, Deserialize)]
pub struct StagedFile {
    pub path: PathBuf,
    pub name: String,
    pub media_type: String,
    pub pointer: String,
    pub owned: bool,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Pipeline {
    pub scope: Scope,
    pub operation: String,
    pub body: Value,
    pub files: Vec<StagedFile>,
    pub at: usize,
    pub submission: cop::Submission,
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn hash(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}
fn scratch(b: &dyn Backend, suffix: &str) -> Result<(PathBuf, File), String> {
    let path = b
        .scratch_dir()
        .join(format!("convention-{}.{suffix}", b.token("source")));
    std::fs::create_dir_all(b.scratch_dir()).map_err(|e| e.to_string())?;
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| e.to_string())?;
    Ok((path, file))
}
fn stage_text(
    b: &dyn Backend,
    text: &str,
    pointer: &str,
    media: &str,
    owned: &mut Vec<PathBuf>,
) -> Result<StagedFile, String> {
    let (path, mut file) = scratch(b, "txt")?;
    owned.push(path.clone());
    file.write_all(text.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|e| e.to_string())?;
    Ok(StagedFile {
        path,
        name: "Convention source".into(),
        media_type: media.into(),
        pointer: pointer.into(),
        owned: true,
    })
}
pub fn prepare_edit(
    b: &dyn Backend,
    scope: Scope,
    d: Draft,
    submission: cop::Submission,
) -> Result<Pipeline, String> {
    let mut owned = Vec::new();
    let result = prepare_edit_inner(b, scope, d, submission, &mut owned);
    if result.is_err() {
        for path in owned {
            crate::attachments::remove_owned(&path, &b.scratch_dir());
        }
    }
    result
}
fn prepare_edit_inner(
    b: &dyn Backend,
    scope: Scope,
    d: Draft,
    submission: cop::Submission,
    owned: &mut Vec<PathBuf>,
) -> Result<Pipeline, String> {
    if !d.body_complete || d.loading {
        return Err("The full opening source must finish loading before saving.".into());
    }
    let order = if d.order.is_empty() && d.target().is_none() {
        None
    } else {
        Some(
            d.order
                .parse::<i32>()
                .map_err(|_| "Order must be a signed 32-bit integer.")?,
        )
    };
    let mut files = Vec::new();
    if let Some(record) = d.target() {
        let original = d
            .base
            .as_ref()
            .ok_or("The opening source is unavailable.")?;
        let content = original
            .content
            .as_ref()
            .ok_or("The opening Markdown artifact is unavailable.")?;
        let mut body = json!({"scope":scope.wire(),"conventionId":record.id,"expectedRevision":record.revision,"content":content,"title":d.title,"slug":d.slug,"sortOrder":order.ok_or("An existing convention needs its captured order.")?.to_string(),"parent":d.parent.as_ref().map_or_else(||json!({"kind":"Root"}),|id|json!({"kind":"Child","parentId":id}))});
        if d.markdown_changed {
            files.push(stage_text(
                b,
                &d.markdown,
                "/content",
                "text/markdown",
                owned,
            )?);
        }
        match d.raw_mode {
            1 => {
                if d.raw_changed || original.raw_content.is_none() {
                    body["rawContent"] = json!({"kind":"Set","content":content});
                    files.push(stage_text(
                        b,
                        &d.raw,
                        "/rawContent/content",
                        "text/plain",
                        owned,
                    )?);
                } else if let Some(raw) = &original.raw_content {
                    body["rawContent"] = json!({"kind":"Set","content":raw});
                }
            }
            2 => {
                body["rawContent"] = json!({"kind":"Remove"});
            }
            _ => {}
        }
        Ok(Pipeline {
            scope,
            operation: "conventions.edit".into(),
            body,
            files,
            at: 0,
            submission,
        })
    } else {
        use devworks_core::services::conventions::model::{
            EditorConvention, EditorResource, EditorText, manifest_schema, write_editor_manifest,
        };
        let md_hash = hash(d.markdown.as_bytes());
        let raw_hash = hash(d.raw.as_bytes());
        let mut markdown = d.markdown.as_bytes();
        let mut raw = d.raw.as_bytes();
        let (path, file) = scratch(b, "json")?;
        let mut file = std::io::BufWriter::with_capacity(65536, file);
        owned.push(path.clone());
        let result = write_editor_manifest(
            &mut file,
            EditorConvention {
                slug: &d.slug,
                title: &d.title,
                parent_path: &d.parent_path,
                sort_order: order,
                lifecycle: if d.retired { "Retired" } else { "Active" },
                applicability: &d.applicability,
                markdown: EditorText {
                    sha256: &md_hash,
                    byte_length: markdown.len() as u64,
                    reader: &mut markdown,
                },
                raw_content: if d.raw_mode == 1 {
                    Some(EditorText {
                        sha256: &raw_hash,
                        byte_length: raw.len() as u64,
                        reader: &mut raw,
                    })
                } else {
                    None
                },
            },
            std::iter::empty::<EditorResource<'_>>(),
        );
        if let Err(error) = result {
            crate::attachments::remove_owned(&path, &b.scratch_dir());
            return Err(error.to_string());
        }
        file.flush()
            .and_then(|()| file.get_ref().sync_all())
            .map_err(|e| e.to_string())?;
        Ok(Pipeline {
            scope: scope.clone(),
            operation: "conventions.import".into(),
            body: json!({"scope":scope.wire(),"schema":manifest_schema()}),
            files: vec![StagedFile {
                path,
                name: "Convention portable manifest".into(),
                media_type: "application/json".into(),
                pointer: "/source".into(),
                owned: true,
            }],
            at: 0,
            submission,
        })
    }
}
pub fn prepare_artifact(b: &dyn Backend, pipeline: &Pipeline) -> Result<Vec<u8>, String> {
    let stage = pipeline
        .files
        .get(pipeline.at)
        .ok_or("There is no source file to stage.")?;
    let (file, len) = crate::files::open_regular(&stage.path).map_err(|e| e.to_string())?;
    let max = crate::attachments::max_attachment_bytes(b);
    if len > max {
        return Err(format!(
            "The source is {len} bytes; the current artifact limit is {max} bytes. The draft is kept."
        ));
    }
    let mut hash = Sha256::new();
    let mut read = 0u64;
    let mut file = file.take(max.saturating_add(1));
    let mut buf = [0u8; 65536];
    loop {
        let n = file.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        read += n as u64;
        hash.update(&buf[..n]);
    }
    if read != len {
        return Err("The source file changed while preparing its upload.".into());
    }
    let digest = hex(&hash.finalize());
    match &pipeline.scope {
        Scope::Workspace => {
            let ticket = b.mint_workspace_upload(&stage.path)?;
            Ok(ops::save_workspace_artifact(
                b,
                ticket.as_str(),
                &stage.name,
                &stage.media_type,
                &digest,
                len,
                &[],
            ))
        }
        Scope::Project(id) => {
            let project = serde_json::from_value(json!(id)).map_err(|e| e.to_string())?;
            let ticket = b.mint_project_upload(&project, &stage.path)?;
            Ok(ops::save_project_artifact(
                b,
                id,
                ticket.as_str(),
                &stage.name,
                &stage.media_type,
                &digest,
                len,
            ))
        }
    }
}
pub fn apply_artifact(p: &mut Pipeline, artifact: ArtifactRef) -> Result<(), String> {
    let stage = p
        .files
        .get(p.at)
        .ok_or("The saved source has no matching stage.")?;
    // All insertion paths are editor-owned constants. No result or imported key chooses a path.
    if stage.pointer == "/source" {
        p.body["source"] = json!(artifact);
    } else {
        let field = p
            .body
            .pointer_mut(&stage.pointer)
            .ok_or("The prepared source target is missing.")?;
        *field = json!(artifact);
    }
    p.at += 1;
    Ok(())
}
/// Write into a new sibling and publish only after all owner pages and integrity checks pass.
pub fn export(
    b: &dyn Backend,
    scope: &Scope,
    record: &ConventionRecord,
    format: usize,
    target: &Path,
) -> Result<(), String> {
    let temp = target.with_file_name(format!(".convention-{}.partial", b.token("export")));
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .map_err(|e| e.to_string())?;
    let result = (|| {
        let mut cursor = None;
        let mut seen = std::collections::HashSet::new();
        let mut digest = Sha256::new();
        let mut length = 0u64;
        loop {
            let mut body = json!({"scope":scope.wire(),"conventionId":record.id,"revision":record.revision,"format":(["Markdown","Raw","Manifest"][format]),"maxChars":"65536"});
            if let Some(cursor) = &cursor {
                body["cursor"] = json!(cursor);
            }
            let view = cop::document(
                b.query("conventions.read", &cop::query(b, "conventions.read", body)),
            )
            .map_err(|e| e.describe())?;
            if view.convention.id != record.id
                || view.convention.revision.get() != record.revision.get()
            {
                return Err("The exported revision changed.".into());
            }
            if !matches!(view.availability, ConventionAvailability::Available) {
                return Err(view
                    .reason
                    .map_or_else(|| "The source is unavailable.".into(), |r| r.0));
            }
            let p = view
                .passage
                .ok_or("The source owner returned no passage.")?;
            output
                .write_all(p.text.0.as_bytes())
                .map_err(|e| e.to_string())?;
            digest.update(p.text.0.as_bytes());
            length += p.text.0.len() as u64;
            if !p.has_more {
                break;
            }
            let next = p
                .next_cursor
                .ok_or("The source continuation is missing.")?
                .0;
            if !seen.insert(next.clone()) {
                return Err("The source continuation did not advance.".into());
            }
            cursor = Some(next);
        }
        if format != 2 {
            let ar = if format == 0 {
                record.content.as_ref()
            } else {
                record.raw_content.as_ref()
            }
            .ok_or("The original source artifact is unavailable.")?;
            if length != ar.byte_length.get() as u64
                || hex(&digest.finalize()) != ar.sha256.as_str()
            {
                return Err("The export does not match the immutable source.".into());
            }
        }
        output.sync_all().map_err(|e| e.to_string())?;
        std::fs::rename(&temp, target).map_err(|e| e.to_string())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}
pub fn download(
    b: &dyn Backend,
    scope: &Scope,
    resource: &ConventionResourceRecord,
    target: &Path,
) -> Result<(), String> {
    use base64::Engine;
    let temp = target.with_file_name(format!(".convention-{}.partial", b.token("download")));
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .map_err(|e| e.to_string())?;
    let result = (|| {
        let mut cursor = None;
        let mut seen = std::collections::HashSet::new();
        let mut digest = Sha256::new();
        let mut length = 0u64;
        loop {
            let mut body = json!({"scope":scope.wire(),"conventionId":resource.convention_id,"revision":resource.convention_revision,"resourceId":resource.id,"format":"Resource","maxBytes":"49152"});
            if let Some(cursor) = &cursor {
                body["cursor"] = json!(cursor);
            }
            let out = ops::query_result(
                b.query("conventions.read", &cop::query(b, "conventions.read", body)),
            )
            .map_err(|e| e.describe())?
            .0;
            let crate::conventions_ops::ReadResource {
                resource: actual,
                passage,
            } = cop::binary(out).map_err(|e| e.describe())?;
            if actual.id != resource.id
                || actual.revision.get() != resource.revision.get()
                || actual.content.sha256.as_str() != resource.content.sha256.as_str()
                || passage.first_byte.get() as u64 != length
            {
                return Err("The attachment identity changed while downloading.".into());
            }
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(passage.base64.0)
                .map_err(|_| "Invalid attachment transfer encoding.")?;
            if bytes.len() as i64 != passage.byte_length.get() {
                return Err("The attachment passage length was inconsistent.".into());
            }
            length += bytes.len() as u64;
            if length > resource.content.byte_length.get() as u64 {
                return Err("The attachment exceeded its immutable size.".into());
            }
            output.write_all(&bytes).map_err(|e| e.to_string())?;
            digest.update(&bytes);
            if !passage.has_more {
                break;
            }
            let next = passage
                .next_cursor
                .ok_or("The attachment continuation is missing.")?
                .0;
            if !seen.insert(next.clone()) {
                return Err("The attachment continuation did not advance.".into());
            }
            cursor = Some(next);
        }
        if length != resource.content.byte_length.get() as u64
            || hex(&digest.finalize()) != resource.content.sha256.as_str()
        {
            return Err("The attachment did not match its immutable digest and length.".into());
        }
        output.sync_all().map_err(|e| e.to_string())?;
        std::fs::rename(&temp, target).map_err(|e| e.to_string())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}
