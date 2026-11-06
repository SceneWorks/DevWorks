//! A portable, streamed manifest. No output contains host authority, foreign author IDs or
//! nonportable artifact references. The actual staging owner enforces its artifact quota.
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
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn json(writer: &mut dyn Write, value: &impl serde::Serialize) -> io::Result<()> {
    serde_json::to_writer(writer, value).map_err(io::Error::other)
}
fn text(writer: &mut dyn Write, source: &mut EditorText<'_>, required: bool) -> io::Result<()> {
    if source.byte_length > MAX_MARKDOWN_BYTES {
        return Err(invalid("convention text exceeds its retained UTF-8 bound"));
    }
    write!(writer, "{{\"sha256\":")?;
    json(writer, &source.sha256)?;
    write!(
        writer,
        ",\"byteLength\":\"{}\",\"text\":\"",
        source.byte_length
    )?;
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
            '"' => writer.write_all(b"\\\"")?,
            '\\' => writer.write_all(b"\\\\")?,
            c if (c as u32) < 32 => write!(writer, "\\u{:04x}", c as u32)?,
            _ => writer.write_all(&raw[..count])?,
        }
    }
    verify(source, bytes, hash)?;
    if required && !nonblank {
        return Err(invalid("Markdown content is required"));
    }
    writer.write_all(b"\"}")
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
    writer.write_all(b"{\"format\":\"devworks-convention\",\"version\":1,\"slug\":")?;
    json(writer, &document.slug)?;
    writer.write_all(b",\"title\":")?;
    json(writer, &document.title)?;
    writer.write_all(b",\"parentPath\":")?;
    json(writer, &document.parent_path)?;
    if let Some(order) = document.sort_order {
        write!(writer, ",\"sortOrder\":{order}")?;
    }
    writer.write_all(b",\"lifecycle\":")?;
    json(writer, &document.lifecycle)?;
    writer.write_all(b",\"applicability\":")?;
    json(writer, &document.applicability)?;
    writer.write_all(b",\"markdown\":")?;
    text(writer, &mut document.markdown, true)?;
    if let Some(mut raw) = document.raw_content {
        writer.write_all(b",\"rawContent\":")?;
        text(writer, &mut raw, false)?;
    }
    writer.write_all(b",\"resources\":[")?;
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
        if !first {
            writer.write_all(b",")?;
        }
        first = false;
        writer.write_all(b"{\"key\":")?;
        json(writer, &resource.key)?;
        writer.write_all(b",\"name\":")?;
        json(writer, &resource.name)?;
        writer.write_all(b",\"mediaType\":")?;
        json(writer, &resource.media_type)?;
        writer.write_all(b",\"lifecycle\":")?;
        json(writer, &resource.lifecycle)?;
        writer.write_all(b",\"sha256\":")?;
        json(writer, &resource.source.sha256)?;
        write!(
            writer,
            ",\"byteLength\":\"{}\",\"base64\":\"",
            resource.source.byte_length
        )?;
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
        writer.write_all(b"\"}")?;
    }
    writer.write_all(b"]}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fingerprint::sha256_hex;
    use base64::Engine;
    use std::io::Cursor;
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
