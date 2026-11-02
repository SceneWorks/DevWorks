use egui::{Context, Pos2, RawInput, Rect, Vec2};
use serde_json::{Value, json};
pub fn run() -> Result<Value, Box<dyn std::error::Error>> {
    let ctx = Context::default();
    ctx.enable_accesskit();
    let mut checks = Vec::new();
    let mut rendered = 0usize;
    let mut trees = 0usize;
    let mut shapes = 0usize;
    let mut draft = String::from("Keep this draft");
    let session = "workspace-session-a";
    let mut pointer = None;
    for frame in 0..6 {
        let mut input = RawInput::default();
        input.screen_rect = Some(Rect::from_min_size(
            Pos2::ZERO,
            Vec2::new(if frame == 3 { 800.0 } else { 1440.0 }, 900.0),
        ));
        input.time = Some(frame as f64 / 60.0);
        let mut count = 0usize;
        let output = ctx.run_ui(input, |ui| {
            egui::Panel::right("shared-home-sidebar")
                .default_size(280.0)
                .show(ui, |ui| {
                    ui.label(session);
                    let r = ui.text_edit_multiline(&mut draft);
                    pointer = Some(r.id);
                });
            egui::CentralPanel::default().show(ui, |ui| {
                ui.heading("Transcript");
                egui::ScrollArea::vertical().show_rows(ui, 20.0, 100_000, |ui, range| {
                    for i in range {
                        ui.label(format!("Message {i} — deterministic streamed fixture"));
                        count += 1;
                    }
                });
            });
        });
        rendered = rendered.max(count);
        trees += output
            .platform_output
            .accesskit_update
            .as_ref()
            .map_or(0, |t| t.nodes.len());
        shapes += ctx.tessellate(output.shapes, output.pixels_per_point).len();
    }
    if rendered >= 1000 || rendered == 0 {
        return Err("paging materialized wrong row count".into());
    }
    checks.push("virtualized_100k_rows_materializes_only_visible_range");
    if trees == 0 || shapes == 0 {
        return Err("missing accessibility tree or paint primitives".into());
    }
    checks.push("native_accesskit_updates_and_tessellation_exist");
    if draft != "Keep this draft" || pointer.is_none() {
        return Err("shared draft lost".into());
    }
    checks.push("shared_session_draft_survives_frames_and_resize");
    let mut input = RawInput::default();
    #[derive(Debug)]
    struct FileFixture;
    impl egui::DroppedFile for FileFixture {
        fn path(&self) -> &std::path::Path {
            std::path::Path::new("fixture.txt")
        }
        fn bytes(&self) -> Result<Vec<u8>, String> {
            Ok(b"fixture".to_vec())
        }
    }
    input.dropped_files.push(std::sync::Arc::new(FileFixture));
    let mut observed = false;
    let _ = ctx.run_ui(input, |ui| {
        observed = ui.input(|i| i.raw.dropped_files.len() == 1);
    });
    if !observed {
        return Err("file event missing".into());
    }
    checks.push("dropped_file_event_available_to_native_ui");
    Ok(
        json!({"host":"macOS arm64","egui":"0.37.0","passed":checks.len(),"checks":checks,"maxRowsMaterialized":rendered,"accesskitNodesAcrossFrames":trees,"paintPrimitivesAcrossFrames":shapes,"noWindowOpened":true,"noInference":true,"limits":["Headless egui frames, not eframe window/GPU, OS screen reader, clipboard/dialog or performance qualification.","Fixture session/draft state is shared by construction, not production session persistence."]}),
    )
}
