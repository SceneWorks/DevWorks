//! Positive native fixtures, enabled with the real ConventionService checkpoint. No backend
//! returns invented records: commands, artifacts, journals and source pages use the runtime.
use super::*;
use devworks_core::protocol::records::{ConventionOperationPhase, ConventionRecord};
use devworks_desktop::{
    backend::Backend,
    conventions_ops as cop,
    shell::{Dialogs, Shell, ShellInit},
};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{BufWriter, Write},
    path::PathBuf,
    sync::{Arc, Mutex},
};
type Ui = egui_kittest::Harness<'static, Shell>;
#[derive(Clone, Default)]
struct Files {
    picked: Script,
    saves: Arc<Mutex<Vec<PathBuf>>>,
}
impl Dialogs for Files {
    fn pick_files(&mut self) -> Vec<PathBuf> {
        self.picked.pick_files()
    }
    fn pick_folder(&mut self, start: Option<&str>) -> Option<PathBuf> {
        self.picked.pick_folder(start)
    }
    fn save_file(&mut self, _: &str) -> Option<PathBuf> {
        let mut paths = self.saves.lock().unwrap();
        (!paths.is_empty()).then(|| paths.remove(0))
    }
}
fn open(b: Arc<dyn Backend>, root: &Root, files: Files) -> Ui {
    let shell = Shell::new(ShellInit {
        backend: Ok(b),
        dialogs: Box::new(files),
        memory_path: Some(root.memory()),
        control: None,
        version: "0.1.0-test".into(),
    });
    let mut h = harness(shell, 1600.0, 1100.0);
    h.ctx
        .set_missing_glyph_policy(egui::MissingGlyphPolicy::Tofu);
    settle(&mut h);
    nav(&h, "Knowledge").scroll_to_me();
    settle(&mut h);
    click_nav(&mut h, "Knowledge");
    if h.state().route_memory(Route::Knowledge).tab != 2 {
        click(&mut h, Role::Button, "Conventions");
    }
    step_until(&mut h, "registered convention listing", |s| {
        s.conventions.state().is_some_and(|v| v.loaded)
    });
    assert!(h.state().conventions.state().unwrap().available);
    h
}
fn imported(h: &mut Ui) {
    step_until(h, "actual accepted import reference", |s| {
        s.conventions
            .state()
            .is_some_and(|v| !v.operation_ids.is_empty())
    });
    let id = h
        .state()
        .conventions
        .state()
        .unwrap()
        .operation_ids
        .last()
        .unwrap()
        .clone();
    step_until(h, "owner-recorded import completion", |s| {
        let Some(status) = s.conventions.state().unwrap().operation_statuses.get(&id) else {
            return false;
        };
        assert!(
            !matches!(
                status.phase,
                ConventionOperationPhase::Failed | ConventionOperationPhase::Cancelled
            ),
            "actual import did not complete: {status:?}"
        );
        matches!(status.phase, ConventionOperationPhase::Completed)
    });
    click(h, Role::Button, "Refresh conventions");
}

fn select(h: &mut Ui, title: &str) -> ConventionRecord {
    click(h, Role::Button, title);
    step_until(h, "captured convention source", |s| {
        s.conventions
            .state()
            .is_some_and(|v| v.reader.view.is_some() && !v.reader.loading)
    });
    h.state()
        .conventions
        .state()
        .unwrap()
        .record()
        .unwrap()
        .clone()
}
fn manifest(root: &Root, slug: &str, title: &str, markdown: &str, raw: Option<&str>) -> PathBuf {
    manifest_at(root, slug, title, markdown, raw, &[])
}
fn manifest_at(
    root: &Root,
    slug: &str,
    title: &str,
    markdown: &str,
    raw: Option<&str>,
    parents: &[String],
) -> PathBuf {
    use devworks_core::services::conventions::model::{
        EditorConvention, EditorResource, EditorText, write_editor_manifest,
    };
    fn digest(bytes: &[u8]) -> String {
        Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }
    let md_hash = digest(markdown.as_bytes());
    let raw_hash = raw.map(|s| digest(s.as_bytes()));
    let mut md = markdown.as_bytes();
    let mut raw_bytes = raw.map(str::as_bytes);
    static NEXT_MANIFEST: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let path = root.file(
        &format!(
            "manifest-{}.json",
            NEXT_MANIFEST.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ),
        b"",
    );
    let mut output = BufWriter::new(File::create(&path).unwrap());
    write_editor_manifest(
        &mut output,
        EditorConvention {
            slug,
            title,
            parent_path: parents,
            sort_order: Some(0),
            lifecycle: "Active",
            applicability: &["literal, label".into()],
            markdown: EditorText {
                sha256: &md_hash,
                byte_length: markdown.len() as u64,
                reader: &mut md,
            },
            raw_content: raw_bytes.as_mut().map(|bytes| EditorText {
                sha256: raw_hash.as_deref().unwrap(),
                byte_length: bytes.len() as u64,
                reader: bytes,
            }),
        },
        std::iter::empty::<EditorResource<'_>>(),
    )
    .unwrap();
    output.flush().unwrap();
    path
}
fn import(h: &mut Ui, files: &Files, path: PathBuf) {
    files.picked.files.lock().unwrap().push(vec![path]);
    click(h, Role::Button, "Import convention");
    imported(h);
}

#[test]
fn native_passage_continuations_keep_slug_path_and_refreshed_id_intents() {
    let _native_case = native_runtime_case();
    let _case = native_runtime_case();
    let root = Root::new("conventions-cursor-intent");
    let engine = start(&root);
    let files = Files::default();
    let mut h = open(engine.backend(), &root, files.clone());
    let source = format!("{}END_NATIVE", "λ-native-source\r\n".repeat(3000));
    import(
        &mut h,
        &files,
        manifest(&root, "cursor-source", "Cursor source", &source, None),
    );
    for selector in ["slug", "path", "refresh"] {
        match selector {
            "slug" => {
                click(
                    &mut h,
                    Role::DisclosureTriangle,
                    "Open a convention by slug",
                );
                replace(&mut h, "Convention slug lookup", "cursor-source");
                click(&mut h, Role::Button, "Open convention slug");
            }
            "path" => {
                click(&mut h, Role::DisclosureTriangle, "Open an exact hierarchy");
                click(&mut h, Role::Button, "Add Hierarchy component");
                replace(&mut h, "Hierarchy component 1", "cursor-source");
                click(&mut h, Role::Button, "Open exact convention hierarchy");
            }
            _ => {
                select(&mut h, "Cursor source");
                click(&mut h, Role::Button, "Refresh conventions");
            }
        }
        let first = h
            .state()
            .conventions
            .state()
            .unwrap()
            .reader
            .view
            .as_ref()
            .unwrap();
        assert!(
            first.passage.as_ref().unwrap().has_more,
            "real multi-passage {selector}"
        );
        click(&mut h, Role::Button, "Next convention passage");
        let reader = &h.state().conventions.state().unwrap().reader;
        assert!(reader.error.is_none(), "{selector}: {:?}", reader.error);
        let passage = reader.view.as_ref().unwrap().passage.as_ref().unwrap();
        assert!(
            passage.offset.get() > 0,
            "continuation advances for {selector}"
        );
        let expected = source
            .chars()
            .skip(passage.offset.get() as usize)
            .take(passage.text.0.chars().count())
            .collect::<String>();
        assert_eq!(passage.text.0, expected);
        click(&mut h, Role::Button, "Previous convention passage");
        let reader = &h.state().conventions.state().unwrap().reader;
        assert!(reader.error.is_none());
        assert_eq!(
            reader
                .view
                .as_ref()
                .unwrap()
                .passage
                .as_ref()
                .unwrap()
                .offset
                .get(),
            0
        );
    }
}

#[test]
fn native_registered_creation_edit_versions_and_source_exports_are_lossless() {
    let _native_case = native_runtime_case();
    let root = Root::new("conventions-real-source");
    let engine = start(&root);
    let files = Files::default();
    let mut h = open(engine.backend(), &root, files.clone());
    click(&mut h, Role::Button, "New convention");
    replace(&mut h, "Convention title", "Native source");
    replace(&mut h, "Convention slug", "source-literal");
    replace(
        &mut h,
        "Convention Markdown",
        "# Native source\r\nλ and NUL \0 kept\n",
    );
    click(&mut h, Role::Button, "Create convention");
    imported(&mut h);
    let first = select(&mut h, "Native source");
    assert_eq!(first.revision.get(), 1);
    assert_eq!(first.slug.0, "source-literal");
    click(&mut h, Role::Button, "Edit convention");
    step_until(&mut h, "complete editor source", |s| {
        s.conventions
            .state()
            .and_then(|s| s.draft())
            .is_some_and(|d| d.body_complete && !d.loading)
    });
    replace(
        &mut h,
        "Convention Markdown",
        "# Changed source\r\nΩ\0 retained\n",
    );
    click(&mut h, Role::Button, "Save convention revision");
    step_until(&mut h, "exact saved record", |s| {
        s.conventions
            .state()
            .and_then(|s| s.record())
            .is_some_and(|r| r.revision.get() == 2)
    });
    settle(&mut h);
    let exported = root.file("exported.md", b"old output");
    files.saves.lock().unwrap().push(exported.clone());
    click(&mut h, Role::Button, "Download Markdown");
    assert_eq!(
        std::fs::read(&exported).unwrap(),
        b"# Changed source\r\n\xce\xa9\0 retained\n"
    );
    let portable = root.file("portable-export.json", b"");
    files.saves.lock().unwrap().push(portable.clone());
    click(&mut h, Role::Button, "Export portable convention");
    let exported: serde_json::Value =
        serde_json::from_reader(File::open(portable).unwrap()).unwrap();
    assert_eq!(
        exported["markdown"]["text"],
        "# Changed source\r\nΩ\0 retained\n"
    );
    assert_eq!(exported["slug"], "source-literal");
    click(&mut h, Role::Button, "Versions");
    click(&mut h, Role::Button, "Load convention versions");
    let state = h.state().conventions.state().unwrap();
    assert_eq!(state.versions.len(), 2);
    assert_eq!(state.versions[0].id, first.id);
    click(&mut h, Role::Button, "Inspect version 1 · Native source");
    assert_eq!(
        h.state()
            .conventions
            .state()
            .unwrap()
            .reader
            .view
            .as_ref()
            .unwrap()
            .passage
            .as_ref()
            .unwrap()
            .text
            .0,
        "# Native source\r\nλ and NUL \0 kept\n"
    );
    click(&mut h, Role::Button, "Back to previous convention");
    assert_eq!(
        h.state()
            .conventions
            .state()
            .unwrap()
            .record()
            .unwrap()
            .revision
            .get(),
        2
    );
    click(&mut h, Role::Button, "Edit convention");
    step_until(&mut h, "complete source before raw edit", |s| {
        s.conventions
            .state()
            .unwrap()
            .draft()
            .is_some_and(|d| d.body_complete && !d.loading)
    });
    click(&mut h, Role::ComboBox, "Raw source change");
    click(&mut h, Role::Button, "Set raw source");
    replace(&mut h, "Convention raw source", "Raw literal Ω\0\r\n");
    click(&mut h, Role::Button, "Save convention revision");
    step_until(&mut h, "raw source saved", |s| {
        s.conventions
            .state()
            .unwrap()
            .record()
            .is_some_and(|r| r.revision.get() == 3)
    });
    click(&mut h, Role::Button, "Read raw source");
    assert_eq!(
        h.state()
            .conventions
            .state()
            .unwrap()
            .reader
            .view
            .as_ref()
            .unwrap()
            .passage
            .as_ref()
            .unwrap()
            .text
            .0,
        "Raw literal Ω\0\r\n"
    );
    click(&mut h, Role::Button, "Edit convention");
    step_until(&mut h, "complete raw opening source", |s| {
        s.conventions
            .state()
            .unwrap()
            .draft()
            .is_some_and(|d| d.body_complete && !d.loading)
    });
    click(&mut h, Role::ComboBox, "Raw source change");
    click(&mut h, Role::Button, "Remove raw source");
    click(&mut h, Role::Button, "Save convention revision");
    step_until(&mut h, "raw removal saved", |s| {
        s.conventions
            .state()
            .unwrap()
            .record()
            .is_some_and(|r| r.revision.get() == 4 && r.raw_content.is_none())
    });
}
#[test]
fn native_registered_import_loads_all_source_pages_and_stale_save_keeps_opening_source() {
    let _native_case = native_runtime_case();
    let root = Root::new("conventions-real-stale");
    let engine = start(&root);
    let backend = engine.backend();
    let files = Files::default();
    let mut h = open(backend.clone(), &root, files.clone());
    let text = format!(
        "# Captured\n{}\nlast λ\0",
        "literal\0日本語\r\n".repeat(12000)
    );
    let raw = "raw Ω\0\r\n".repeat(9000);
    let path = manifest(&root, "captured", "Captured source", &text, Some(&raw));
    import(&mut h, &files, path);
    let first = select(&mut h, "Captured source");
    click(&mut h, Role::Button, "Edit convention");
    step_until(&mut h, "whole captured source", |s| {
        s.conventions
            .state()
            .and_then(|s| s.draft())
            .is_some_and(|d| d.body_complete && !d.loading)
    });
    let d = h.state().conventions.state().unwrap().draft().unwrap();
    assert_eq!(&**d.markdown, text);
    assert_eq!(&**d.raw, raw);
    replace(&mut h, "Convention title", "My retained title");
    let frame = cop::command(
        &*backend,
        "conventions.edit",
        serde_json::json!({"scope":first.scope,"conventionId":first.id,"expectedRevision":first.revision,"content":first.content,"title":"Concurrent title","sortOrder":"91"}),
    );
    let concurrent = cop::changed(backend.command("conventions.edit", &frame))
        .unwrap_or_else(|e| panic!("actual concurrent edit failed: {}", e.describe()));
    assert!(matches!(concurrent, cop::Change::Record(_)));
    click(&mut h, Role::Button, "Save convention revision");
    step_until(&mut h, "stale save refusal", |s| {
        s.conventions
            .state()
            .and_then(|s| s.draft())
            .is_some_and(|d| d.error.is_some() && !d.busy)
    });
    let d = h.state().conventions.state().unwrap().draft().unwrap();
    assert_eq!(d.target().unwrap().revision.get(), 1);
    assert_eq!(d.title, "My retained title");
    assert_eq!(&**d.markdown, text);
    assert_eq!(&**d.raw, raw);
    click(&mut h, Role::Button, "Refresh conventions");
    let d = h.state().conventions.state().unwrap().draft().unwrap();
    assert_eq!(d.target().unwrap().revision.get(), 1);
    assert_eq!(d.title, "My retained title");
    assert_eq!(&**d.markdown, text);
    click(&mut h, Role::Button, "Edit");
    click(
        &mut h,
        Role::Button,
        "Keep draft and rebase to inspected revision",
    );
    click(&mut h, Role::Button, "Save convention revision");
    step_until(&mut h, "explicit rebased save", |s| {
        s.conventions
            .state()
            .and_then(|s| s.record())
            .is_some_and(|r| r.revision.get() == 3)
    });
    assert_eq!(
        h.state()
            .conventions
            .state()
            .unwrap()
            .record()
            .unwrap()
            .sort_order
            .0,
        "91",
        "explicit source rebase must not overwrite unedited organization fields"
    );
    let output = root.file("raw-output.txt", b"");
    files.saves.lock().unwrap().push(output.clone());
    click(&mut h, Role::Button, "Download raw source");
    assert_eq!(std::fs::read(output).unwrap(), raw.as_bytes());
}
#[test]
fn native_registered_unknown_artifact_save_recovers_exact_pipeline_after_reopen() {
    let _native_case = native_runtime_case();
    let root = Root::new("conventions-real-unknown");
    let engine = start(&root);
    let real = engine.backend();
    let lossy = LoseAcks::over(real, "artifacts.save", 4);
    let files = Files::default();
    let source = manifest(
        &root,
        "recover-source",
        "Recovered source",
        "# Saved once\n",
        None,
    );
    let mut h = open(lossy.clone(), &root, files.clone());
    files.picked.files.lock().unwrap().push(vec![source]);
    click(&mut h, Role::Button, "Import convention");
    assert!(h.state().conventions.state().unwrap().busy);
    let before = lossy.frames.lock().unwrap().clone();
    assert_eq!(before.len(), 4);
    assert!(before.windows(2).all(|p| p[0] == p[1]));
    assert_eq!(h.state().memory().pending.commands.len(), 1);
    h.state_mut().on_exit();
    drop(h);
    let mut h = open(lossy.clone(), &root, Files::default());
    imported(&mut h);
    let frames = lossy.frames.lock().unwrap();
    assert!(frames.len() > 4);
    assert!(frames.iter().all(|f| f == &before[0]));
    drop(frames);
    let state = h.state().conventions.state().unwrap();
    assert_eq!(
        state
            .items
            .iter()
            .filter(|r| r.slug.0 == "recover-source")
            .count(),
        1
    );
    assert!(h.state().memory().pending.commands.is_empty());
    assert!(h.state().conventions.pipelines.is_empty());
}

#[test]
fn native_registered_resource_bytes_lifecycle_and_deletion_use_exact_owner_revisions() {
    let _native_case = native_runtime_case();
    let root = Root::new("conventions-real-resources");
    let engine = start(&root);
    let files = Files::default();
    let mut h = open(engine.backend(), &root, files.clone());
    let source = manifest(
        &root,
        "resource-guide",
        "Resource guide",
        "# Resource source\n",
        None,
    );
    import(&mut h, &files, source);
    select(&mut h, "Resource guide");
    click(&mut h, Role::Button, "Resources");
    click(&mut h, Role::Button, "Load convention resources");
    assert!(h.state().conventions.state().unwrap().resources.is_empty());
    let bytes = b"\0\xff\x01binary\r\n".repeat(7000);
    let path = root.file("binary-resource.bin", &bytes);
    files.picked.files.lock().unwrap().push(vec![path]);
    click(&mut h, Role::Button, "Upload convention resource");
    replace(&mut h, "New resource name", "Literal attachment");
    let media = "x".repeat(200);
    replace(&mut h, "New resource media description", &media);
    click(&mut h, Role::Button, "Attach selected resource");
    step_until(&mut h, "attached resource commit", |s| {
        s.conventions
            .state()
            .and_then(|s| s.record())
            .is_some_and(|r| r.revision.get() == 2)
    });
    settle(&mut h);
    click(&mut h, Role::Button, "Resources");
    click(&mut h, Role::Button, "Load convention resources");
    let first = h.state().conventions.state().unwrap().resources[0].clone();
    assert_eq!(first.media_type.0, media);
    assert_eq!(first.content.byte_length.get(), bytes.len() as i64);
    assert_eq!(first.content.media_type, "application/octet-stream");
    let output = root.file("download.bin", b"old");
    files.saves.lock().unwrap().push(output.clone());
    click(&mut h, Role::Button, "Download resource");
    assert_eq!(std::fs::read(output).unwrap(), bytes);
    click(&mut h, Role::Button, "Edit resource");
    replace(&mut h, "Resource name", "Renamed attachment");
    replace(&mut h, "Resource media description", "");
    click(&mut h, Role::CheckBox, "Retired resource");
    click(&mut h, Role::Button, "Save resource metadata");
    step_until(&mut h, "resource metadata commit", |s| {
        s.conventions
            .state()
            .and_then(|s| s.record())
            .is_some_and(|r| r.revision.get() == 3)
    });
    settle(&mut h);
    click(&mut h, Role::Button, "Resources");
    click(&mut h, Role::Button, "Load convention resources");
    let changed = h.state().conventions.state().unwrap().resources[0].clone();
    assert_eq!(changed.id, first.id);
    assert_eq!(changed.name.0, "Renamed attachment");
    assert!(changed.media_type.0.is_empty());
    assert!(matches!(
        changed.lifecycle,
        devworks_core::protocol::records::ConventionLifecycle::Retired
    ));
    click(&mut h, Role::Button, "Edit resource");
    let replacement = b"replacement\0\xfe\r\n".repeat(5000);
    let new_path = root.file("replacement-resource.bin", &replacement);
    files.picked.files.lock().unwrap().push(vec![new_path]);
    click(&mut h, Role::Button, "Choose replacement resource");
    assert!(
        h.state()
            .conventions
            .state()
            .unwrap()
            .resource_drafts
            .get(first.id.as_str())
            .unwrap()
            .replacement
            .is_some()
    );
    click(&mut h, Role::Button, "Save resource changes");
    step_until(&mut h, "replacement bytes committed", |s| {
        s.conventions
            .state()
            .and_then(|s| s.record())
            .is_some_and(|r| r.revision.get() == 4)
    });
    settle(&mut h);
    click(&mut h, Role::Button, "Resources");
    click(&mut h, Role::Button, "Load convention resources");
    let replaced = h.state().conventions.state().unwrap().resources[0].clone();
    assert_eq!(replaced.id, first.id);
    assert!(
        replaced.media_type.0.is_empty(),
        "replacement retains the literal descriptive media value"
    );
    let replaced_output = root.file("replacement-download.bin", b"");
    files.saves.lock().unwrap().push(replaced_output.clone());
    click(&mut h, Role::Button, "Download resource");
    assert_eq!(std::fs::read(replaced_output).unwrap(), replacement);
    click(&mut h, Role::Button, "Delete resource");
    click(&mut h, Role::Button, "Confirm convention deletion");
    step_until(&mut h, "resource deletion commit", |s| {
        s.conventions
            .state()
            .and_then(|s| s.record())
            .is_some_and(|r| r.revision.get() == 5)
    });
    settle(&mut h);
    click(&mut h, Role::Button, "Resources");
    click(&mut h, Role::Button, "Load convention resources");
    let deleted = h
        .state()
        .conventions
        .state()
        .unwrap()
        .resources
        .iter()
        .find(|r| r.id == first.id)
        .unwrap();
    assert!(matches!(
        deleted.lifecycle,
        devworks_core::protocol::records::ConventionLifecycle::Deleted
    ));
    assert!(deleted.deleted_at_ms.is_some());
}

#[test]
fn native_registered_applicability_retirement_and_virtual_document_resources_remain_inspectable() {
    let _native_case = native_runtime_case();
    let root = Root::new("conventions-real-lifecycle");
    let engine = start(&root);
    let files = Files::default();
    let mut h = open(engine.backend(), &root, files.clone());
    let source = manifest(
        &root,
        "literal-root",
        "Lifecycle source",
        "# Lifecycle body\n\nA real body resource.\n",
        None,
    );
    h.input_mut().dropped_files.push(Arc::new(Dropped(source)));
    settle(&mut h);
    imported(&mut h);
    assert!(
        h.state().knowledge.import.is_none(),
        "Conventions drops never enter the unrelated Knowledge-entry importer"
    );
    select(&mut h, "Lifecycle source");
    click(&mut h, Role::Button, "Retire convention");
    step_until(&mut h, "retired source record", |s| {
        s.conventions
            .state()
            .and_then(|s| s.record())
            .is_some_and(|r| {
                matches!(
                    r.lifecycle,
                    devworks_core::protocol::records::ConventionLifecycle::Retired
                )
            })
    });
    settle(&mut h);
    click(&mut h, Role::Button, "Activate convention");
    step_until(&mut h, "reactivated source record", |s| {
        s.conventions
            .state()
            .and_then(|s| s.record())
            .is_some_and(|r| {
                r.revision.get() == 3
                    && matches!(
                        r.lifecycle,
                        devworks_core::protocol::records::ConventionLifecycle::Active
                    )
            })
    });
    settle(&mut h);
    click(&mut h, Role::Button, "Edit convention");
    step_until(&mut h, "complete lifecycle draft", |s| {
        s.conventions
            .state()
            .and_then(|s| s.draft())
            .is_some_and(|d| d.body_complete && !d.loading)
    });
    replace(&mut h, "Applicability label 1", "literal, comma label");
    click(&mut h, Role::Button, "Apply applicability and lifecycle");
    step_until(&mut h, "literal applicability commit", |s| {
        s.conventions
            .state()
            .and_then(|s| s.record())
            .is_some_and(|r| r.revision.get() == 4)
    });
    settle(&mut h);
    assert_eq!(
        h.state()
            .conventions
            .state()
            .unwrap()
            .record()
            .unwrap()
            .applicability
            .0[0]
            .0,
        "literal, comma label"
    );
    click(&mut h, Role::Button, "Resources");
    click(&mut h, Role::ComboBox, "Resource view");
    click(&mut h, Role::Button, "Document catalog");
    click(&mut h, Role::Button, "Load convention resources");
    assert_eq!(
        h.state()
            .conventions
            .state()
            .unwrap()
            .resource_catalog
            .len(),
        1
    );
    click(&mut h, Role::ComboBox, "Resource view");
    click(&mut h, Role::Button, "Exact root document");
    replace(&mut h, "Exact root slug", "literal-root");
    click(&mut h, Role::Button, "Load convention resources");
    assert_eq!(
        h.state()
            .conventions
            .state()
            .unwrap()
            .resource_document
            .as_ref()
            .unwrap()
            .passage
            .as_ref()
            .unwrap()
            .text
            .0,
        "# Lifecycle body\n\nA real body resource.\n"
    );
    click(&mut h, Role::Button, "Read");
    click(&mut h, Role::Button, "Delete convention");
    click(&mut h, Role::Button, "Confirm convention deletion");
    settle(&mut h);
    click(&mut h, Role::Button, "Refresh conventions");
    assert!(h.state().conventions.state().unwrap().items.is_empty());
}

#[test]
fn native_registered_hierarchical_move_preserves_subtree_scope_and_independent_draft() {
    let _native_case = native_runtime_case();
    use devworks_core::protocol::results::OperationResult;
    let root = Root::new("conventions-real-move");
    let engine = start(&root);
    let backend = engine.backend();
    let project=match devworks_desktop::ops::command_result(backend.command("projects.create",&cop::command(&*backend,"projects.create",serde_json::json!({"name":"Convention destination","description":"Scoped convention fixture","repositoryIds":[]})))).unwrap(){OperationResult::ProjectsCreate{value}=>value,other=>panic!("actual projectcreation: {other:?}")};
    let files = Files::default();
    let mut h = open(backend, &root, files.clone());
    let parent = manifest(
        &root,
        "parent/root",
        "Parent guide",
        "# Parent source\n",
        None,
    );
    import(&mut h, &files, parent);
    let child = manifest_at(
        &root,
        "child\0/entry",
        "Child guide",
        "# Child source\n",
        None,
        &["parent/root".into()],
    );
    import(&mut h, &files, child);
    let first = select(&mut h, "Parent guide");
    click(&mut h, Role::Button, "Edit convention");
    step_until(&mut h, "complete move-source draft", |s| {
        s.conventions
            .state()
            .and_then(|s| s.draft())
            .is_some_and(|d| d.body_complete && !d.loading)
    });
    replace(&mut h, "Convention title", "Unsaved parent title");
    click(&mut h, Role::DisclosureTriangle, "Move and organize");
    click(&mut h, Role::ComboBox, "Move destination");
    click(&mut h, Role::Button, "Convention destination");
    click(&mut h, Role::Button, "Load destination parents");
    click(&mut h, Role::Button, "Move convention");
    step_until(&mut h, "subtree move confirmed", |s| {
        s.conventions
            .state()
            .is_some_and(|v| !v.busy && v.items.is_empty())
    });
    let draft = h
        .state()
        .conventions
        .states
        .get(&Scope::Workspace)
        .unwrap()
        .drafts
        .get(first.id.as_str())
        .unwrap();
    assert_eq!(draft.title, "Unsaved parent title");
    assert_eq!(draft.target().unwrap().revision.get(), 1);
    click(&mut h, Role::ComboBox, "Convention scope");
    click(&mut h, Role::Button, "Convention destination");
    step_until(&mut h, "actual project-scoped subtree listing", |s| {
        s.conventions
            .state()
            .is_some_and(|v| v.loaded && v.items.len() == 2)
    });
    assert_eq!(
        h.state().conventions.scope,
        Scope::Project(project.id.0.as_str().into())
    );
    click(&mut h, Role::DisclosureTriangle, "Open an exact hierarchy");
    click(&mut h, Role::Button, "Add Hierarchy component");
    replace(&mut h, "Hierarchy component 1", "parent/root");
    click(&mut h, Role::Button, "Add Hierarchy component");
    replace(&mut h, "Hierarchy component 2", "child\0/entry");
    click(&mut h, Role::Button, "Open exact convention hierarchy");
    let child = h
        .state()
        .conventions
        .state()
        .unwrap()
        .record()
        .unwrap()
        .clone();
    assert_eq!(child.title.0, "Child guide");
    assert_eq!(child.slug.0, "child\0/entry");
    let target = h
        .state()
        .conventions
        .state()
        .unwrap()
        .reader
        .target
        .as_ref()
        .unwrap();
    assert!(
        target.path.is_none(),
        "later source pages bind the served identity, never re-resolve mutable hierarchy"
    );
    assert_eq!(target.id, child.id.as_str());
    assert_eq!(child.parent_id.as_ref(), Some(&first.id));
    assert_eq!(child.revision.get(), 2);
    assert_eq!(
        serde_json::to_value(&child.scope).unwrap(),
        serde_json::json!({"kind":"Project","projectId":project.id})
    );
    click(&mut h, Role::ComboBox, "Convention scope");
    click(&mut h, Role::Button, "Workspace");
    assert_eq!(
        h.state()
            .conventions
            .states
            .get(&Scope::Workspace)
            .unwrap()
            .drafts
            .get(first.id.as_str())
            .unwrap()
            .title,
        "Unsaved parent title"
    );
}

#[path = "../embedding_spaces/fixture.rs"]
mod embedding;

fn qualify(h: &mut Ui, backend: &dyn Backend, http: &embedding::RecordedEmbedding) {
    embedding::create_candidate(h, http, false);
    click(
        h,
        Role::Button,
        "Queue qualification for Recorded space (may incur usage)",
    );
    let until = std::time::Instant::now() + SETTLE_BOUND;
    loop {
        let page = devworks_desktop::ops::embedding_space_page(backend.query(
            "router.embedding_spaces",
            &devworks_desktop::ops::router_embedding_spaces(backend, None),
        ))
        .unwrap();
        let status = serde_json::to_value(&page.items.0[0].status).unwrap();
        if status == "Qualified" {
            break;
        }
        assert!(status == "Qualifying", "actual qualification: {page:?}");
        assert!(
            std::time::Instant::now() < until,
            "actual qualification closes: {page:?}"
        );
        h.step();
        std::thread::yield_now();
    }
    click(h, Role::Button, "Reload embedding spaces");
    click(h, Role::Button, "Use embedding space Recorded space");
    click_nav(h, "Knowledge");
    settle(h);
    assert_eq!(h.state().route_memory(Route::Knowledge).tab, 2);
}

fn indexed(h: &mut Ui) {
    let old = h.state().conventions.state().unwrap().operation_ids.len();
    click(h, Role::Button, "Index convention");
    step_until(h, "actual index accepted", |s| {
        s.conventions.state().unwrap().operation_ids.len() > old
    });
    let id = h
        .state()
        .conventions
        .state()
        .unwrap()
        .operation_ids
        .last()
        .unwrap()
        .clone();
    step_until(h, "actual accounted index publication", |s| {
        let state = s.conventions.state().unwrap();
        let Some(op) = state.operation_statuses.get(&id) else {
            return false;
        };
        assert!(
            !matches!(
                op.phase,
                ConventionOperationPhase::Failed | ConventionOperationPhase::Cancelled
            ),
            "{op:?}"
        );
        matches!(op.phase, ConventionOperationPhase::Completed)
    });
    step_until(h, "same-revision source reflects completed index", |s| {
        s.conventions.state().unwrap().record().is_some_and(|r| {
            matches!(
                r.index.status,
                devworks_core::protocol::records::ConventionIndexPhase::Ready
            )
        })
    });
}

/// Only loses a real query reply. Every success and all records still come from the runtime.
struct QueryWire {
    inner: Arc<dyn Backend>,
    lose: std::sync::atomic::AtomicBool,
    frames: Mutex<Vec<Vec<u8>>>,
}
impl QueryWire {
    fn replied(
        &self,
        op: &str,
        frame: &[u8],
        outcome: devworks_core::gateway::QueryOutcome,
    ) -> devworks_core::gateway::QueryOutcome {
        if op != "conventions.search" {
            return outcome;
        }
        self.frames.lock().unwrap().push(frame.to_vec());
        if self.lose.swap(false, std::sync::atomic::Ordering::SeqCst) {
            Err(devworks_core::gateway::GatewayFailure::HandlerContract(
                "recorded lost query acknowledgment".into(),
            ))
        } else {
            outcome
        }
    }
}
impl Backend for QueryWire {
    fn query(&self, op: &str, frame: &[u8]) -> devworks_core::gateway::QueryOutcome {
        self.replied(op, frame, self.inner.query(op, frame))
    }
    fn supports_query_cancellation(&self) -> bool {
        self.inner.supports_query_cancellation()
    }
    fn query_cancellable(
        &self,
        op: &str,
        frame: &[u8],
        options: &devworks_core::ports::storage::ReadOptions,
    ) -> devworks_core::gateway::QueryOutcome {
        self.replied(op, frame, self.inner.query_cancellable(op, frame, options))
    }
    fn command(&self, op: &str, frame: &[u8]) -> devworks_core::gateway::CommandOutcome {
        self.inner.command(op, frame)
    }
    fn mint_workspace_upload(
        &self,
        p: &std::path::Path,
    ) -> Result<devworks_core::protocol::Id, String> {
        self.inner.mint_workspace_upload(p)
    }
    fn mint_project_upload(
        &self,
        id: &devworks_core::protocol::ProjectId,
        p: &std::path::Path,
    ) -> Result<devworks_core::protocol::Id, String> {
        self.inner.mint_project_upload(id, p)
    }
    fn keep_awake_health(&self) -> devworks_core::settings::view::Health {
        self.inner.keep_awake_health()
    }
    fn hints(&self) -> Option<tokio::sync::watch::Receiver<u64>> {
        self.inner.hints()
    }
    fn set_waker(&self, w: devworks_desktop::backend::Waker) {
        self.inner.set_waker(w)
    }
    fn token(&self, p: &str) -> String {
        self.inner.token(p)
    }
    fn scratch_dir(&self) -> PathBuf {
        self.inner.scratch_dir()
    }
    fn read_stream(&self, id: &devworks_core::protocol::Id, n: u64) -> Result<Vec<u8>, String> {
        self.inner.read_stream(id, n)
    }
}

#[test]
fn native_persisted_embedding_setup_indexes_searches_and_replays_lost_query_without_new_usage() {
    let _native_case = native_runtime_case();
    let root = Root::new("conventions-real-hybrid");
    let http = Arc::new(embedding::RecordedEmbedding::default());
    let engine = embedding::configured(&root, http.clone());
    let real = engine.backend();
    let backend = Arc::new(QueryWire {
        inner: real.clone(),
        lose: std::sync::atomic::AtomicBool::new(false),
        frames: Mutex::new(Vec::new()),
    });
    let files = Files::default();
    let mut h = open(backend.clone(), &root, files.clone());
    qualify(&mut h, &*real, &http);
    assert_eq!(
        http.requests.lock().unwrap().len(),
        1,
        "only the recorded qualification request"
    );
    let path = manifest(
        &root,
        "hybrid-guide",
        "Hybrid guide",
        "# Practices\n## Exact section\nNative alpha guidance and authored [link text](https://example.invalid/no-fetch).\n",
        None,
    );
    import(&mut h, &files, path);
    let record = select(&mut h, "Hybrid guide");
    indexed(&mut h);
    let indexed_requests = http.requests.lock().unwrap().len();
    assert!(
        indexed_requests > 1,
        "actual body indexing uses the persisted selected space"
    );
    backend
        .lose
        .store(true, std::sync::atomic::Ordering::SeqCst);
    replace(&mut h, "Find convention", "alpha");
    click(&mut h, Role::Button, "Search conventions");
    let recovery = h
        .state()
        .conventions
        .state()
        .unwrap()
        .search_recovery
        .clone()
        .expect("lost result retains exact original query");
    assert_eq!(backend.frames.lock().unwrap().len(), 1);
    assert_eq!(http.requests.lock().unwrap().len(), indexed_requests + 1);
    h.state_mut().on_exit();
    drop(h);
    let mut h = open(backend.clone(), &root, files);
    assert_eq!(
        h.state()
            .conventions
            .state()
            .unwrap()
            .search_recovery
            .as_ref()
            .unwrap()
            .frame,
        recovery.frame
    );
    replace(&mut h, "Find convention", "a changed unsent query");
    click(&mut h, Role::Button, "Confirm search outcome");
    let state = h.state().conventions.state().unwrap();
    assert!(state.search_recovery.is_none());
    assert!(matches!(
        state.retrieval.as_ref().unwrap().mode,
        devworks_core::protocol::records::ConventionRetrievalMode::Hybrid
    ));
    assert_eq!(state.hits[0].convention.id, record.id);
    assert_eq!(
        state.hits[0].convention.revision.get(),
        record.revision.get()
    );
    assert!(
        state.search_preparation.is_some(),
        "actual preparation identity is visible"
    );
    assert_eq!(
        state.text, "a changed unsent query",
        "confirmation never adopts current input as a new intent"
    );
    assert_eq!(
        http.requests.lock().unwrap().len(),
        indexed_requests + 1,
        "exact replay observes the owned result, never sends another embedding"
    );
    let frames = backend.frames.lock().unwrap();
    assert_eq!(frames.len(), 2);
    assert!(frames.iter().all(|f| f == &recovery.frame));
    drop(frames);
    let path_generation = h.state().conventions.state().unwrap().hits[0]
        .indexed_path
        .generation_id
        .clone();
    click(&mut h, Role::Button, "Read section path");
    step_until(&mut h, "ranked section path read", |s| {
        s.conventions
            .state()
            .unwrap()
            .reader
            .view
            .as_ref()
            .is_some_and(|v| {
                v.passage
                    .as_ref()
                    .is_some_and(|p| p.text.0.contains("Practices"))
            })
    });
    assert_eq!(
        h.state()
            .conventions
            .state()
            .unwrap()
            .reader
            .target
            .as_ref()
            .unwrap()
            .section
            .as_ref()
            .unwrap()
            .generation_id,
        path_generation
    );
}

#[test]
fn native_full_legal_markdown_with_nul_is_verified_before_edit_and_never_truncated_on_save() {
    let _native_case = native_runtime_case();
    let root = Root::new("conventions-real-max-source");
    let engine = start(&root);
    let files = Files::default();
    let mut h = open(engine.backend(), &root, files.clone());
    let mut text =
        "x".repeat(devworks_core::services::conventions::model::MAX_MARKDOWN_BYTES as usize);
    text.replace_range(0..8, "# Legal\n");
    let len = text.len();
    text.replace_range(len - 8..len, "tail\0end");
    let source = manifest(&root, "maximum-source", "Maximum source", &text, None);
    import(&mut h, &files, source);
    let record = select(&mut h, "Maximum source");
    assert_eq!(
        record.content.as_ref().unwrap().byte_length.get() as usize,
        text.len()
    );
    click(&mut h, Role::Button, "Edit convention");
    step_until(&mut h, "all legal source pages verified", |s| {
        s.conventions
            .state()
            .unwrap()
            .draft()
            .is_some_and(|d| d.body_complete && !d.loading)
    });
    assert_eq!(
        &**h.state()
            .conventions
            .state()
            .unwrap()
            .draft()
            .unwrap()
            .markdown,
        text
    );
    replace(&mut h, "Convention title", "Maximum source retained");
    click(&mut h, Role::Button, "Save convention revision");
    step_until(&mut h, "full-source revision saved", |s| {
        s.conventions
            .state()
            .unwrap()
            .record()
            .is_some_and(|r| r.revision.get() == 2)
    });
    let output = root.file("maximum-output.md", b"");
    files.saves.lock().unwrap().push(output.clone());
    click(&mut h, Role::Button, "Download Markdown");
    assert_eq!(std::fs::read(output).unwrap(), text.as_bytes());
}

#[derive(Default)]
struct BodyGate {
    block: std::sync::atomic::AtomicBool,
    entered: std::sync::atomic::AtomicBool,
    cancelled: std::sync::atomic::AtomicBool,
}
struct GateRelease(Arc<BodyGate>);
impl Drop for GateRelease {
    fn drop(&mut self) {
        self.0
            .block
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }
}
struct ControlledEmbedding {
    recorded: Arc<embedding::RecordedEmbedding>,
    gate: Arc<BodyGate>,
}
impl devworks_core::ports::http::HttpTransport for ControlledEmbedding {
    fn send(
        &self,
        request: &devworks_core::ports::http::HttpRequest,
    ) -> Result<devworks_core::ports::http::HttpResponse, devworks_core::ports::http::HttpError>
    {
        devworks_core::ports::http::HttpTransport::send(&*self.recorded, request)
    }
}
struct ControlledStream {
    inner: Box<dyn devworks_core::ports::provider_http::HttpStream>,
    gate: Arc<BodyGate>,
    closed: Arc<std::sync::atomic::AtomicBool>,
}
struct ControlledCloser {
    inner: Arc<dyn devworks_core::ports::provider_http::StreamCloser>,
    closed: Arc<std::sync::atomic::AtomicBool>,
}
impl devworks_core::ports::provider_http::StreamCloser for ControlledCloser {
    fn close(&self) -> Result<(), devworks_core::ports::provider_http::TransportError> {
        self.closed.store(true, std::sync::atomic::Ordering::SeqCst);
        self.inner.close()
    }
}
impl devworks_core::ports::provider_http::HttpStream for ControlledStream {
    fn head(&self) -> &devworks_core::ports::provider_http::ResponseHead {
        self.inner.head()
    }
    fn closer(&self) -> Arc<dyn devworks_core::ports::provider_http::StreamCloser> {
        Arc::new(ControlledCloser {
            inner: self.inner.closer(),
            closed: self.closed.clone(),
        })
    }
    fn read(
        &mut self,
        out: &mut [u8],
        cancel: &devworks_core::ports::storage::Cancellation,
    ) -> Result<usize, devworks_core::ports::provider_http::TransportError> {
        use std::sync::atomic::Ordering::SeqCst;
        if self.gate.block.load(SeqCst) {
            self.gate.entered.store(true, SeqCst);
            while self.gate.block.load(SeqCst)
                && !cancel.is_cancelled()
                && !self.closed.load(SeqCst)
            {
                std::thread::park_timeout(std::time::Duration::from_millis(2));
            }
            if cancel.is_cancelled() || self.closed.load(SeqCst) {
                self.gate.cancelled.store(true, SeqCst);
                return Err(devworks_core::ports::provider_http::TransportError {
                    phase: devworks_core::ports::provider_http::SendPhase::AfterSend,
                    kind: devworks_core::ports::provider_http::TransportFailure::Cancelled,
                });
            }
        }
        self.inner.read(out, cancel)
    }
    fn close(&mut self) -> Result<(), devworks_core::ports::provider_http::TransportError> {
        self.closed.store(true, std::sync::atomic::Ordering::SeqCst);
        self.inner.close()
    }
}
impl devworks_core::ports::provider_http::StreamingHttp for ControlledEmbedding {
    fn open(
        &self,
        request: &devworks_core::ports::http::HttpRequest,
        limits: &devworks_core::ports::provider_http::StreamLimits,
        cancel: &devworks_core::ports::storage::Cancellation,
    ) -> Result<
        Box<dyn devworks_core::ports::provider_http::HttpStream>,
        devworks_core::ports::provider_http::TransportError,
    > {
        let inner = devworks_core::ports::provider_http::StreamingHttp::open(
            &*self.recorded,
            request,
            limits,
            cancel,
        )?;
        Ok(Box::new(ControlledStream {
            inner,
            gate: self.gate.clone(),
            closed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }))
    }
}
#[test]
fn native_search_cancel_reaches_owned_transport_and_keeps_unknown_custody_after_selection_changes()
{
    let _native_case = native_runtime_case();
    use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
    let root = Root::new("conventions-real-cancel");
    let http = Arc::new(embedding::RecordedEmbedding::default());
    let gate = Arc::new(BodyGate::default());
    let transport = Arc::new(ControlledEmbedding {
        recorded: http.clone(),
        gate: gate.clone(),
    });
    // Same audited common config: SessionSecrets and only a recorded in-process transport.
    let mut config = config(&root.0);
    config.http = Some(transport.clone());
    config.connection_tick = None;
    config.providers =
        Some(devworks_engine::providers::driver::ProviderComposition::new(transport));
    let engine = devworks_desktop::host::Engine::start(config).unwrap();
    let backend = Arc::new(QueryWire {
        inner: engine.backend(),
        lose: AtomicBool::new(false),
        frames: Mutex::new(Vec::new()),
    });
    let files = Files::default();
    let mut h = open(backend.clone(), &root, files.clone());
    qualify(&mut h, &*backend, &http);
    let source = manifest(
        &root,
        "cancel-guide",
        "Cancel guide",
        "# Search guidance\nNative alpha guidance.\n",
        None,
    );
    import(&mut h, &files, source);
    select(&mut h, "Cancel guide");
    indexed(&mut h);
    let before = http.requests.lock().unwrap().len();
    replace(&mut h, "Find convention", "alpha");
    h.get_by_role_and_label(Role::Button, "Search conventions")
        .scroll_to_me();
    settle(&mut h);
    let _release = GateRelease(gate.clone());
    gate.block.store(true, SeqCst);
    h.get_by_role_and_label(Role::Button, "Search conventions")
        .click();
    step_until(&mut h, "actual query body is in flight", |_| {
        gate.entered.load(SeqCst)
    });
    h.get_by_role_and_label(Role::Button, "Cancel search request")
        .scroll_to_me();
    for _ in 0..3 {
        h.step();
    }
    h.get_by_role_and_label(Role::Button, "Cancel search request")
        .click();
    step_until(&mut h, "owned query cancellation returns", |s| {
        s.conventions.state().unwrap().active_search.is_none()
    });
    assert!(
        gate.cancelled.load(SeqCst),
        "the actual native token interrupts the read"
    );
    assert!(
        h.state()
            .conventions
            .state()
            .unwrap()
            .search_recovery
            .is_some(),
        "cancel is not proof that a sent attempt has no unknown financial tail"
    );
    gate.block.store(false, SeqCst);
    click(&mut h, Role::Button, "Confirm search outcome");
    let state = h.state().conventions.state().unwrap();
    let preparation = state
        .search_preparation
        .clone()
        .expect("actual owned preparation metadata");
    assert!(cop::preparation_unsettled(&preparation.1));
    assert!(state.search_recovery.is_some());
    let exact = state.search_recovery.as_ref().unwrap().frame.clone();
    assert_eq!(
        http.requests.lock().unwrap().len(),
        before + 1,
        "confirmation observes the original native attempt"
    );
    click(&mut h, Role::Button, "Embedding setup");
    click(&mut h, Role::Button, "Stop using embedding space");
    click_nav(&mut h, "Knowledge");
    click(&mut h, Role::Button, "Confirm search outcome");
    let state = h.state().conventions.state().unwrap();
    assert!(
        state
            .list_error
            .as_ref()
            .is_some_and(|e| e.contains("CursorExpired")),
        "changed source must refuse before a new paid admission: {:?}",
        state.list_error
    );
    assert_eq!(
        state.search_preparation.as_ref().unwrap().1.operation_id,
        preparation.1.operation_id
    );
    assert_eq!(state.search_recovery.as_ref().unwrap().frame, exact);
    assert_eq!(http.requests.lock().unwrap().len(), before + 1);
    click(&mut h, Role::Button, "Start a new search");
    let state = h.state().conventions.state().unwrap();
    assert!(matches!(
        state.retrieval.as_ref().unwrap().mode,
        devworks_core::protocol::records::ConventionRetrievalMode::Lexical
    ));
    assert!(
        state.search_preparation.is_none(),
        "no selection means no invented query preparation"
    );
    assert_eq!(state.previous_searches.len(), 1);
    assert_eq!(
        state.previous_searches[0]
            .preparation
            .as_ref()
            .unwrap()
            .1
            .operation_id,
        preparation.1.operation_id
    );
    assert_eq!(http.requests.lock().unwrap().len(), before + 1);
    h.state_mut().on_exit();
    drop(h);
    let h = open(backend, &root, files);
    assert_eq!(
        h.state().conventions.state().unwrap().previous_searches[0]
            .preparation
            .as_ref()
            .unwrap()
            .1
            .operation_id,
        preparation.1.operation_id
    );
}

#[test]
fn native_new_conventions_use_server_order_and_preserve_an_explicit_empty_slug() {
    let _native_case = native_runtime_case();
    let root = Root::new("conventions-real-default-order");
    let engine = start(&root);
    let mut h = open(engine.backend(), &root, Files::default());
    for (title, slug, order, expected) in [
        ("First ordered", "first", None, 0),
        ("Literal empty slug", "", None, 1),
        ("Explicit zero", "zero", Some("0"), 0),
    ] {
        click(&mut h, Role::Button, "New convention");
        assert!(
            h.state()
                .conventions
                .state()
                .unwrap()
                .new_draft
                .as_ref()
                .unwrap()
                .order
                .is_empty()
        );
        replace(&mut h, "Convention title", title);
        replace(&mut h, "Convention slug", slug);
        replace(&mut h, "Convention Markdown", "# Exact source\n");
        if let Some(order) = order {
            replace(&mut h, "Initial sort order", order);
        }
        click(&mut h, Role::Button, "Create convention");
        imported(&mut h);
        let record = select(&mut h, title);
        assert_eq!(record.slug.0, slug);
        assert_eq!(record.sort_order.0, expected.to_string());
        click(&mut h, Role::Button, "Resume new convention");
        click(&mut h, Role::Button, "Discard convention draft");
    }
    click(
        &mut h,
        Role::DisclosureTriangle,
        "Open a convention by slug",
    );
    // The default empty input is a literal selector, distinct from an omitted selector.
    click(&mut h, Role::Button, "Open convention slug");
    assert_eq!(
        h.state()
            .conventions
            .state()
            .unwrap()
            .record()
            .unwrap()
            .title
            .0,
        "Literal empty slug"
    );
}

#[test]
fn native_owned_index_operation_can_be_inspected_cancelled_and_replaced_by_scope_index() {
    let _native_case = native_runtime_case();
    use std::sync::atomic::Ordering::SeqCst;
    let root = Root::new("conventions-real-index-cancel");
    let http = Arc::new(embedding::RecordedEmbedding::default());
    let gate = Arc::new(BodyGate::default());
    let transport = Arc::new(ControlledEmbedding {
        recorded: http.clone(),
        gate: gate.clone(),
    });
    let mut c = config(&root.0);
    c.http = Some(transport.clone());
    c.connection_tick = None;
    c.providers = Some(devworks_engine::providers::driver::ProviderComposition::new(transport));
    let engine = devworks_desktop::host::Engine::start(c).unwrap();
    let backend = engine.backend();
    let files = Files::default();
    let mut h = open(backend.clone(), &root, files.clone());
    qualify(&mut h, &*backend, &http);
    let source = manifest(
        &root,
        "index-cancel",
        "Index cancellation guide",
        "# Index guidance\nNative alpha text.\n",
        None,
    );
    import(&mut h, &files, source);
    let record = select(&mut h, "Index cancellation guide");
    let _release = GateRelease(gate.clone());
    gate.block.store(true, SeqCst);
    click(&mut h, Role::Button, "Index convention");
    let id = h
        .state()
        .conventions
        .state()
        .unwrap()
        .operation_ids
        .last()
        .unwrap()
        .clone();
    step_until(&mut h, "owned index provider body starts", |_| {
        gate.entered.load(SeqCst)
    });
    click(&mut h, Role::Button, "Operations");
    step_until(&mut h, "actual index cancellation is available", |s| {
        s.conventions
            .state()
            .unwrap()
            .operation_statuses
            .get(&id)
            .is_some_and(|op| op.can_cancel)
    });
    h.query_all_by_role_and_label(Role::Button, "Cancel convention operation")
        .find(|node| !node.accesskit_node().is_disabled())
        .expect("the exact active index is cancellable")
        .scroll_to_me();
    settle(&mut h);
    h.query_all_by_role_and_label(Role::Button, "Cancel convention operation")
        .find(|node| !node.accesskit_node().is_disabled())
        .unwrap()
        .click();
    settle(&mut h);
    step_until(
        &mut h,
        "after-send index cancellation retains its unknown outcome",
        |s| {
            s.conventions
                .state()
                .unwrap()
                .operation_statuses
                .get(&id)
                .is_some_and(|op| matches!(op.phase, ConventionOperationPhase::Unknown))
        },
    );
    assert!(
        gate.cancelled.load(SeqCst),
        "durable cancellation interrupts the actual in-flight provider"
    );
    assert_eq!(
        h.state()
            .conventions
            .state()
            .unwrap()
            .record()
            .unwrap()
            .revision
            .get(),
        record.revision.get(),
        "cancellation cannot invent a content revision"
    );
    gate.block.store(false, SeqCst);
    click(&mut h, Role::Button, "Refresh conventions");
    let old = h.state().conventions.state().unwrap().operation_ids.len();
    click(&mut h, Role::Button, "Index this scope");
    step_until(&mut h, "scope index accepted", |s| {
        s.conventions.state().unwrap().operation_ids.len() > old
    });
    let scope_id = h
        .state()
        .conventions
        .state()
        .unwrap()
        .operation_ids
        .last()
        .unwrap()
        .clone();
    assert_ne!(scope_id, id);
    step_until(&mut h, "real scope indexing completed", |s| {
        s.conventions
            .state()
            .unwrap()
            .operation_statuses
            .get(&scope_id)
            .is_some_and(|op| matches!(op.phase, ConventionOperationPhase::Completed))
    });
    assert_eq!(
        h.state()
            .conventions
            .state()
            .unwrap()
            .operation_statuses
            .get(&id)
            .unwrap()
            .operation
            .operation_id
            .0
            .as_str(),
        id
    );
    h.state_mut().on_exit();
    drop(h);
    let h = open(backend, &root, files);
    assert!(
        h.state()
            .conventions
            .state()
            .unwrap()
            .operation_ids
            .contains(&id)
    );
    assert!(
        h.state()
            .conventions
            .state()
            .unwrap()
            .operation_ids
            .contains(&scope_id)
    );
    assert!(matches!(
        h.state()
            .conventions
            .state()
            .unwrap()
            .operation_statuses
            .get(&id)
            .unwrap()
            .phase,
        ConventionOperationPhase::Unknown
    ));
}
