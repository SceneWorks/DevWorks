use super::*;

/// UI quietness does not settle native bootstrap/effect writers. Paging intentionally retains
/// a full read-snapshot fence, so a positive paging fixture first observes those real owners.
/// Startup is a count of durable writes (Help publication, history indexing), so like
/// [`step_until_progressing`] this fails only once `root`'s [`Root::write_progress`] stops
/// advancing for [`SETTLE_BOUND`].
pub(super) fn settled(engine: &devworks_desktop::host::Engine, root: &Root) {
    use devworks_core::ports::storage::{ReadOptions, read_snapshot};
    let runtime = engine.host().runtime();
    let accounting = runtime.accounting().watch();
    let help = engine
        .backend()
        .native_job_capabilities()
        .iter()
        .any(|cap| cap.operation == "help.generate_capability_docs");
    let mut seen = root.write_progress();
    let mut deadline = std::time::Instant::now() + SETTLE_BOUND;
    let mut sampled = std::time::Instant::now();
    loop {
        let (bootstrap, queue) =
            read_snapshot(&**runtime.store(), &ReadOptions::default(), |read| {
                Ok((
                    read.usage().usage_bootstrap()?,
                    read.queue().status("help_docs")?,
                ))
            })
            .unwrap();
        let account_ready = accounting.borrow().as_ref().is_some_and(|pass| {
            pass.as_ref().is_ok_and(|pass| {
                pass.indexing_complete && pass.recovery_complete && !pass.recovery_more
            })
        });
        assert_eq!(queue.failed, 0, "native startup Help failed: {queue:?}");
        if bootstrap.phase == "Complete"
            && account_ready
            && (!help
                || (queue.completed > 0
                    && queue.ready == 0
                    && queue.claimed == 0
                    && queue.reconciling == 0))
        {
            return;
        }
        if sampled.elapsed() >= std::time::Duration::from_millis(250) {
            sampled = std::time::Instant::now();
            let now = root.write_progress();
            if now != seen {
                seen = now;
                deadline = sampled + SETTLE_BOUND;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "actual startup owners settle (no progress for 30 s): bootstrap={bootstrap:?}, help={queue:?}, accounting_ready={account_ready}, accounting={:?}",
            *accounting.borrow()
        );
        std::thread::yield_now();
    }
}
