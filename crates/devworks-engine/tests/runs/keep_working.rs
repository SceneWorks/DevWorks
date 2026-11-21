use super::*;
use devworks_core::protocol::helpers::ControlEpoch;

fn ready(f: &Fx, root: &str, project: &ProjectId, priority: i64) {
    let (root, project) = (id(root), project.clone());
    f.tx(move |_, tx| {
        devworks_core::services::work::workflow::ensure_workflow(tx, &project)?;
        let status = tx
            .work()
            .statuses(&project)?
            .into_iter()
            .find(|status| status.semantic == devworks_core::services::work::workflow::READY)
            .unwrap();
        let mut work = tx.work().work(&root)?.unwrap();
        let expected = work.revision;
        work.status_id = Some(status.id);
        work.priority = priority;
        work.revision += 1;
        tx.work().update_work(&work, expected)
    });
}

async fn wait_until(condition: impl Fn() -> bool) {
    while !condition() {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[test]
fn project_keep_working_is_opt_in_independent_and_latches_when_no_item_is_ready() {
    let _native_case = native_runtime_case();
    let f = Fx::new();
    let a = ProjectId(id("project-a"));
    let b = ProjectId(id("project-b"));
    ready(&f, "root-i", &a, 100);
    let mut ports = composition(&f, None, Arc::new(FakePower::default()));
    ports.config.housekeeping_interval =
        devworks_engine::RuntimeConfig::new(id("default-timing")).housekeeping_interval;
    runtime().block_on(async {
        let app = boot(ports).await.unwrap();
        within("project keep-working", async {
            // Let the worker enter its ordinary idle wait before the first user action.
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert!(!app.keep_working_status(&a).unwrap().enabled);
            assert!(f.live("root-i").is_none());
            assert!(matches!(
                app.set_keep_working(&f.user, &a, true, ControlEpoch::UNKNOWN),
                Err(StorageError::ScopeDenied)
            ));
            app.set_keep_working(&f.user, &a, true, ControlEpoch::QUIET)
                .unwrap();
            tokio::time::timeout(
                Duration::from_secs(20),
                wait_until(|| f.live("root-i").is_some()),
            )
            .await
            .expect("the reservation should reach Start before the next housekeeping interval");
            app.set_keep_working(&f.user, &b, true, ControlEpoch::QUIET)
                .unwrap();
            wait_until(|| {
                app.keep_working_status(&b)
                    .is_some_and(|s| !s.enabled && s.detail.is_some())
            })
            .await;
            assert!(f.live("root-b").is_none());

            ready(&f, "root-b", &b, 50);
            wait_until(|| app.keep_working_status(&b).is_some_and(|s| !s.enabled)).await;
            assert!(f.live("root-b").is_none());
            app.set_keep_working(&f.user, &b, true, ControlEpoch::QUIET)
                .unwrap();
            wait_until(|| f.live("root-b").is_some()).await;
            let root_a = f.live("root-i").unwrap();
            let root_b = f.live("root-b").unwrap();
            assert_ne!(root_a.id, root_b.id);
            let a_status = app
                .set_keep_working(&f.user, &a, false, ControlEpoch::QUIET)
                .unwrap();
            assert!(!a_status.enabled);
            assert_eq!(f.live("root-i").unwrap().id, root_a.id);
        })
        .await;
        app.shutdown().await;
    });
}
