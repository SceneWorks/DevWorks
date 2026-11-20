//! Requests for the Documents tab, using the same shell client and artifact upload custody.
use super::KnowledgeCtl;
use crate::{
    attachments,
    backend::{self, Reply, TaskOutput},
    documents_ops::{self, DocumentSlot},
    documents_screen::{Action, ImportPlan, StagedImport, Upload},
    frame,
    knowledge_ops::KnowledgeSlot,
    ops, paged,
    widgets::Tone,
};
use devworks_core::protocol::{
    ProjectId,
    data::{CitationSource, DataScope, DocumentVersions, RetrievalMode},
    records::DocumentResult,
    results::OperationResult,
};
use std::path::{Path, PathBuf};

impl KnowledgeCtl<'_, '_> {
    pub(super) fn load_documents(&mut self, refresh: bool) {
        let documents = &mut self.knowledge.documents;
        if documents.list.loading() {
            return;
        }
        let fetch = if refresh {
            documents.list.refresh()
        } else {
            documents.list.reload()
        };
        self.send_documents(fetch);
    }

    fn send_documents(&mut self, fetch: paged::Fetch) {
        let Some(scope) = frame::scope(self.knowledge.documents.project.as_deref()) else {
            self.knowledge.documents.list.abandon(fetch.generation);
            return;
        };
        self.cx.paged(
            &mut self.knowledge.documents.list,
            fetch.clone(),
            KnowledgeSlot::Documents(DocumentSlot::List(fetch.generation)),
            "Loading documents",
            "documents.list",
            move |b, cursor| documents_ops::list(b, scope, cursor),
        );
    }

    pub(super) fn read_document(&mut self, more: bool) {
        let documents = &mut self.knowledge.documents;
        let Some(version) = documents.selected.clone() else {
            return;
        };
        let cursor = if more {
            let Some(cursor) = documents
                .detail
                .as_ref()
                .and_then(|detail| detail.next_version_cursor.as_ref())
            else {
                return;
            };
            Some(cursor.0.clone())
        } else {
            None
        };
        documents.read_generation += 1;
        let generation = documents.read_generation;
        self.query(
            KnowledgeSlot::Documents(DocumentSlot::Read { generation, more }),
            "Reading the exact document version",
            "documents.read",
            move |b| documents_ops::read(b, version, cursor),
        );
    }

    pub(super) fn document_actions(&mut self, actions: Vec<Action>) {
        for action in actions {
            match action {
                Action::Scope(project) => {
                    self.knowledge.follow_navigation(project.as_deref());
                    self.knowledge.documents.scope(project.as_deref());
                    self.load_documents(false);
                }
                Action::Reload => {
                    self.load_documents(true);
                    self.read_document(false);
                }
                Action::More => {
                    if let Some(fetch) = self.knowledge.documents.list.more_page() {
                        self.send_documents(fetch);
                    }
                }
                Action::Pick { replacement } => {
                    let picked = self.dialogs.pick_document();
                    if let Some(path) = picked {
                        self.upload_document(vec![path], replacement);
                    }
                }
                Action::Drop(paths) => self.upload_document(paths, false),
                Action::Select(source) => {
                    self.knowledge.documents.select(Some(source));
                    self.read_document(false);
                }
                Action::MoreVersions => self.read_document(true),
                Action::Search => self.search_documents(),
                Action::Follow(citation) => {
                    let documents = &mut self.knowledge.documents;
                    documents.citation = None;
                    documents.citation_generation += 1;
                    let generation = documents.citation_generation;
                    self.query(
                        KnowledgeSlot::Documents(DocumentSlot::Citation(generation)),
                        "Resolving the exact citation",
                        "documents.resolve_citation",
                        move |b| documents_ops::follow(b, citation),
                    );
                }
                Action::ViewOriginal(version) => {
                    let generation = self.knowledge.documents.source_generation;
                    self.source_task(generation, "Reading the original text", move |b| {
                        let bytes = ops::read_artifact_bytes(b, &version)?;
                        let text = String::from_utf8(bytes)
                            .map_err(|_| "The original is not UTF-8 text.")?;
                        let mut display = text.chars().take(32768).collect::<String>();
                        if display.len() < text.len() {
                            display.push_str("\n\nUse Save original to read the rest.");
                        }
                        Ok(TaskOutput::DocumentSource(Some(display)))
                    });
                }
                Action::ExportOriginal(version, title) => {
                    let name = std::path::Path::new(&title)
                        .file_stem()
                        .and_then(|name| name.to_str())
                        .unwrap_or("document");
                    let extension = match version.media_type.as_str() {
                        "application/pdf" => "pdf",
                        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => {
                            "docx"
                        }
                        "text/markdown" => "md",
                        _ => "txt",
                    };
                    let Some(path) = self.dialogs.save_file(&format!("{name}.{extension}")) else {
                        continue;
                    };
                    let generation = self.knowledge.documents.source_generation;
                    self.source_task(generation, "Saving the original document", move |b| {
                        let bytes = ops::read_artifact_bytes(b, &version)?;
                        let parent = path
                            .parent()
                            .ok_or_else(|| "The destination has no directory.".to_owned())?;
                        let mut file =
                            tempfile::NamedTempFile::new_in(parent).map_err(|e| e.to_string())?;
                        std::io::Write::write_all(&mut file, &bytes).map_err(|e| e.to_string())?;
                        file.persist(path).map_err(|e| e.to_string())?;
                        Ok(TaskOutput::DocumentSource(None))
                    });
                }
                Action::Change(change) => {
                    if self.knowledge.documents.busy() {
                        continue;
                    }
                    let documents = &self.knowledge.documents;
                    let Some(detail) = documents.detail.clone() else {
                        continue;
                    };
                    let Some(version) = documents.selected.clone() else {
                        continue;
                    };
                    let target = documents
                        .target_project
                        .as_deref()
                        .and_then(frame::id)
                        .map(ProjectId);
                    let op = documents_ops::operation(change);
                    let submitted = self.command(
                        KnowledgeSlot::Documents(DocumentSlot::Write),
                        change.label(),
                        op,
                        move |b| documents_ops::change(b, change, detail, version, target),
                    );
                    self.knowledge.documents.writing = submitted;
                }
            }
        }
    }

    fn source_task(
        &mut self,
        generation: u64,
        label: &str,
        task: impl FnOnce(&dyn backend::Backend) -> Result<TaskOutput, String> + Send + 'static,
    ) {
        if self.online().is_ok() {
            self.client.as_deref_mut().map(|client| {
                client.task(
                    KnowledgeSlot::Documents(DocumentSlot::Source(generation)).into(),
                    label,
                    task,
                )
            });
        }
    }

    fn search_documents(&mut self) {
        let documents = &mut self.knowledge.documents;
        let text = documents.text.trim();
        if text.is_empty() || text.len() > 8192 {
            self.document_message(Tone::Attention, "Enter search text up to 8,192 bytes.");
            return;
        }
        let Some(scope) = frame::scope(documents.project.as_deref()) else {
            return;
        };
        let versions = if documents.exact {
            let Some(selected) = documents.selected.clone() else {
                return;
            };
            DocumentVersions::Exact {
                versions: vec![selected],
            }
        } else {
            DocumentVersions::Current
        };
        let search = documents_ops::Search {
            scope,
            versions,
            text: text.to_owned(),
            mode: if documents.hybrid {
                RetrievalMode::Hybrid
            } else {
                RetrievalMode::Lexical
            },
            fallback: documents.hybrid && documents.fallback,
        };
        documents.search_generation += 1;
        let generation = documents.search_generation;
        documents.results = None;
        documents.citation = None;
        documents.citation_generation += 1;
        documents.message = None;
        self.query(
            KnowledgeSlot::Documents(DocumentSlot::Search(generation)),
            "Searching managed documents",
            "documents.search",
            move |b| search.frame(b),
        );
    }

    fn upload_document(&mut self, paths: Vec<PathBuf>, replacing: bool) {
        let documents = &mut self.knowledge.documents;
        if documents.busy() {
            self.document_message(
                Tone::Attention,
                "A document operation is already in progress.",
            );
            return;
        }
        let [path] = paths.as_slice() else {
            if !paths.is_empty() {
                self.document_message(Tone::Attention, "Import one document at a time.");
            }
            return;
        };
        let Some(media) = media_type(path) else {
            self.document_message(
                Tone::Attention,
                "Choose a Markdown, plain text, PDF or Word (.docx) file.",
            );
            return;
        };
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("Document");
        let name = name.chars().take(256).collect::<String>();
        let Some(scope) = frame::scope(documents.project.as_deref()) else {
            return;
        };
        let plan = if replacing {
            let Some(detail) = documents.detail.as_ref() else {
                return;
            };
            ImportPlan {
                title: detail.title.0.clone(),
                scope: detail.scope.clone(),
                replacement: Some((detail.id.clone(), detail.revision.clone())),
            }
        } else {
            ImportPlan {
                title: name.clone(),
                scope,
                replacement: None,
            }
        };
        let path = path.clone();
        let upload_scope = plan.scope.clone();
        let submitted = self.online().is_ok()
            && self.client.as_deref_mut().is_some_and(|client| {
                client.task(
                    KnowledgeSlot::Documents(DocumentSlot::Upload).into(),
                    "Preparing the local document",
                    move |b| match &upload_scope {
                        DataScope::Project { project_id } => {
                            attachments::prepare_project_file(b, project_id, &path, &name, media)
                        }
                        DataScope::Workspace => {
                            attachments::prepare_file(b, &path, &name, media, "document-import")
                        }
                    },
                )
            });
        if submitted {
            self.knowledge.documents.upload = Some(Upload::Preparing(plan));
            self.knowledge.documents.message = None;
        }
    }

    fn save_document_source(&mut self, staged: StagedImport) {
        let frame = staged.frame.clone();
        let sent = self.online().is_ok()
            && self.client.as_deref_mut().is_some_and(|client| {
                client.task(
                    KnowledgeSlot::Documents(DocumentSlot::Upload).into(),
                    "Storing the exact document source",
                    move |b| attachments::save(b, frame, None),
                )
            });
        self.knowledge.documents.upload = Some(if sent {
            Upload::Storing(staged)
        } else {
            Upload::Unconfirmed(staged)
        });
    }

    pub(super) fn confirm_document_source(&mut self) {
        if let Some(Upload::Unconfirmed(staged)) = self.knowledge.documents.upload.as_ref() {
            self.save_document_source(staged.clone());
        }
    }

    fn document_message(&mut self, tone: Tone, message: impl Into<String>) {
        self.knowledge.documents.message = Some((tone, message.into()));
    }

    pub(super) fn apply_documents(&mut self, slot: DocumentSlot, reply: Reply) {
        match (slot, reply) {
            (DocumentSlot::List(generation), Reply::Query(reply)) => {
                let page = match ops::query_result(reply) {
                    Ok((
                        OperationResult::DocumentsList {
                            value:
                                devworks_core::protocol::records::DocumentListResult::Current(page),
                        },
                        _,
                    )) => Ok(paged::Page::new(
                        page.items.0,
                        page.next_cursor.map(|cursor| cursor.0),
                    )
                    .fenced(page.snapshot_revision.as_str())),
                    Ok(_) => Err("Unexpected document listing response.".into()),
                    Err(error) => Err(error.describe()),
                };
                match self
                    .knowledge
                    .documents
                    .list
                    .apply_unique(generation, page, |doc| doc.id.as_str().to_owned())
                {
                    paged::Applied::Continue(fetch) => self.send_documents(fetch),
                    paged::Applied::Fenced => self.load_documents(false),
                    _ => {}
                }
            }
            (DocumentSlot::Read { generation, more }, Reply::Query(reply))
                if generation == self.knowledge.documents.read_generation =>
            {
                match ops::query_result(reply) {
                    Ok((
                        OperationResult::DocumentsRead {
                            value: DocumentResult::Current(mut detail),
                        },
                        _,
                    )) => {
                        if more && let Some(previous) = self.knowledge.documents.detail.as_ref() {
                            let mut retained = previous.retained_versions.0.clone();
                            retained.append(&mut detail.retained_versions.0);
                            detail.retained_versions.0 = retained;
                        }
                        self.knowledge.documents.detail = Some(detail);
                    }
                    Ok(_) => {
                        self.document_message(Tone::Attention, "Unexpected document read response.")
                    }
                    Err(error) => {
                        self.knowledge.documents.detail = None;
                        self.knowledge.documents.original = None;
                        self.knowledge.documents.citation = None;
                        self.document_message(Tone::Attention, error.describe());
                    }
                }
            }
            (DocumentSlot::Search(generation), Reply::Query(reply))
                if generation == self.knowledge.documents.search_generation =>
            {
                match ops::query_result(reply) {
                    Ok((OperationResult::DocumentsSearch { value }, _)) => {
                        self.knowledge.documents.results = Some(value)
                    }
                    Ok(_) => self
                        .document_message(Tone::Attention, "Unexpected document search response."),
                    Err(error) => self.document_message(Tone::Attention, error.describe()),
                }
            }
            (DocumentSlot::Citation(generation), Reply::Query(reply))
                if generation == self.knowledge.documents.citation_generation =>
            {
                match ops::query_result(reply) {
                    Ok((OperationResult::DocumentsResolveCitation { value }, _)) => {
                        if let CitationSource::Managed { version, .. } = &value.citation.source
                            && self
                                .knowledge
                                .documents
                                .selected
                                .as_ref()
                                .is_none_or(|selected| selected.version_id != version.version_id)
                        {
                            self.knowledge.documents.select(Some(version.clone()));
                            self.read_document(false);
                        }
                        self.knowledge.documents.citation = Some(value);
                    }
                    Ok(_) => {
                        self.document_message(Tone::Attention, "Unexpected citation response.")
                    }
                    Err(error) => self.document_message(Tone::Attention, error.describe()),
                }
            }
            (DocumentSlot::Source(generation), Reply::Task(result))
                if generation == self.knowledge.documents.source_generation =>
            {
                match result {
                    Ok(TaskOutput::DocumentSource(Some(text))) => {
                        self.knowledge.documents.original = Some(text)
                    }
                    Ok(TaskOutput::DocumentSource(None)) => {
                        self.document_message(Tone::Good, "Original saved.")
                    }
                    Err(error) => self.document_message(Tone::Attention, error),
                    _ => {}
                }
            }
            (DocumentSlot::Upload, Reply::Task(Ok(TaskOutput::Prepared { frame }))) => {
                if let Some(Upload::Preparing(plan)) = self.knowledge.documents.upload.take() {
                    self.save_document_source(StagedImport { plan, frame });
                }
            }
            (DocumentSlot::Upload, Reply::Task(Ok(TaskOutput::Stored { outcome, frame }))) => {
                let Some(Upload::Storing(staged)) = self.knowledge.documents.upload.take() else {
                    return;
                };
                let StagedImport { plan, .. } = staged;
                if backend::is_unknown(&outcome) {
                    self.knowledge.documents.upload =
                        Some(Upload::Unconfirmed(StagedImport { plan, frame }));
                    return;
                }
                match ops::saved_artifact(outcome) {
                    Ok(source) => {
                        let command_plan = plan.clone();
                        let sent = self.command(
                            KnowledgeSlot::Documents(DocumentSlot::Write),
                            "Importing the retained document",
                            "documents.import",
                            move |b| documents_ops::import(b, command_plan, source.artifact),
                        );
                        if sent {
                            self.knowledge.documents.writing = true;
                        } else {
                            self.knowledge.documents.upload =
                                Some(Upload::Unconfirmed(StagedImport { plan, frame }));
                        }
                    }
                    Err(error) => self.document_message(Tone::Attention, error.describe()),
                }
            }
            (DocumentSlot::Upload, Reply::Task(Err(error))) => {
                self.knowledge.documents.upload = None;
                self.document_message(Tone::Attention, error);
            }
            (DocumentSlot::Write, Reply::Command(reply)) => self.document_written(reply),
            _ => {}
        }
    }

    fn document_written(&mut self, reply: devworks_core::gateway::CommandOutcome) {
        if backend::is_unknown(&reply) {
            self.document_message(
                Tone::Attention,
                "The document operation is not confirmed. Use Confirm outcomes.",
            );
            return;
        }
        self.knowledge.documents.writing = false;
        match ops::command_result(reply) {
            Ok(OperationResult::DocumentsImport {
                value: DocumentResult::Current(detail),
            }) => {
                self.document_message(Tone::Good, "Document imported.");
                let imported_scope = match &detail.scope {
                    DataScope::Workspace => None,
                    DataScope::Project { project_id } => Some(project_id.0.as_str()),
                };
                if self.knowledge.documents.project.as_deref() == imported_scope {
                    self.knowledge.documents.select(detail.current_version);
                }
            }
            Ok(OperationResult::DocumentsMove {
                value: DocumentResult::Current(detail),
            }) => {
                if self
                    .knowledge
                    .documents
                    .selected
                    .as_ref()
                    .is_some_and(|source| source.artifact_id == detail.id)
                {
                    self.knowledge.documents.select(None);
                }
                self.document_message(Tone::Good, "Document moved.");
            }
            Ok(OperationResult::DocumentsShare { .. }) => {
                self.document_message(Tone::Good, "Document shared.")
            }
            Ok(OperationResult::DocumentsDisableRetrieval { .. }) => {
                self.document_message(Tone::Good, "Retrieval disabled.")
            }
            Ok(OperationResult::DocumentsRemoveIndex { .. }) => {
                self.document_message(Tone::Good, "Index removed.")
            }
            Ok(OperationResult::DocumentsIndex { .. } | OperationResult::DocumentsRetry { .. }) => {
                self.document_message(Tone::Good, "Indexing queued.")
            }
            Ok(OperationResult::IndexingCancel { .. }) => {
                self.document_message(Tone::Good, "Cancellation requested.")
            }
            Ok(_) => {
                self.document_message(Tone::Attention, "Unexpected document operation response.")
            }
            Err(error) => self.document_message(Tone::Attention, error.describe()),
        }
        let documents = &mut self.knowledge.documents;
        documents.search_generation += 1;
        documents.citation_generation += 1;
        documents.results = None;
        documents.citation = None;
        documents.original = None;
        documents.source_generation += 1;
        self.load_documents(false);
        self.read_document(false);
    }
}

fn media_type(path: &Path) -> Option<&'static str> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "txt" | "log" => Some("text/plain"),
        "md" | "markdown" => Some("text/markdown"),
        "pdf" => Some("application/pdf"),
        "docx" => Some("application/vnd.openxmlformats-officedocument.wordprocessingml.document"),
        _ => None,
    }
}
