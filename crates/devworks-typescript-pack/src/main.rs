//! Managed TypeScript and JavaScript semantic pack.
#[cfg(feature = "bundle")]
mod bundle;
mod compiler;
mod source;

use devworks_semantic_helper::{Helper, IndexFormat};

const PACKAGE: &str = "devworks-typescript-semantic";
const CONTRIBUTION: &str = "devworks-typescript-semantic/index";

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
        format: IndexFormat::Native,
        media_type: "application/json",
        analyze: compiler::index,
    });
}
