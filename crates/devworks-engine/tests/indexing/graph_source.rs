use super::*;
use devworks_core::ports::files::ArtifactFiles;
use std::io::Read;

#[test]
fn native_index_publishes_private_exact_structural_and_plain_text_sources() {
    let _case = native_runtime_case();
    let h = Harness::new();
    let executor = runtime();
    let store = h.open();
    let src = sources();
    let repo = src.join("captured");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    let rust = b"pub fn original() {}\n";
    let text = "plain text\n\0with a NUL and Unicode: 世界\n".as_bytes();
    std::fs::write(repo.join("src/lib.rs"), rust).unwrap();
    std::fs::write(repo.join("notes.txt"), text).unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "captured"]);
    let app = executor
        .block_on(boot(composition(&h, store.clone(), "git")))
        .unwrap();
    let ctx = principal(&*store, "user", PrincipalKind::Human);
    base_directory(&store, &ctx, &src);
    let repositories = discovered(&app, &ctx);
    let run = index(&app, &ctx, &repositories[0], "capture-source");
    executor.block_on(within("the exact source is published", async {
        loop {
            let state = run_state(&store, &run);
            if state.state == RUN_SUCCEEDED {
                break;
            }
            assert!(!run_terminal(&state.state), "{state:?}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }));
    let repository = id(&repositories[0]);
    let captures = read(&*store, |snapshot| {
        let published = snapshot.indexing().published(&repository)?.unwrap();
        let view = snapshot.indexing().view(&published.view_id)?.unwrap();
        let mut captures = Vec::new();
        for path in ["src/lib.rs", "notes.txt"] {
            let unit = snapshot.indexing().unit(&view.baseline_id, path)?.unwrap();
            let capture_id: devworks_core::protocol::Id =
                serde_json::from_value(unit.payload["sourceCapture"]["id"].clone()).unwrap();
            let capture = snapshot.graph().graph_source_capture(&capture_id)?.unwrap();
            assert_eq!(capture.repository_id, repository);
            assert_eq!(capture.unit_key, path);
            assert_eq!(capture.source_digest, unit.content_digest);
            assert_eq!(
                capture.blob_digest,
                unit.payload["sourceCapture"]["sha256"].as_str().unwrap()
            );
            assert!(
                snapshot.artifacts().artifact(&capture.id)?.is_none(),
                "captured source is not a public artifact"
            );
            assert!(snapshot.artifacts().live_references(&capture.blob_digest)? > 0);
            captures.push(capture);
        }
        Ok(captures)
    });
    // Old-source access cannot depend on the checkout or even the continued existence of Git.
    std::fs::remove_dir_all(&repo).unwrap();
    let files = NativeArtifactFiles::new(h.root.clone(), h.root.with_extension("cache"));
    for (capture, expected) in captures.iter().zip([rust.as_slice(), text]) {
        let mut reader = files
            .open(
                &capture.blob_digest,
                &capture.blob_generation,
                capture.byte_count as u64,
            )
            .unwrap();
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, expected);
        assert_eq!(
            devworks_core::fingerprint::sha256_hex(&bytes),
            capture.blob_digest
        );
    }
    executor.block_on(app.shutdown());
    let _ = std::fs::remove_dir_all(src);
}
