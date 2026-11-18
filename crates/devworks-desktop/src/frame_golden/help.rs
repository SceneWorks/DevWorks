//! Sample frames of the Help requests: search, screen context, passage reads, package guides and
//! capability documentation generation.
use super::Silent;
use crate::help_ops as help;

pub fn frames() -> Vec<(&'static str, Vec<u8>)> {
    let b = &Silent;
    vec![
        ("help_search", help::search(b, "cargo test", None)),
        (
            "help_search_blank_cursor",
            help::search(b, "  ", Some("c1")),
        ),
        ("help_context", help::context(b, "screen-a", None)),
        (
            "help_context_cursor",
            help::context(b, "screen-a", Some("c1")),
        ),
        ("help_read", help::read(b, "topic-a", "1.2", None)),
        (
            "help_read_cursor",
            help::read(b, "topic-a", "1.2", Some("c1")),
        ),
        ("help_guides", help::guides(b, "package-a", "2.0", None)),
        (
            "help_guides_cursor",
            help::guides(b, "package-a", "2.0", Some("c1")),
        ),
        ("help_generate", help::generate(b, 12, 34)),
    ]
}
