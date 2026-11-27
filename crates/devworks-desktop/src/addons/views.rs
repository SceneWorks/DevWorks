use super::AddonsAction;
use crate::backend::{Backend, TaskOutput};
use devworks_core::protocol::provider::ArtifactRef;
use devworks_engine::package_host::native_ui::{
    FieldKind, NativeView, NativeViews, UiNode, ViewData,
};
use eframe::egui::{self, ColorImage, Ui};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

#[derive(Default)]
pub(crate) struct PackageViews {
    publication: Option<(String, NativeViews)>,
    forms: BTreeMap<String, Map<String, Value>>,
    data: BTreeMap<String, ViewData>,
    pending: BTreeMap<String, u64>,
    request_number: u64,
    images: BTreeMap<String, ColorImage>,
    textures: BTreeMap<String, egui::TextureHandle>,
}

impl PackageViews {
    pub(crate) fn load(
        &mut self,
        package: String,
        views: NativeViews,
        images: BTreeMap<String, ColorImage>,
    ) {
        self.publication = Some((package, views));
        self.forms.clear();
        self.data.clear();
        self.pending.clear();
        self.images = images;
        self.textures.clear();
    }

    pub(crate) fn request_data(&mut self, view: &str) -> u64 {
        self.request_number += 1;
        self.pending.insert(view.to_owned(), self.request_number);
        self.request_number
    }

    pub(crate) fn finish_data(&mut self, view: &str, request: u64) -> bool {
        if self.pending.get(view) == Some(&request) {
            self.pending.remove(view);
            true
        } else {
            false
        }
    }

    pub(crate) fn update_data(
        &mut self,
        package: &str,
        digest: &str,
        view: String,
        data: ViewData,
    ) {
        if self
            .publication
            .as_ref()
            .is_some_and(|(owner, publication)| {
                owner == package
                    && publication.digest == digest
                    && publication.views.iter().any(|entry| entry.id == view)
            })
        {
            self.data.insert(view, data);
        }
    }

    pub(crate) fn render(
        &mut self,
        ui: &mut Ui,
        package: &str,
        digest: &str,
        ready: bool,
        actions: &mut Vec<AddonsAction>,
    ) {
        if !ready {
            return;
        }
        if ui.button("Load package views").clicked() {
            actions.push(AddonsAction::LoadViews {
                package: package.to_owned(),
                digest: digest.to_owned(),
            });
        }
        let Some((owner, publication)) = &self.publication else {
            return;
        };
        if owner != package || publication.digest != digest {
            return;
        }
        for view in &publication.views {
            ui.group(|ui| {
                let fields = self.forms.entry(view.id.clone()).or_default();
                let mut action = None;
                ui.push_id(&view.id, |ui| {
                    let mut paint = Paint {
                        view,
                        data: self.data.get(&view.id),
                        images: &self.images,
                        textures: &mut self.textures,
                    };
                    render_node(ui, &view.tree, &mut paint, fields, &mut action);
                });
                if view.has_tables
                    && ui
                        .add_enabled(
                            !self.pending.contains_key(&view.id),
                            egui::Button::new("Refresh view data"),
                        )
                        .clicked()
                {
                    actions.push(AddonsAction::RefreshView {
                        package: owner.clone(),
                        digest: publication.digest.clone(),
                        view: view.id.clone(),
                        form: Value::Object(fields.clone()),
                    });
                }
                if let Some(action) = action {
                    actions.push(AddonsAction::ViewAction {
                        package: owner.clone(),
                        digest: publication.digest.clone(),
                        view: view.id.clone(),
                        action,
                        form: Value::Object(fields.clone()),
                    });
                }
            });
        }
    }
}

struct Paint<'a> {
    view: &'a NativeView,
    data: Option<&'a ViewData>,
    images: &'a BTreeMap<String, ColorImage>,
    textures: &'a mut BTreeMap<String, egui::TextureHandle>,
}

fn render_node(
    ui: &mut Ui,
    node: &UiNode,
    paint: &mut Paint<'_>,
    fields: &mut Map<String, Value>,
    requested: &mut Option<String>,
) {
    match node {
        UiNode::Text { text } => {
            ui.label(text);
        }
        UiNode::Group { children } => {
            ui.vertical(|ui| {
                for (index, child) in children.iter().enumerate() {
                    ui.push_id(index, |ui| render_node(ui, child, paint, fields, requested));
                }
            });
        }
        UiNode::Field {
            field_id,
            label,
            field_kind,
            read_only,
        } => {
            ui.add_enabled_ui(!*read_only, |ui| match field_kind {
                FieldKind::Text => {
                    let mut value = fields
                        .get(field_id.as_str())
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    let response = ui.horizontal(|ui| {
                        let text = ui.label(label);
                        let edit = ui.text_edit_singleline(&mut value);
                        edit.labelled_by(text.id)
                    });
                    if response.inner.changed() {
                        fields.insert(field_id.as_str().to_owned(), json!(value));
                    }
                }
                FieldKind::Toggle => {
                    let mut value = fields
                        .get(field_id.as_str())
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    if ui.checkbox(&mut value, label).changed() {
                        fields.insert(field_id.as_str().to_owned(), json!(value));
                    }
                }
                FieldKind::Select => {
                    let mut value = fields
                        .get(field_id.as_str())
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    let Some(choices) = paint.view.choices.get(field_id.as_str()) else {
                        ui.label(format!("{label} (no choices supplied)"));
                        return;
                    };
                    ui.horizontal(|ui| {
                        ui.label(label);
                        eframe::egui::ComboBox::from_id_salt(field_id.as_str())
                            .selected_text(if value.is_empty() {
                                "Select a value"
                            } else {
                                &value
                            })
                            .show_ui(ui, |ui| {
                                for option in choices {
                                    if ui
                                        .selectable_value(&mut value, option.clone(), option)
                                        .clicked()
                                    {
                                        fields.insert(field_id.as_str().to_owned(), json!(value));
                                    }
                                }
                            });
                    });
                }
            });
        }
        UiNode::Action { action_id, label } => {
            if ui.button(label).clicked() {
                *requested = Some(action_id.as_str().to_owned());
            }
        }
        UiNode::Table {
            columns,
            data_schema,
        } => {
            let rows = paint
                .data
                .and_then(|data| data.tables.get(data_schema.schema_id.as_str()));
            eframe::egui::Grid::new(data_schema.schema_id.as_str())
                .striped(true)
                .show(ui, |ui| {
                    for column in columns {
                        ui.strong(column.as_str());
                    }
                    ui.end_row();
                    if let Some(rows) = rows {
                        for row in rows {
                            for column in columns {
                                let value = &row[column.as_str()];
                                let cell = match value {
                                    Value::String(text) => text.clone(),
                                    Value::Null => String::new(),
                                    other => other.to_string(),
                                };
                                ui.label(cell.chars().take(4096).collect::<String>());
                            }
                            ui.end_row();
                        }
                    }
                });
        }
        UiNode::Image { artifact } => {
            let Ok(reference) =
                serde_json::to_value(artifact).and_then(serde_json::from_value::<ArtifactRef>)
            else {
                ui.label("Image data unavailable");
                return;
            };
            let key = image_key(&reference);
            let Some(image) = paint.images.get(&key) else {
                ui.label("Image data unavailable");
                return;
            };
            let texture = paint.textures.entry(key.clone()).or_insert_with(|| {
                ui.ctx()
                    .load_texture(key, image.clone(), egui::TextureOptions::LINEAR)
            });
            ui.image((texture.id(), texture.size_vec2()));
        }
    }
}

fn image_key(reference: &ArtifactRef) -> String {
    format!(
        "{}:{}:{}:{}:{}",
        reference.artifact_id.as_str(),
        reference.version_id.as_str(),
        reference.sha256.as_str(),
        reference.byte_length.get(),
        reference.media_type
    )
}

pub(crate) fn prepare_images(
    backend: &dyn Backend,
    views: &NativeViews,
) -> BTreeMap<String, ColorImage> {
    let mut images = BTreeMap::new();
    let mut attempts = 0;
    for view in &views.views {
        load_images(backend, &view.tree, &mut images, &mut attempts);
    }
    images
}

fn load_images(
    backend: &dyn Backend,
    node: &UiNode,
    images: &mut BTreeMap<String, ColorImage>,
    attempts: &mut usize,
) {
    match node {
        UiNode::Group { children } => {
            for child in children {
                load_images(backend, child, images, attempts);
            }
        }
        UiNode::Image { artifact } if *attempts < 16 => {
            let Ok(reference) =
                serde_json::to_value(artifact).and_then(serde_json::from_value::<ArtifactRef>)
            else {
                return;
            };
            let key = image_key(&reference);
            if images.contains_key(&key)
                || !crate::attachments::previewable(&reference.media_type)
                || reference.byte_length.get() > 2 * 1024 * 1024
            {
                return;
            }
            *attempts += 1;
            if let Ok(TaskOutput::Preview { size, rgba }) =
                crate::attachments::preview_stored(backend, &reference)
            {
                images.insert(key, ColorImage::from_rgba_unmultiplied(size, &rgba));
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use devworks_engine::package_host::native_ui::NativeView;
    use egui_kittest::{Harness, kittest::Queryable};
    use std::cell::RefCell;
    use std::rc::Rc;

    #[test]
    fn an_approved_native_view_renders_its_table_and_routes_its_action() {
        let tree = serde_json::from_value(json!({"kind":"Group","children":[
            {"kind":"Text","text":"Package status"},
            {"kind":"Table","columns":["result"],"dataSchema":{
                "schemaId":"pack/rows","revision":"1","sha256":"a".repeat(64)
            }},
            {"kind":"Action","actionId":"pack/run","label":"Run package action"}
        ]}))
        .unwrap();
        let mut views = PackageViews::default();
        views.load(
            "pack".into(),
            NativeViews {
                digest: "active-digest".into(),
                views: vec![NativeView {
                    id: "pack/view".into(),
                    tree,
                    choices: BTreeMap::new(),
                    has_tables: true,
                }],
            },
            BTreeMap::new(),
        );
        views.update_data(
            "pack",
            "active-digest",
            "pack/view".into(),
            ViewData {
                revision: 1,
                tables: BTreeMap::from([(
                    "pack/rows".into(),
                    vec![json!({"result":"Ready for review"})],
                )]),
            },
        );
        let actions = Rc::new(RefCell::new(Vec::new()));
        let sink = actions.clone();
        let mut harness = Harness::new_ui(move |ui| {
            views.render(ui, "pack", "active-digest", true, &mut sink.borrow_mut());
        });
        harness.run();
        assert!(harness.query_by_label("Package status").is_some());
        assert!(harness.query_by_label("Ready for review").is_some());
        harness.get_by_label("Run package action").click();
        harness.run();
        assert!(
            matches!(actions.borrow().last(), Some(AddonsAction::ViewAction {
            package, digest, view, action, form,
        }) if package == "pack" && digest == "active-digest" && view == "pack/view"
            && action == "pack/run" && form == &json!({}))
        );
    }
}
