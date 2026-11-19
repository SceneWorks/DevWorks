//! Redacted, bounded process output and the live tail.
use super::*;

/// Secret values can straddle a capture page or budget boundary. Remove any retained suffix
/// matching the beginning of a secret before publishing an incomplete output prefix.
pub(crate) fn redacted_output(bytes: &[u8], values: &[String], incomplete: bool) -> Vec<u8> {
    if values.is_empty() {
        return bytes.to_vec();
    }
    // The shared redactor intentionally ignores values shorter than four bytes. A process
    // credential has no such length guarantee, so suppress the body in that case.
    if values
        .iter()
        .any(|value| !value.is_empty() && value.len() < 4)
    {
        return Vec::new();
    }
    let mut redactor = Redactor::new();
    for value in values {
        redactor.register(value);
    }
    let mut remaining = bytes;
    let mut redacted = Vec::with_capacity(remaining.len());
    while !remaining.is_empty() {
        match std::str::from_utf8(remaining) {
            Ok(text) => {
                redacted.extend_from_slice(redactor.redact_text(text).as_bytes());
                break;
            }
            Err(error) => {
                let valid = error.valid_up_to();
                if valid > 0 {
                    // A registered credential is valid UTF-8, so its bytes cannot straddle an
                    // invalid byte. Redact each valid span without changing unrelated binary.
                    let text = std::str::from_utf8(&remaining[..valid]).expect("valid prefix");
                    redacted.extend_from_slice(redactor.redact_text(text).as_bytes());
                }
                let invalid = error.error_len().unwrap_or(remaining.len() - valid);
                redacted.extend_from_slice(&remaining[valid..valid + invalid]);
                remaining = &remaining[valid + invalid..];
            }
        }
    }
    // Redact complete matches first. Trimming a repeated secret's suffix beforehand can
    // split its full match and expose the retained prefix.
    if incomplete {
        let mut trim = 0;
        for value in values {
            let secret = value.as_bytes();
            for length in 1..secret.len() {
                if redacted.ends_with(&secret[..length]) {
                    trim = trim.max(length);
                }
            }
        }
        redacted.truncate(redacted.len() - trim);
    }
    redacted
}

/// A capture budget applies to the combined stdout and stderr stream. Readers continue draining
/// after the budget is reached so a child cannot remain blocked on a full pipe; `seen` records
/// the discarded tail without retaining it. Only the retained prefix may be paged to callers.
#[derive(Clone, Debug)]
pub struct BoundedOutput {
    pub(super) bytes: Vec<u8>,
    pub(super) seen: u64,
    pub(super) start: u64,
    pub(super) limit: usize,
}

/// The last `limit` bytes of a combined output stream (a watcher reports the end of a run).
#[derive(Clone, Debug)]
pub struct TailOutput {
    pub(super) bytes: std::collections::VecDeque<u8>,
    pub(super) seen: u64,
    pub(super) limit: usize,
}

impl TailOutput {
    pub fn new(limit: usize) -> Self {
        Self {
            bytes: std::collections::VecDeque::with_capacity(limit),
            seen: 0,
            limit,
        }
    }
    pub fn append(&mut self, chunk: &[u8]) {
        self.seen = self.seen.saturating_add(chunk.len() as u64);
        let keep = &chunk[chunk.len().saturating_sub(self.limit)..];
        let overflow = (self.bytes.len() + keep.len()).saturating_sub(self.limit);
        self.bytes.drain(..overflow.min(self.bytes.len()));
        self.bytes.extend(keep);
    }
    /// Earlier bytes were discarded.
    pub fn truncated(&self) -> bool {
        self.seen > self.bytes.len() as u64
    }
    pub fn retained(&self) -> Vec<u8> {
        self.bytes.iter().copied().collect()
    }
}

/// Redacts a tail whose front may be cut: a credential's suffix at the start of the retained
/// bytes is withheld along with complete matches. While the process still runs (`open_end`) a
/// credential's prefix at the end is withheld too, since its remainder may not be written yet.
pub(crate) fn redacted_tail(
    bytes: &[u8],
    values: &[String],
    cut_front: bool,
    open_end: bool,
) -> Vec<u8> {
    let mut redacted = redacted_output(bytes, values, open_end);
    if cut_front
        && !values
            .iter()
            .any(|value| !value.is_empty() && value.len() < 4)
    {
        let mut trim = 0;
        for value in values {
            let secret = value.as_bytes();
            for length in 1..secret.len() {
                if redacted.starts_with(&secret[secret.len() - length..]) {
                    trim = trim.max(length);
                }
            }
        }
        redacted.drain(..trim);
    }
    redacted
}

/// Counts a regular expression's matches over a stream, line by line (a line split across reads
/// is counted once complete; the last unterminated line at the end).
pub(crate) struct MatchCounter {
    pub(super) regex: regex::Regex,
    pub(super) partial: Vec<u8>,
    pub(super) count: i64,
}

impl MatchCounter {
    pub(crate) fn new(pattern: &str) -> Option<Self> {
        regex::Regex::new(pattern).ok().map(|regex| Self {
            regex,
            partial: Vec::new(),
            count: 0,
        })
    }
    pub(super) fn count_line(&mut self, line: &[u8]) {
        let text = String::from_utf8_lossy(line);
        self.count += self.regex.find_iter(&text).count() as i64;
    }
    pub(crate) fn feed(&mut self, chunk: &[u8]) {
        let mut rest = chunk;
        while let Some(end) = rest.iter().position(|b| *b == b'\n') {
            let (line, after) = rest.split_at(end);
            if self.partial.is_empty() {
                self.count_line(line);
            } else {
                self.partial.extend_from_slice(line);
                let whole = std::mem::take(&mut self.partial);
                self.count_line(&whole);
            }
            rest = &after[1..];
        }
        // A runaway line is counted in pieces rather than held without bound.
        self.partial.extend_from_slice(rest);
        if self.partial.len() > 65_536 {
            let whole = std::mem::take(&mut self.partial);
            self.count_line(&whole);
        }
    }
    pub(crate) fn finish(&mut self) {
        if !self.partial.is_empty() {
            let whole = std::mem::take(&mut self.partial);
            self.count_line(&whole);
        }
    }
    pub(crate) fn count(&self) -> i64 {
        self.count
    }
}

/// A watched launch's report to the effect caller, made from the monitor thread: `None` once
/// the process is launched, or the outcome when it never launched. Which of the two was sent,
/// not whether the caller still listens, decides who records the outcome.
pub(crate) struct LaunchReport {
    pub(super) to_caller: std::sync::mpsc::Sender<Option<EffectOutcome>>,
    pub(super) launched: std::cell::Cell<bool>,
}

impl LaunchReport {
    pub(super) fn new(to_caller: std::sync::mpsc::Sender<Option<EffectOutcome>>) -> Self {
        Self {
            to_caller,
            launched: std::cell::Cell::new(false),
        }
    }

    pub(super) fn launched(&self) {
        self.launched.set(true);
        let _ = self.to_caller.send(None);
    }

    /// Hands `outcome` to the caller as the effect result when the launch was never reported.
    /// Answers it back for the monitor to record when the launch was reported, or when the
    /// caller is gone.
    pub(super) fn finish(&self, outcome: EffectOutcome) -> Option<EffectOutcome> {
        if self.launched.get() {
            return Some(outcome);
        }
        self.to_caller
            .send(Some(outcome))
            .err()
            .and_then(|unsent| unsent.0)
    }
}

pub(super) type LiveTail = (Arc<Mutex<TailOutput>>, Arc<Vec<String>>);

pub(super) struct LiveOutputGuard<'a> {
    pub(super) service: &'a ProcessToolService,
    pub(super) id: Id,
}

impl Drop for LiveOutputGuard<'_> {
    fn drop(&mut self) {
        self.service.live_output.locked().remove(self.id.as_str());
        self.service.live_matches.locked().remove(self.id.as_str());
        self.service.live_tails.locked().remove(self.id.as_str());
    }
}

impl BoundedOutput {
    pub fn new(limit: usize) -> Self {
        Self::from_offset(limit, 0)
    }

    pub fn from_offset(limit: usize, start: u64) -> Self {
        Self {
            bytes: Vec::new(),
            seen: 0,
            start,
            limit,
        }
    }

    pub fn append(&mut self, chunk: &[u8]) {
        let before = self.seen;
        self.seen = self.seen.saturating_add(chunk.len() as u64);
        let skip = self.start.saturating_sub(before).min(chunk.len() as u64) as usize;
        let keep = self
            .limit
            .saturating_sub(self.bytes.len())
            .min(chunk.len() - skip);
        self.bytes.extend_from_slice(&chunk[skip..skip + keep]);
    }

    pub fn truncated(&self) -> bool {
        self.seen.saturating_sub(self.start) > self.bytes.len() as u64
    }

    pub fn seen(&self) -> u64 {
        self.seen
    }

    pub fn page(&self, after: u64, max: usize) -> (u64, &[u8], u64) {
        let first = after.min(self.bytes.len() as u64) as usize;
        let end = first.saturating_add(max).min(self.bytes.len());
        (first as u64, &self.bytes[first..end], end as u64)
    }

    pub fn retained(&self) -> &[u8] {
        &self.bytes
    }
}
