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
