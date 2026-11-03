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
//!   blocks keep their structure (CACHE-03): each content block's text is its own ordered run
//!   of `AssistantAttempt` chunks of at most 64 KiB, and native continuation items keep their
//!   position among the blocks. A tool-only turn writes no assistant text.
//! * `take` is incremental: it returns only what was not taken before (the flushed watermark),
//!   so a long stream can be flushed while it runs without duplicating text. Chunks flushed
//!   before the terminal event are not marked interrupted (the outcome is not known yet); the
//!   final take marks the last chunk — or, when the turn ended interrupted with nothing left, an
//!   empty marker — `interrupted`. After a crash mid-stream the runtime appends
//!   [`interruption_marker`], since the turn will never end.
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

/// Content not yet taken, in arrival order.
#[derive(Clone, Debug)]
enum Item {
    Text { block: i64, text: String },
    Native(ArtifactRef),
}

#[derive(Clone, Debug, Default)]
pub struct TurnAssembler {
    request: Option<Id>,
    attempt: Option<AttemptId>,
    binding: Option<NativeBinding>,
    next: i64,
    /// Content after the flushed watermark.
    pending: Vec<Item>,
    preview: String,
    calls: Vec<CompleteToolCall>,
    terminal: Option<TurnOutcome>,
    calls_taken: bool,
    finished: bool,
}

impl TurnAssembler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn accept(&mut self, e: &ProviderEvent) -> Result<(), StreamRefusal> {
        e.validate().map_err(StreamRefusal::Invalid)?;
        if self.terminal.is_some() {
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
            ProviderBody::TextDelta { block_index, text } => {
                let block = block_index.get();
                self.preview.push_str(text);
                // Deltas of a content block extend that block, even when another block's
                // deltas arrived in between.
                match self
                    .pending
                    .iter_mut()
                    .find(|i| matches!(i, Item::Text { block: b, .. } if *b == block))
                {
                    Some(Item::Text { text: t, .. }) => t.push_str(text),
                    _ => self.pending.push(Item::Text {
                        block,
                        text: text.clone(),
                    }),
                }
            }
            ProviderBody::ToolCallComplete { tool_call } => self.calls.push(tool_call.clone()),
            ProviderBody::NativeContinuation { artifact, .. } => {
                self.pending.push(Item::Native(artifact.clone()))
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

    /// The durable entries not taken before (see the module documentation), advancing the
    /// flushed watermark: text chunks per content block and native items in arrival order, then
    /// — once the turn is ready — the complete calls in ordinal order.
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
            None => None,
            Some(TurnOutcome::Completed | TurnOutcome::ToolCallsReady) => Some(false),
            Some(_) => Some(true),
        };
        let mut out = Vec::new();
        for item in std::mem::take(&mut self.pending) {
            match item {
                Item::Text { text, .. } => {
                    for chunk in chunks(&text) {
                        out.push(TranscriptBody::AssistantAttempt {
                            request_id: request.clone(),
                            attempt_id: attempt.clone(),
                            visible_text: chunk.to_owned(),
                            interrupted: false,
                        });
                    }
                }
                Item::Native(artifact) => out.push(TranscriptBody::NativeItem {
                    binding: binding.clone(),
                    artifact,
                }),
            }
        }
        if interrupted == Some(true) {
            match out
                .iter_mut()
                .rev()
                .find(|b| matches!(b, TranscriptBody::AssistantAttempt { .. }))
            {
                Some(TranscriptBody::AssistantAttempt { interrupted, .. }) => *interrupted = true,
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
