//! The convention manifest format, written in one place. No output contains host authority,
//! foreign author IDs or nonportable artifact references. The actual staging owner enforces its
//! artifact quota.
//!
//! [`write_editor_manifest`] streams a whole manifest to a writer; the resumable transport in
//! `manifest.rs` emits the same bytes a page at a time. Both build every literal piece (the
//! header, a source's opening and closing, a resource's opening, the escaping of one text
//! scalar) with the functions of the [`format`] section below, so the two cannot drift. The
//! hand-written reader of this format is `importing/parser.rs`; it is incremental and
//! checkpointed, which a general JSON decoder is not, and `portable_manifest_round_trips` pins the
//! writer and the reader together.
use super::MAX_MARKDOWN_BYTES;
use sha2::{Digest, Sha256};
use std::io::{self, BufReader, Read, Write};

pub struct EditorText<'a> {
    pub sha256: &'a str,
    pub byte_length: u64,
    pub reader: &'a mut dyn Read,
}
pub struct EditorResource<'a> {
    pub key: &'a str,
    pub name: &'a str,
    pub media_type: &'a str,
    pub lifecycle: &'a str,
    pub source: EditorText<'a>,
}
pub struct EditorConvention<'a> {
    pub slug: &'a str,
    pub title: &'a str,
    pub parent_path: &'a [String],
    pub sort_order: Option<i32>,
    pub lifecycle: &'a str,
    pub applicability: &'a [String],
    pub markdown: EditorText<'a>,
    pub raw_content: Option<EditorText<'a>>,
}
// ---------------------------------------------------------------------------------------------
// format: the literal pieces of a manifest

fn quoted(value: &impl serde::Serialize) -> io::Result<String> {
    serde_json::to_string(value).map_err(io::Error::other)
}
/// The manifest up to and including `"markdown":`. `sort_order` is omitted when `None`.
pub(in crate::services::conventions) fn header(
    slug: &str,
    title: &str,
    parent_path: &[String],
    sort_order: Option<i64>,
    lifecycle: &str,
    applicability: &[String],
) -> io::Result<String> {
    let order = sort_order.map_or_else(String::new, |order| format!(",\"sortOrder\":{order}"));
    Ok(format!(
        "{{\"format\":\"devworks-convention\",\"version\":1,\"slug\":{},\"title\":{},\"parentPath\":{}{order},\"lifecycle\":{},\"applicability\":{},\"markdown\":",
        quoted(&slug)?,
        quoted(&title)?,
        quoted(&parent_path)?,
        quoted(&lifecycle)?,
        quoted(&applicability)?,
    ))
}
/// The opening of a text source, through the opening quote of its `text`.
pub(in crate::services::conventions) fn source_prefix(
    sha256: &str,
    byte_length: u64,
) -> io::Result<String> {
    Ok(format!(
        "{{\"sha256\":{},\"byteLength\":\"{byte_length}\",\"text\":\"",
        quoted(&sha256)?
    ))
}
/// The key that opens the optional raw content, before its source.
pub(in crate::services::conventions) const RAW_CONTENT_KEY: &str = ",\"rawContent\":";
/// The closing of a text source or a resource, after its payload.
pub(in crate::services::conventions) const PAYLOAD_END: &str = "\"}";
/// The opening of the resource list.
pub(in crate::services::conventions) const RESOURCES_OPEN: &str = ",\"resources\":[";
/// The closing of the resource list and the manifest.
pub(in crate::services::conventions) const MANIFEST_END: &str = "]}";
/// The opening of one resource, through the opening quote of its `base64`.
pub(in crate::services::conventions) fn resource_prefix(
    first: bool,
    key: &str,
    name: &str,
    media_type: &str,
    lifecycle: &str,
    sha256: &str,
    byte_length: u64,
) -> io::Result<String> {
    Ok(format!(
        "{}{{\"key\":{},\"name\":{},\"mediaType\":{},\"lifecycle\":{},\"sha256\":{},\"byteLength\":\"{byte_length}\",\"base64\":\"",
        if first { "" } else { "," },
        quoted(&key)?,
        quoted(&name)?,
        quoted(&media_type)?,
        quoted(&lifecycle)?,
        quoted(&sha256)?,
    ))
}
/// One text scalar as it appears inside a JSON string of a manifest.
pub(in crate::services::conventions) fn escape_scalar(ch: char) -> String {
    match ch {
        '"' => "\\\"".into(),
        '\\' => "\\\\".into(),
        c if (c as u32) < 32 => format!("\\u{:04x}", c as u32),
        c => c.to_string(),
    }
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn text(writer: &mut dyn Write, source: &mut EditorText<'_>, required: bool) -> io::Result<()> {
    if source.byte_length > MAX_MARKDOWN_BYTES {
        return Err(invalid("convention text exceeds its retained UTF-8 bound"));
    }
    writer.write_all(source_prefix(source.sha256, source.byte_length)?.as_bytes())?;
    let mut input = BufReader::new(&mut source.reader);
    let mut hash = Sha256::new();
    let mut bytes = 0_u64;
    let mut nonblank = false;
    while let Some((ch, raw, count)) = scalar(&mut input)? {
        nonblank |= !ch.is_whitespace();
        bytes = bytes
            .checked_add(count as u64)
            .ok_or_else(|| invalid("source byte count overflow"))?;
        if bytes > source.byte_length {
            return Err(invalid("source exceeds its declared byte count"));
        }
        hash.update(&raw[..count]);
        match ch {
            '"' | '\\' => writer.write_all(escape_scalar(ch).as_bytes())?,
            c if (c as u32) < 32 => writer.write_all(escape_scalar(c).as_bytes())?,
            _ => writer.write_all(&raw[..count])?,
        }
    }
    verify(source, bytes, hash)?;
    if required && !nonblank {
        return Err(invalid("Markdown content is required"));
    }
    writer.write_all(PAYLOAD_END.as_bytes())
}
fn verify(source: &EditorText<'_>, bytes: u64, hash: Sha256) -> io::Result<()> {
    if bytes != source.byte_length
        || hash
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
            != source.sha256
    {
        return Err(invalid("portable source digest or length changed"));
    }
    Ok(())
}
/// UTF-8 is validated a scalar at a time, including NUL. A legal long word/heading never
/// becomes a temporary full string or a narrowed protocol Content value.
pub(super) fn scalar(reader: &mut dyn Read) -> io::Result<Option<(char, [u8; 4], usize)>> {
    let mut raw = [0_u8; 4];
    if reader.read(&mut raw[..1])? == 0 {
        return Ok(None);
    }
    let count = match raw[0] {
        0..=0x7f => 1,
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return Err(invalid("source is not UTF-8")),
    };
    reader.read_exact(&mut raw[1..count])?;
    let text = std::str::from_utf8(&raw[..count]).map_err(|_| invalid("source is not UTF-8"))?;
    Ok(Some((text.chars().next().expect("one scalar"), raw, count)))
}
/// Writes one complete source-legal manifest. Partial output after an error must not be
/// published by its caller. Resources are consumed one at a time without a retained bundle.
pub fn write_editor_manifest<'a>(
    writer: &mut dyn Write,
    mut document: EditorConvention<'_>,
    resources: impl IntoIterator<Item = EditorResource<'a>>,
) -> io::Result<()> {
    if document.title.trim().is_empty()
        || document.title.chars().count() > 500
        || document.slug.chars().count() > 200
        || document.parent_path.len() > super::MAX_DEPTH
        || document.parent_path.iter().any(|s| s.chars().count() > 200)
        || !matches!(document.lifecycle, "Active" | "Retired")
        || document.applicability.len() > 128
        || document
            .applicability
            .iter()
            .any(|s| s.is_empty() || s.chars().count() > 256 || s.contains('\0'))
    {
        return Err(invalid("invalid portable convention metadata"));
    }
    let sort_order = document.sort_order.map(i64::from);
    writer.write_all(
        header(
            document.slug,
            document.title,
            document.parent_path,
            sort_order,
            document.lifecycle,
            document.applicability,
        )?
        .as_bytes(),
    )?;
    text(writer, &mut document.markdown, true)?;
    if let Some(mut raw) = document.raw_content {
        writer.write_all(RAW_CONTENT_KEY.as_bytes())?;
        text(writer, &mut raw, false)?;
    }
    writer.write_all(RESOURCES_OPEN.as_bytes())?;
    let mut first = true;
    for resource in resources {
        if resource.key.is_empty()
            || resource.key.chars().count() > 200
            || resource.key.contains('\0')
            || resource.name.is_empty()
            || resource.name.chars().count() > 255
            || resource.name.contains('\0')
            || resource.media_type.chars().count() > 200
            || resource.media_type.contains('\0')
            || !matches!(resource.lifecycle, "Active" | "Retired")
        {
            return Err(invalid("invalid portable resource metadata"));
        }
        writer.write_all(
            resource_prefix(
                first,
                resource.key,
                resource.name,
                resource.media_type,
                resource.lifecycle,
                resource.source.sha256,
                resource.source.byte_length,
            )?
            .as_bytes(),
        )?;
        first = false;
        let mut hash = Sha256::new();
        let mut count = 0_u64;
        {
            let mut encoder = base64::write::EncoderWriter::new(
                &mut *writer,
                &base64::engine::general_purpose::STANDARD,
            );
            let mut buffer = [0_u8; 16384];
            loop {
                let n = resource.source.reader.read(&mut buffer)?;
                if n == 0 {
                    break;
                }
                count = count
                    .checked_add(n as u64)
                    .ok_or_else(|| invalid("resource length overflow"))?;
                if count > resource.source.byte_length {
                    return Err(invalid("resource exceeds declared length"));
                }
                hash.update(&buffer[..n]);
                encoder.write_all(&buffer[..n])?;
            }
            encoder.finish()?;
        }
        verify(&resource.source, count, hash)?;
        writer.write_all(PAYLOAD_END.as_bytes())?;
    }
    writer.write_all(MANIFEST_END.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fingerprint::sha256_hex;
    use base64::Engine;
    use std::io::Cursor;
    #[test]
    fn the_on_disk_format_is_byte_stable() {
        let body = "a\"b\\c\n";
        let digest = sha256_hex(body.as_bytes());
        let data = [1_u8, 2];
        let data_digest = sha256_hex(&data);
        let (mut m, mut d) = (Cursor::new(body.as_bytes()), Cursor::new(data));
        let mut out = Vec::new();
        write_editor_manifest(
            &mut out,
            EditorConvention {
                slug: "s",
                title: "T",
                parent_path: &["p".to_owned()],
                sort_order: Some(3),
                lifecycle: "Active",
                applicability: &["rust".to_owned()],
                markdown: EditorText {
                    sha256: &digest,
                    byte_length: body.len() as u64,
                    reader: &mut m,
                },
                raw_content: None,
            },
            [EditorResource {
                key: "k",
                name: "n.bin",
                media_type: "x/y",
                lifecycle: "Retired",
                source: EditorText {
                    sha256: &data_digest,
                    byte_length: 2,
                    reader: &mut d,
                },
            }],
        )
        .unwrap();
        let expected = format!(
            "{{\"format\":\"devworks-convention\",\"version\":1,\"slug\":\"s\",\"title\":\"T\",\"parentPath\":[\"p\"],\"sortOrder\":3,\"lifecycle\":\"Active\",\"applicability\":[\"rust\"],\"markdown\":{{\"sha256\":\"{digest}\",\"byteLength\":\"{}\",\"text\":\"a\\\"b\\\\c\\u000a\"}},\"resources\":[{{\"key\":\"k\",\"name\":\"n.bin\",\"mediaType\":\"x/y\",\"lifecycle\":\"Retired\",\"sha256\":\"{data_digest}\",\"byteLength\":\"2\",\"base64\":\"AQI=\"}}]}}",
            body.len()
        );
        assert_eq!(String::from_utf8(out).unwrap(), expected);
    }
    #[test]
    fn portable_writer_keeps_verbatim_text_and_exact_binary_without_foreign_identity() {
        let markdown = "Title\0 λ\r\n\"literal\"";
        let raw = "\0raw";
        let bytes = [0, 255, 128, 13, 10];
        let md = sha256_hex(markdown.as_bytes());
        let rd = sha256_hex(raw.as_bytes());
        let bd = sha256_hex(&bytes);
        let mut m = Cursor::new(markdown.as_bytes());
        let mut r = Cursor::new(raw.as_bytes());
        let mut b = Cursor::new(bytes);
        let document = EditorConvention {
            slug: "slug\0suffix",
            title: "Title\0suffix",
            parent_path: &[],
            sort_order: Some(-2),
            lifecycle: "Retired",
            applicability: &[],
            markdown: EditorText {
                sha256: &md,
                byte_length: markdown.len() as u64,
                reader: &mut m,
            },
            raw_content: Some(EditorText {
                sha256: &rd,
                byte_length: raw.len() as u64,
                reader: &mut r,
            }),
        };
        let resource = EditorResource {
            key: "portable-key",
            name: "binary.dat",
            media_type: "",
            lifecycle: "Active",
            source: EditorText {
                sha256: &bd,
                byte_length: bytes.len() as u64,
                reader: &mut b,
            },
        };
        let mut out = Vec::new();
        write_editor_manifest(&mut out, document, [resource]).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(value["markdown"]["text"], markdown);
        assert_eq!(value["rawContent"]["text"], raw);
        assert_eq!(value["slug"], "slug\0suffix");
        assert_eq!(value["resources"][0]["mediaType"], "");
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(value["resources"][0]["base64"].as_str().unwrap())
                .unwrap(),
            bytes
        );
        assert!(value.get("authorId").is_none());
        assert!(value["resources"][0].get("artifactId").is_none());
    }
    struct Counter(u64);
    impl Write for Counter {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.0 += b.len() as u64;
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    #[test]
    fn a_full_legal_nul_document_streams_escaped_bytes_without_a_manifest_buffer() {
        let count = MAX_MARKDOWN_BYTES;
        let mut source = io::repeat(0).take(count);
        let mut hash = Sha256::new();
        let block = [0_u8; 16_384];
        let mut left = count;
        while left > 0 {
            let n = left.min(block.len() as u64) as usize;
            hash.update(&block[..n]);
            left -= n as u64;
        }
        let digest = hash
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let mut out = Counter(0);
        write_editor_manifest(
            &mut out,
            EditorConvention {
                slug: "large",
                title: "Large",
                parent_path: &[],
                sort_order: Some(0),
                lifecycle: "Active",
                applicability: &[],
                markdown: EditorText {
                    sha256: &digest,
                    byte_length: count,
                    reader: &mut source,
                },
                raw_content: None,
            },
            std::iter::empty(),
        )
        .unwrap();
        assert!(out.0 > count * 6);
        assert!(out.0 < count * 6 + 1024);
    }
}

#[cfg(test)]
mod optional_order_tests {
    use super::*;
    #[test]
    fn omitted_order_and_explicit_zero_are_distinct_and_empty_slug_is_literal() {
        let body = "# Actual";
        let digest = crate::fingerprint::sha256_hex(body.as_bytes());
        for order in [None, Some(0)] {
            let mut source = std::io::Cursor::new(body.as_bytes());
            let mut bytes = Vec::new();
            write_editor_manifest(
                &mut bytes,
                EditorConvention {
                    slug: "",
                    title: "Actual",
                    parent_path: &[String::new()],
                    sort_order: order,
                    lifecycle: "Active",
                    applicability: &[],
                    markdown: EditorText {
                        sha256: &digest,
                        byte_length: body.len() as u64,
                        reader: &mut source,
                    },
                    raw_content: None,
                },
                std::iter::empty(),
            )
            .unwrap();
            let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(value.get("sortOrder").is_some(), order.is_some());
            assert_eq!(value["slug"], "");
            assert_eq!(value["parentPath"][0], "");
            if order.is_some() {
                assert_eq!(value["sortOrder"], 0);
            } else {
                assert!(value.get("sortOrder").is_none());
            }
        }
    }
}
