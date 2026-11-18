//! Sample frames of the repository graph requests: the reads the screen offers, their view and
//! scope variations, paging, and the trust change.
use super::{Silent, from};
use crate::graph_ops;
use crate::graph_screen::{GraphScreen, Read};
use devworks_core::protocol::data::ResolvedViewSet;
use devworks_core::protocol::records::CodeNode;
use serde_json::json;

/// A name built at run time, kept for the rest of the test process.
fn key(name: String) -> &'static str {
    Box::leak(name.into_boxed_str())
}

pub fn frames() -> Vec<(&'static str, Vec<u8>)> {
    let b = &Silent;
    let node: CodeNode = from(json!({
        "nodeId": "node-a", "repositoryId": "repo-a", "viewId": "view-a", "unitKey": "src/lib.rs",
        "nodeKind": "function", "name": "run", "qualifiedName": "app::run",
        "source": {"snapshot": {"repositoryId": "repo-a", "base": {"format": "Sha1", "hex": "1".repeat(40)},
            "head": {"format": "Sha1", "hex": "2".repeat(40)}},
            "unitKey": "src/lib.rs", "sourceDigest": "c".repeat(64), "path": "src/lib.rs",
            "startLine": "3", "endLine": "9"},
        "properties": [], "propertiesComplete": true, "doNotTrust": false,
        "trustRevision": "4", "viewDigest": "d".repeat(64), "contentIdentity": "content-a"
    }));
    let pin: ResolvedViewSet = from(json!({"pinId": "pin-a",
        "stamp": {"storeId": "store-a", "restoreEpoch": "0", "sequence": "9", "policyRevision": "1",
            "scopeRevision": "1"},
        "expiresAtMs": "100", "views": []}));
    let mut screen = GraphScreen {
        repository: "repo-a".into(),
        depth: 3,
        relationship_depth: 5,
        line_start: "4".into(),
        line_end: "20".into(),
        ..Default::default()
    };
    let mut out: Vec<(&'static str, Vec<u8>)> = Vec::new();
    // One read frame per kind of read the screen builds, under every view and scope variation.
    let mut reads: Vec<(String, Read)> = vec![
        ("overview".into(), Read::overview("repo-a")),
        ("dependencies".into(), Read::dependencies("repo-a")),
        ("node".into(), Read::node("Node details", "node-a")),
        (
            "readme".into(),
            Read::readme(&from(json!("repo-a")), &from(json!("README.md"))),
        ),
        ("nodes_plain".into(), Read::nodes("repo-a", None, None)),
        (
            "nodes_filtered".into(),
            Read::nodes("repo-a", Some("core::util"), Some("function")),
        ),
        ("nodes_screen".into(), screen.nodes()),
        ("search".into(), screen.search_read()),
    ];
    screen.search_mode = 2;
    screen.search_all = true;
    screen.filter = " run ".into();
    screen.node_kind = "function".into();
    screen.module = "core::util".into();
    reads.push(("search_all".into(), screen.search_read()));
    reads.push(("nodes_screen_filtered".into(), screen.nodes()));
    for (i, read) in screen.scope_reads().into_iter().enumerate() {
        reads.push((format!("scope_{i}"), read));
    }
    screen.selected = Some(node.clone());
    screen.direction = 1;
    screen.edge_kind = "calls, imports ,".into();
    for (i, read) in screen.target_reads().into_iter().enumerate() {
        reads.push((format!("target_node_{i}"), read));
    }
    screen.target_name = " helper ".into();
    for (i, read) in screen.target_reads().into_iter().enumerate() {
        reads.push((format!("target_name_{i}"), read));
    }
    screen.target_all = true;
    for (i, read) in screen.target_reads().into_iter().enumerate() {
        reads.push((format!("target_name_all_{i}"), read));
    }
    reads.push((
        "traverse".into(),
        screen.traverse_read("code.traverse", "Relationships", &node),
    ));
    reads.push((
        "dependency".into(),
        screen.traverse_read("code.dependency", "Node dependencies", &node),
    ));
    screen.file = "src/lib.rs".into();
    screen.file_node_line = "7".into();
    for (i, (read, _)) in screen.file_reads().into_iter().enumerate() {
        reads.push((format!("file_{i}"), read));
    }
    screen.file_node_line = String::new();
    for (i, (read, _)) in screen.file_reads().into_iter().enumerate() {
        reads.push((format!("file_all_nodes_{i}"), read));
    }
    for (name, read) in reads {
        let frame = |view, scope, cursor: Option<&str>, read: Read| {
            graph_ops::read(b, read.operation, scope, view, read.query, cursor)
        };
        let mut screen = GraphScreen::default();
        out.push((
            key(format!("graph_{name}_current")),
            frame(screen.view(), screen.scope(), None, read.clone()),
        ));
        screen.project = Some("project-a".into());
        screen.view_mode = 1;
        screen.published_repositories = ["repo-b".to_string(), "repo-a".to_string()].into();
        out.push((
            key(format!("graph_{name}_project_published_cursor")),
            frame(
                screen.view(),
                screen.scope(),
                Some("cursor-1"),
                read.clone(),
            ),
        ));
        screen.view_mode = 2;
        screen.exact_views = ["view-b".to_string(), "view-a".to_string()].into();
        out.push((
            key(format!("graph_{name}_exact")),
            frame(screen.view(), screen.scope(), None, read.clone()),
        ));
        screen.pin = Some(pin.clone());
        out.push((
            key(format!("graph_{name}_pinned")),
            frame(screen.view(), screen.scope(), None, read),
        ));
    }
    let workspace = GraphScreen::default();
    let project = GraphScreen {
        project: Some("project-a".into()),
        ..Default::default()
    };
    out.push((
        "graph_trust",
        graph_ops::trust(b, &node, workspace.scope(), None, true),
    ));
    out.push((
        "graph_trust_pinned_project",
        graph_ops::trust(b, &node, project.scope(), Some(from(json!("pin-a"))), false),
    ));
    out
}
