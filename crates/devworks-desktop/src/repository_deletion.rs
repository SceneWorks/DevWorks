//! Review and confirmation of a registered primary repository's folder deletion (SEC-04): the
//! person reads exactly what would be removed and what is kept, acknowledges it, and confirms.
//! The confirmation is bound to the reviewed preview; the host refuses it when anything that
//! preview binds changed, and while desktop automation may be generating UI events.
use crate::widgets;
use devworks_core::authority::deletion::{DeletionPreview, DeletionRefusal};
use devworks_core::protocol::records::RepositoryRecord;
use eframe::egui;

/// Why the action is not offered where the filesystem adapter performs no destructive steps.
pub const UNSUPPORTED: &str = "Deleting a repository folder is not available on this platform (DevWorks cannot yet remove \
     it safely here).";

/// A repository folder deletion being reviewed.
#[derive(Clone, Debug)]
pub struct DeletionReview {
    pub repository: RepositoryRecord,
    /// What would be removed, once read.
    pub preview: Option<DeletionPreview>,
    pub error: Option<String>,
    /// The person acknowledged that the folder is deleted permanently.
    pub acknowledged: bool,
}
impl DeletionReview {
    pub fn new(repository: RepositoryRecord) -> Self {
        Self {
            repository,
            preview: None,
            error: None,
            acknowledged: false,
        }
    }
}

/// What a refusal tells the person.
pub fn refusal_text(refusal: &DeletionRefusal) -> String {
    match refusal {
        DeletionRefusal::NotRegistered => "This repository is not registered.".into(),
        DeletionRefusal::Unsupported => UNSUPPORTED.into(),
        DeletionRefusal::Unresolvable => {
            "The repository folder cannot be found or identified, so nothing was deleted.".into()
        }
        DeletionRefusal::FilesystemRoot => {
            "The repository folder is a drive or filesystem root, which is never deleted.".into()
        }
        DeletionRefusal::Blocked(blockers) => {
            format!("Nothing was deleted: {}.", blockers.join("; "))
        }
        DeletionRefusal::HumanOnly => {
            "Only you can confirm a repository deletion, from DevWorks itself while no agent \
             controls the desktop. Nothing was deleted."
                .into()
        }
        DeletionRefusal::ConfirmationUnavailable => {
            "The confirmation is no longer available. Review the deletion again.".into()
        }
        DeletionRefusal::Changed => {
            "The repository changed since you reviewed it, so nothing was deleted. Review it \
             again."
                .into()
        }
        DeletionRefusal::Guarded(rationale) => format!("Nothing was deleted: {rationale}"),
        DeletionRefusal::Failed(why) => {
            format!("The deletion stopped part way ({why}). What remains is recorded.")
        }
        DeletionRefusal::Unrecorded(outcome) if outcome.complete => {
            "The folder was deleted, but recording the deletion in history failed.".into()
        }
        DeletionRefusal::Unrecorded(outcome) => format!(
            "The folder was partly deleted ({} entries remain), but recording the deletion in \
             history failed.",
            outcome.remaining.len()
        ),
        DeletionRefusal::Storage(e) => format!("Nothing was deleted: {e:?}"),
    }
}

/// The review window: `Some(true)` when the person confirms, `Some(false)` to cancel.
pub(crate) fn review_window(
    ctx: &egui::Context,
    review: &mut DeletionReview,
    busy: bool,
) -> Option<bool> {
    let mut decided = None;
    egui::Window::new("Delete repository folder")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ctx, |ui| {
            ui.set_max_width(480.0);
            let name = &review.repository.name.0;
            match &review.preview {
                None if review.error.is_none() => {
                    ui.label(format!("Reading what deleting {name} removes…"));
                }
                None => {}
                Some(p) => {
                    ui.label(format!(
                        "Delete the folder of {name}? DevWorks permanently deletes this folder \
                         and everything in it. There is no undo or backup."
                    ));
                    ui.monospace(&p.resolved);
                    if p.worktrees.is_empty() {
                        widgets::caption(ui, "No task worktrees of this repository.");
                    } else {
                        for w in &p.worktrees {
                            widgets::caption(ui, format!("Task worktree {}", w.location));
                        }
                    }
                    widgets::caption(
                        ui,
                        format!("Kept: {}, and this registration.", p.retained.join(", ")),
                    );
                    for b in &p.blockers {
                        widgets::chip(ui, format!("Blocked: {b}"), widgets::Tone::Bad);
                    }
                    if p.blockers.is_empty() {
                        ui.checkbox(
                            &mut review.acknowledged,
                            "I understand this permanently deletes the folder",
                        );
                    }
                }
            }
            if let Some(e) = &review.error {
                widgets::chip(ui, e.clone(), widgets::Tone::Bad);
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                let ready = !busy
                    && review.acknowledged
                    && review
                        .preview
                        .as_ref()
                        .is_some_and(|p| p.blockers.is_empty());
                if ui
                    .add_enabled(ready, egui::Button::new("Delete repository folder"))
                    .clicked()
                {
                    decided = Some(true);
                }
                if widgets::secondary_button(ui, "Cancel").clicked() {
                    decided = Some(false);
                }
            });
        });
    decided
}

#[cfg(test)]
mod tests {
    use super::*;
    use devworks_core::authority::destructive::RemovalOutcome;

    #[test]
    fn a_removal_whose_record_failed_never_reads_as_nothing_deleted() {
        let done = refusal_text(&DeletionRefusal::Unrecorded(RemovalOutcome {
            removed_entries: 3,
            remaining: Vec::new(),
            complete: true,
        }));
        assert!(done.contains("was deleted") && !done.contains("Nothing was deleted"));
        let partial = refusal_text(&DeletionRefusal::Unrecorded(RemovalOutcome {
            removed_entries: 1,
            remaining: vec!["a".into(), "b".into()],
            complete: false,
        }));
        assert!(partial.contains("2 entries remain") && !partial.contains("Nothing was deleted"));
    }
}
