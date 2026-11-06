//! Settings-owned explicit embedding configuration. Every state comes from the router owner;
//! the panel neither qualifies a model nor chooses a conversational/default route.
use crate::connections_screen::ConnectionAction;
use crate::widgets::{self, Tone, chip};
use devworks_core::protocol::records::{ConnectionRecord, EmbeddingSpacePage};
use eframe::egui::{self, Ui};
use serde_json::{Value, json};

#[derive(Default)]
pub struct EmbeddingSpacesPanel {
    pub open: bool,
    pub page: Option<EmbeddingSpacePage>,
    pub loading: bool,
    pub error: Option<String>,
    pub generation: u64,
    pub last_load: Option<std::time::Instant>,
    name: String,
    connection: Option<(String, String)>,
    model: String,
    dimensions: String,
    parameter: bool,
    l2: bool,
}
fn wire(value: &impl serde::Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}
fn field(ui: &mut Ui, label: &str, text: &mut String) {
    ui.horizontal(|ui| {
        let name = ui.label(label);
        ui.text_edit_singleline(text).labelled_by(name.id);
    });
}
fn configure(actions: &mut Vec<ConnectionAction>, label: String, action: Value) {
    actions.push(ConnectionAction::Configure {
        label,
        action,
        key: None,
        code: None,
    });
}
impl EmbeddingSpacesPanel {
    pub fn loaded(&mut self, page: EmbeddingSpacePage) {
        self.page = Some(page);
        self.loading = false;
        self.error = None;
    }
    pub fn qualifying(&self) -> bool {
        self.page
            .as_ref()
            .is_some_and(|p| p.items.0.iter().any(|r| wire(&r.status) == "Qualifying"))
    }
    pub fn ui(
        &mut self,
        ui: &mut Ui,
        connections: &[ConnectionRecord],
        idle: bool,
        actions: &mut Vec<ConnectionAction>,
    ) {
        ui.separator();
        ui.heading("Embedding spaces");
        if !self.open {
            if widgets::secondary_button(ui, "Configure embedding spaces").clicked() {
                actions.push(ConnectionAction::EmbeddingSetup);
            }
            return;
        }
        if widgets::secondary_button(ui, "Hide embedding spaces").clicked() {
            self.open = false;
            return;
        }

        ui.label("Choose an exact embedding connection, model and vector shape. Nothing is selected automatically.");
        if let Some(error) = &self.error {
            chip(ui, error, Tone::Bad);
        }
        if self.loading {
            ui.spinner();
        }
        if let Some(reason) = self
            .page
            .as_ref()
            .and_then(|p| p.availability_reason.as_ref())
        {
            chip(ui, &reason.0, Tone::Attention);
        }
        if widgets::secondary_button(ui, "Reload embedding spaces").clicked() {
            actions.push(ConnectionAction::EmbeddingSpaces { cursor: None });
        }
        ui.add_enabled_ui(idle && !self.loading,|ui| {
            field(ui,"Embedding space name",&mut self.name);
            let current=self.connection.as_ref().and_then(|(id,_)|connections.iter().find(|c|c.id.as_str()==id));
            egui::ComboBox::from_label("Embedding connection")
                .selected_text(current.map_or("Choose an embedding connection",|c|c.name.0.as_str()))
                .show_ui(ui,|ui| {for c in connections.iter().filter(|c|wire(&c.profile)=="Embedding") {
                    if ui.selectable_label(self.connection.as_ref().is_some_and(|(id,_)|id==c.id.as_str()),&c.name.0).clicked() {self.connection=Some((c.id.as_str().into(),c.revision.as_str().into()));}
                }});
            field(ui,"Embedding upstream model",&mut self.model);
            field(ui,"Embedding dimensions (1–65536)",&mut self.dimensions);
            ui.checkbox(&mut self.parameter,"Send dimensions parameter");
            ui.checkbox(&mut self.l2,"Use L2 distance instead of Cosine");
            if self.l2 {ui.label("Conventions search requires Cosine. With L2, it reports lexical fallback.");}
            let dimensions=self.dimensions.parse::<u32>().ok().filter(|d|(1..=65536).contains(d));
            let valid=self.connection.is_some() && !self.name.trim().is_empty() && !self.model.trim().is_empty() && dimensions.is_some();
            if ui.add_enabled(valid,egui::Button::new("Create embedding space")).clicked()
                && let (Some((connection,revision)),Some(dimensions))=(&self.connection,dimensions)
            {
                configure(actions,"Create embedding space".into(),json!({"kind":"CreateEmbeddingSpace","name":self.name,"connectionId":connection,"expectedConnectionRevision":revision,"upstreamModelId":self.model,"dimensions":dimensions.to_string(),"dimensionMode":if self.parameter {"Parameter"}else{"Native"},"metric":if self.l2 {"L2"}else{"Cosine"}}));
            }
            ui.label("Creating a space sends no inference. Qualification sends one fixed probe through that connection and records its usage; it may incur a charge.");
        });
        let Some(page) = &self.page else {
            return;
        };
        ui.label("A selected space can be used for future indexing and searches. Usage is recorded through its connection.");
        if let Some(id) = &page.selected_candidate_id {
            ui.label(format!("Selected embedding candidate: {}", id.as_str()));
        } else {
            ui.label("No embedding space selected. Retrieval can report its lexical fallback.");
        }
        if page.selected_candidate_id.is_some()
            && ui
                .add_enabled(
                    idle && !self.loading,
                    egui::Button::new("Stop using embedding space"),
                )
                .clicked()
        {
            configure(
                actions,
                "Clear embedding selection".into(),
                json!({"kind":"ClearEmbeddingSpace","expectedSelectionRevision":page.selection_revision}),
            );
        }
        for record in &page.items.0 {
            ui.push_id(record.id.as_str(),|ui| {ui.group(|ui| {
                ui.strong(format!("{}: {}",record.name.0,wire(&record.status)));
                ui.label(format!("Model: {} · {} dimensions · {}",record.upstream_model_id.0,record.dimensions.0,wire(&record.metric)));
                ui.monospace(format!("Space: {}",record.space_id.as_str()));
                if let Some(reason)=&record.reason {ui.label(&reason.0);}
                if let Some(operation)=&record.qualification_operation {ui.label(format!("Qualification operation: {} ({})",operation.operation_id.0.as_str(),wire(&operation.state)));}
                let state=wire(&record.status);
                if record.can_qualify && ui.add_enabled(idle&&!self.loading,egui::Button::new(format!("Queue qualification for {} (may incur usage)",record.name.0))).clicked() {
                    configure(actions,format!("Queue qualification for {}",record.name.0),json!({"kind":"QualifyEmbeddingSpace","candidateId":record.id,"expectedRevision":record.revision}));
                }
                if state=="Qualified" && page.selected_candidate_id.as_ref()!=Some(&record.id) && ui.add_enabled(idle&&!self.loading,egui::Button::new(format!("Use embedding space {}",record.name.0))).clicked() {
                    configure(actions,format!("Select embedding space {}",record.name.0),json!({"kind":"SelectEmbeddingSpace","candidateId":record.id,"expectedRevision":record.revision,"expectedSelectionRevision":page.selection_revision}));
                }
            });});
        }
        if page.has_more
            && let Some(cursor) = &page.next_cursor
            && widgets::secondary_button(ui, "More embedding spaces").clicked()
        {
            actions.push(ConnectionAction::EmbeddingSpaces {
                cursor: Some(cursor.0.clone()),
            });
        }
    }
}
