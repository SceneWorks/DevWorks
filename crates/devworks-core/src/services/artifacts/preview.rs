//! Bounded, derived previews (a collectible cache class, never retained evidence).
//!
//! A preview is computed deterministically from the first bytes of one exact version, so it
//! can be rebuilt whenever its cached copy was collected. Text previews are a UTF-8-safe prefix;
//! image previews report the sniffed format and pixel dimensions (no image decoding: rendering
//! is the shell's job); anything else gets a metadata-only preview with `Unavailable`
//! coverage. Previews identify their source version and their own digest.
use crate::fingerprint::sha256_hex;
use serde_json::json;

/// Revision of the preview generator; part of every derived identity, so a changed generator
/// never reuses an older cached payload.
pub const PREVIEW_REVISION: u32 = 1;
/// Source bytes a preview inspects (and the largest text preview).
pub const PREVIEW_BYTES: usize = 4096;
/// Media type of a text preview payload.
pub const TEXT_PREVIEW_MEDIA: &str = "text/plain; charset=utf-8";
/// Media type of an image or metadata preview payload.
pub const INFO_PREVIEW_MEDIA: &str = "application/vnd.devworks.artifact-preview+json";

/// How much of the source a preview covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Extent {
    Complete,
    Partial,
    Unavailable,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Preview {
    pub bytes: Vec<u8>,
    pub media_type: &'static str,
    pub extent: Extent,
}

/// Serializes a preview for the derived cache: a versioned header (format, media type, extent)
/// followed by the payload bytes.
pub fn encode_cached(p: &Preview) -> Vec<u8> {
    let extent = match p.extent {
        Extent::Complete => "complete",
        Extent::Partial => "partial",
        Extent::Unavailable => "unavailable",
    };
    let mut out = format!("dwpv{PREVIEW_REVISION}\n{}\n{extent}\n", p.media_type).into_bytes();
    out.extend_from_slice(&p.bytes);
    out
}

/// Reads a cached preview back; anything malformed or from another generator revision is
/// `None` (and rebuilt).
pub fn decode_cached(bytes: &[u8]) -> Option<Preview> {
    let mut parts = bytes.splitn(4, |b| *b == b'\n');
    let header = parts.next()?;
    if header != format!("dwpv{PREVIEW_REVISION}").as_bytes() {
        return None;
    }
    let media_type = match parts.next()? {
        m if m == TEXT_PREVIEW_MEDIA.as_bytes() => TEXT_PREVIEW_MEDIA,
        m if m == INFO_PREVIEW_MEDIA.as_bytes() => INFO_PREVIEW_MEDIA,
        _ => return None,
    };
    let extent = match parts.next()? {
        b"complete" => Extent::Complete,
        b"partial" => Extent::Partial,
        b"unavailable" => Extent::Unavailable,
        _ => return None,
    };
    let bytes = parts.next()?.to_vec();
    (bytes.len() <= PREVIEW_BYTES || media_type == INFO_PREVIEW_MEDIA).then_some(Preview {
        bytes,
        media_type,
        extent,
    })
}

/// The derived identity of a version's preview (`pv-` + 40 hex characters).
pub fn preview_id(version_id: &str) -> String {
    let digest = sha256_hex(format!("{version_id}|preview|{PREVIEW_REVISION}").as_bytes());
    format!("pv-{}", &digest[..40])
}

/// Whether a declared media type is text the reader may return inline.
pub fn textual(media_type: &str) -> bool {
    let m = media_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    m.starts_with("text/")
        || m.ends_with("+json")
        || m.ends_with("+xml")
        || matches!(
            m.as_str(),
            "application/json"
                | "application/xml"
                | "application/javascript"
                | "application/x-yaml"
                | "application/yaml"
                | "application/toml"
                | "application/x-sh"
                | "application/sql"
        )
}

/// The longest prefix of `bytes` that is valid UTF-8, or `None` when `bytes` contains invalid
/// UTF-8 (a truncated character at the very end is cut, not treated as invalid).
pub fn utf8_prefix(bytes: &[u8]) -> Option<&str> {
    match std::str::from_utf8(bytes) {
        Ok(s) => Some(s),
        Err(e) if e.error_len().is_none() => {
            Some(std::str::from_utf8(&bytes[..e.valid_up_to()]).expect("valid prefix"))
        }
        Err(_) => None,
    }
}

/// Image format and pixel dimensions sniffed from the leading bytes.
pub fn sniff_image(head: &[u8]) -> Option<(&'static str, u32, u32)> {
    let be16 = |i: usize| u16::from_be_bytes([head[i], head[i + 1]]) as u32;
    let be32 = |i: usize| u32::from_be_bytes([head[i], head[i + 1], head[i + 2], head[i + 3]]);
    let le16 = |i: usize| u16::from_le_bytes([head[i], head[i + 1]]) as u32;
    if head.len() >= 24 && head.starts_with(b"\x89PNG\r\n\x1a\n") && &head[12..16] == b"IHDR" {
        return Some(("png", be32(16), be32(20)));
    }
    if head.len() >= 10 && (head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a")) {
        return Some(("gif", le16(6), le16(8)));
    }
    if head.len() >= 30 && head.starts_with(b"RIFF") && &head[8..12] == b"WEBP" {
        let le24 = |i: usize| u32::from_le_bytes([head[i], head[i + 1], head[i + 2], 0]);
        return match &head[12..16] {
            b"VP8X" => Some(("webp", le24(24) + 1, le24(27) + 1)),
            b"VP8 " => Some(("webp", le16(26) & 0x3fff, le16(28) & 0x3fff)),
            b"VP8L" if head.len() >= 25 => {
                let b = u32::from_le_bytes([head[21], head[22], head[23], head[24]]);
                Some(("webp", (b & 0x3fff) + 1, ((b >> 14) & 0x3fff) + 1))
            }
            _ => None,
        };
    }
    if head.len() >= 4 && head.starts_with(&[0xff, 0xd8]) {
        // Walk JPEG segments to the first start-of-frame marker within the inspected bytes.
        let mut i = 2;
        while i + 9 < head.len() {
            if head[i] != 0xff {
                return None;
            }
            let marker = head[i + 1];
            if marker == 0xff {
                i += 1;
                continue;
            }
            let len = be16(i + 2) as usize;
            let sof = matches!(marker, 0xc0..=0xcf) && !matches!(marker, 0xc4 | 0xc8 | 0xcc);
            if sof {
                return Some(("jpeg", be16(i + 7), be16(i + 5)));
            }
            if len < 2 {
                return None;
            }
            i += 2 + len;
        }
    }
    None
}

/// Builds the preview of a source whose total length is `total` from its first bytes `head`
/// (at most [`PREVIEW_BYTES`] are inspected).
pub fn build(media_type: &str, head: &[u8], total: u64) -> Preview {
    let head = &head[..head.len().min(PREVIEW_BYTES)];
    if textual(media_type)
        && let Some(text) = utf8_prefix(head)
    {
        let complete = text.len() as u64 == total;
        return Preview {
            bytes: text.as_bytes().to_vec(),
            media_type: TEXT_PREVIEW_MEDIA,
            extent: if complete {
                Extent::Complete
            } else {
                Extent::Partial
            },
        };
    }
    if let Some((format, width, height)) = sniff_image(head) {
        let doc = json!({
            "kind": "image", "format": format, "width": width, "height": height,
            "mediaType": media_type, "byteLength": total, "previewRevision": PREVIEW_REVISION,
        });
        return Preview {
            bytes: doc.to_string().into_bytes(),
            media_type: INFO_PREVIEW_MEDIA,
            extent: Extent::Complete,
        };
    }
    let doc = json!({
        "kind": "metadata", "mediaType": media_type, "byteLength": total,
        "previewRevision": PREVIEW_REVISION,
    });
    Preview {
        bytes: doc.to_string().into_bytes(),
        media_type: INFO_PREVIEW_MEDIA,
        extent: Extent::Unavailable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_previews_are_bounded_and_utf8_safe() {
        let long = "é".repeat(PREVIEW_BYTES);
        let p = build("text/markdown", long.as_bytes(), long.len() as u64);
        assert!(p.bytes.len() <= PREVIEW_BYTES);
        assert!(std::str::from_utf8(&p.bytes).is_ok());
        assert_eq!(p.extent, Extent::Partial);
        let p = build("text/plain", b"short", 5);
        assert_eq!(
            (p.bytes.as_slice(), p.extent),
            (&b"short"[..], Extent::Complete)
        );
        // Invalid UTF-8 in a text-typed source is not returned as text.
        assert_eq!(
            build("text/plain", b"\xff\xfe", 2).extent,
            Extent::Unavailable
        );
    }

    #[test]
    fn image_dimensions_are_sniffed_without_decoding() {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend(640u32.to_be_bytes());
        png.extend(480u32.to_be_bytes());
        assert_eq!(sniff_image(&png), Some(("png", 640, 480)));
        let gif = b"GIF89a\x20\x00\x10\x00rest";
        assert_eq!(sniff_image(gif), Some(("gif", 32, 16)));
        let jpeg = [
            0xff, 0xd8, 0xff, 0xe0, 0x00, 0x04, 0x00, 0x00, 0xff, 0xc0, 0x00, 0x11, 0x08, 0x00,
            0x78, 0x00, 0xa0, 0x03,
        ];
        assert_eq!(sniff_image(&jpeg), Some(("jpeg", 160, 120)));
        let p = build("image/png", &png, 1000);
        let doc: serde_json::Value = serde_json::from_slice(&p.bytes).unwrap();
        assert_eq!(
            (doc["width"].as_u64(), doc["height"].as_u64()),
            (Some(640), Some(480))
        );
        assert_eq!(
            build("application/zip", b"PK\x03\x04", 4).extent,
            Extent::Unavailable
        );
    }

    #[test]
    fn preview_identity_is_derived_and_distinct() {
        let a = preview_id("version-1");
        assert!(a.starts_with("pv-") && a.len() == 43);
        assert_ne!(a, preview_id("version-2"));
        assert!(textual("application/vnd.api+json") && !textual("image/png"));
    }
}
