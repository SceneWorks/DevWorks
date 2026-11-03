//! Virtualization for large histories (UI-07): only blocks intersecting the viewport are laid
//! out. Uniform rows use egui's `show_rows`; variable-height blocks (transcript messages) use
//! [`Blocks`], which remembers each block's measured height and estimates unmeasured ones, so
//! a ten-thousand-entry transcript costs the same per frame as a short one.
//!
//! [`Scrolls`] records every list's vertical offset by its stable salt, so a pane returns where
//! it was after navigation and after a restart (pane restoration).
use eframe::egui::{Rect, ScrollArea, Ui, UiBuilder, Vec2};
use std::collections::BTreeMap;

/// Vertical scroll offsets by list salt, and offsets still to restore.
#[derive(Clone, Debug, Default)]
pub struct Scrolls {
    offsets: BTreeMap<String, f32>,
    restore: BTreeMap<String, f32>,
}
impl Scrolls {
    /// Offsets saved by an earlier launch, applied the first time each list is shown.
    pub fn restoring(saved: &BTreeMap<String, f32>) -> Self {
        Self {
            offsets: saved.clone(),
            restore: saved.clone(),
        }
    }
    pub fn offset(&self, salt: &str) -> Option<f32> {
        self.offsets.get(salt).copied()
    }
    pub fn offsets(&self) -> &BTreeMap<String, f32> {
        &self.offsets
    }
    /// A scroll area for `salt`, positioned at the restored offset the first time.
    pub fn area(&mut self, salt: &str) -> ScrollArea {
        let area = ScrollArea::vertical()
            .id_salt(salt.to_owned())
            .auto_shrink([false, false]);
        match self.restore.remove(salt) {
            Some(y) => area.vertical_scroll_offset(y),
            None => area,
        }
    }
    pub fn record(&mut self, salt: &str, y: f32) {
        self.offsets.insert(salt.to_owned(), y);
    }
}

/// Measured heights of variable-height blocks.
#[derive(Clone, Debug)]
pub struct Blocks {
    heights: Vec<f32>,
    estimate: f32,
}
impl Default for Blocks {
    fn default() -> Self {
        Self::new(56.0)
    }
}

impl Blocks {
    pub fn new(estimate: f32) -> Self {
        Self {
            heights: Vec::new(),
            estimate,
        }
    }

    /// Forgets measurements (the content of existing blocks changed, or the width did).
    pub fn reset(&mut self) {
        self.heights.clear();
    }

    /// Shows `count` blocks in a vertical scroll area identified by `salt`, calling
    /// `render(ui, index)` only for the visible ones. Returns how many were rendered.
    pub fn show(
        &mut self,
        ui: &mut Ui,
        scrolls: &mut Scrolls,
        salt: &str,
        count: usize,
        stick_to_bottom: bool,
        mut render: impl FnMut(&mut Ui, usize),
    ) -> usize {
        self.heights.resize(count, self.estimate);
        let spacing = ui.spacing().item_spacing.y;
        let mut rendered = 0;
        let mut remeasured = false;
        let out = scrolls
            .area(salt)
            .stick_to_bottom(stick_to_bottom)
            .show_viewport(ui, |ui, viewport| {
                let width = ui.available_width();
                let total: f32 = self.heights.iter().map(|h| h + spacing).sum();
                let origin = ui.max_rect().min;
                ui.set_height(total.max(viewport.height()));
                let mut top = 0.0;
                for i in 0..count {
                    let h = self.heights[i] + spacing;
                    if top + h < viewport.min.y {
                        top += h;
                        continue;
                    }
                    if top > viewport.max.y {
                        break;
                    }
                    let rect = Rect::from_min_size(
                        origin + Vec2::new(0.0, top),
                        Vec2::new(width, f32::INFINITY),
                    );
                    let used = ui
                        .scope_builder(UiBuilder::new().max_rect(rect), |ui| render(ui, i))
                        .response
                        .rect
                        .height();
                    if (used - self.heights[i]).abs() > 0.5 {
                        self.heights[i] = used;
                        remeasured = true;
                    }
                    rendered += 1;
                    top += h;
                }
            });
        scrolls.record(salt, out.state.clamped_offset().y);
        if remeasured {
            ui.ctx().request_repaint();
        }
        rendered
    }
}

/// Uniform rows, laid out only when visible. Returns how many were rendered.
pub fn rows(
    ui: &mut Ui,
    scrolls: &mut Scrolls,
    salt: &str,
    row_height: f32,
    count: usize,
    mut render: impl FnMut(&mut Ui, usize),
) -> usize {
    let mut rendered = 0;
    let out = scrolls
        .area(salt)
        .show_rows(ui, row_height, count, |ui, range| {
            for i in range {
                render(ui, i);
                rendered += 1;
            }
        });
    scrolls.record(salt, out.state.clamped_offset().y);
    rendered
}
