//! Native source reads for exact, path-scoped provider instruction material; no model execution.
use devworks_adapters::index::NativeSourceTree;
use devworks_core::{
    protocol::Id,
    providers::instructions::{InstructionFault, resolve},
};
use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

fn root() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "devworks-instructions-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(path.join("src/nested")).unwrap();
    path
}

#[test]
fn native_instruction_reads_are_scoped_bounded_and_refuse_links() {
    let root = root();
    std::fs::write(root.join("AGENTS.md"), "root guidance").unwrap();
    std::fs::write(root.join("src/CLAUDE.md"), "src guidance").unwrap();
    let source = NativeSourceTree::new();
    let repo = Id::new("repo").unwrap();
    let paths = ["src/nested/lib.rs".into(), "README.md".into()];
    let first = resolve(&source, &root, repo.clone(), &paths).unwrap();
    assert_eq!(
        first
            .sources
            .iter()
            .map(|s| s.path.as_str())
            .collect::<Vec<_>>(),
        ["AGENTS.md", "src/CLAUDE.md"]
    );
    assert_eq!(first.sources[1].applies_to, ["src/nested/lib.rs"]);
    std::fs::write(root.join("src/CLAUDE.md"), "edited guidance").unwrap();
    let edited = resolve(&source, &root, repo.clone(), &paths).unwrap();
    assert_ne!(first.revision, edited.revision);
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(root.join("AGENTS.md"), root.join("src/nested/AGENTS.md"))
            .unwrap();
        assert!(matches!(resolve(&source, &root, repo, &paths),
            Err(InstructionFault::Read { path, .. }) if path == "src/nested/AGENTS.md"));
    }
    std::fs::remove_dir_all(root).unwrap();
}
