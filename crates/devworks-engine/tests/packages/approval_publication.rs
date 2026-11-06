use super::*;
use devworks_core::ports::files::{ArtifactFiles, FileFault, PublishedBlob};
use std::{io::Read, path::Path};

/// The blob has been staged durably, but its artifact metadata UOW has not begun.
struct PublicationGate {
    inner: NativeArtifactFiles,
    gate: Arc<Gate>,
}
impl ArtifactFiles for PublicationGate {
    fn publish(
        &self,
        name: &str,
        source: &mut dyn Read,
        digest: &str,
        bytes: u64,
        reuse: Option<&str>,
        generation: &str,
    ) -> Result<PublishedBlob, FileFault> {
        let published = self
            .inner
            .publish(name, source, digest, bytes, reuse, generation)?;
        self.gate.pass(IdKind::ArtifactVersion);
        Ok(published)
    }
    fn managed_path(&self, d: &str, g: &str) -> Result<String, FileFault> {
        self.inner.managed_path(d, g)
    }
    fn present(&self, d: &str, g: &str, n: u64) -> Result<bool, FileFault> {
        self.inner.present(d, g, n)
    }
    fn inspect(&self, d: &str, g: &str) -> Result<Option<u64>, FileFault> {
        self.inner.inspect(d, g)
    }
    fn read_range(&self, d: &str, g: &str, n: u64, o: u64, m: usize) -> Result<Vec<u8>, FileFault> {
        self.inner.read_range(d, g, n, o, m)
    }
    fn open(&self, d: &str, g: &str, n: u64) -> Result<Box<dyn Read + Send>, FileFault> {
        self.inner.open(d, g, n)
    }
    fn unlink(&self, d: &str, g: &str) -> Result<(), FileFault> {
        self.inner.unlink(d, g)
    }
    fn quarantine(&self, d: &str, g: &str) -> Result<(), FileFault> {
        self.inner.quarantine(d, g)
    }
    fn staged(&self) -> Result<Vec<String>, FileFault> {
        self.inner.staged()
    }
    fn remove_staged(&self, n: &str) -> Result<(), FileFault> {
        self.inner.remove_staged(n)
    }
    fn stored_files(&self) -> Result<Vec<(String, String)>, FileFault> {
        self.inner.stored_files()
    }
    fn write_derived(&self, k: &str, b: &[u8]) -> Result<(), FileFault> {
        self.inner.write_derived(k, b)
    }
    fn read_derived(&self, k: &str, m: usize) -> Result<Option<Vec<u8>>, FileFault> {
        self.inner.read_derived(k, m)
    }
    fn collect_derived(&self) -> Result<u64, FileFault> {
        self.inner.collect_derived()
    }
    fn export(&self, d: &str, g: &str, n: u64, p: &Path, o: bool) -> Result<(), FileFault> {
        self.inner.export(d, g, n, p, o)
    }
}
fn domain_rows(a: &App) -> (i64, i64, i64, i64) {
    rusqlite::Connection::open(a.h.root.join("workspace.sqlite")).unwrap().query_row("SELECT (SELECT count(*) FROM decision_approvals),(SELECT count(*) FROM work_items),(SELECT count(*) FROM repositories),(SELECT count(*) FROM knowledge_entities)",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap()
}
#[test]
fn evidence_publication_rechecks_revocation_tools_and_scope_in_its_commit() {
    let _serial = serial();
    for change in ["revoked", "tools", "project"] {
        let mut a = App::unbooted(Harness::new(), None, None);
        let gate = Arc::new(Gate::default());
        a.artifact_files = Some(Arc::new(PublicationGate {
            inner: NativeArtifactFiles::new(a.h.root.clone(), a.h.root.with_extension("cache")),
            gate: gate.clone(),
        }));
        let mut composition = a.composition();
        composition.authority = Arc::new(RealAuthority {
            store: a.store.clone(),
            clock: a.h.clock(),
        });
        a.app = Some(a.rt.block_on(boot(composition)).unwrap());
        let (spec, bytes, _, _) = large_signed_bundle(&format!("publication-{change}"));
        let bundle = a.save(&a.agent, &bytes);
        let args = json!({"command":{"kind":"Install","bundle":bundle,"expectedRegistryRevision":a.registry().to_string()}});
        let request =
            devworks_core::services::approvals::requests::decode("addons.install", 2, &args, true)
                .unwrap();
        let frame=serde_json::to_vec(&json!({"mode":"Command","protocol":"1.0-draft.1","schemaRevision":3,"requestKey":"publisher-revocation-race","body":{"kind":"approvals.propose","scope":{"kind":"Workspace"},"operationKind":"addons.install","target":{"id":spec.package,"revision":"1"},"contentDigest":request.fingerprint.as_str(),"summary":"Exact signed source","payload":{"kind":"SavedOperation","schemaRevision":"2","input":args}}})).unwrap();
        let before = domain_rows(&a);
        let artifacts = read(&*a.store, |s| {
            s.artifacts()
                .list_workspace(Some(a.agent.principal()), &[], None, 500)
        })
        .len();
        gate.arm(IdKind::ArtifactVersion);
        let _release = Release(gate.clone());
        let reply = std::thread::scope(|scope| {
            let a_ref = &a;
            let pending = scope.spawn(move || {
                a_ref
                    .app()
                    .command(&a_ref.agent, "approvals.propose", &frame)
                    .unwrap()
            });
            assert!(
                gate.entered(Duration::from_secs(10)),
                "publisher did not enter its pre-commit gate"
            );
            let principal =
                read(&*a.store, |s| s.authority().principal(a.agent.principal())).unwrap();
            let human = a.user.clone();
            let actor = a.agent.principal().clone();
            write(&*a.store, move |tx| {
                if change == "revoked" {
                    devworks_core::authority::service::revoke_principal(
                        tx,
                        &human,
                        &actor,
                        principal.revision,
                    )
                } else {
                    bind_principal(
                        tx,
                        &human,
                        NewPrincipalBinding {
                            actor,
                            kind: PrincipalKind::Agent,
                            role: Some(AgentRole::Worker),
                            project_id: (change == "project").then(|| ProjectId(id("project-a"))),
                            tool_families: (change == "tools").then(|| vec!["approvals".into()]),
                            bound_by: human.principal().clone(),
                        },
                        Some(principal.revision),
                    )
                }
                .map_err(|e| StorageError::InvalidRecord(format!("{e:?}")))
            })
            .unwrap();
            gate.release();
            pending.join().unwrap()
        });
        assert!(
            !matches!(
                reply,
                CommandReply::Recorded {
                    receipt: devworks_core::protocol::Receipt {
                        decision: DomainDecision::Applied { .. }
                            | DomainDecision::NoChange { .. }
                            | DomainDecision::Accepted { .. },
                        ..
                    },
                    ..
                }
            ),
            "{change}: {reply:?}"
        );
        assert_eq!(
            read(&*a.store, |s| s.artifacts().list_workspace(
                Some(a.agent.principal()),
                &[],
                None,
                500
            ))
            .len(),
            artifacts,
            "{change}: unauthorized evidence became visible"
        );
        assert_eq!(
            domain_rows(&a),
            before,
            "{change}: owner snapshot publication changed domain rows"
        );
        assert!(read(&*a.store, |s| s.packages().package(&spec.package)).is_none());
        let current = read(&*a.store, |s| s.authority().principal(a.agent.principal())).unwrap();
        assert_eq!(
            current.revision, 2,
            "the independent authority change must remain committed"
        );
        if change == "revoked" {
            assert!(!current.active)
        }
    }
}
