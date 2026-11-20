//! OPC parts are opened by canonical package name through a bounded in-memory ZIP reader.
use super::{DocxError, DocxLimits};
use devworks_core::services::documents::extraction::ExtractionControl;
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{self, Cursor, Read, Seek, SeekFrom},
};
use zip::{
    CompressionMethod, ZipArchive,
    read::{ArchiveOffset, Config},
};

const OLE_MAGIC: &[u8] = b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1";
const END: &[u8] = b"PK\x05\x06";

pub(super) struct Package<'a> {
    archive: ZipArchive<Cursor<&'a [u8]>>,
    pub(super) names: BTreeMap<String, usize>,
    remaining: usize,
    part_bytes: usize,
}

impl<'a> Package<'a> {
    pub(super) fn open(
        data: &'a [u8],
        limits: &DocxLimits,
        control: &ExtractionControl<'_>,
    ) -> Result<Self, DocxError> {
        control.check()?;
        if data.starts_with(OLE_MAGIC) {
            return Err(classify_compound(data, control)?);
        }
        if !data.starts_with(b"PK\x03\x04") {
            return Err(DocxError::MalformedPackage);
        }
        let (count, directory) = directory_shape(data, limits.parts)?;
        let mut archive = ZipArchive::with_config(
            Config {
                archive_offset: ArchiveOffset::Known(0),
            },
            Cursor::new(data),
        )
        .map_err(|_| DocxError::MalformedPackage)?;
        control.check()?;
        if archive.has_duplicate_names() {
            return Err(DocxError::DuplicatePart);
        }
        if archive.len() != count || archive.central_directory_start() != directory as u64 {
            return Err(DocxError::MalformedPackage);
        }

        let mut names = BTreeMap::new();
        let mut seen = BTreeSet::new();
        let mut declared = 0u64;
        for index in 0..count {
            control.check()?;
            let entry = archive
                .by_index_data(index)
                .map_err(|_| DocxError::MalformedPackage)?;
            if entry.encrypted() {
                return Err(DocxError::Encrypted);
            }
            if entry.is_symlink() {
                return Err(DocxError::UnsafePartName);
            }
            if !matches!(
                entry.compression(),
                CompressionMethod::Stored | CompressionMethod::Deflated
            ) {
                return Err(DocxError::UnsupportedCompression);
            }
            check_local_header(
                &data[..directory],
                entry.header_start(),
                entry.name_raw(),
                entry.compression(),
                entry.compressed_size(),
            )?;
            let raw =
                std::str::from_utf8(entry.name_raw()).map_err(|_| DocxError::UnsafePartName)?;
            let name = part_name(if entry.is_dir() {
                raw.trim_end_matches('/')
            } else {
                raw
            })?;
            if !seen.insert(name.clone()) {
                return Err(DocxError::DuplicatePart);
            }
            if entry.is_dir() && (entry.size() != 0 || !raw.ends_with('/') || raw.ends_with("//")) {
                return Err(DocxError::UnsafePartName);
            }
            if entry.size() > limits.part_bytes as u64 {
                return Err(DocxError::PartSizeLimit);
            }
            declared = declared
                .checked_add(entry.size())
                .ok_or(DocxError::ExpandedLimit)?;
            if declared > limits.expanded_bytes as u64 {
                return Err(DocxError::ExpandedLimit);
            }
            if name.ends_with("vbaproject.bin") {
                return Err(DocxError::UnsupportedVariant);
            }
            if !entry.is_dir() {
                names.insert(name, index);
            }
        }
        if archive
            .has_overlapping_entries()
            .map_err(|_| DocxError::MalformedPackage)?
        {
            return Err(DocxError::MalformedPackage);
        }
        Ok(Self {
            archive,
            names,
            remaining: limits.expanded_bytes,
            part_bytes: limits.part_bytes,
        })
    }

    pub(super) fn read(
        &mut self,
        name: &str,
        control: &ExtractionControl<'_>,
    ) -> Result<Vec<u8>, DocxError> {
        let index = *self.names.get(name).ok_or(DocxError::MissingPart)?;
        let mut file = self
            .archive
            .by_index(index)
            .map_err(|_| DocxError::MalformedPackage)?;
        let declared = file.size() as usize;
        let mut bytes = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            control.check()?;
            let authorized = (self.part_bytes - bytes.len())
                .min(self.remaining)
                .min(declared - bytes.len());
            let request = chunk.len().min(authorized + 1);
            let count = file
                .read(&mut chunk[..request])
                .map_err(|_| DocxError::MalformedPackage)?;
            if count == 0 {
                break;
            }
            if bytes.len() + count > self.part_bytes {
                return Err(DocxError::PartSizeLimit);
            }
            self.remaining = self
                .remaining
                .checked_sub(count)
                .ok_or(DocxError::ExpandedLimit)?;
            if bytes.len() + count > declared {
                return Err(DocxError::MalformedPackage);
            }
            bytes.extend_from_slice(&chunk[..count]);
        }
        if bytes.len() != declared {
            return Err(DocxError::MalformedPackage);
        }
        Ok(bytes)
    }
}

fn check_local_header(
    package: &[u8],
    offset: u64,
    name: &[u8],
    method: CompressionMethod,
    compressed_bytes: u64,
) -> Result<(), DocxError> {
    use DocxError::MalformedPackage as Bad;
    let start = usize::try_from(offset).map_err(|_| Bad)?;
    let header = package
        .get(start..)
        .filter(|tail| tail.len() >= 30)
        .ok_or(Bad)?;
    if !header.starts_with(b"PK\x03\x04")
        || u16_at(header, 8)
            != match method {
                CompressionMethod::Stored => 0,
                CompressionMethod::Deflated => 8,
                _ => return Err(DocxError::UnsupportedCompression),
            }
    {
        return Err(Bad);
    }
    if u16_at(header, 6) & 1 != 0 {
        return Err(DocxError::Encrypted);
    }
    let name_end = 30 + usize::from(u16_at(header, 26));
    if header.get(30..name_end) != Some(name) {
        return Err(Bad);
    }
    let payload = name_end + usize::from(u16_at(header, 28));
    if header
        .get(payload..)
        .is_none_or(|tail| (tail.len() as u64) < compressed_bytes)
    {
        return Err(Bad);
    }
    Ok(())
}

fn classify_compound(
    bytes: &[u8],
    control: &ExtractionControl<'_>,
) -> Result<DocxError, DocxError> {
    let opened = cfb::CompoundFile::open_strict(ControlledCursor {
        cursor: Cursor::new(bytes),
        control,
    });
    control.check()?;
    let file = opened.map_err(|_| DocxError::MalformedPackage)?;
    let encrypted = file.is_stream("/EncryptedPackage") && file.is_stream("/EncryptionInfo");
    Ok(if encrypted {
        DocxError::Encrypted
    } else {
        DocxError::CompoundContainer
    })
}

struct ControlledCursor<'a, 'b> {
    cursor: Cursor<&'a [u8]>,
    control: &'a ExtractionControl<'b>,
}

impl Read for ControlledCursor<'_, '_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.control
            .check()
            .map_err(|_| io::Error::other("stopped"))?;
        self.cursor.read(buffer)
    }
}

impl Seek for ControlledCursor<'_, '_> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.control
            .check()
            .map_err(|_| io::Error::other("stopped"))?;
        self.cursor.seek(position)
    }
}

// ZIP writes its end record after the central directory. Bound the declared part count before
// zip allocates central-directory state, and bind its chosen directory to the same record.
fn directory_shape(bytes: &[u8], max_parts: usize) -> Result<(usize, usize), DocxError> {
    use DocxError::MalformedPackage as Bad;
    let minimum = bytes.len().saturating_sub(22 + usize::from(u16::MAX));
    let last = bytes.len().checked_sub(22).ok_or(Bad)?;
    let mut candidates = (minimum..=last).rev().filter(|start| {
        bytes[*start..].starts_with(END)
            && *start + 22 + usize::from(u16_at(&bytes[*start..], 20)) == bytes.len()
    });
    let end_at = candidates.next().ok_or(Bad)?;
    if candidates.next().is_some() {
        return Err(Bad);
    }
    let end = &bytes[end_at..];
    if u16_at(end, 4) != 0 || u16_at(end, 6) != 0 || u16_at(end, 8) != u16_at(end, 10) {
        return Err(Bad);
    }
    let (count, size, start, directory_end) = if u16_at(end, 10) == u16::MAX
        || u32_at(end, 12) == u32::MAX
        || u32_at(end, 16) == u32::MAX
    {
        let locator_at = end_at.checked_sub(20).ok_or(Bad)?;
        let locator = &bytes[locator_at..end_at];
        if !locator.starts_with(b"PK\x06\x07")
            || u32_at(locator, 4) != 0
            || u32_at(locator, 16) != 1
        {
            return Err(Bad);
        }
        let at = usize::try_from(u64_at(locator, 8)).map_err(|_| Bad)?;
        let record = bytes
            .get(at..locator_at)
            .filter(|record| record.len() >= 56)
            .ok_or(Bad)?;
        if !record.starts_with(b"PK\x06\x06")
            || u64_at(record, 4) != (record.len() - 12) as u64
            || u32_at(record, 16) != 0
            || u32_at(record, 20) != 0
            || u64_at(record, 24) != u64_at(record, 32)
        {
            return Err(Bad);
        }
        (
            u64_at(record, 32),
            u64_at(record, 40),
            u64_at(record, 48),
            at,
        )
    } else {
        (
            u64::from(u16_at(end, 10)),
            u64::from(u32_at(end, 12)),
            u64::from(u32_at(end, 16)),
            end_at,
        )
    };
    if count > max_parts as u64 {
        return Err(DocxError::PartCountLimit);
    }
    if count == 0 || size < count * 46 || start.checked_add(size) != Some(directory_end as u64) {
        return Err(Bad);
    }
    Ok((count as usize, start as usize))
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap())
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

pub(super) fn part_name(raw: &str) -> Result<String, DocxError> {
    if raw.len() > 1024 || raw.split('/').any(|segment| !valid_segment(segment)) {
        return Err(DocxError::UnsafePartName);
    }
    Ok(raw.to_ascii_lowercase())
}

fn valid_segment(segment: &str) -> bool {
    if segment.eq_ignore_ascii_case("[Content_Types].xml") {
        return true;
    }
    if segment.is_empty() || segment.ends_with('.') {
        return false;
    }
    let mut rest = segment.as_bytes();
    while let Some((&byte, tail)) = rest.split_first() {
        rest = tail;
        if byte == b'%' {
            let Some(hex) = rest.get(..2) else {
                return false;
            };
            let digit = |digit: u8| char::from(digit).to_digit(16);
            let Some((first, second)) = digit(hex[0]).zip(digit(hex[1])) else {
                return false;
            };
            let decoded = (first * 16 + second) as u8;
            if unreserved(decoded)
                || decoded.is_ascii_control()
                || matches!(decoded, b'/' | b'\\' | b'%')
            {
                return false;
            }
            rest = &rest[2..];
        } else if !unreserved(byte) && !b"!$&'()+,;=@".contains(&byte) {
            return false;
        }
    }
    true
}

fn unreserved(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"-._~".contains(&byte)
}

/// Resolve an internal package relationship against its source part. A link may traverse to its
/// parent package directory; it can never escape the root, name a URI, or name a filesystem path.
pub(super) fn resolve_part(source: &str, target: &str) -> Result<String, DocxError> {
    let mut segments = if target.starts_with('/') {
        Vec::new()
    } else {
        source
            .rsplit_once('/')
            .map(|(parent, _)| parent.split('/').collect())
            .unwrap_or_default()
    };
    for segment in target.strip_prefix('/').unwrap_or(target).split('/') {
        match segment {
            "." => {}
            ".." if segments.pop().is_some() => {}
            _ if valid_segment(segment) => segments.push(segment),
            _ => return Err(DocxError::UnsafePartName),
        }
    }
    part_name(&segments.join("/"))
}

pub(super) fn relationship_part(source: &str) -> String {
    source.rsplit_once('/').map_or_else(
        || format!("_rels/{source}.rels"),
        |(parent, file)| format!("{parent}/_rels/{file}.rels"),
    )
}
