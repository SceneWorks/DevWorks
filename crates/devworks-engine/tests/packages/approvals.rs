use super::*;
use devworks_core::ports::{
    AuthorityDenial, AuthorityPort, AuthorityRequest, Authorization, Clock,
};

struct RealAuthority {
    store: Arc<dyn ApplicationStore>,
    clock: Arc<dyn Clock>,
}
impl AuthorityPort for RealAuthority {
    fn authorize(
        &self,
        ctx: &InvocationContext,
        request: &AuthorityRequest<'_>,
    ) -> Result<Authorization, AuthorityDenial> {
        devworks_core::authority::HostAuthority {
            store: &*self.store,
            clock: &*self.clock,
            protected_branches: &[],
        }
        .authorize(ctx, request)
    }
}
fn native_app() -> App {
    let mut a = App::unbooted(Harness::new(), None, None);
    let mut c = a.composition();
    c.authority = Arc::new(RealAuthority {
        store: a.store.clone(),
        clock: a.h.clock(),
    });
    c.config.approval_resume_interval = Duration::from_secs(3600);
    a.app = Some(a.rt.block_on(boot(c)).unwrap());
    a
}
fn saved(a: &App, source: &ProposalRecord) -> StoredApproval {
    let rows = read(&*a.store, |s| {
        s.decisions().list_approval_sources(
            &ApprovalSourceFilter {
                scope: ConfigScope::Workspace,
                state: None,
            },
            None,
            500,
        )
    });
    rows.into_iter()
        .filter(|r| r.source == ApprovalSource::Generic)
        .find_map(|r| {
            let row = read(&*a.store, |s| s.decisions().approval(&r.id)).unwrap();
            (row.payload.pointer("/binding/proposal/id") == Some(&json!(source.id))).then_some(row)
        })
        .expect("owner-derived saved package review")
}
fn decide(a: &App, p: &StoredApproval, decision: &str) {
    let frame = json!({"mode":"Command","protocol":"1.0-draft.1","schemaRevision":3,"requestKey":a.key("decision"),"body":{"kind":"approvals.record_human_decision","proposalId":p.id,"expectedRevision":p.revision.to_string(),"contentDigest":p.content_digest,"decision":decision,"rationale":"Review exact signed fixture version"}});
    applied(
        a.app()
            .record_human_decision(&a.user, &serde_json::to_vec(&frame).unwrap())
            .unwrap(),
    );
}
#[test]
fn saved_package_source_rejection_uses_exact_owner_proposal() {
    let _serial = serial();
    let a = native_app();
    let spec = Spec::new("saved-reject", "1.0.0");
    let (source, digest, _) = propose(&a, &spec);
    let review = saved(&a, &source);
    assert_eq!(review.author, *a.agent.principal());
    assert_eq!(review.author_kind, PrincipalKind::Agent);
    assert_eq!(review.state, "Pending");
    decide(&a, &review, "Reject");
    let source = read(&*a.store, |s| s.packages().proposal(&source.id)).unwrap();
    assert_eq!(source.state, "Rejected");
    let version = read(&*a.store, |s| {
        s.packages().package_version("saved-reject", &digest)
    })
    .unwrap();
    assert_eq!(version.phase, VersionPhase::Rejected);
    assert!(read(&*a.store, |s| s.decisions().approval_resume(&review.id)).is_none());
}
#[test]
fn saved_package_approval_readmits_original_agent_and_spends_exact_grant() {
    let _serial = serial();
    let a = native_app();
    let spec = Spec::new("saved-approve", "1.0.0");
    let (source, digest, _) = propose(&a, &spec);
    let review = saved(&a, &source);
    decide(&a, &review, "Approve");
    a.rt.block_on(a.app().approval_resumes().tick()).unwrap();
    let resume = read(&*a.store, |s| s.decisions().approval_resume(&review.id)).unwrap();
    assert_eq!(resume.state, "Admitted", "{resume:?}");
    assert_eq!(resume.principal, *a.agent.principal());
    assert_eq!(resume.principal_kind, PrincipalKind::Agent);
    let version = read(&*a.store, |s| {
        s.packages().package_version("saved-approve", &digest)
    })
    .unwrap();
    assert!(version.approval.is_some(), "{version:?}");
    let source = read(&*a.store, |s| s.packages().proposal(&source.id)).unwrap();
    assert_eq!(source.state, "Approved");
    assert_eq!(source.decided_by, Some(a.user.principal().clone()));
    let grant = read(&*a.store, |s| {
        s.authority()
            .authority_grant(resume.grant_id.as_ref().unwrap())
    })
    .unwrap();
    assert_eq!(grant.uses_remaining, Some(0));
    a.rt.block_on(a.app().approval_resumes().tick()).unwrap();
    assert_eq!(
        read(&*a.store, |s| s.decisions().approval_resume(&review.id)).unwrap(),
        resume
    );
}

struct PauseBeforeReadmission {
    runtime: Arc<Runtime>,
    user: InvocationContext,
    pause: Vec<u8>,
    paused: std::sync::atomic::AtomicBool,
}
impl devworks_engine::approvals::ResumeAdmission for PauseBeforeReadmission {
    fn readmit(
        &self,
        ctx: &InvocationContext,
        operation: &str,
        raw: &[u8],
        options: WriteOptions,
    ) -> devworks_core::gateway::CommandOutcome {
        if !self.paused.swap(true, Ordering::SeqCst) {
            applied(
                self.runtime
                    .command(&self.user, "runs.pause", &self.pause)
                    .unwrap(),
            );
        }
        self.runtime.command_with(ctx, operation, raw, options)
    }
}
#[test]
fn saved_package_late_root_pause_preserves_exact_pending_source_and_unspent_grant() {
    let _serial = serial();
    let mut a = native_app();
    seed_root(&a.h, "package-root");
    rusqlite::Connection::open(a.h.root.join("workspace.sqlite"))
        .unwrap()
        .execute(
            "UPDATE work_items SET kind='issue' WHERE id='package-root'",
            [],
        )
        .unwrap();
    a.agent = a.agent.clone().with_lineage(
        Some(ProjectId(id("project-a"))),
        Some(devworks_core::protocol::RootId(id("package-root"))),
        None,
    );
    a.rt.block_on(a.app().approval_resumes().drain());
    let spec = Spec::new("saved-paused", "1.0.0");
    let (source, digest, _) = propose(&a, &spec);
    // The source inherited the real root's project scope.
    let rows = read(&*a.store, |s| {
        s.decisions().list_approval_sources(
            &ApprovalSourceFilter {
                scope: ConfigScope::Project(ProjectId(id("project-a"))),
                state: None,
            },
            None,
            500,
        )
    });
    let review = rows
        .into_iter()
        .filter(|r| r.source == ApprovalSource::Generic)
        .find_map(|r| read(&*a.store, |s| s.decisions().approval(&r.id)))
        .unwrap();
    decide(&a, &review, "Approve");
    let runtime = Arc::new(a.app.take().unwrap());
    let admission = Arc::new(PauseBeforeReadmission {runtime:runtime.clone(),user:a.user.clone(),pause:serde_json::to_vec(&json!({"protocol":"1.0-draft.1","requestKey":"late-package-pause","command":{"kind":"runs.pause","rootId":"package-root","expectedControlRevision":"1"}})).unwrap(),paused:std::sync::atomic::AtomicBool::new(false)});
    let dispatcher = devworks_engine::approvals::ResumeDispatcher::new(
        a.store.clone(),
        runtime.approvals().clone(),
        admission.clone(),
    );
    a.rt.block_on(dispatcher.tick()).unwrap();
    assert!(
        admission.paused.load(Ordering::SeqCst),
        "pause must happen after prepare_resume returned Ready"
    );
    let pending = read(&*a.store, |s| s.decisions().approval_resume(&review.id)).unwrap();
    assert!(
        matches!(pending.state.as_str(), "Pending" | "Unknown"),
        "{pending:?}"
    );
    let grant = read(&*a.store, |s| {
        s.authority()
            .authority_grant(pending.grant_id.as_ref().unwrap())
    })
    .unwrap();
    assert_eq!(grant.uses_remaining, Some(1));
    assert_eq!(
        read(&*a.store, |s| s.packages().proposal(&source.id))
            .unwrap()
            .state,
        "Pending"
    );
    assert!(
        read(&*a.store, |s| s
            .packages()
            .package_version("saved-paused", &digest))
        .unwrap()
        .approval
        .is_none()
    );
    let root = devworks_core::protocol::RootId(id("package-root"));
    let control = read(&*a.store, |s| s.runs().root_control(&root)).unwrap();
    assert_eq!(control.desired, DesiredExecution::Paused);
    let holds = read(&*a.store, |s| {
        s.runs()
            .blockers(&BlockerFilter::RootLevel(root), None, 500)
    });
    let covered: Vec<_> = holds
        .iter()
        .filter(|h| h.cause.manual_resume())
        .map(|h| json!({"holdId":h.id,"revision":h.revision.to_string()}))
        .collect();
    let frame=serde_json::to_vec(&json!({"protocol":"1.0-draft.1","requestKey":"resume-package-root","command":{"kind":"runs.continue","rootId":"package-root","expectedControlRevision":control.revision.to_string(),"coveredHolds":covered}})).unwrap();
    applied(runtime.command(&a.user, "runs.continue", &frame).unwrap());
    a.rt.block_on(dispatcher.tick()).unwrap();
    assert_eq!(
        read(&*a.store, |s| s.decisions().approval_resume(&review.id))
            .unwrap()
            .state,
        "Admitted"
    );
    assert_eq!(
        read(&*a.store, |s| s.packages().proposal(&source.id))
            .unwrap()
            .state,
        "Approved"
    );
    a.rt.block_on(dispatcher.drain());
    drop(dispatcher);
    drop(admission);
    let runtime = Arc::try_unwrap(runtime)
        .unwrap_or_else(|_| panic!("runtime retained after drained dispatcher"));
    a.rt.block_on(runtime.shutdown());
}

fn large_signed_bundle(package: &str) -> (Spec, Vec<u8>, String, usize) {
    let mut spec = Spec::new(package, "1.0.0");
    spec.config_schema = Some(
        json!({"type":"object","description":"schema evidence ".repeat(3500),"additionalProperties":true}),
    );
    let (original, _) = bundle(&spec);
    let verified =
        devworks_core::ports::packages::PackageInspector::inspect(&BundleInspector, &original)
            .unwrap();
    let length = u32::from_le_bytes(original[8..12].try_into().unwrap()) as usize;
    let mut manifest: Value = serde_json::from_slice(&original[12..12 + length]).unwrap();
    let mut files: Vec<(String, Vec<u8>)> = verified
        .files
        .into_iter()
        .map(|f| (f.path, f.bytes))
        .collect();
    for i in 0..1800 {
        let path = format!("evidence/{i:04}-{}.txt", "x".repeat(180));
        let bytes = format!("exact evidence {i}\n").into_bytes();
        manifest["files"].as_array_mut().unwrap().push(
            json!({"path":path,"sha256":sha256_hex(&bytes),"byteLength":bytes.len().to_string()}),
        );
        files.push((path, bytes));
    }
    let manifest = serde_json::to_vec(&manifest).unwrap();
    assert!(
        manifest.len() > 524288,
        "legal source must exceed the public preview JSON limit"
    );
    let refs: Vec<_> = files
        .iter()
        .map(|(p, b)| (p.as_str(), b.as_slice()))
        .collect();
    let bytes = write_bundle(&manifest, &refs, Some(&key(1)));
    devworks_core::ports::packages::PackageInspector::inspect(&BundleInspector, &bytes).unwrap();
    (spec, bytes, sha256_hex(&manifest), files.len())
}

#[test]
fn large_signed_source_review_is_complete_human_readable_and_mutation_fenced() {
    let _serial = serial();
    let a = native_app();
    let (spec, bytes, manifest_digest, file_count) = large_signed_bundle("saved-large");
    let source = a.save(&a.agent, &bytes);
    let proposal=match applied(a.domain(&a.agent,"addons.propose",json!({"kind":"addons.propose","source":source,"manifestDigest":manifest_digest,"basePackageId":spec.package}))){OperationResult::AddonsPropose{value}=>value,other=>panic!("{other:?}")};
    let rows = read(&*a.store, |s| {
        s.decisions().list_approval_sources(
            &ApprovalSourceFilter {
                scope: ConfigScope::Workspace,
                state: None,
            },
            None,
            500,
        )
    });
    let review = rows
        .into_iter()
        .filter(|r| r.source == ApprovalSource::Generic)
        .find_map(|r| read(&*a.store, |s| s.decisions().approval(&r.id)))
        .unwrap();
    assert!(review.payload["binding"].get("packageReview").is_some());
    assert!(
        serde_json::to_vec(&review.payload["reviewed"])
            .unwrap()
            .len()
            < 524288
    );
    assert!(
        review.payload["binding"]["packageReview"]["document"]
            .as_str()
            .unwrap()
            .len()
            > 524288
    );
    assert!(
        serde_json::to_vec(&review.payload["binding"]["owner"])
            .unwrap()
            .len()
            < 65536
    );
    let inspect=serde_json::to_vec(&json!({"mode":"Query","protocol":"1.0-draft.1","schemaRevision":3,"queryId":"inspect-large","body":{"kind":"approvals.inspect","proposalId":review.id}})).unwrap();
    assert!(matches!(
        a.app()
            .query(&a.user, "approvals.inspect", &inspect)
            .unwrap(),
        DomainQueryReply::Result { .. }
    ));
    let artifact: devworks_core::protocol::provider::ArtifactRef =
        serde_json::from_value(review.artifact.clone().unwrap()).unwrap();
    let mut cursor = None;
    let mut complete = String::new();
    loop {
        let mut frame = json!({"protocol":"1.0-draft.1","schemaRevision":4,"queryId":a.key("read-review"),"query":{"kind":"Read","artifactId":artifact.artifact_id,"versionId":artifact.version_id},"limit":1});
        if let Some(cursor) = &cursor {
            frame["cursor"] = json!(cursor);
        }
        match a
            .app()
            .query(
                &a.user,
                "artifacts.read",
                &serde_json::to_vec(&frame).unwrap(),
            )
            .unwrap()
        {
            DomainQueryReply::Result {
                result:
                    OperationResult::ArtifactsRead {
                        value:
                            devworks_core::protocol::records::ArtifactReadContent::Text {
                                content,
                                next_byte,
                                truncated,
                                ..
                            },
                    },
                ..
            } => {
                complete.push_str(&content.0);
                if !truncated {
                    break;
                }
                cursor = Some(next_byte.get().to_string());
            }
            other => panic!("Human must read entire original Agent review evidence: {other:?}"),
        }
    }
    assert_eq!(
        sha256_hex(complete.as_bytes()),
        String::from(artifact.sha256.clone())
    );
    let facts: Value = serde_json::from_str(&complete).unwrap();
    assert_eq!(
        facts["inspection"]["files"].as_array().unwrap().len(),
        file_count
    );
    assert_eq!(
        facts["inspection"]["configurationSchemaDocument"],
        spec.config_schema.clone().unwrap()
    );
    // A late owner mutation of the retained evidence invalidates the exact reviewed generation.
    let tag=serde_json::to_vec(&json!({"mode":"Command","protocol":"1.0-draft.1","schemaRevision":1,"requestKey":"mutate-review-artifact","body":{"kind":"artifacts.tag","artifactId":artifact.artifact_id,"expectedRevision":"1","tags":["changed-after-review"]}})).unwrap();
    applied(a.app().command(&a.user, "artifacts.tag", &tag).unwrap());
    let frame=serde_json::to_vec(&json!({"mode":"Command","protocol":"1.0-draft.1","schemaRevision":3,"requestKey":"refuse-mutated-review","body":{"kind":"approvals.record_human_decision","proposalId":review.id,"expectedRevision":review.revision.to_string(),"contentDigest":review.content_digest,"decision":"Approve","rationale":"Exact original evidence"}})).unwrap();
    let reply = a.app().record_human_decision(&a.user, &frame).unwrap();
    assert!(
        matches!(
            reply,
            CommandReply::Recorded {
                receipt: devworks_core::protocol::Receipt {
                    decision: DomainDecision::Refused { .. },
                    ..
                },
                ..
            }
        ),
        "{reply:?}"
    );
    assert_eq!(
        read(&*a.store, |s| s.packages().proposal(&proposal.id))
            .unwrap()
            .state,
        "Pending"
    );
    assert!(read(&*a.store, |s| s.decisions().approval_resume(&review.id)).is_none());
    let before = read(&*a.store, |s| {
        s.artifacts()
            .list_workspace(Some(a.agent.principal()), &[], None, 500)
    })
    .len();
    decide(&a, &review, "Reject");
    assert_eq!(
        read(&*a.store, |s| s.decisions().approval(&review.id))
            .unwrap()
            .state,
        "Rejected"
    );
    assert_eq!(
        read(&*a.store, |s| s.packages().proposal(&proposal.id))
            .unwrap()
            .state,
        "Rejected"
    );
    assert_eq!(
        read(&*a.store, |s| s.artifacts().list_workspace(
            Some(a.agent.principal()),
            &[],
            None,
            500
        ))
        .len(),
        before,
        "reject must not prepare or publish replacement evidence"
    );
}

#[test]
fn large_signed_package_readmission_uses_compact_exact_owner_evidence() {
    let _serial = serial();
    let a = native_app();
    let (spec, bytes, digest, _) = large_signed_bundle("saved-large-admit");
    let source = a.save(&a.agent, &bytes);
    applied(a.domain(&a.agent,"addons.propose",json!({"kind":"addons.propose","source":source,"manifestDigest":digest,"basePackageId":spec.package})));
    let rows = read(&*a.store, |s| {
        s.decisions().list_approval_sources(
            &ApprovalSourceFilter {
                scope: ConfigScope::Workspace,
                state: None,
            },
            None,
            500,
        )
    });
    let review = rows
        .into_iter()
        .filter(|r| r.source == ApprovalSource::Generic)
        .find_map(|r| read(&*a.store, |s| s.decisions().approval(&r.id)))
        .unwrap();
    decide(&a, &review, "Approve");
    a.rt.block_on(a.app().approval_resumes().tick()).unwrap();
    let resume = read(&*a.store, |s| s.decisions().approval_resume(&review.id)).unwrap();
    assert_eq!(resume.state, "Admitted", "{resume:?}");
    let version = read(&*a.store, |s| {
        s.packages().package_version(&spec.package, &digest)
    })
    .unwrap();
    assert!(version.approval.is_some());
    assert_eq!(resume.principal, *a.agent.principal());
}

#[test]
fn saved_enable_preserves_large_current_configuration_without_input_narrowing() {
    let _serial = serial();
    let a = native_app();
    let mut spec = Spec::new("saved-large-config", "1.0.0");
    let doc = json!({"type":"object","properties":{"padding":{"type":"string"}},"additionalProperties":false});
    spec.config_schema = Some(doc.clone());
    package(a.install(&a.user, &spec));
    a.ready(&spec.package);
    let current = read(&*a.store, |s| s.packages().package(&spec.package)).unwrap();
    let configuration = json!({"padding":"exact retained config ".repeat(20000)});
    assert!(serde_json::to_vec(&configuration).unwrap().len() > 400000);
    applied(a.domain(&a.user,"addons.configure",json!({"kind":"addons.configure","packageId":spec.package,"expectedRevision":current.revision.to_string(),"config":{"schema":schema(&format!("{}/config",spec.package),&sha256_hex(doc.to_string().as_bytes())),"value":configuration}})));
    applied(a.simple(&a.user, "addons.disable", "Disable", &spec.package));
    let current = read(&*a.store, |s| s.packages().package(&spec.package)).unwrap();
    let args = json!({"command":{"kind":"Enable","packageId":spec.package,"expectedRegistryRevision":a.registry().to_string()}});
    let request =
        devworks_core::services::approvals::requests::decode("addons.enable", 2, &args, true)
            .unwrap();
    let frame=serde_json::to_vec(&json!({"mode":"Command","protocol":"1.0-draft.1","schemaRevision":3,"requestKey":"review-large-current-config","body":{"kind":"approvals.propose","scope":{"kind":"Workspace"},"operationKind":"addons.enable","target":{"id":spec.package,"revision":current.revision.to_string()},"contentDigest":request.fingerprint.as_str(),"summary":"Enable exact current configuration","payload":{"kind":"SavedOperation","schemaRevision":"2","input":args}}})).unwrap();
    applied(
        a.app()
            .command(&a.agent, "approvals.propose", &frame)
            .unwrap(),
    );
    let rows = read(&*a.store, |s| {
        s.decisions().list_approval_sources(
            &ApprovalSourceFilter {
                scope: ConfigScope::Workspace,
                state: None,
            },
            None,
            500,
        )
    });
    let review = rows
        .into_iter()
        .filter(|r| r.source == ApprovalSource::Generic)
        .find_map(|r| read(&*a.store, |s| s.decisions().approval(&r.id)))
        .unwrap();
    assert!(review.payload["binding"].get("packageReview").is_some());
    assert!(
        serde_json::to_vec(&review.payload["binding"]["owner"])
            .unwrap()
            .len()
            < 16000
    );
    decide(&a, &review, "Approve");
    a.rt.block_on(a.app().approval_resumes().tick()).unwrap();
    let resumed = read(&*a.store, |s| s.decisions().approval_resume(&review.id)).unwrap();
    assert_eq!(resumed.state, "Admitted", "{resumed:?}");
    let current = read(&*a.store, |s| s.packages().package(&spec.package)).unwrap();
    assert!(current.enabled);
    assert_eq!(current.config.unwrap()["value"], configuration);
}

#[test]
fn legacy_package_source_requires_fresh_context_instead_of_fabricated_replay() {
    let _serial = serial();
    let a = native_app();
    let spec = Spec::new("legacy-source", "1.0.0");
    let (old, digest, _) = propose(&a, &spec);
    let old_review = saved(&a, &old);
    // A pre-DW14 durable fixture: exact version/source and known actor exist, invocation does not.
    let connection = rusqlite::Connection::open(a.h.root.join("workspace.sqlite")).unwrap();
    connection.execute("UPDATE package_proposals SET origin_json=json_remove(origin_json,'$.sourceContext') WHERE id=?1",[old.id.as_str()]).unwrap();
    connection
        .execute(
            "DELETE FROM decision_approvals WHERE id=?1",
            [old_review.id.as_str()],
        )
        .unwrap();
    drop(connection);
    assert_eq!(
        a.app()
            .packages()
            .unwrap()
            .service()
            .sync_pending_reviews()
            .unwrap(),
        0
    );
    assert!(read(&*a.store, |s| s.decisions().approval(&old_review.id)).is_none());
    let shown = a.record(&spec.package);
    assert!(
        shown
            .availability_reason
            .unwrap()
            .0
            .contains("Original invocation was not recorded")
    );
    let fresh = devworks_core::context::HostIngress::new(&*a.store).context(
        a.agent.principal().clone(),
        PrincipalKind::Agent,
        id("actual-fresh-invocation"),
        id("actual-fresh-ingress"),
        a.agent.restore_epoch().clone(),
        a.agent.policy_revision().clone(),
    );
    let (fresh_bytes, _) = bundle(&spec);
    let source = a.save(&fresh, &fresh_bytes);
    let new=match applied(a.domain(&fresh,"addons.propose",json!({"kind":"addons.propose","source":source,"manifestDigest":digest,"basePackageId":spec.package}))){OperationResult::AddonsPropose{value}=>value,other=>panic!("{other:?}")};
    assert_ne!(new.id, old.id);
    let old = read(&*a.store, |s| s.packages().proposal(&old.id)).unwrap();
    assert_eq!(old.state, "Superseded");
    assert_eq!(
        old.decision.unwrap()["reason"],
        "FreshProposalRequiredOriginalInvocationUnavailable"
    );
    let current = read(&*a.store, |s| s.packages().proposal(&new.id)).unwrap();
    assert_eq!(
        current.origin["sourceContext"]["invocationContext"]["invocation"],
        json!(fresh.invocation_id())
    );
    let review = saved(&a, &new);
    assert_eq!(
        review.payload["invocation"]["invocation"],
        json!(fresh.invocation_id())
    );
    assert_eq!(
        review.payload["invocation"]["namespace"],
        json!(fresh.ingress_namespace())
    );
    decide(&a, &review, "Approve");
    a.rt.block_on(a.app().approval_resumes().tick()).unwrap();
    assert_eq!(
        read(&*a.store, |s| s.decisions().approval_resume(&review.id))
            .unwrap()
            .state,
        "Admitted"
    );
}

#[test]
fn saved_configuration_preserves_the_legal_business_json_boundary() {
    let _serial = serial();
    let a = native_app();
    let mut spec = Spec::new("saved-config-boundary", "1.0.0");
    let doc = json!({"type":"object","properties":{"text":{"type":"string","maxLength":524270}},"required":["text"],"additionalProperties":false});
    spec.config_schema = Some(doc.clone());
    package(a.install(&a.user, &spec));
    a.ready(&spec.package);
    let value = json!({"text":"x".repeat(524270)});
    assert_eq!(serde_json::to_vec(&value).unwrap().len(), 524281);
    let config = json!({"schema":schema(&format!("{}/config",spec.package),&sha256_hex(doc.to_string().as_bytes())),"value":value});
    let current = read(&*a.store, |s| s.packages().package(&spec.package)).unwrap();
    // The actual owner and real signed package schema accept this existing 512KiB inner value.
    applied(a.domain(&a.user,"addons.configure",json!({"kind":"addons.configure","packageId":spec.package,"expectedRevision":current.revision.to_string(),"config":config})));
    let current = read(&*a.store, |s| s.packages().package(&spec.package)).unwrap();
    let mut proposed = config.clone();
    proposed["value"]["text"] = json!("y".repeat(524270));
    let args = json!({"packageId":spec.package,"expectedRevision":current.revision.to_string(),"config":proposed});
    assert!(serde_json::to_vec(&args).unwrap().len() > 524288);
    let request =
        devworks_core::services::approvals::requests::decode("addons.configure", 2, &args, true)
            .unwrap();
    let frame=serde_json::to_vec(&json!({"mode":"Command","protocol":"1.0-draft.1","schemaRevision":3,"requestKey":"save-config-boundary","body":{"kind":"approvals.propose","scope":{"kind":"Workspace"},"operationKind":"addons.configure","target":{"id":spec.package,"revision":current.revision.to_string()},"contentDigest":request.fingerprint.as_str(),"summary":"Exact complete current configuration","payload":{"kind":"SavedOperation","schemaRevision":"2","input":args}}})).unwrap();
    applied(
        a.app()
            .command(&a.agent, "approvals.propose", &frame)
            .unwrap(),
    );
    let rows = read(&*a.store, |s| {
        s.decisions().list_approval_sources(
            &ApprovalSourceFilter {
                scope: ConfigScope::Workspace,
                state: None,
            },
            None,
            500,
        )
    });
    let review = rows
        .into_iter()
        .filter(|r| r.source == ApprovalSource::Generic)
        .find_map(|r| read(&*a.store, |s| s.decisions().approval(&r.id)))
        .unwrap();
    assert!(
        serde_json::to_vec(&review.payload["content"]["input"])
            .unwrap()
            .len()
            > 524288
    );
    assert!(
        serde_json::from_value::<devworks_core::protocol::support::DynamicJson>(
            review.payload["reviewed"].clone()
        )
        .is_ok()
    );
    decide(&a, &review, "Approve");
    a.rt.block_on(a.app().approval_resumes().tick()).unwrap();
    assert_eq!(
        read(&*a.store, |s| s.decisions().approval_resume(&review.id))
            .unwrap()
            .state,
        "Admitted"
    );
    let actual = read(&*a.store, |s| s.packages().package(&spec.package))
        .unwrap()
        .config
        .unwrap();
    assert_eq!(actual["value"]["text"].as_str().unwrap().len(), 524270);
    assert!(
        actual["value"]["text"]
            .as_str()
            .unwrap()
            .bytes()
            .all(|byte| byte == b'y')
    );
}

#[test]
fn selected_tool_hard_deny_prevents_large_owner_snapshot_publication() {
    let _serial = serial();
    let a = native_app();
    let (spec, bytes, _, _) = large_signed_bundle("unreviewable-source");
    let source = a.save(&a.agent, &bytes);
    let current = read(&*a.store, |s| s.authority().principal(a.agent.principal())).unwrap();
    let human = a.user.clone();
    let actor = a.agent.principal().clone();
    write(&*a.store, move |tx| {
        bind_principal(
            tx,
            &human,
            NewPrincipalBinding {
                actor,
                kind: PrincipalKind::Agent,
                role: Some(AgentRole::Worker),
                project_id: None,
                tool_families: Some(vec!["approvals".into()]),
                bound_by: human.principal().clone(),
            },
            Some(current.revision),
        )
        .map_err(|e| StorageError::InvalidRecord(format!("{e:?}")))
    })
    .unwrap();
    let before = read(&*a.store, |s| {
        s.artifacts()
            .list_workspace(Some(a.agent.principal()), &[], None, 500)
    })
    .len();
    let args = json!({"command":{"kind":"Install","bundle":source,"expectedRegistryRevision":a.registry().to_string()}});
    let request =
        devworks_core::services::approvals::requests::decode("addons.install", 2, &args, true)
            .unwrap();
    let frame=serde_json::to_vec(&json!({"mode":"Command","protocol":"1.0-draft.1","schemaRevision":3,"requestKey":"deny-before-evidence","body":{"kind":"approvals.propose","scope":{"kind":"Workspace"},"operationKind":"addons.install","target":{"id":spec.package,"revision":"1"},"contentDigest":request.fingerprint.as_str(),"summary":"Unqualified source must stay unread","payload":{"kind":"SavedOperation","schemaRevision":"2","input":args}}})).unwrap();
    let reply = a
        .app()
        .command(&a.agent, "approvals.propose", &frame)
        .unwrap();
    assert!(
        matches!(
            reply,
            CommandReply::Recorded {
                receipt: devworks_core::protocol::Receipt {
                    decision: DomainDecision::Refused {
                        fault: devworks_core::protocol::DomainFault {
                            code: DomainErrorCode::PermissionDenied,
                            ..
                        }
                    },
                    ..
                },
                ..
            }
        ),
        "{reply:?}"
    );
    assert_eq!(
        read(&*a.store, |s| s.artifacts().list_workspace(
            Some(a.agent.principal()),
            &[],
            None,
            500
        ))
        .len(),
        before,
        "denied owner requests cannot publish review snapshots"
    );
    assert!(read(&*a.store, |s| s.packages().package(&spec.package)).is_none());
}

#[test]
fn saved_package_preserves_an_original_human_author_too() {
    let _serial = serial();
    let a = native_app();
    let spec = Spec::new("human-saved-source", "1.0.0");
    let (bytes, digest) = bundle(&spec);
    let source = a.save(&a.user, &bytes);
    let source=match applied(a.domain(&a.user,"addons.propose",json!({"kind":"addons.propose","source":source,"manifestDigest":digest,"basePackageId":spec.package}))){OperationResult::AddonsPropose{value}=>value,other=>panic!("{other:?}")};
    let review = saved(&a, &source);
    assert_eq!(review.author_kind, PrincipalKind::Human);
    decide(&a, &review, "Approve");
    a.rt.block_on(a.app().approval_resumes().tick()).unwrap();
    let resumed = read(&*a.store, |s| s.decisions().approval_resume(&review.id)).unwrap();
    assert_eq!(resumed.state, "Admitted", "{resumed:?}");
    assert_eq!(resumed.principal, *a.user.principal());
    assert_eq!(resumed.principal_kind, PrincipalKind::Human);
    let version = read(&*a.store, |s| {
        s.packages().package_version(&spec.package, &digest)
    })
    .unwrap();
    assert_eq!(version.approval.unwrap()["approvalId"], json!(review.id));
}
