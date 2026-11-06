//! The volatile native preview has no public command and never enters persisted UI memory.
use super::*;

impl Shell {
    pub(super) fn poll_live_text(&mut self, ctx: &egui::Context) {
        let target = (self.route == Route::Chat || self.coordinator_open)
            .then(|| self.chat.selected.clone())
            .flatten();
        if target != self.preview_target {
            self.preview_target = target.clone();
            self.preview_generation = self.preview_generation.wrapping_add(1);
            self.last_preview_poll = None;
            self.chat.text_preview = None;
        }
        self.chat.reconcile_text_preview();
        let Some(id) = target else { return };
        let interval = if self.chat.text_preview.is_some() {
            Duration::from_millis(100)
        } else {
            Duration::from_millis(500)
        };
        ctx.request_repaint_after(interval);
        let slot = Slot::TextPreview(id.clone(), self.preview_generation);
        if self.pending(&slot)
            || self
                .last_preview_poll
                .is_some_and(|t| t.elapsed() < interval)
        {
            return;
        }
        let Some(client) = &mut self.client else {
            return;
        };
        self.last_preview_poll = Some(Instant::now());
        client.task(slot, "Reading the live response", move |b| {
            b.text_preview(&devworks_core::protocol::Id::try_from(id).map_err(str::to_owned)?)
                .map(TaskOutput::TextPreview)
        });
    }

    pub(super) fn apply_live_text(
        &mut self,
        id: &str,
        generation: u64,
        result: Result<TaskOutput, String>,
    ) {
        if generation != self.preview_generation
            || self.preview_target.as_deref() != Some(id)
            || self.chat.selected.as_deref() != Some(id)
        {
            return;
        }
        match result {
            Ok(TaskOutput::TextPreview(turn)) => self.chat.apply_text_preview(turn),
            _ => self.chat.text_preview = None,
        }
        if self
            .chat
            .text_preview
            .as_ref()
            .is_some_and(|t| t.finalized_through.is_some())
            && !self.pending(&Slot::Transcript(id.to_owned()))
        {
            self.load_transcript(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use devworks_core::protocol::{
        AttemptId, Id, Revision,
        provider::{TranscriptBody, TranscriptEntry},
    };
    use devworks_engine::providers::live::LiveTurn;
    fn id(s: &str) -> Id {
        s.to_owned().try_into().unwrap()
    }
    fn turn(revision: u64, finalized_through: Option<i64>) -> LiveTurn {
        LiveTurn {
            session_id: id("current"),
            attempt_id: AttemptId(id("attempt")),
            context_epoch: 0,
            revision,
            text: format!("response {revision}"),
            finalized_through,
        }
    }
    #[test]
    fn live_text_reply_cannot_replace_a_newer_selection_or_clear_it_from_an_old_generation() {
        let mut shell = Shell::new(ShellInit {
            backend: Err("offline".into()),
            dialogs: Box::new(NativeDialogs),
            memory_path: None,
            control: None,
            version: "test".into(),
        });
        shell.preview_target = Some("current".into());
        shell.chat.selected = Some("current".into());
        shell.preview_generation = 3;
        shell.apply_live_text(
            "current",
            3,
            Ok(TaskOutput::TextPreview(Some(turn(2, None)))),
        );
        shell.apply_live_text(
            "current",
            2,
            Ok(TaskOutput::TextPreview(Some(turn(3, None)))),
        );
        shell.apply_live_text("other", 3, Ok(TaskOutput::TextPreview(None)));
        shell.apply_live_text("current", 2, Err("old authorization refusal".into()));
        assert_eq!(shell.chat.text_preview.as_ref().unwrap().revision, 2);
        shell.apply_live_text(
            "current",
            3,
            Ok(TaskOutput::TextPreview(Some(turn(1, None)))),
        );
        assert_eq!(shell.chat.text_preview.as_ref().unwrap().revision, 2);
        shell.chat.selected = Some("other".into());
        shell.apply_live_text(
            "current",
            3,
            Ok(TaskOutput::TextPreview(Some(turn(4, None)))),
        );
        shell.chat.reconcile_text_preview();
        assert!(shell.chat.text_preview.is_none());
    }
    #[test]
    fn final_preview_survives_registry_eviction_until_the_canonical_sequence_arrives() {
        let mut chat = Chat::new();
        chat.selected = Some("current".into());
        chat.apply_text_preview(Some(turn(1, None)));
        chat.apply_text_preview(None);
        assert!(
            chat.text_preview.is_none(),
            "dropped unfinished output is cleared"
        );
        chat.apply_text_preview(Some(turn(2, Some(5))));
        chat.apply_text_preview(None);
        assert!(
            chat.text_preview.is_some(),
            "final canonical handover cannot blank the response"
        );
        chat.transcripts
            .entry("current".into())
            .or_default()
            .entries
            .push(TranscriptEntry {
                entry_id: id("entry"),
                sequence: Revision::new(5).unwrap(),
                body: TranscriptBody::AssistantAttempt {
                    request_id: id("request"),
                    attempt_id: AttemptId(id("attempt")),
                    visible_text: "response 2".into(),
                    interrupted: false,
                },
            });
        chat.reconcile_text_preview();
        assert!(chat.text_preview.is_none());
    }
}
