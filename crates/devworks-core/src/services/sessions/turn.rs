//! The session turn assembler: normalized provider events of one attempt in, durable transcript
//! entries out (devworks-architecture.md "Streaming tool calls and native continuation"). It
//! adopts the recorded `StreamModel` candidate's acceptance rules and adds what persistence
//! needs: what becomes durable, and when.
//!
//! * An event is accepted only in transport order (`sequence` = previous + 1), for the same
//!   request, attempt and binding, before the terminal event. A native continuation bound to
//!   another connection/model/context is refused, so foreign opaque state never enters history.
//! * A complete tool call is recorded once per native ID and ordinal. Calls become dispatchable
//!   durable `ToolCallRecorded` entries only when the turn ends `ToolCallsReady`; they are then
//!   assembled in recorded ordinal order, whatever order they arrived in. A call observed before
//!   EOF, an incomplete, failed or cancelled turn is retained by the assembler but never written
//!   as a durable call.
//! * Streamed text is a preview until [`TurnAssembler::take`] moves it into a batch. Content
//!   keeps its arrival order (CACHE-03): a text run ends whenever the content block index changes
//!   or a native continuation item arrives, so each run is its own ordered sequence of
//!   `AssistantAttempt` chunks of at most 64 KiB and native items keep their position. Empty
//!   deltas carry no content and a tool-only turn writes no assistant text.
//! * The durable entries never depend on when `take` is called. `take` returns only content
//!   that can no longer change: ended runs, native items, and the full 64 KiB chunks of the run
//!   still being streamed; the open run's tail waits for the run to end. Chunks are never marked
//!   interrupted before the outcome is known. When the turn ends interrupted (an interrupted
//!   terminal, or [`TurnAssembler::end_of_stream`] at EOF), the last chunk is marked
//!   `interrupted` if text was the last content; otherwise one [`interruption_marker`] records
//!   it — the only empty assistant entry ever written. After a crash mid-stream the runtime
//!   appends that marker itself, since the turn will never end.
//!
//! Endpoint wire decoding, broker admission and result assembly are the transport and tool-turn
//! owners'; this assembler only decides what of a normalized turn is durable.
use crate::protocol::provider::{
    ArtifactRef, CompleteToolCall, NativeBinding, ProviderBody, ProviderEvent, TranscriptBatch,
    TranscriptBody, TranscriptEntry, TurnOutcome,
};
use crate::protocol::{AttemptId, Counter, Id, Protocol, Revision};

/// Largest visible-text chunk of one `AssistantAttempt` entry.
pub const TEXT_CHUNK: usize = 65536;

/// Why an event was not accepted. The assembler is unchanged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StreamRefusal {
    Invalid(&'static str),
    AfterTerminal,
    /// The request, attempt or binding changed inside the attempt.
    IdentityChanged,
    SequenceMismatch {
        expected: i64,
        presented: i64,
    },
    DuplicateCall,
    /// `ToolCallsReady` without calls, or `Completed` with calls.
    TerminalMismatch,
}

/// Content that can no longer change, not yet taken, in arrival order.
#[derive(Clone, Debug)]
enum Item {
    /// An ended text run (or a full chunk of the open run).
    Text(String),
    Native(ArtifactRef),
}

/// The text run still being streamed: its block and the text not yet taken.
#[derive(Clone, Debug)]
struct Run {
    block: i64,
    text: String,
}

#[derive(Clone, Debug, Default)]
pub struct TurnAssembler {
    request: Option<Id>,
    attempt: Option<AttemptId>,
    binding: Option<NativeBinding>,
    next: i64,
    pending: Vec<Item>,
    open: Option<Run>,
    preview: String,
    calls: Vec<CompleteToolCall>,
    terminal: Option<TurnOutcome>,
    /// The stream ended without a terminal event (EOF).
    cut_off: bool,
    calls_taken: bool,
    finished: bool,
}

impl TurnAssembler {
    pub fn new() -> Self {
        Self::default()
    }

    fn end_run(&mut self) {
        if let Some(run) = self.open.take() {
            for chunk in chunks(&run.text) {
                self.pending.push(Item::Text(chunk.to_owned()));
            }
        }
    }

    pub fn accept(&mut self, e: &ProviderEvent) -> Result<(), StreamRefusal> {
        e.validate().map_err(StreamRefusal::Invalid)?;
        if self.terminal.is_some() || self.cut_off {
            return Err(StreamRefusal::AfterTerminal);
        }
        if self.request.is_some()
            && (self.request.as_ref() != Some(&e.request_id)
                || self.attempt.as_ref() != Some(&e.attempt_id)
                || self.binding.as_ref() != Some(&e.binding))
        {
            return Err(StreamRefusal::IdentityChanged);
        }
        let presented = e.sequence.get();
        if presented != self.next + 1 {
            return Err(StreamRefusal::SequenceMismatch {
                expected: self.next + 1,
                presented,
            });
        }
        let duplicate = |t: &CompleteToolCall| {
            self.calls.iter().any(|c| {
                c.call.native_call_id == t.call.native_call_id || c.call.ordinal == t.call.ordinal
            })
        };
        let mismatched = |o: &TurnOutcome| {
            matches!(o, TurnOutcome::ToolCallsReady) && self.calls.is_empty()
                || matches!(o, TurnOutcome::Completed) && !self.calls.is_empty()
        };
        match &e.event {
            ProviderBody::ToolCallComplete { tool_call } if duplicate(tool_call) => {
                return Err(StreamRefusal::DuplicateCall);
            }
            ProviderBody::Terminal { outcome, .. } if mismatched(outcome) => {
                return Err(StreamRefusal::TerminalMismatch);
            }
            _ => {}
        }
        self.request = Some(e.request_id.clone());
        self.attempt = Some(e.attempt_id.clone());
        self.binding = Some(e.binding.clone());
        self.next = presented;
        match &e.event {
            // An empty delta carries no content: it neither starts nor ends a run.
            ProviderBody::TextDelta { text, .. } if text.is_empty() => {}
            ProviderBody::TextDelta { block_index, text } => {
                let block = block_index.get();
                self.preview.push_str(text);
                if self.open.as_ref().is_some_and(|r| r.block != block) {
                    self.end_run();
                }
                match &mut self.open {
                    Some(run) => run.text.push_str(text),
                    None => {
                        self.open = Some(Run {
                            block,
                            text: text.clone(),
                        })
                    }
                }
                // Full chunks of the open run can no longer change.
                if let Some(run) = &mut self.open {
                    while run.text.len() > TEXT_CHUNK {
                        let mut cut = TEXT_CHUNK;
                        while !run.text.is_char_boundary(cut) {
                            cut -= 1;
                        }
                        let rest = run.text.split_off(cut);
                        let full = std::mem::replace(&mut run.text, rest);
                        self.pending.push(Item::Text(full));
                    }
                }
            }
            ProviderBody::ToolCallComplete { tool_call } => self.calls.push(tool_call.clone()),
            ProviderBody::NativeContinuation { artifact, .. } => {
                self.end_run();
                self.pending.push(Item::Native(artifact.clone()));
            }
            ProviderBody::Terminal { outcome, .. } => self.terminal = Some(outcome.clone()),
            ProviderBody::Usage { .. } | ProviderBody::Heartbeat => {}
        }
        Ok(())
    }

    /// Whether the turn ended with every emitted call complete (the default dispatch boundary).
    pub fn tools_ready(&self) -> bool {
        matches!(self.terminal, Some(TurnOutcome::ToolCallsReady))
    }
    pub fn terminal(&self) -> Option<&TurnOutcome> {
        self.terminal.as_ref()
    }
    /// All streamed text so far: a preview, durable only once taken and appended.
    pub fn preview(&self) -> &str {
        &self.preview
    }
    /// Complete calls observed so far, in arrival order (retained even when not dispatchable).
    pub fn observed_calls(&self) -> &[CompleteToolCall] {
        &self.calls
    }

    /// The stream ended without a terminal event (EOF, a dropped connection): the turn is
    /// interrupted and its calls are not dispatchable. Returns the remaining durable entries.
    pub fn end_of_stream(&mut self) -> Vec<TranscriptBody> {
        if self.terminal.is_none() {
            self.cut_off = true;
        }
        self.take()
    }

    /// The durable entries that can no longer change and were not taken before (see the module
    /// documentation), then — once the turn is ready — the complete calls in ordinal order.
    pub fn take(&mut self) -> Vec<TranscriptBody> {
        let (Some(request), Some(attempt), Some(binding)) = (
            self.request.clone(),
            self.attempt.clone(),
            self.binding.clone(),
        ) else {
            return Vec::new();
        };
        if self.finished {
            return Vec::new();
        }
        let interrupted = match &self.terminal {
            None if self.cut_off => Some(true),
            None => None,
            Some(TurnOutcome::Completed | TurnOutcome::ToolCallsReady) => Some(false),
            Some(_) => Some(true),
        };
        // Whether text is the last content of the turn (decided by content, not flush timing).
        let text_last = self.open.is_some();
        if interrupted.is_some() {
            self.end_run();
        }
        let mut out = Vec::new();
        for item in std::mem::take(&mut self.pending) {
            out.push(match item {
                Item::Text(text) => TranscriptBody::AssistantAttempt {
                    request_id: request.clone(),
                    attempt_id: attempt.clone(),
                    visible_text: text,
                    interrupted: false,
                },
                Item::Native(artifact) => TranscriptBody::NativeItem {
                    binding: binding.clone(),
                    artifact,
                },
            });
        }
        if interrupted == Some(true) {
            match out.last_mut() {
                Some(TranscriptBody::AssistantAttempt { interrupted, .. }) if text_last => {
                    *interrupted = true
                }
                _ => out.push(interruption_marker(request.clone(), attempt.clone())),
            }
        }
        if self.tools_ready() && !self.calls_taken {
            let mut calls = self.calls.clone();
            calls.sort_by_key(|c| c.call.ordinal.get());
            out.extend(
                calls
                    .into_iter()
                    .map(|tool_call| TranscriptBody::ToolCallRecorded { tool_call }),
            );
            self.calls_taken = true;
        }
        if interrupted.is_some() {
            self.finished = true;
        }
        out
    }
}

/// The entry that records an attempt as interrupted when its stream can no longer end (a crash
/// or restart after a partial flush): an empty, interrupted assistant attempt.
pub fn interruption_marker(request_id: Id, attempt_id: AttemptId) -> TranscriptBody {
    TranscriptBody::AssistantAttempt {
        request_id,
        attempt_id,
        visible_text: String::new(),
        interrupted: true,
    }
}

/// Ordered chunks of at most [`TEXT_CHUNK`] bytes split on character boundaries; one (possibly
/// empty) chunk for short text.
pub fn chunks(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = text;
    while rest.len() > TEXT_CHUNK {
        let mut cut = TEXT_CHUNK;
        while !rest.is_char_boundary(cut) {
            cut -= 1;
        }
        out.push(&rest[..cut]);
        rest = &rest[cut..];
    }
    out.push(rest);
    out
}

/// A transcript batch of `bodies` for `session` at its next sequence and epoch, with entry IDs
/// from `entry_id`.
pub fn batch_of(
    session: &Id,
    batch_id: Id,
    context_epoch: i64,
    next_sequence: i64,
    bodies: Vec<TranscriptBody>,
    mut entry_id: impl FnMut() -> Id,
) -> TranscriptBatch {
    let entries = bodies
        .into_iter()
        .enumerate()
        .map(|(i, body)| TranscriptEntry {
            entry_id: entry_id(),
            sequence: Revision::new(next_sequence + i as i64).expect("positive sequence"),
            body,
        })
        .collect();
    TranscriptBatch {
        protocol: Protocol::Draft1,
        batch_id,
        session_id: session.clone(),
        context_epoch: Counter::new(context_epoch).expect("non-negative epoch"),
        expected_next_sequence: Revision::new(next_sequence).expect("positive sequence"),
        entries,
    }
}
