//! Closed streaming parser for the registered portable format. Payloads are located and
//! hashed without retaining their JSON strings; callbacks can reread one immutable range.
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};

#[derive(Debug)]
pub enum Error {
    Invalid(&'static str),
    Io(io::Error),
    Stopped,
}
impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(reason) => f.write_str(reason),
            Self::Io(_) => f.write_str("import source read failed"),
            Self::Stopped => f.write_str("import stopped"),
        }
    }
}
impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Metadata {
    pub slug: Option<String>,
    pub title: Option<String>,
    pub parent_path: Option<Vec<String>>,
    pub sort_order: Option<i32>,
    pub lifecycle: Option<String>,
    pub applicability: Option<Vec<String>>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Checkpoint {
    pub version: u8,
    pub offset: u64,
    pub seen: u16,
    pub resources: bool,
    pub separator: bool,
    pub resource_index: i64,
    pub metadata: Metadata,
}
impl Default for Checkpoint {
    fn default() -> Self {
        Self {
            version: 1,
            offset: 0,
            seen: 0,
            resources: false,
            separator: false,
            resource_index: 0,
            metadata: Metadata::default(),
        }
    }
}
#[derive(Clone, Debug)]
pub struct Payload {
    pub start: u64,
    pub end: u64,
    pub sha256: String,
    pub byte_length: u64,
    pub base64: bool,
    pub non_whitespace: bool,
}
#[derive(Clone, Debug)]
pub struct Entry {
    pub ordinal: i64,
    pub kind: &'static str,
    pub key: Option<String>,
    pub name: Option<String>,
    pub media_type: Option<String>,
    pub lifecycle: Option<String>,
    pub payload: Payload,
}
pub enum Step {
    Entry {
        entry: Entry,
        checkpoint: Checkpoint,
    },
    Complete {
        metadata: Metadata,
        resources: i64,
    },
}
pub trait ReadSeek: Read + Seek + Send {}
impl<T: Read + Seek + Send> ReadSeek for T {}
pub struct Parser<'a> {
    reader: BufReader<&'a mut dyn ReadSeek>,
    offset: u64,
    stop: &'a (dyn Fn() -> bool + Sync),
    max_resource: u64,
}
impl<'a> Parser<'a> {
    pub fn new(
        reader: &'a mut dyn ReadSeek,
        checkpoint: &Checkpoint,
        max_resource: u64,
        stop: &'a (dyn Fn() -> bool + Sync),
    ) -> Result<Self, Error> {
        if checkpoint.version != 1 || checkpoint.resource_index < 0 {
            return Err(Error::Invalid("invalid import checkpoint"));
        }
        reader.seek(SeekFrom::Start(checkpoint.offset))?;
        Ok(Self {
            reader: BufReader::with_capacity(64 * 1024, reader),
            offset: checkpoint.offset,
            stop,
            max_resource,
        })
    }
    fn peek(&mut self) -> Result<Option<u8>, Error> {
        if self.reader.buffer().is_empty() && (self.stop)() {
            return Err(Error::Stopped);
        }
        Ok(self.reader.fill_buf()?.first().copied())
    }
    fn byte(&mut self) -> Result<u8, Error> {
        let value = self
            .peek()?
            .ok_or(Error::Invalid("unexpected end of manifest"))?;
        self.reader.consume(1);
        self.offset = self
            .offset
            .checked_add(1)
            .ok_or(Error::Invalid("source position overflow"))?;
        Ok(value)
    }
    fn whitespace(&mut self) -> Result<(), Error> {
        while self
            .peek()?
            .is_some_and(|b| matches!(b, b' ' | b'\n' | b'\r' | b'\t'))
        {
            self.byte()?;
        }
        Ok(())
    }
    fn expect(&mut self, value: u8) -> Result<(), Error> {
        self.whitespace()?;
        if self.byte()? != value {
            return Err(Error::Invalid("unexpected manifest token"));
        }
        Ok(())
    }
    fn small(&mut self, max: usize, empty: bool) -> Result<String, Error> {
        let mut bytes = Vec::new();
        self.string(&mut bytes, (max as u64) * 4)?;
        let value = String::from_utf8(bytes).map_err(|_| Error::Invalid("invalid UTF-8"))?;
        if value.chars().count() > max || (!empty && value.is_empty()) {
            return Err(Error::Invalid("manifest scalar length"));
        }
        Ok(value)
    }
    fn key(&mut self) -> Result<String, Error> {
        let key = self.small(32, false)?;
        self.expect(b':')?;
        Ok(key)
    }
    fn object_separator(&mut self, first: &mut bool) -> Result<bool, Error> {
        self.whitespace()?;
        if self.peek()? == Some(b'}') {
            self.byte()?;
            return Ok(false);
        }
        if !*first {
            self.expect(b',')?;
            self.whitespace()?;
            if self.peek()? == Some(b'}') {
                return Err(Error::Invalid("trailing comma"));
            }
        }
        *first = false;
        Ok(true)
    }
    fn integer(&mut self) -> Result<i32, Error> {
        self.whitespace()?;
        let negative = if self.peek()? == Some(b'-') {
            self.byte()?;
            true
        } else {
            false
        };
        let mut number = Number::default();
        let first = self.byte()?;
        if !first.is_ascii_digit() {
            return Err(Error::Invalid("invalid integer"));
        }
        number.digit(first - b'0');
        if first == b'0' && self.peek()?.is_some_and(|b| b.is_ascii_digit()) {
            return Err(Error::Invalid("leading numeric zero"));
        }
        if first != b'0' {
            while self.peek()?.is_some_and(|b| b.is_ascii_digit()) {
                number.digit(self.byte()? - b'0');
            }
        }
        let mut fraction = 0i128;
        if self.peek()? == Some(b'.') {
            self.byte()?;
            if !self.peek()?.is_some_and(|b| b.is_ascii_digit()) {
                return Err(Error::Invalid("missing fractional digit"));
            }
            while self.peek()?.is_some_and(|b| b.is_ascii_digit()) {
                number.digit(self.byte()? - b'0');
                fraction += 1;
            }
        }
        let mut exponent = 0i128;
        if self.peek()?.is_some_and(|b| matches!(b, b'e' | b'E')) {
            self.byte()?;
            let minus = match self.peek()? {
                Some(b'-') => {
                    self.byte()?;
                    true
                }
                Some(b'+') => {
                    self.byte()?;
                    false
                }
                _ => false,
            };
            if !self.peek()?.is_some_and(|b| b.is_ascii_digit()) {
                return Err(Error::Invalid("missing exponent digit"));
            }
            while self.peek()?.is_some_and(|b| b.is_ascii_digit()) {
                exponent = (exponent.saturating_mul(10) + i128::from(self.byte()? - b'0'))
                    .min(i128::from(u64::MAX) * 2);
            }
            if minus {
                exponent = -exponent;
            }
        }
        if number.value == 0 && !number.overflow {
            return Ok(0);
        }
        let power = exponent + number.trailing - fraction;
        if number.overflow || !(0..=10).contains(&power) {
            return Err(Error::Invalid("number is not an int32"));
        }
        let value = i128::from(number.value) * 10i128.pow(power as u32);
        i32::try_from(if negative { -value } else { value })
            .map_err(|_| Error::Invalid("number exceeds int32"))
    }
    fn string_array(
        &mut self,
        max_items: usize,
        max_chars: usize,
        unique: bool,
    ) -> Result<Vec<String>, Error> {
        self.expect(b'[')?;
        let mut values = Vec::new();
        self.whitespace()?;
        if self.peek()? == Some(b']') {
            self.byte()?;
            return Ok(values);
        }
        loop {
            if values.len() >= max_items {
                return Err(Error::Invalid("manifest array bound"));
            }
            let value = self.small(max_chars, !unique)?;
            if unique && value.contains('\0') {
                return Err(Error::Invalid("applicability contains NUL"));
            }
            if unique && values.contains(&value) {
                return Err(Error::Invalid("duplicate applicability"));
            }
            values.push(value);
            self.whitespace()?;
            match self.byte()? {
                b']' => return Ok(values),
                b',' => {}
                _ => return Err(Error::Invalid("invalid array delimiter")),
            }
        }
    }
    pub fn next(&mut self, mut state: Checkpoint) -> Result<Step, Error> {
        if state.offset == 0 {
            self.expect(b'{')?;
        }
        loop {
            if state.resources {
                self.whitespace()?;
                if self.peek()? == Some(b']') {
                    self.byte()?;
                    state.resources = false;
                    state.separator = true;
                    continue;
                }
                if state.separator {
                    self.expect(b',')?;
                    self.whitespace()?;
                    if self.peek()? == Some(b']') {
                        return Err(Error::Invalid("trailing resource comma"));
                    }
                }
                let ordinal = state
                    .resource_index
                    .checked_add(2)
                    .ok_or(Error::Invalid("resource count overflow"))?;
                let entry = self.resource(ordinal)?;
                state.resource_index = state
                    .resource_index
                    .checked_add(1)
                    .ok_or(Error::Invalid("resource count overflow"))?;
                state.separator = true;
                state.offset = self.offset;
                return Ok(Step::Entry {
                    entry,
                    checkpoint: state,
                });
            }
            self.whitespace()?;
            if self.peek()? == Some(b'}') {
                self.byte()?;
                self.whitespace()?;
                if self.peek()?.is_some() {
                    return Err(Error::Invalid("trailing manifest content"));
                }
                // Portable sortOrder is optional; omission appends under publication CAS.
                let required = ((1 << 10) - 1) & !(1 << 5);
                if state.seen & required != required {
                    return Err(Error::Invalid("missing required manifest field"));
                }
                return Ok(Step::Complete {
                    metadata: state.metadata,
                    resources: state.resource_index,
                });
            }
            if state.separator {
                self.expect(b',')?;
                self.whitespace()?;
                if self.peek()? == Some(b'}') {
                    return Err(Error::Invalid("trailing manifest comma"));
                }
            }
            let key = self.key()?;
            let bit = match key.as_str() {
                "format" => 0,
                "version" => 1,
                "slug" => 2,
                "title" => 3,
                "parentPath" => 4,
                "sortOrder" => 5,
                "lifecycle" => 6,
                "applicability" => 7,
                "markdown" => 8,
                "resources" => 9,
                "rawContent" => 10,
                _ => return Err(Error::Invalid("unknown manifest field")),
            };
            if state.seen & (1 << bit) != 0 {
                return Err(Error::Invalid("duplicate manifest field"));
            }
            state.seen |= 1 << bit;
            state.separator = true;
            match key.as_str() {
                "format" => {
                    if self.small(32, false)? != "devworks-convention" {
                        return Err(Error::Invalid("unsupported manifest format"));
                    }
                }
                "version" => {
                    if self.integer()? != 1 {
                        return Err(Error::Invalid("unsupported manifest version"));
                    }
                }
                "slug" => state.metadata.slug = Some(self.small(200, true)?),
                "title" => {
                    let title = self.small(500, false)?;
                    if title.chars().all(char::is_whitespace) {
                        return Err(Error::Invalid("blank title"));
                    }
                    state.metadata.title = Some(title);
                }
                "parentPath" => {
                    state.metadata.parent_path = Some(self.string_array(3, 200, false)?)
                }
                "sortOrder" => state.metadata.sort_order = Some(self.integer()?),
                "lifecycle" => state.metadata.lifecycle = Some(self.lifecycle()?),
                "applicability" => {
                    state.metadata.applicability = Some(self.string_array(128, 256, true)?)
                }
                "resources" => {
                    self.expect(b'[')?;
                    state.resources = true;
                    state.separator = false;
                }
                "markdown" | "rawContent" => {
                    let payload = self.body(false, super::super::model::MAX_MARKDOWN_BYTES)?;
                    if key == "markdown" && !payload.non_whitespace {
                        return Err(Error::Invalid("blank Markdown"));
                    }
                    let entry = Entry {
                        ordinal: if key == "markdown" { 0 } else { 1 },
                        kind: if key == "markdown" { "Markdown" } else { "Raw" },
                        key: None,
                        name: None,
                        media_type: None,
                        lifecycle: None,
                        payload,
                    };
                    state.offset = self.offset;
                    return Ok(Step::Entry {
                        entry,
                        checkpoint: state,
                    });
                }
                _ => unreachable!(),
            }
        }
    }
    fn lifecycle(&mut self) -> Result<String, Error> {
        let value = self.small(16, false)?;
        if !matches!(value.as_str(), "Active" | "Retired") {
            return Err(Error::Invalid("invalid lifecycle"));
        }
        Ok(value)
    }
    fn body(&mut self, resource: bool, max: u64) -> Result<Payload, Error> {
        self.expect(b'{')?;
        let mut first = true;
        let mut seen = 0;
        let (mut digest, mut length, mut actual) = (None, None, None);
        while self.object_separator(&mut first)? {
            let key = self.key()?;
            let bit = match key.as_str() {
                "sha256" => 0,
                "byteLength" => 1,
                "text" if !resource => 2,
                _ => return Err(Error::Invalid("unknown document field")),
            };
            if seen & (1 << bit) != 0 {
                return Err(Error::Invalid("duplicate document field"));
            }
            seen |= 1 << bit;
            match bit {
                0 => digest = Some(self.digest()?),
                1 => length = Some(self.length()?),
                2 => actual = Some(self.payload(false, max)?),
                _ => unreachable!(),
            }
        }
        if seen != 7 {
            return Err(Error::Invalid("missing document field"));
        }
        let mut payload = actual.unwrap();
        if digest.as_ref() != Some(&payload.sha256) || length != Some(payload.byte_length) {
            return Err(Error::Invalid("document digest or length mismatch"));
        }
        payload.base64 = false;
        Ok(payload)
    }
    fn resource(&mut self, ordinal: i64) -> Result<Entry, Error> {
        self.expect(b'{')?;
        let mut first = true;
        let mut seen = 0u8;
        let (mut key, mut name, mut media, mut lifecycle, mut digest, mut length, mut payload) =
            (None, None, None, None, None, None, None);
        while self.object_separator(&mut first)? {
            let field = self.key()?;
            let bit = match field.as_str() {
                "key" => 0,
                "name" => 1,
                "mediaType" => 2,
                "lifecycle" => 3,
                "sha256" => 4,
                "byteLength" => 5,
                "base64" => 6,
                _ => return Err(Error::Invalid("unknown resource field")),
            };
            if seen & (1 << bit) != 0 {
                return Err(Error::Invalid("duplicate resource field"));
            }
            seen |= 1 << bit;
            match bit {
                0 => {
                    let value = self.small(200, false)?;
                    if value.contains('\0') {
                        return Err(Error::Invalid("resource key contains NUL"));
                    }
                    key = Some(value);
                }
                1 => {
                    let value = self.small(255, false)?;
                    if value.contains('\0') {
                        return Err(Error::Invalid("resource name contains NUL"));
                    }
                    name = Some(value);
                }
                2 => {
                    let value = self.small(200, true)?;
                    if value.contains('\0') {
                        return Err(Error::Invalid("resource media type contains NUL"));
                    }
                    media = Some(value);
                }
                3 => lifecycle = Some(self.lifecycle()?),
                4 => digest = Some(self.digest()?),
                5 => length = Some(self.length()?),
                6 => payload = Some(self.payload(true, self.max_resource)?),
                _ => unreachable!(),
            }
        }
        if seen != 127 {
            return Err(Error::Invalid("missing resource field"));
        }
        let payload = payload.unwrap();
        if digest.as_ref() != Some(&payload.sha256) || length != Some(payload.byte_length) {
            return Err(Error::Invalid("resource digest or length mismatch"));
        }
        Ok(Entry {
            ordinal,
            kind: "Resource",
            key,
            name,
            media_type: media,
            lifecycle,
            payload,
        })
    }
    fn digest(&mut self) -> Result<String, Error> {
        let value = self.small(64, false)?;
        if value.len() != 64
            || !value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(Error::Invalid("invalid digest"));
        }
        Ok(value)
    }
    fn length(&mut self) -> Result<u64, Error> {
        let value = self.small(20, false)?;
        if !value.bytes().all(|b| b.is_ascii_digit()) || value.len() > 1 && value.starts_with('0') {
            return Err(Error::Invalid("invalid byte length"));
        }
        value
            .parse()
            .map_err(|_| Error::Invalid("byte length overflow"))
    }
    fn payload(&mut self, encoded: bool, max: u64) -> Result<Payload, Error> {
        self.whitespace()?;
        let start = self.offset;
        let mut sink = HashSink {
            hash: Sha256::new(),
            bytes: 0,
            max,
            text: !encoded,
            non_whitespace: false,
        };
        if encoded {
            let mut decoded = Base64Sink {
                inner: &mut sink,
                quartet: Vec::with_capacity(4),
                ended: false,
            };
            self.string(
                &mut decoded,
                max.saturating_add(2).saturating_div(3).saturating_mul(4),
            )?;
            if !decoded.quartet.is_empty() {
                return Err(Error::Invalid("incomplete base64 quartet"));
            }
            decoded.flush()?;
        } else {
            self.string(&mut sink, max)?;
        }
        let sha256 = sink
            .hash
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        Ok(Payload {
            start,
            end: self.offset,
            sha256,
            byte_length: sink.bytes,
            base64: encoded,
            non_whitespace: sink.non_whitespace,
        })
    }
    fn scalar(&mut self) -> Result<Option<([u8; 4], usize)>, Error> {
        let byte = self.byte()?;
        if byte == b'"' {
            return Ok(None);
        }
        let mut encoded = [0u8; 4];
        let len = if byte == b'\\' {
            let code = self.byte()?;
            if code == b'u' {
                let first = self.hex()?;
                let scalar = if (0xd800..=0xdbff).contains(&first) {
                    if self.byte()? != b'\\' || self.byte()? != b'u' {
                        return Err(Error::Invalid("unpaired surrogate"));
                    }
                    let low = self.hex()?;
                    if !(0xdc00..=0xdfff).contains(&low) {
                        return Err(Error::Invalid("invalid low surrogate"));
                    }
                    0x10000 + (((first as u32) - 0xd800) << 10) + (low as u32) - 0xdc00
                } else {
                    first as u32
                };
                char::from_u32(scalar)
                    .ok_or(Error::Invalid("invalid Unicode scalar"))?
                    .encode_utf8(&mut encoded)
                    .len()
            } else {
                encoded[0] = match code {
                    b'"' => b'"',
                    b'\\' => b'\\',
                    b'/' => b'/',
                    b'b' => 8,
                    b'f' => 12,
                    b'n' => 10,
                    b'r' => 13,
                    b't' => 9,
                    _ => return Err(Error::Invalid("invalid JSON escape")),
                };
                1
            }
        } else {
            if byte < 0x20 {
                return Err(Error::Invalid("unescaped JSON control"));
            }
            encoded[0] = byte;
            let len = if byte < 0x80 {
                1
            } else if byte & 0xe0 == 0xc0 {
                2
            } else if byte & 0xf0 == 0xe0 {
                3
            } else if byte & 0xf8 == 0xf0 {
                4
            } else {
                return Err(Error::Invalid("invalid UTF-8 prefix"));
            };
            for slot in encoded.iter_mut().take(len).skip(1) {
                *slot = self.byte()?;
            }
            std::str::from_utf8(&encoded[..len])
                .map_err(|_| Error::Invalid("invalid UTF-8 string"))?;
            len
        };
        Ok(Some((encoded, len)))
    }
    fn string(&mut self, output: &mut dyn Write, max: u64) -> Result<(), Error> {
        self.expect(b'"')?;
        let mut count = 0u64;
        let mut pending = Vec::with_capacity(16 * 1024 + 4);
        while let Some((encoded, len)) = self.scalar()? {
            count = count
                .checked_add(len as u64)
                .ok_or(Error::Invalid("string length overflow"))?;
            if count > max {
                return Err(Error::Invalid("decoded payload exceeds configured bound"));
            }
            pending.extend_from_slice(&encoded[..len]);
            if pending.len() >= 16 * 1024 {
                output.write_all(&pending)?;
                pending.clear();
            }
        }
        output.write_all(&pending)?;
        Ok(())
    }
    fn hex(&mut self) -> Result<u16, Error> {
        let mut value = 0u16;
        for _ in 0..4 {
            let byte = self.byte()?;
            let digit = match byte {
                b'0'..=b'9' => byte - b'0',
                b'a'..=b'f' => byte - b'a' + 10,
                b'A'..=b'F' => byte - b'A' + 10,
                _ => return Err(Error::Invalid("invalid Unicode escape")),
            };
            value = (value << 4) | digit as u16;
        }
        Ok(value)
    }
}
struct HashSink {
    hash: Sha256,
    bytes: u64,
    max: u64,
    text: bool,
    non_whitespace: bool,
}
impl Write for HashSink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.bytes = self
            .bytes
            .checked_add(bytes.len() as u64)
            .filter(|n| *n <= self.max)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "decoded resource exceeds configured byte bound",
                )
            })?;
        if self.text {
            self.non_whitespace |= std::str::from_utf8(bytes)
                .map_err(|_| io::Error::other("invalid UTF-8"))?
                .chars()
                .any(|c| !c.is_whitespace());
        }
        self.hash.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
struct Base64Sink<'a> {
    inner: &'a mut dyn Write,
    quartet: Vec<u8>,
    ended: bool,
}
impl Write for Base64Sink<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        for byte in bytes {
            if self.ended {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "data after base64 padding",
                ));
            }
            self.quartet.push(*byte);
            if self.quartet.len() == 4 {
                let mut decoded = [0u8; 3];
                let count = base64::engine::general_purpose::STANDARD
                    .decode_slice(&self.quartet, &mut decoded)
                    .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid base64"))?;
                self.ended = self.quartet.contains(&b'=');
                self.inner.write_all(&decoded[..count])?;
                self.quartet.clear();
            }
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[derive(Default)]
struct Number {
    value: u64,
    digits: u32,
    trailing: i128,
    overflow: bool,
}
impl Number {
    fn digit(&mut self, digit: u8) {
        if digit == 0 {
            if self.digits > 0 {
                self.trailing += 1;
            }
            return;
        }
        if i128::from(self.digits) + self.trailing + 1 > 10 {
            self.overflow = true;
            return;
        }
        self.value = self.value * 10u64.pow(self.trailing as u32 + 1) + u64::from(digit);
        self.digits += self.trailing as u32 + 1;
        self.trailing = 0;
    }
}

/// One payload reread from its exact immutable string range. At most one UTF-8 scalar and
/// one base64 quartet are retained; the artifact owner receives a normal pull-based stream.
pub struct PayloadReader<'a> {
    parser: Parser<'a>,
    payload: Payload,
    buffer: [u8; 4],
    begin: usize,
    end: usize,
    quartet: [u8; 4],
    quartet_len: usize,
    padded: bool,
    finished: bool,
    bytes: u64,
}
impl<'a> PayloadReader<'a> {
    pub fn new(
        source: &'a mut dyn ReadSeek,
        payload: Payload,
        stop: &'a (dyn Fn() -> bool + Sync),
    ) -> Result<Self, Error> {
        let checkpoint = Checkpoint {
            offset: payload.start,
            ..Default::default()
        };
        let mut parser = Parser::new(source, &checkpoint, payload.byte_length, stop)?;
        parser.expect(b'"')?;
        Ok(Self {
            parser,
            payload,
            buffer: [0; 4],
            begin: 0,
            end: 0,
            quartet: [0; 4],
            quartet_len: 0,
            padded: false,
            finished: false,
            bytes: 0,
        })
    }
    fn fill(&mut self) -> Result<(), Error> {
        loop {
            let Some((bytes, len)) = self.parser.scalar()? else {
                if self.quartet_len != 0
                    || self.bytes != self.payload.byte_length
                    || self.parser.offset != self.payload.end
                {
                    return Err(Error::Invalid("payload locator/length changed"));
                }
                self.finished = true;
                return Ok(());
            };
            if self.payload.base64 {
                if len != 1 || !bytes[0].is_ascii() || self.padded {
                    return Err(Error::Invalid("invalid base64 data"));
                }
                self.quartet[self.quartet_len] = bytes[0];
                self.quartet_len += 1;
                if self.quartet_len < 4 {
                    continue;
                }
                self.end = base64::engine::general_purpose::STANDARD
                    .decode_slice(self.quartet, &mut self.buffer[..3])
                    .map_err(|_| Error::Invalid("invalid base64 quartet"))?;
                self.padded = self.quartet.contains(&b'=');
                self.quartet_len = 0;
            } else {
                self.buffer = bytes;
                self.end = len;
            }
            self.begin = 0;
            self.bytes = self
                .bytes
                .checked_add(self.end as u64)
                .filter(|n| *n <= self.payload.byte_length)
                .ok_or(Error::Invalid("payload length changed"))?;
            return Ok(());
        }
    }
}
impl Read for PayloadReader<'_> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let mut written = 0;
        while written < output.len() {
            if self.begin == self.end {
                if self.finished {
                    break;
                }
                self.fill()
                    .map_err(|error| io::Error::other(error.to_string()))?;
                if self.finished {
                    break;
                }
            }
            let count = (self.end - self.begin).min(output.len() - written);
            output[written..written + count]
                .copy_from_slice(&self.buffer[self.begin..self.begin + count]);
            self.begin += count;
            written += count;
        }
        Ok(written)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn body(bytes: &[u8]) -> serde_json::Value {
        json!({"text":String::from_utf8(bytes.to_vec()).unwrap(),"byteLength":bytes.len().to_string(),"sha256":crate::fingerprint::sha256_hex(bytes)})
    }
    fn manifest() -> serde_json::Value {
        json!({"format":"devworks-convention","version":1,"slug":"slug\0x","title":"title\0x","parentPath":[],"sortOrder":-2147483648i64,"lifecycle":"Retired","applicability":["one"],"resources":[],"markdown":body("a\0🙂\n".as_bytes())})
    }
    #[test]
    fn arbitrary_field_order_escaped_unicode_and_resumable_payload_ranges() {
        let mut value = manifest();
        value["rawContent"] = body(b"raw\0");
        value["resources"] = json!([{"base64":"AAECAw==","byteLength":"4","sha256":crate::fingerprint::sha256_hex(&[0,1,2,3]),"key":"foreign-key","name":"resource.bin","mediaType":"","lifecycle":"Active"}]);
        let mut source = io::Cursor::new(serde_json::to_vec(&value).unwrap());
        let stop = || false;
        let mut checkpoint = Checkpoint::default();
        let mut seen = 0;
        loop {
            let step = Parser::new(&mut source, &checkpoint, 100, &stop)
                .unwrap()
                .next(checkpoint.clone())
                .unwrap();
            match step {
                Step::Entry {
                    entry,
                    checkpoint: next,
                } => {
                    let mut decoded = Vec::new();
                    PayloadReader::new(&mut source, entry.payload.clone(), &stop)
                        .unwrap()
                        .read_to_end(&mut decoded)
                        .unwrap();
                    assert_eq!(decoded.len() as u64, entry.payload.byte_length);
                    assert_eq!(
                        crate::fingerprint::sha256_hex(&decoded),
                        entry.payload.sha256
                    );
                    checkpoint =
                        serde_json::from_value(serde_json::to_value(next).unwrap()).unwrap();
                    seen += 1;
                }
                Step::Complete {
                    metadata,
                    resources,
                } => {
                    assert_eq!(seen, 3);
                    assert_eq!(resources, 1);
                    assert_eq!(metadata.title.as_deref(), Some("title\0x"));
                    break;
                }
            }
        }
    }
    #[test]
    fn rejects_unknown_duplicate_fields_bad_hash_invalid_unicode_and_cancel() {
        let stop = || false;
        let text = serde_json::to_string(&manifest()).unwrap();
        for invalid in [
            text.replacen("{", "{\"unknown\":1,", 1),
            text.replacen("{", "{\"format\":\"devworks-convention\",", 1),
            text.replace("\\u0000", "\\ud800"),
            text.replace(
                &crate::fingerprint::sha256_hex("a\0🙂\n".as_bytes()),
                &"f".repeat(64),
            ),
        ] {
            let mut input = io::Cursor::new(invalid);
            let mut checkpoint = Checkpoint::default();
            let mut failed = false;
            for _ in 0..4 {
                match Parser::new(&mut input, &checkpoint, 100, &stop)
                    .unwrap()
                    .next(checkpoint.clone())
                {
                    Err(_) => {
                        failed = true;
                        break;
                    }
                    Ok(Step::Entry {
                        checkpoint: next, ..
                    }) => checkpoint = next,
                    Ok(Step::Complete { .. }) => break,
                }
            }
            assert!(failed);
        }
        let mut source = io::Cursor::new(text);
        let stopped = || true;
        assert!(matches!(
            Parser::new(&mut source, &Checkpoint::default(), 100, &stopped)
                .unwrap()
                .next(Checkpoint::default()),
            Err(Error::Stopped)
        ));
    }
    #[test]
    fn maximum_escaped_documents_use_bounded_seek_reads_and_checkpoint_state() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        struct Remove(std::path::PathBuf);
        impl Drop for Remove {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
        struct Measured {
            file: std::fs::File,
            maximum: Arc<AtomicUsize>,
        }
        impl Read for Measured {
            fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
                self.maximum.fetch_max(out.len(), Ordering::Relaxed);
                self.file.read(out)
            }
        }
        impl Seek for Measured {
            fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
                self.file.seek(from)
            }
        }
        let bytes = super::super::super::model::MAX_MARKDOWN_BYTES;
        let mut hash = Sha256::new();
        let zeros = [0u8; 65536];
        let mut remaining = bytes;
        while remaining > 0 {
            let n = remaining.min(zeros.len() as u64) as usize;
            hash.update(&zeros[..n]);
            remaining -= n as u64;
        }
        let digest = hash
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let path = std::env::temp_dir().join(format!(
            "devworks-portable-documents-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let cleanup = Remove(path.clone());
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        file.write_all(br#"{"format":"devworks-convention","version":1,"slug":"","title":"Exact NUL documents","parentPath":[],"sortOrder":0,"lifecycle":"Active","applicability":[],"resources":[],"#).unwrap();
        let escaped = r"\u0000".repeat(4096);
        for (i, key) in ["markdown", "rawContent"].iter().enumerate() {
            if i > 0 {
                file.write_all(b",").unwrap();
            }
            write!(
                file,
                "\"{key}\":{{\"sha256\":\"{digest}\",\"byteLength\":\"{bytes}\",\"text\":\""
            )
            .unwrap();
            let mut remaining = bytes;
            while remaining > 0 {
                let n = remaining.min(4096) as usize;
                file.write_all(&escaped.as_bytes()[..n * 6]).unwrap();
                remaining -= n as u64;
            }
            file.write_all(b"\"}").unwrap();
        }
        file.write_all(b"}").unwrap();
        drop(file);
        assert!(std::fs::metadata(&path).unwrap().len() > 192_000_000);
        let maximum = Arc::new(AtomicUsize::new(0));
        let mut source = Measured {
            file: std::fs::File::open(&path).unwrap(),
            maximum: maximum.clone(),
        };
        let mut checkpoint = Checkpoint::default();
        let mut documents = 0;
        loop {
            match Parser::new(&mut source, &checkpoint, bytes, &|| false)
                .unwrap()
                .next(checkpoint.clone())
                .unwrap()
            {
                Step::Entry {
                    entry,
                    checkpoint: next,
                } => {
                    assert_eq!(entry.payload.byte_length, bytes);
                    assert_eq!(entry.payload.sha256, digest);
                    assert!(serde_json::to_vec(&next).unwrap().len() < 4096);
                    let mut decoded =
                        PayloadReader::new(&mut source, entry.payload, &|| false).unwrap();
                    assert_eq!(io::copy(&mut decoded, &mut io::sink()).unwrap(), bytes);
                    documents += 1;
                    checkpoint = next;
                }
                Step::Complete { resources, .. } => {
                    assert_eq!(resources, 0);
                    break;
                }
            }
        }
        assert_eq!(documents, 2);
        assert!(maximum.load(Ordering::Relaxed) <= 65536);
        drop(cleanup);
    }
    #[test]
    fn preserves_source_slug_components_but_refuses_blank_authored_title_and_markdown() {
        let mut value = manifest();
        value["slug"] = json!("");
        value["parentPath"] = json!(["", " ", "\0"]);
        value["rawContent"] = body(b"");
        let parse = |value: &serde_json::Value| -> Result<(), Error> {
            let mut reader = io::Cursor::new(serde_json::to_vec(value).unwrap());
            let mut checkpoint = Checkpoint::default();
            loop {
                match Parser::new(&mut reader, &checkpoint, 100, &|| false)?
                    .next(checkpoint.clone())?
                {
                    Step::Entry {
                        checkpoint: next, ..
                    } => checkpoint = next,
                    Step::Complete { metadata, .. } => {
                        assert_eq!(metadata.slug.as_deref(), Some(""));
                        return Ok(());
                    }
                }
            }
        };
        parse(&value).unwrap();
        value["title"] = json!(" \u{85}\u{2003}");
        assert!(parse(&value).is_err());
        value["title"] = json!("\0");
        value["markdown"] = body(" \u{85}\u{2003}".as_bytes());
        assert!(parse(&value).is_err());
        value["markdown"] = body(b"\0");
        parse(&value).unwrap();
    }
    #[test]
    fn numeric_json_integer_forms_are_exact_without_float_or_token_buffers() {
        let stop = || false;
        for (text, expected) in [
            ("1.0", 1),
            ("0.0001e4", 1),
            ("123000000000000000e-15", 123),
            ("-0e99999999999999999999", 0),
            ("2147483647", i32::MAX),
            ("-2147483648", i32::MIN),
        ] {
            let mut input = io::Cursor::new(text);
            let mut parser = Parser::new(&mut input, &Checkpoint::default(), 100, &stop).unwrap();
            assert_eq!(parser.integer().unwrap(), expected);
        }
        for text in [
            "2147483648",
            "1.1",
            "1e-1",
            "01",
            "-01",
            "1e100000000000000000000",
            "1e",
        ] {
            let mut input = io::Cursor::new(text);
            assert!(
                Parser::new(&mut input, &Checkpoint::default(), 100, &stop)
                    .unwrap()
                    .integer()
                    .is_err(),
                "{text}"
            );
        }
    }
}
