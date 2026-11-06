//! Bounded UTF-8 source projection and full Unicode lowercase substring matching.
use crate::ports::storage::*;
use sha2::{Digest as _, Sha256};
use std::{
    collections::{HashMap, VecDeque},
    io::Read,
    sync::LazyLock,
};
const CHUNK: usize = 16 * 1024;
fn invalid(_message: &str) -> StorageError {
    StorageError::ViewUnavailable
}

/// UTF-8 decoding retains only an incomplete scalar (at most three bytes) between reads.
pub(super) fn visit(
    reader: &mut dyn Read,
    stopped: &mut dyn FnMut() -> bool,
    mut text: impl FnMut(&str) -> Result<bool, StorageError>,
) -> Result<(u64, bool), StorageError> {
    let mut buffer = [0u8; CHUNK + 4];
    let mut carry = 0usize;
    let mut bytes = 0u64;
    loop {
        if stopped() {
            return Err(StorageError::Cancelled);
        }
        let count = match reader.read(&mut buffer[carry..carry + CHUNK]) {
            Ok(count) => count,
            Err(_) if stopped() => return Err(StorageError::Cancelled),
            Err(_) => {
                return Err(StorageError::unavailable(
                    Unavailable::Io,
                    "Help source read failed",
                ));
            }
        };
        if count == 0 {
            if carry != 0 {
                return Err(invalid("incomplete Help UTF-8"));
            }
            return Ok((bytes, false));
        }
        bytes = bytes
            .checked_add(count as u64)
            .ok_or_else(|| invalid("Help source length overflow"))?;
        if bytes > crate::ports::packages::MAX_BUNDLE_BYTES {
            return Err(invalid("Help source exceeds package bound"));
        }
        let end = carry + count;
        let valid = match std::str::from_utf8(&buffer[..end]) {
            Ok(value) => value,
            Err(error) if error.error_len().is_none() => {
                std::str::from_utf8(&buffer[..error.valid_up_to()]).expect("valid prefix")
            }
            Err(_) => return Err(invalid("Help source is not UTF-8")),
        };
        if valid.contains('\0') {
            return Err(invalid("Help source contains NUL"));
        }
        if text(valid)? {
            return Ok((bytes, true));
        }
        let consumed = valid.len();
        carry = end - consumed;
        buffer.copy_within(consumed..end, 0);
    }
}
struct Hashed<'a> {
    reader: &'a mut dyn Read,
    hash: Sha256,
}
impl Read for Hashed<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        let n = self.reader.read(bytes)?;
        self.hash.update(&bytes[..n]);
        Ok(n)
    }
}
pub(super) fn index(
    reader: &mut dyn Read,
    expected_digest: &str,
    expected_bytes: u64,
    stopped: &mut dyn FnMut() -> bool,
) -> Result<StoredHelpContent, StorageError> {
    let mut source = Hashed {
        reader,
        hash: Sha256::new(),
    };
    let mut prefix = String::with_capacity(MAX_HELP_INDEX_BYTES);
    let mut offsets = vec![HelpOffset {
        byte_offset: 0,
        character_offset: 0,
    }];
    let mut character_count = 0i64;
    let mut offset = 0u64;
    let mut collecting = true;
    let (bytes, _) = visit(&mut source, stopped, |text| {
        if collecting {
            let mut count = text.len().min(MAX_HELP_INDEX_BYTES - prefix.len());
            while !text.is_char_boundary(count) {
                count -= 1;
            }
            prefix.push_str(&text[..count]);
            if count < text.len() {
                collecting = false;
            }
        }
        let mut remaining = text;
        while !remaining.is_empty() {
            let until = HELP_CHECKPOINT_BYTES
                - (offset - offsets.last().expect("initial checkpoint").byte_offset);
            let mut count = remaining.len().min(until as usize);
            while !remaining.is_char_boundary(count) {
                count += 1;
            }
            character_count += remaining[..count].chars().count() as i64;
            offset += count as u64;
            remaining = &remaining[count..];
            if offset - offsets.last().expect("initial checkpoint").byte_offset
                >= HELP_CHECKPOINT_BYTES
            {
                offsets.push(HelpOffset {
                    byte_offset: offset,
                    character_offset: character_count,
                });
            }
        }
        Ok(false)
    })?;
    let actual = source
        .hash
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    if bytes != expected_bytes || actual != expected_digest {
        return Err(invalid("Help source differs from verified identity"));
    }
    if offsets.last().is_some_and(|o| o.byte_offset != bytes) {
        offsets.push(HelpOffset {
            byte_offset: bytes,
            character_offset: character_count,
        });
    }
    let indexed_bytes = prefix.len() as u64;
    // Bound the input BEFORE case conversion: no full-guide formatted/lowercase temporary.
    let mut search_text = prefix.to_lowercase();
    let complete = bytes == indexed_bytes && search_text.len() <= MAX_HELP_INDEX_BYTES;
    let mut end = search_text.len().min(MAX_HELP_INDEX_BYTES);
    while !search_text.is_char_boundary(end) {
        end -= 1;
    }
    search_text.truncate(end);
    Ok(StoredHelpContent {
        digest: expected_digest.into(),
        byte_count: bytes,
        character_count,
        indexed_bytes,
        index_complete: complete,
        search_text,
        offsets,
    })
}

#[derive(Clone, Copy)]
struct Class {
    cased: bool,
    ignorable: bool,
}
fn classify(c: char) -> Class {
    // Ask this Rust version's own lowercase implementation for its contextual properties.
    // This avoids a second Unicode-version table and handles cased+ignorable scalars too.
    let before = format!("AΣ{c}A").to_lowercase();
    let after = format!(" {c}Σ ").to_lowercase();
    Class {
        cased: c.is_uppercase()
            || c.is_lowercase()
            || c.to_uppercase().ne(std::iter::once(c))
            || c.to_lowercase().ne(std::iter::once(c)),
        ignorable: before.chars().nth(1) == Some('σ') && after.ends_with("σ "),
    }
}
static ASCII: LazyLock<[Class; 128]> =
    LazyLock::new(|| std::array::from_fn(|n| classify(char::from(n as u8))));
#[derive(Clone, Copy, Default)]
struct Match {
    prefix: usize,
    found: bool,
}
struct Search {
    pattern: Vec<char>,
    failure: Vec<usize>,
    state: Match,
    pending: Option<(Match, Match)>,
    previous_cased: bool,
    classes: HashMap<char, Class>,
    order: VecDeque<char>,
}
impl Search {
    fn new(query: &str) -> Self {
        let pattern = query.chars().collect::<Vec<_>>();
        let mut failure = vec![0; pattern.len()];
        for n in 1..pattern.len() {
            let mut previous = failure[n - 1];
            while previous > 0 && pattern[n] != pattern[previous] {
                previous = failure[previous - 1];
            }
            if pattern[n] == pattern[previous] {
                previous += 1;
            }
            failure[n] = previous;
        }
        Self {
            pattern,
            failure,
            state: Match::default(),
            pending: None,
            previous_cased: false,
            classes: HashMap::with_capacity(256),
            order: VecDeque::with_capacity(256),
        }
    }
    fn class(&mut self, c: char) -> Class {
        if c.is_ascii() {
            return ASCII[c as usize];
        }
        if let Some(value) = self.classes.get(&c) {
            return *value;
        }
        let value = classify(c);
        if self.order.len() == 256 {
            self.classes
                .remove(&self.order.pop_front().expect("full cache"));
        }
        self.order.push_back(c);
        self.classes.insert(c, value);
        value
    }
    fn feed(&self, mut state: Match, c: char) -> Match {
        if state.found || self.pattern.is_empty() {
            state.found = true;
            return state;
        }
        while state.prefix > 0 && self.pattern[state.prefix] != c {
            state.prefix = self.failure[state.prefix - 1];
        }
        if self.pattern[state.prefix] == c {
            state.prefix += 1;
        }
        if state.prefix == self.pattern.len() {
            state.found = true;
            state.prefix = self.failure[state.prefix - 1];
        }
        state
    }
    fn push(&mut self, c: char) -> bool {
        let class = self.class(c);
        if let Some((normal, final_sigma)) = self.pending.take() {
            if class.ignorable {
                let mut normal = normal;
                let mut final_sigma = final_sigma;
                for lower in c.to_lowercase() {
                    normal = self.feed(normal, lower);
                    final_sigma = self.feed(final_sigma, lower);
                }
                self.pending = Some((normal, final_sigma));
                return normal.found && final_sigma.found;
            }
            self.state = if class.cased { normal } else { final_sigma };
        }
        if self.state.found {
            return true;
        }
        if c == 'Σ' && self.previous_cased {
            self.pending = Some((self.feed(self.state, 'σ'), self.feed(self.state, 'ς')));
        } else {
            for lower in c.to_lowercase() {
                self.state = self.feed(self.state, lower);
            }
        }
        if !class.ignorable {
            self.previous_cased = class.cased;
        }
        self.state.found
    }
    fn text(&mut self, text: &str) -> bool {
        if self.pending.is_none()
            && self.state.prefix == 0
            && !self.state.found
            && text.is_ascii()
            && self.pattern.first().is_some_and(|first| {
                !first.is_ascii()
                    || (!text
                        .as_bytes()
                        .contains(&(*first as u8).to_ascii_lowercase())
                        && !text
                            .as_bytes()
                            .contains(&(*first as u8).to_ascii_uppercase()))
            })
        {
            if let Some(last) = text.bytes().rev().find(|b| !ASCII[*b as usize].ignorable) {
                self.previous_cased = ASCII[last as usize].cased;
            }
            return false;
        }
        text.chars().any(|c| self.push(c))
    }
    fn finish(&mut self) -> bool {
        if let Some((_, final_sigma)) = self.pending.take() {
            self.state = final_sigma;
        }
        self.state.found || self.pattern.is_empty()
    }
}
pub(super) fn contains(
    title: &str,
    reader: &mut dyn Read,
    query: &str,
    expected_bytes: u64,
    stopped: &mut dyn FnMut() -> bool,
) -> Result<bool, StorageError> {
    let mut search = Search::new(query);
    for c in title.chars().chain(std::iter::once('\n')) {
        if search.push(c) {
            return Ok(true);
        }
    }
    let (bytes, found) = visit(reader, stopped, |text| Ok(search.text(text)))?;
    if !found && bytes != expected_bytes {
        return Err(StorageError::ViewUnavailable);
    }
    Ok(found || search.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Pieces<'a> {
        body: &'a [u8],
        step: usize,
    }
    impl Read for Pieces<'_> {
        fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
            let n = self.step.min(bytes.len()).min(self.body.len());
            bytes[..n].copy_from_slice(&self.body[..n]);
            self.body = &self.body[n..];
            Ok(n)
        }
    }
    #[test]
    fn streaming_unicode_matches_whole_string_lowercase_at_every_byte_split() {
        let body = "AΣ\u{0345}\u{0301} AΣ. AΣ 中 AΣ\u{200d}A İ🦀 É漢字 ΣΣΣ ΟΣ:Α ΟΣ.. ";
        let whole = format!("Title\n{body}").to_lowercase();
        for query in [
            "title\naσ",
            "aς.",
            "aσ\u{0345}",
            "aς\u{0345}",
            "i\u{307}🦀",
            "é漢字",
            "σς",
            "ος:α",
            "οσ:α",
            "ος.. ",
            "not present",
        ] {
            for step in 1..=body.len() {
                let mut source = Pieces {
                    body: body.as_bytes(),
                    step,
                };
                assert_eq!(
                    contains("Title", &mut source, query, body.len() as u64, &mut || {
                        false
                    })
                    .unwrap(),
                    whole.contains(query),
                    "query={query:?}, step={step}"
                );
            }
        }
    }
    #[test]
    fn incomplete_or_cancelled_full_search_never_reports_a_successful_negative() {
        let body = vec![b'x'; CHUNK * 3];
        let mut calls = 0;
        assert!(matches!(
            contains(
                "",
                &mut body.as_slice(),
                "absent",
                body.len() as u64,
                &mut || {
                    calls += 1;
                    calls == 2
                }
            ),
            Err(StorageError::Cancelled)
        ));
        assert_eq!(calls, 2);
        assert!(matches!(
            contains(
                "",
                &mut &body[..CHUNK],
                "absent",
                body.len() as u64,
                &mut || false
            ),
            Err(StorageError::ViewUnavailable)
        ));
        assert!(
            !contains(
                "",
                &mut body.as_slice(),
                "absent",
                body.len() as u64,
                &mut || false
            )
            .unwrap()
        );
    }
    #[test]
    fn arbitrarily_long_ignorable_run_keeps_bounded_state_and_exact_final_sigma() {
        let body = format!("AΣ{} ", "\u{0345}".repeat(100_000));
        for query in ["aς\u{0345}", "aσ\u{0345}", "\u{0345} "] {
            let mut source = Pieces {
                body: body.as_bytes(),
                step: 7,
            };
            assert_eq!(
                contains("", &mut source, query, body.len() as u64, &mut || false).unwrap(),
                body.to_lowercase().contains(query)
            );
        }
    }
}
