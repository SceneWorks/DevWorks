//! Byte-incremental SSE framing only. JSON, completion markers and provider errors belong to
//! the profile decoder. EOF never releases an event without its terminating blank line.
use std::fmt;

#[derive(Clone, Copy, Debug)]
pub struct SseLimits {
    pub max_line_bytes: usize,
    pub max_event_bytes: usize,
    pub max_stream_bytes: usize,
    pub max_events: usize,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SseError {
    InvalidLimits,
    InvalidUtf8,
    TooLarge,
    Finished,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum End {
    Complete,
    Incomplete,
}
#[derive(Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
    pub id: Option<String>,
    pub retry_ms: Option<u64>,
}
impl fmt::Debug for SseEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SseEvent")
            .field("data_bytes", &self.data.len())
            .field("has_event", &self.event.is_some())
            .field("has_id", &self.id.is_some())
            .field("retry_ms", &self.retry_ms)
            .finish()
    }
}
pub struct SseDecoder {
    limits: SseLimits,
    line: Vec<u8>,
    first_line: bool,
    skip_lf: bool,
    data: String,
    has_data: bool,
    event: Option<String>,
    id: Option<String>,
    retry_ms: Option<u64>,
    event_bytes: usize,
    stream_bytes: usize,
    output_bytes: usize,
    events: usize,
    finished: bool,
}
impl SseDecoder {
    pub fn new(limits: SseLimits) -> Result<Self, SseError> {
        if limits.max_line_bytes == 0
            || limits.max_event_bytes == 0
            || limits.max_stream_bytes == 0
            || limits.max_events == 0
        {
            return Err(SseError::InvalidLimits);
        }
        Ok(Self {
            limits,
            line: Vec::new(),
            first_line: true,
            skip_lf: false,
            data: String::new(),
            has_data: false,
            event: None,
            id: None,
            retry_ms: None,
            event_bytes: 0,
            stream_bytes: 0,
            output_bytes: 0,
            events: 0,
            finished: false,
        })
    }
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<SseEvent>, SseError> {
        if self.finished {
            return Err(SseError::Finished);
        }
        let result = self.push_inner(bytes);
        if result.is_err() {
            self.finished = true;
        }
        result
    }
    fn push_inner(&mut self, bytes: &[u8]) -> Result<Vec<SseEvent>, SseError> {
        self.stream_bytes = self
            .stream_bytes
            .checked_add(bytes.len())
            .ok_or(SseError::TooLarge)?;
        if self.stream_bytes > self.limits.max_stream_bytes {
            return Err(SseError::TooLarge);
        }
        let mut output = Vec::new();
        for &byte in bytes {
            if self.skip_lf {
                self.skip_lf = false;
                if byte == b'\n' {
                    continue;
                }
            }
            if byte == b'\r' || byte == b'\n' {
                self.finish_line(&mut output)?;
                self.skip_lf = byte == b'\r';
            } else {
                if self.line.len() == self.limits.max_line_bytes {
                    return Err(SseError::TooLarge);
                }
                self.line.push(byte);
            }
        }
        Ok(output)
    }
    fn finish_line(&mut self, output: &mut Vec<SseEvent>) -> Result<(), SseError> {
        let line = std::mem::take(&mut self.line);
        let line = std::str::from_utf8(&line).map_err(|_| SseError::InvalidUtf8)?;
        let line = if self.first_line {
            self.first_line = false;
            line.strip_prefix('\u{feff}').unwrap_or(line)
        } else {
            line
        };
        if line.is_empty() {
            if self.has_data {
                self.events = self.events.checked_add(1).ok_or(SseError::TooLarge)?;
                let size = self.data.len()
                    + self.event.as_ref().map_or(0, String::len)
                    + self.id.as_ref().map_or(0, String::len);
                self.output_bytes = self
                    .output_bytes
                    .checked_add(size)
                    .ok_or(SseError::TooLarge)?;
                // Bound retained-ID amplification as well as incoming bytes.
                if self.events > self.limits.max_events
                    || self.output_bytes > self.limits.max_stream_bytes
                {
                    return Err(SseError::TooLarge);
                }
                self.data.pop(); // Every data line appended exactly one newline.
                output.push(SseEvent {
                    event: self.event.take(),
                    data: std::mem::take(&mut self.data),
                    id: self.id.clone(),
                    retry_ms: self.retry_ms,
                });
            }
            self.data.clear();
            self.has_data = false;
            self.event = None;
            self.event_bytes = 0;
            return Ok(());
        }
        self.event_bytes = self
            .event_bytes
            .checked_add(line.len() + 1)
            .ok_or(SseError::TooLarge)?;
        if self.event_bytes > self.limits.max_event_bytes {
            return Err(SseError::TooLarge);
        }
        if line.starts_with(':') {
            return Ok(());
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "data" => {
                self.has_data = true;
                self.data.push_str(value);
                self.data.push('\n');
            }
            "event" => self.event = (!value.is_empty()).then(|| value.to_owned()),
            "id" if !value.contains('\0') => self.id = Some(value.to_owned()),
            "retry" if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) => {
                if let Ok(value) = value.parse() {
                    self.retry_ms = Some(value);
                }
            }
            _ => {}
        }
        Ok(())
    }
    pub fn finish(&mut self) -> Result<End, SseError> {
        if self.finished {
            return Err(SseError::Finished);
        }
        self.finished = true;
        std::str::from_utf8(&self.line).map_err(|_| SseError::InvalidUtf8)?;
        Ok(
            if self.line.is_empty() && !self.has_data && self.event_bytes == 0 {
                End::Complete
            } else {
                End::Incomplete
            },
        )
    }
}
