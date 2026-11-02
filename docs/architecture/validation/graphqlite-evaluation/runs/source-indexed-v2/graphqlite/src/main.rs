mod adapter;
mod cache_profile;
mod freshness;
fn main() -> anyhow::Result<()> {
    match std::env::args().nth(1).as_deref() {
        Some("--workload") => eval_common::workload::run("graphqlite", adapter::open),
        Some("--freshness") => freshness::run(),
        Some("--cache-profile") => cache_profile::run(),
        _ => anyhow::bail!("Use --workload CONFIG.json or --freshness FRESH_ROOT"),
    }
}
