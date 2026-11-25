//! Managed Swift semantic pack.
#[cfg(feature = "bundle")]
mod bundle;
mod compiler;
mod graph;
mod indexstore;
mod source;
mod workspace;

use devworks_semantic_helper::{Helper, IndexFormat};

use devworks_core::ports::packages::swift::PACKAGE;
const CONTRIBUTION: &str = "devworks-swift-semantic/index";

fn main() {
    if std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == "--decode-index")
    {
        if let Err(error) = compiler::decode() {
            eprintln!("{PACKAGE}: {}", error.code);
            std::process::exit(1);
        }
        return;
    }

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
        format: IndexFormat::Native,
        media_type: "application/json",
        analyze: compiler::index,
    });
}
