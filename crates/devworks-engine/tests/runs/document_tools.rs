//! The normal agent executor dispatches selected document tools over native SQLite. Provider
//! replies are recorded HTTP frames. No model or external document service is involved.
use super::{approval_wait::pinned_agent_session, *};
use devworks_core::{
    ports::provider_http::{
        HttpStream, ResponseHead, StreamCloser, StreamLimits, StreamingHttp, TransportError,
    },
    protocol::{
        data::{Coverage, CoverageState},
        provider::ArtifactRef,
        records::DocumentResult,
        results::OperationResult,
    },
    services::{
        artifacts::{host::HostArtifactPublication, version_ref},
        documents::{
            evidence::CONTEXT_SOURCE_KIND,
            extraction::{DocumentExtractor, ExtractionControl, ExtractionInput, TextExtractor},
            indexing::{DocumentExtraction, DocumentProcessor},
        },
    },
};
use devworks_engine::{documents::indexing::DocumentComposition, runtime::Runtime};
use std::sync::{OnceLock, Weak};

const PASSAGE: &str = "The original greenhouse evidence is uniquely attributable.";
const ATTACHMENT: &str = "The attached control sample is not a library document.";
const UNAVAILABLE: &str = "Document evidence is unavailable in the current scope.";

struct TextProcessor;
impl DocumentProcessor for TextProcessor {
    fn revision(&self) -> Revision {
        Revision::new(1).unwrap()
    }

    fn extract(
        &self,
        input: ExtractionInput<'_>,
        control: &ExtractionControl<'_>,
    ) -> Result<DocumentExtraction, Id> {
        Ok(DocumentExtraction {
            blocks: TextExtractor::default()
                .extract(input, control)
                .map_err(|error| id(&format!("Text{error:?}")))?,
            coverage: Coverage {
                state: CoverageState::Complete,
                gaps: vec![],
                more_gaps: false,
            },
            vision_targets: vec![],
        })
    }
}

struct Body(std::io::Cursor<Vec<u8>>);
struct Close;
impl StreamCloser for Close {
    fn close(&self) -> Result<(), TransportError> {
        Ok(())
    }
}
impl HttpStream for Body {
    fn closer(&self) -> Arc<dyn StreamCloser> {
        Arc::new(Close)
    }
    fn head(&self) -> &ResponseHead {
        static HEAD: OnceLock<ResponseHead> = OnceLock::new();
        HEAD.get_or_init(|| ResponseHead {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
        })
    }
    fn read(&mut self, buffer: &mut [u8], _: &Cancellation) -> Result<usize, TransportError> {
        Ok(std::io::Read::read(&mut self.0, buffer).unwrap())
    }
    fn close(&mut self) -> Result<(), TransportError> {
        Ok(())
    }
}

#[derive(Clone)]
struct ChangeScope {
    app: Weak<Runtime>,
    user: InvocationContext,
    session: Id,
    move_frame: Value,
}

#[derive(Default)]
struct Wire {
    calls: Mutex<Vec<Value>>,
    change_scope: OnceLock<ChangeScope>,
    bodies: Mutex<Vec<Value>>,
}

fn emitted_tool(id: &str, name: &str, arguments: Value) -> Value {
    json!({"id":id,"type":"function","function":{"name":name,"arguments":arguments.to_string()}})
}

fn tool_result<'a>(body: &'a Value, call: &str) -> &'a str {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool" && message["tool_call_id"] == call)
        .unwrap_or_else(|| panic!("missing provider tool result for {call}: {body}"))["content"]
        .as_str()
        .unwrap()
}

impl StreamingHttp for Wire {
    fn open(
        &self,
        request: &HttpRequest,
        _: &StreamLimits,
        _: &Cancellation,
    ) -> Result<Box<dyn HttpStream>, TransportError> {
        let body: Value = serde_json::from_slice(request.body.as_deref().unwrap()).unwrap();
        let mut bodies = self.bodies.lock().unwrap();
        let index = bodies.len();
        let calls = match index {
            0 => self.calls.lock().unwrap().clone(),
            1 | 2 => {
                let result: Value = serde_json::from_str(tool_result(&body, "search")).unwrap();
                let citation = result["result"]["value"]["passages"][0]["citation"].clone();
                assert!(citation.is_object(), "{result}");
                let changed = self.change_scope.get().unwrap();
                let app = changed.app.upgrade().unwrap();
                if index == 2 {
                    applied_result(&app, &changed.user, "documents.move", &changed.move_frame);
                    vec![emitted_tool(
                        "reopen",
                        "documents_resolve_citation_query",
                        json!({"citation":citation}),
                    )]
                } else {
                    let artifact = read(app.store().as_ref(), |snapshot| {
                        let entries = snapshot.sessions().entries(&changed.session, 0, 500)?;
                        let search = entries
                            .iter()
                            .find(|entry| {
                                entry.kind == "ToolResponseRecorded"
                                    && entry.body["call"]["nativeCallId"] == "search"
                            })
                            .expect("durable search reply");
                        Ok(search.body["outcome"]["artifacts"][0].clone())
                    });
                    vec![
                        emitted_tool(
                            "open",
                            "documents_resolve_citation_query",
                            json!({"citation":citation}),
                        ),
                        emitted_tool(
                            "copy-read",
                            "artifacts_read_query",
                            json!({
                                "query":{"kind":"Read", "artifactId":artifact["artifactId"], "versionId":artifact["versionId"]},
                                "limit":10
                            }),
                        ),
                    ]
                }
            }
            _ => vec![],
        };
        bodies.push(body);
        let (delta, finish) = if calls.is_empty() {
            (
                json!({"content":"Finished checking document evidence."}),
                "stop",
            )
        } else {
            let calls: Vec<_> = calls
                .into_iter()
                .enumerate()
                .map(|(index, mut call)| {
                    call["index"] = json!(index);
                    call
                })
                .collect();
            (json!({"tool_calls":calls}), "tool_calls")
        };
        let event = json!({"model":"fixture-model","choices":[{"index":0,"delta":delta,"finish_reason":null}]});
        let finish = json!({"choices":[{"index":0,"delta":{},"finish_reason":finish}]});
        let response = format!("data: {event}\n\ndata: {finish}\n\ndata: [DONE]\n\n");
        Ok(Box::new(Body(std::io::Cursor::new(response.into_bytes()))))
    }
}

struct Entropy;
impl devworks_core::ports::http::Entropy for Entropy {
    fn fill(&self, bytes: &mut [u8]) {
        bytes.fill(8);
    }
}

fn frame(op: &str, key: &str, mut body: Value, revision: u32) -> Value {
    body["kind"] = json!(op);
    json!({"protocol":PROTOCOL,"schemaRevision":revision,"mode":"Command","requestKey":key,"body":body})
}

fn applied_result(
    app: &Runtime,
    ctx: &InvocationContext,
    op: &str,
    frame: &Value,
) -> OperationResult {
    match app.command(ctx, op, frame.to_string().as_bytes()).unwrap() {
        CommandReply::Recorded { receipt, .. } => {
            let DomainDecision::Applied { result } = receipt.decision else {
                panic!("{op} refused: {:?}", receipt.decision)
            };
            result
        }
        other => panic!("{op}: {other:?}"),
    }
}

fn markdown(app: &Runtime, fx: &Fx, ctx: &InvocationContext, text: &str) -> ArtifactRef {
    let service = app.artifacts().clone();
    let staged = service
        .stage_host_bytes(&*fx.store, text.as_bytes(), json!({"kind":"FixtureUpload"}))
        .unwrap()
        .unwrap();
    let write_stage = staged.clone();
    let writer = service.clone();
    let owner = ctx.principal().clone();
    let scope = ArtifactScope::Project(ctx.project_id().unwrap().clone());
    let stored = write(&*fx.store, move |tx| {
        writer.commit_host_bytes_in_scope(
            tx,
            &write_stage,
            HostArtifactPublication {
                title: "Exact source fixture",
                media_type: "text/markdown",
                provenance: json!({"kind":"FixtureUpload"}),
                scope,
                owner,
            },
        )
    })
    .unwrap();
    service.settle_host_bytes(&*fx.store, &staged, true);
    version_ref(&stored)
}

#[test]
fn document_tools_preserve_exact_history_and_refuse_revoked_evidence_on_continuation() {
    let _native_case = native_runtime_case();
    let fx = Fx::new();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let wire = Arc::new(Wire::default());
            let mut config = composition(&fx, None, Arc::new(FakePower::default()));
            config.application.connections = Some(devworks_engine::ConnectionPorts {
                http: Arc::new(devworks_core::ports::http::NoNetwork),
                secrets: Arc::new(devworks_core::ports::secrets::SessionSecrets::new()),
                entropy: Arc::new(Entropy),
                endpoints: None,
            });
            config.application.router = Some(Default::default());
            config.application.sessions = Some(Default::default());
            config.config.documents = Some(DocumentComposition { processor: Arc::new(TextProcessor) });
            config.config.providers = Some(devworks_engine::providers::driver::ProviderComposition::new(wire.clone()));
            let app = Arc::new(boot(config).await.unwrap());
            let (context, primary) = pinned_agent_session(&fx);
            let project = context.project_id().unwrap().clone();
            let actor = context.principal().clone();
            let owner = fx.user.clone();
            let reviewer = principal(&*fx.store, "document-reviewer", PrincipalKind::Agent)
                .with_lineage(Some(project.clone()), None, None);
            let reviewer_actor = reviewer.principal().clone();
            let reviewer_project = project.clone();
            fx.tx(move |_, tx| {
                // Leave room for both recorded citation calls and their exact source material.
                let mut model = tx.router().route_model(&id("approval-wait-model"))?.unwrap();
                model.context_tokens = Some(128_000);
                tx.router().put_route_model(&model, Some(model.revision))?;
                let binding = tx.authority().principal(&actor)?.unwrap();
                bind_principal(tx, &owner, NewPrincipalBinding {
                    actor, kind: binding.kind, role: binding.role, project_id: binding.project_id,
                    tool_families: Some(vec!["documents".into(), "artifacts".into()]), bound_by: owner.principal().clone(),
                }, Some(binding.revision)).map_err(|error| StorageError::InvalidRecord(format!("{error:?}")))?;
                bind_principal(tx, &owner, NewPrincipalBinding {
                    actor: reviewer_actor, kind: PrincipalKind::Agent, role: Some(AgentRole::CodeReviewer),
                    project_id: Some(reviewer_project), tool_families: Some(vec!["documents".into()]),
                    bound_by: owner.principal().clone(),
                }, None).map_err(|error| StorageError::InvalidRecord(format!("{error:?}")))?;
                Ok(())
            });
            let reviewer_tools = app.providers().unwrap().schemas().selected_agent_tools(&reviewer).unwrap();
            assert!(reviewer_tools.iter().any(|tool| tool.wire_name == "documents_search_query"));
            assert!(reviewer_tools.iter().all(|tool| !tool.capability_id.as_str().starts_with("devworks-command/")));
            let user = fx.user.clone().with_lineage(Some(project.clone()), None, None);
            let session_id = context.session_id().unwrap();
            let scope = json!({"kind":"Project","projectId":project});
            let original = markdown(&app, &fx, &context, PASSAGE);
            let imported = applied_result(&app, &user, "documents.import",
                &frame("documents.import", "manual-import", json!({"scope":scope,"title":"Greenhouse","source":original}), 10));
            let OperationResult::DocumentsImport { value: DocumentResult::Current(note) } = imported
            else { panic!("{imported:?}"); };
            let source = note.current_version.as_ref().unwrap().clone();
            applied_result(&app, &user, "documents.index",
                &frame("documents.index", "manual-index", json!({"documentId":note.id,"versionId":source.version_id,"expectedRevision":note.revision}), 12));
            tokio::time::timeout(std::time::Duration::from_secs(60), async {
                loop {
                    let ready = read(&*fx.store, |r| r.documents().document_index(&source.version_id))
                        .is_some_and(|row| row.stage == DocumentIndexStage::Ready);
                    if ready { break; }
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
            }).await.expect("native lexical index");

            let attachment = markdown(&app, &fx, &context, ATTACHMENT);
            let revision = read(&*fx.store, |r| r.sessions().session(session_id)).unwrap().revision;
            applied_result(&app, &user, "sessions.append_message", &json!({
                "protocol":PROTOCOL,"requestKey":"attach-source", "sessionId":session_id,
                "expectedSessionRevision":revision.to_string(),
                "content":[{"kind":"Text","text":"Check the indexed greenhouse evidence."},
                    {"kind":"Attachment","artifact":attachment}]
            }));

            let list = || read(&*fx.store, |r| r.artifacts().document_artifacts(
                &ArtifactScope::Project(project.clone()), None, DocumentListFilter { retrieval_enabled: None, current_only: false }, None, 10));
            assert_eq!(list().len(), 1, "attaching Markdown must not import it");
            let tools = app.providers().unwrap().schemas().selected_agent_tools(&context).unwrap();
            let offered = |name: &str| tools.iter().any(|tool| tool.wire_name == name);
            assert!(offered("documents_search_query"));
            assert!(offered("documents_read_query"));
            assert!(offered("documents_resolve_citation_query"));
            assert!(offered("documents_import_command"));
            assert!(offered("artifacts_read_query"));
            assert!(tools.iter().all(|tool| tool.wire_name.starts_with("documents_") || tool.wire_name.starts_with("artifacts_")));
            let search = |scope: &Value, text: &str| json!({
                "scope":scope,"sources":["Document"],"versions":{"kind":"Current"},
                "input":{"kind":"Text","text":text},"mode":"Lexical","lexicalFallback":false,
                "limits":{"maxResults":10,"maxVisited":1000,"deadlineMs":10000}
            });
            let move_frame = json!({"protocol":PROTOCOL,"schemaRevision":10,"requestKey":"user-moves-source",
                "command":{"kind":"Move","artifactId":note.id,"expectedScopeRevision":note.scope_revision,
                    "targetProjectId":"project-b"}});
            let mut calls = vec![
                emitted_tool("search", "documents_search_query", search(&scope, "greenhouse")),
                emitted_tool("only-attachment", "documents_search_query", search(&scope, "control")),
                emitted_tool("explicit-import", "documents_import_command",
                    json!({"scope":scope,"title":"Explicitly imported attachment","source":attachment})),
                emitted_tool("foreign", "documents_search_query",
                    search(&json!({"kind":"Project","projectId":"project-b"}), "greenhouse")),
                emitted_tool("forged", "documents_read_query",
                    json!({"query":{"kind":"Export","artifactId":note.id,"versionId":source.version_id},"limit":10})),
            ];
            if offered("documents_move_command") {
                calls.push(emitted_tool("unauthorized-move", "documents_move_command",
                    json!({"command":move_frame["command"]})));
            } else {
                let denied = app.command(&context, "documents.move", move_frame.to_string().as_bytes()).unwrap();
                assert!(matches!(&denied, CommandReply::Rejected { .. })
                    || matches!(&denied, CommandReply::Recorded { receipt, .. }
                        if matches!(receipt.decision, DomainDecision::Refused { .. })), "{denied:?}");
            }
            *wire.calls.lock().unwrap() = calls;
            assert!(wire.change_scope.set(ChangeScope {
                app: Arc::downgrade(&app), user: fx.user.clone(), session: session_id.clone(), move_frame
            }).is_ok());

            let (_stop_tx, stop) = tokio::sync::watch::channel(false);
            let end = tokio::time::timeout(std::time::Duration::from_secs(180),
                NativeAgentTurnExecutor::new(&app).run(context.clone(), primary, stop))
                .await.expect("document tool turn finished");
            assert!(matches!(end, LaunchEnd::Finished { outcome: ActivationOutcome::Completed, .. }), "{end:?}");

            let bodies = wire.bodies.lock().unwrap().clone();
            assert_eq!(bodies.len(), 4);
            let visible = tool_result(&bodies[1], "search");
            let result: Value = serde_json::from_str(visible).unwrap();
            let passage = &result["result"]["value"]["passages"][0];
            assert_eq!(passage["text"], PASSAGE);
            assert_eq!(passage["citation"]["source"]["version"], json!(source));
            assert_eq!(passage["citation"]["locator"]["kind"], "Text");
            let only_attachment: Value = serde_json::from_str(tool_result(&bodies[1], "only-attachment")).unwrap();
            assert_eq!(only_attachment["result"]["value"]["passages"], json!([]));
            assert!(tool_result(&bodies[1], "foreign").contains("PermissionDenied"));
            assert!(tool_result(&bodies[1], "forged").contains("tool_invalid_arguments"));
            if offered("documents_move_command") {
                assert!(tool_result(&bodies[1], "unauthorized-move").contains("PermissionDenied"));
            }
            let opened: Value = serde_json::from_str(tool_result(&bodies[2], "open")).unwrap();
            assert_eq!(opened["result"]["value"]["text"], PASSAGE);
            assert_eq!(opened["result"]["value"]["citation"], passage["citation"]);
            assert!(tool_result(&bodies[2], "copy-read").contains(PASSAGE));
            assert!(tool_result(&bodies[3], "copy-read").contains(UNAVAILABLE));
            assert!(tool_result(&bodies[3], "search").contains(UNAVAILABLE));
            assert!(tool_result(&bodies[3], "open").contains(UNAVAILABLE));
            assert!(!bodies[3].to_string().contains(PASSAGE));
            assert!(tool_result(&bodies[3], "reopen").contains("ViewUnavailable"));
            assert!(tool_result(&bodies[1], "explicit-import").contains("Explicitly imported attachment"));
            assert_eq!(list().iter().map(|document| document.title.as_str()).collect::<Vec<_>>(),
                ["Explicitly imported attachment"]);

            let session = read(&*fx.store, |r| r.sessions().session(session_id)).unwrap();
            let links = read(&*fx.store, |r| r.sessions().links(context.root_id().unwrap(), &["tool_response"], None, 50));
            let history: Vec<ArtifactRef> = links.iter().filter_map(|link|
                serde_json::from_value(link.body["documentEvidence"].clone()).ok()).collect();
            assert_eq!(history.len(), 4);
            let records = read(&*fx.store, |r| r.sessions().context_sources(session_id, session.context_epoch, 0, 100));
            assert_eq!(records.iter().filter(|entry| entry.kind == CONTEXT_SOURCE_KIND).count(), 4);
            assert!(history.iter().any(|evidence|
                app.artifacts().read_exact(&*fx.store, &context, evidence, 1024 * 1024).is_err()));
            let old_source_holds = read(&*fx.store, |r| r.artifacts().links(&source.artifact_id));
            assert!(old_source_holds.iter().any(|link| link.subject == SubjectKey::session(session_id) && link.version_id == source.version_id));

            Arc::try_unwrap(app).ok().expect("runtime has one strong owner").shutdown().await;
        });
}
