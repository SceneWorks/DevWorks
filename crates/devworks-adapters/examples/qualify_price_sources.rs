//! Read-only qualification of downloaded public official documents through the actual native
//! parser. No provider credentials or inference. Arguments: OpenAI Markdown, Anthropic Markdown.
use devworks_core::ports::usage::OfficialPriceProvider;
fn main() {
    let paths: Vec<_> = std::env::args().skip(1).collect();
    assert_eq!(
        paths.len(),
        2,
        "provide the two downloaded official Markdown sources"
    );
    for (provider, path) in [
        OfficialPriceProvider::OpenAi,
        OfficialPriceProvider::Anthropic,
    ]
    .into_iter()
    .zip(paths)
    {
        let bytes = std::fs::read(path).expect("read official source");
        match devworks_adapters::prices::parse_catalog(provider, &bytes, 0) {
            Ok(catalog) => println!(
                "{}: {} rates; source SHA256 {}",
                provider.id(),
                catalog.tariffs.len(),
                catalog.source_digest
            ),
            Err(error) => {
                eprintln!("{}: {error:?}", provider.id());
                std::process::exit(1);
            }
        }
    }
}
