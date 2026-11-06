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
        let mut events = Vec::new();
        self.push_events(bytes, |event| events.push(event))?;
        Ok(events)
    }
    /// Delivers each complete prefix event even if a later byte in this same chunk is invalid.
    /// Streaming owners should use this form so usage/evidence is independent of network chunks.
    pub fn push_events(
        &mut self,
        bytes: &[u8],
        mut emit: impl FnMut(SseEvent),
    ) -> Result<(), SseError> {
        if self.finished {
            return Err(SseError::Finished);
        }
        let result = self.push_inner(bytes, &mut emit);
        if result.is_err() {
            self.finished = true;
        }
        result
    }
    fn push_inner(
        &mut self,
        bytes: &[u8],
        emit: &mut impl FnMut(SseEvent),
    ) -> Result<(), SseError> {
        for &byte in bytes {
            self.stream_bytes = self.stream_bytes.checked_add(1).ok_or(SseError::TooLarge)?;
            if self.stream_bytes > self.limits.max_stream_bytes {
                return Err(SseError::TooLarge);
            }
            if self.skip_lf {
                self.skip_lf = false;
                if byte == b'\n' {
                    continue;
                }
            }
            if byte == b'\r' || byte == b'\n' {
                self.finish_line(emit)?;
                self.skip_lf = byte == b'\r';
            } else {
                if self.line.len() == self.limits.max_line_bytes {
                    return Err(SseError::TooLarge);
                }
                self.line.push(byte);
            }
        }
        Ok(())
    }
    fn finish_line(&mut self, emit: &mut impl FnMut(SseEvent)) -> Result<(), SseError> {
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
                emit(SseEvent {
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
            if self.line.is_empty() && !self.has_data && self.event.is_none() {
                End::Complete
            } else {
                End::Incomplete
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sse_limits() -> SseLimits {
        SseLimits {
            max_line_bytes: 1024,
            max_event_bytes: 4096,
            max_stream_bytes: 8192,
            max_events: 32,
        }
    }
    #[test]
    fn sse_every_split_preserves_utf8_crlf_bom_fields_and_multiline_data() {
        let input = "\u{feff}:heartbeat\r\nid: e1\r\nevent: delta\r\nretry: 25\r\ndata: 世界\r\ndata: second\r\n\r\n:data ignored\ndata:\n\nid: forbidden\0id\ndata: third\n\n".as_bytes();
        for split in 0..=input.len() {
            let mut decoder = SseDecoder::new(sse_limits()).unwrap();
            let mut events = decoder.push(&input[..split]).unwrap();
            events.extend(decoder.push(&input[split..]).unwrap());
            assert_eq!(decoder.finish(), Ok(End::Complete), "split {split}");
            assert_eq!(events.len(), 3, "split {split}");
            assert_eq!(events[0].data, "世界\nsecond");
            assert_eq!(events[0].event.as_deref(), Some("delta"));
            assert_eq!(events[1].data, "");
            assert_eq!(events[2].id.as_deref(), Some("e1"));
            assert_eq!(events[2].retry_ms, Some(25));
            assert!(!format!("{:?}", events[0]).contains("世界"));
        }
        let mut decoder = SseDecoder::new(sse_limits()).unwrap();
        let mut count = 0;
        for byte in input {
            count += decoder.push(&[*byte]).unwrap().len();
        }
        assert_eq!(count, 3);
        assert_eq!(decoder.finish(), Ok(End::Complete));
    }

    #[test]
    fn sse_eof_never_releases_unterminated_data_and_failure_is_terminal() {
        for input in [
            b"data: x".as_slice(),
            b"data: x\n",
            b"event: x\n",
            b":heartbeat",
        ] {
            let mut decoder = SseDecoder::new(sse_limits()).unwrap();
            assert!(decoder.push(input).unwrap().is_empty());
            assert_eq!(decoder.finish(), Ok(End::Incomplete));
            assert_eq!(decoder.push(b"\n"), Err(SseError::Finished));
        }
        let mut heartbeat = SseDecoder::new(sse_limits()).unwrap();
        assert!(heartbeat.push(b":heartbeat\n").unwrap().is_empty());
        assert_eq!(heartbeat.finish(), Ok(End::Complete));
        let mut decoder = SseDecoder::new(sse_limits()).unwrap();
        assert_eq!(decoder.push(b"data: \xff\n"), Err(SseError::InvalidUtf8));
        assert_eq!(decoder.finish(), Err(SseError::Finished));
        let mut decoder = SseDecoder::new(sse_limits()).unwrap();
        decoder.push(b"data: \xe4").unwrap();
        assert_eq!(decoder.finish(), Err(SseError::InvalidUtf8));
    }

    #[test]
    fn sse_each_limit_is_finite_including_comments_and_retained_id_amplification() {
        let cases = [
            (
                SseLimits {
                    max_line_bytes: 6,
                    ..sse_limits()
                },
                "data: x\n\n",
            ),
            (
                SseLimits {
                    max_event_bytes: 7,
                    ..sse_limits()
                },
                ":aaaa\n:bbbb\n\n",
            ),
            (
                SseLimits {
                    max_stream_bytes: 8,
                    ..sse_limits()
                },
                "data: x\n\n",
            ),
            (
                SseLimits {
                    max_events: 1,
                    ..sse_limits()
                },
                "data: x\n\ndata: y\n\n",
            ),
        ];
        for (limits, input) in cases {
            assert_eq!(
                SseDecoder::new(limits).unwrap().push(input.as_bytes()),
                Err(SseError::TooLarge)
            );
        }
        let mut decoder = SseDecoder::new(SseLimits {
            max_line_bytes: 7,
            max_event_bytes: 8,
            max_stream_bytes: 9,
            max_events: 1,
        })
        .unwrap();
        assert_eq!(decoder.push(b"data: x\n\n").unwrap()[0].data, "x");
        assert_eq!(decoder.finish(), Ok(End::Complete));
        let input = format!("id: {}\n\n{}", "x".repeat(80), "data:\n\n".repeat(20));
        assert_eq!(
            SseDecoder::new(SseLimits {
                max_stream_bytes: input.len(),
                ..sse_limits()
            })
            .unwrap()
            .push(input.as_bytes()),
            Err(SseError::TooLarge)
        );
    }

    #[test]
    fn sse_valid_prefix_survives_malformed_tail_at_every_split() {
        let input = b"data: observed-usage\n\ndata: \xff\n\n";
        for split in 0..=input.len() {
            let mut decoder = SseDecoder::new(sse_limits()).unwrap();
            let mut events = Vec::new();
            let first = decoder.push_events(&input[..split], |event| events.push(event));
            let outcome = match first {
                Ok(()) => decoder.push_events(&input[split..], |event| events.push(event)),
                Err(error) => Err(error),
            };
            assert_eq!(outcome, Err(SseError::InvalidUtf8));
            assert_eq!(events.len(), 1, "split {split}");
            assert_eq!(events[0].data, "observed-usage");
        }
    }
}
