//! Private graph reads use the same gateway and published source captures as other callers.
use super::{NativeDreaming, invalid};
use devworks_core::context::InvocationContext;
use devworks_core::fingerprint::sha256_hex;
use devworks_core::ports::storage::{
    DreamRunRow, KnowledgeScope, ReadOptions, StorageError, read_snapshot,
};
use devworks_core::protocol::Id;
use devworks_core::protocol::data::{GraphPage, GraphResult, RepositoryView};
use devworks_core::protocol::records::{CodeSearchHit, CodeSourceText};
use devworks_core::protocol::results::{DomainQueryReply, OperationResult};
use devworks_core::services::indexing::views;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::Path;

const MAX_SOURCE_BYTES: u64 = 2 * 1024 * 1024;

impl NativeDreaming {
    pub(super) fn read_graph(
        &self,
        ctx: &InvocationContext,
        run: &DreamRunRow,
        action: &Id,
        tool: &str,
        args: &Value,
    ) -> Result<Option<Value>, StorageError> {
        let (op, selection) = match tool {
            "dream_search_graph" => ("code.search", search(args)),
            "dream_read_source" => ("code.source", source(args)),
            _ => return Ok(None),
        };
        let KnowledgeScope::Project(project) = &run.scope else {
            return Ok(Some(json!({"error":"ProjectRepositoryRequired"})));
        };
        let Some(gateway) = self.gateway.upgrade() else {
            return Ok(Some(json!({"error":"GraphUnavailable"})));
        };
        let request = serde_json::to_vec(&json!({
            "protocol":"1.0-draft.1", "schemaRevision":3, "queryId":action,
            "scope":{"kind":"Project","projectId":project}, "current":true,
            "query":selection,
            "limits":{"maxResults":args["limit"].as_u64().unwrap_or(20),"maxVisited":5000,"deadlineMs":1500}
        })).map_err(|_| invalid("invalid private graph query"))?;
        let outcome = gateway.query_with(ctx, op, &request, &ReadOptions::default());
        let Ok(DomainQueryReply::Result { result, .. }) = outcome else {
            return Ok(Some(json!({"error":"SelectedGraphViewUnavailable"})));
        };
        let result = match result {
            OperationResult::CodeSearch { value } => render_search(value)?,
            OperationResult::CodeSource { value } => self.render_source(run, value)?,
            _ => {
                return Err(invalid(
                    "private graph result did not match selected operation",
                ));
            }
        };
        Ok(Some(result))
    }

    /// Recheck the current bytes outside SQLite's writer before a prior capture can back a write.
    /// The writer then verifies that the scope, repository revision and published index still match.
    pub(super) fn refresh_proofs(
        &self,
        run: &DreamRunRow,
        arguments: &Value,
    ) -> Result<BTreeSet<String>, StorageError> {
        let (Some(tree), Some(project), Some(evidence)) = (
            self.source_tree.as_deref(),
            run.scope.project(),
            arguments["evidence"].as_array(),
        ) else {
            return Ok(BTreeSet::new());
        };
        let mut refs: BTreeSet<_> = evidence
            .iter()
            .filter(|entry| entry["kind"] == "graph_node")
            .filter_map(|entry| entry["reference"].as_str())
            .collect();
        if refs.is_empty() {
            return Ok(BTreeSet::new());
        }
        let latest = read_snapshot(self.store.as_ref(), &ReadOptions::default(), |read| {
            let mut captures = Vec::new();
            let mut before = None;
            let repo = read.dreaming()?;
            while !refs.is_empty() {
                let actions = repo.dream_actions(&run.id, before, 100)?;
                let Some(last) = actions.last() else { break };
                before = Some(last.position);
                for action in actions {
                    let Some(proofs) = action.action["result"]["proofs"].as_array() else {
                        continue;
                    };
                    for proof in proofs {
                        if proof["kind"] != "graph_node" {
                            continue;
                        }
                        let Some(reference) = proof["id"].as_str() else {
                            continue;
                        };
                        if !refs.remove(reference) {
                            continue;
                        }
                        let id: Id = serde_json::from_value(proof["repositoryId"].clone())
                            .map_err(|_| invalid("invalid Dreaming source proof"))?;
                        let projects = read.projects();
                        if !projects.repository_projects(&id)?.contains(project) {
                            continue;
                        }
                        let Some(record) = projects.repository(&id)?.filter(|repo| {
                            repo.lifecycle == "Active"
                                && Some(repo.revision) == proof["repositoryRevision"].as_i64()
                        }) else {
                            continue;
                        };
                        let Some((path, fingerprint)) =
                            proof["unitKey"].as_str().zip(proof["fingerprint"].as_str())
                        else {
                            continue;
                        };
                        captures.push((
                            format!("{reference}:{fingerprint}"),
                            record.location,
                            path.to_owned(),
                            fingerprint.to_owned(),
                        ));
                    }
                }
            }
            Ok(captures)
        })?;
        Ok(latest
            .into_iter()
            .filter_map(|(id, location, path, fingerprint)| {
                tree.read_file(Path::new(&location), &path, MAX_SOURCE_BYTES)
                    .is_ok_and(|raw| sha256_hex(&raw) == fingerprint)
                    .then_some(id)
            })
            .collect())
    }

    fn render_source(&self, run: &DreamRunRow, page: GraphPage) -> Result<Value, StorageError> {
        let GraphResult::SourceText { value: source } = page.result else {
            return Err(invalid("unexpected private source reply"));
        };
        let repository = &source.source.snapshot.repository_id;
        let Some(view) = page.view.as_ref().and_then(|set| {
            set.views
                .iter()
                .find(|view| &view.snapshot.repository_id == repository)
        }) else {
            return Ok(json!({"error":"ExactGraphViewUnavailable"}));
        };
        let proof = self.source_proof(run, view, &source)?;
        Ok(
            json!({"source":source,"view":view,"completion":page.completion,
            "nextGraphCursor":page.next_cursor,"verifiedIndexMatch":proof.is_some(),"proofs":proof.into_iter().collect::<Vec<_>>() }),
        )
    }

    fn source_proof(
        &self,
        run: &DreamRunRow,
        view: &RepositoryView,
        source: &CodeSourceText,
    ) -> Result<Option<Value>, StorageError> {
        let Some(tree) = self.source_tree.as_deref() else {
            return Ok(None);
        };
        let Some(project) = run.scope.project() else {
            return Ok(None);
        };
        let repository = &view.snapshot.repository_id;
        let unit_key = &source.source.unit_key.0;
        let eligible = read_snapshot(self.store.as_ref(), &ReadOptions::default(), |snapshot| {
            let projects = snapshot.projects();
            if !projects.repository_projects(repository)?.contains(project)
                || snapshot
                    .indexing()
                    .published(repository)?
                    .as_ref()
                    .map(|p| &p.view_id)
                    != Some(&view.view_id)
            {
                return Ok(None);
            }
            let Some(record) = projects
                .repository(repository)?
                .filter(|repo| repo.lifecycle == "Active" && repo.primary)
            else {
                return Ok(None);
            };
            let Ok(layers) = views::layers(snapshot.indexing(), &view.view_id) else {
                return Ok(None);
            };
            let Ok(Some((_, unit))) = views::resolve_unit(snapshot.indexing(), &layers, unit_key)
            else {
                return Ok(None);
            };
            let Some(raw_id) = unit.payload["sourceCapture"]["id"].as_str() else {
                return Ok(None);
            };
            let id = Id::new(raw_id).map_err(|_| invalid("invalid source capture identity"))?;
            let capture = snapshot
                .graph()
                .graph_source_capture(&id)?
                .filter(|capture| {
                    &capture.repository_id == repository
                        && &capture.unit_key == unit_key
                        && capture.source_digest == unit.content_digest
                        && unit.payload["sourceCapture"]["sha256"] == capture.blob_digest
                });
            Ok(capture.map(|capture| (record, capture)))
        })?;
        let Some((record, capture)) = eligible else {
            return Ok(None);
        };
        let matches = tree
            .read_file(Path::new(&record.location), unit_key, MAX_SOURCE_BYTES)
            .is_ok_and(|raw| sha256_hex(&raw) == capture.blob_digest);
        if !matches {
            return Ok(None);
        }
        let source_ref = format!("repo:{}/{unit_key}", repository.as_str());
        let reference = source.node_id.as_ref().map_or_else(
            || format!("dream-source-{}", sha256_hex(source_ref.as_bytes())),
            |node| node.as_str().to_owned(),
        );
        Ok(Some(json!({
            "kind":"graph_node","id":reference,
            "sourceRef":source_ref,"sourceRevision":capture.source_digest,"fingerprint":capture.blob_digest,
            "repositoryId":repository,"repositoryRevision":record.revision,"viewId":view.view_id,
            "unitKey":unit_key,"verifiedIndexMatch":true
        })))
    }
}

fn search(args: &Value) -> Value {
    let mut selection = json!({"mode":"NodeList","text":args["name_pattern"].as_str().unwrap_or(""),
        "repositoryId":args["repository"]});
    if let Some(label) = args.get("label") {
        selection["nodeKind"] = label.clone();
    }
    json!({"kind":"Search","selection":selection})
}

fn source(args: &Value) -> Value {
    let node = args["node_id"]
        .as_str()
        .map(str::to_owned)
        .or_else(|| args["node_id"].as_u64().map(|value| value.to_string()));
    let selection = if let Some(node) = node {
        json!({"kind":"Node","nodeId":node,"mode": if args["detail"] == "full" {"WholeFile"} else {"NodeWindow"}})
    } else {
        json!({"kind":"File","repositoryId":args["repository"],"path":args["path"],
            "startLine":"1","endLine":if args["detail"] == "full" {"0"} else {"200"}})
    };
    json!({"kind":"Source","selection":selection})
}

fn render_search(page: GraphPage) -> Result<Value, StorageError> {
    let GraphResult::CodeSearch { value } = page.result else {
        return Err(invalid("unexpected private graph search reply"));
    };
    let items = value
        .items
        .0
        .into_iter()
        .filter_map(|hit| {
            let CodeSearchHit::Node { node } = hit else {
                return None;
            };
            Some(node)
        })
        .collect::<Vec<_>>();
    Ok(json!({"items":items,"completion":page.completion,"nextGraphCursor":page.next_cursor}))
}
