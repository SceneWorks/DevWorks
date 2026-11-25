//! Managed rust-analyzer semantic pack.
#[cfg(feature = "bundle")]
mod bundle;
mod compiler;
mod snapshot;

use devworks_semantic_helper::{Helper, IndexFormat};

const PACKAGE: &str = "devworks-rust-semantic";
const CONTRIBUTION: &str = "devworks-rust-semantic/index";

fn main() {
    #[cfg(feature = "bundle")]
    if std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == "--bundle")
    {
        if let Err(error) = bundle::build_from_args() {
            eprintln!("{PACKAGE}: {error}");
            std::process::exit(1);
        }
        return;
    }
    devworks_semantic_helper::run(Helper {
        package: PACKAGE,
        contribution: CONTRIBUTION,
        format: IndexFormat::Scip,
        media_type: "application/octet-stream",
        analyze: |bytes, cancelled, deadline| {
            let source = snapshot::Source::parse(bytes)?;
            compiler::index(&source, cancelled, deadline)
        },
    });
}
