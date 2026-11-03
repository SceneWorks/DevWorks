//! Row and variable-block virtualization (UI-07 large histories), headless.
use devworks_desktop::virtual_list::{Blocks, Scrolls, rows};
use eframe::egui;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;

struct State {
    blocks: Blocks,
    scrolls: Scrolls,
    rendered: usize,
    count: usize,
    rows: bool,
}

fn harness(count: usize, rows_mode: bool) -> Harness<'static, State> {
    Harness::builder()
        .with_size(egui::vec2(600.0, 500.0))
        .with_max_steps(16)
        .build_ui_state(
            |ui, s: &mut State| {
                s.rendered = if s.rows {
                    rows(ui, &mut s.scrolls, "rows", 24.0, s.count, |ui, i| {
                        ui.label(format!("Row {i}"));
                    })
                } else {
                    s.blocks
                        .show(ui, &mut s.scrolls, "blocks", s.count, false, |ui, i| {
                            // Variable heights: every third block wraps onto several lines.
                            let text = if i % 3 == 0 {
                                format!("Block {i} ") + &"long text ".repeat(40)
                            } else {
                                format!("Block {i}")
                            };
                            ui.label(text);
                        })
                };
            },
            State {
                blocks: Blocks::default(),
                scrolls: Scrolls::default(),
                rendered: 0,
                count,
                rows: rows_mode,
            },
        )
}

#[test]
fn ten_thousand_variable_blocks_lay_out_only_the_visible_ones() {
    let mut h = harness(10_000, false);
    h.run();
    let rendered = h.state().rendered;
    assert!((1..60).contains(&rendered), "rendered {rendered}");
    assert!(h.query_by_label_contains("Block 0 ").is_some());
    assert!(h.query_by_label("Block 9999").is_none());
    // Scrolling far down shows later blocks without laying out the earlier ones.
    for _ in 0..30 {
        h.get_by_label("Block 1").scroll_down();
        h.run();
        if h.query_by_label("Block 1").is_none() {
            break;
        }
    }
    assert!(h.state().rendered < 60);
    let offset = h.state().scrolls.offset("blocks").unwrap();
    assert!(offset > 0.0);
}

#[test]
fn uniform_rows_lay_out_only_the_visible_range() {
    let mut h = harness(50_000, true);
    h.run();
    let rendered = h.state().rendered;
    assert!((1..40).contains(&rendered), "rendered {rendered}");
    assert!(h.query_by_label("Row 0").is_some());
    assert!(h.query_by_label("Row 49999").is_none());
}

#[test]
fn a_saved_offset_is_restored_the_first_time_a_list_is_shown() {
    let saved = [("rows".to_owned(), 24.0 * 1000.0)].into_iter().collect();
    let mut h = harness(50_000, true);
    h.state_mut().scrolls = Scrolls::restoring(&saved);
    h.run();
    assert!(h.query_by_label("Row 0").is_none());
    let first = h
        .query_all_by_label_contains("Row ")
        .filter_map(|n| {
            use egui_kittest::kittest::NodeT;
            n.accesskit_node()
                .value()?
                .strip_prefix("Row ")?
                .parse::<usize>()
                .ok()
        })
        .min()
        .unwrap();
    assert!(
        first > 500,
        "the list opens where it was left (first row {first})"
    );
}
