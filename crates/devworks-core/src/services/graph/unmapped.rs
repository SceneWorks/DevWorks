//! Historical endpoint evidence is metadata, never a reason to expand a hidden/off-scope node.
use super::{dataset,impact,read::Reader,scan,scope};
use crate::{ports::storage::*,protocol::records::CodeNodeSelection,services::sessions::Failure};
use std::collections::{BTreeMap,BTreeSet};
pub(super) fn count(reader:&mut Reader<'_>,edges:&[scan::Unmapped],nodes:&[GraphNodeIdentity],selection:Option<&CodeNodeSelection>)->Result<Option<u64>,Failure>{
 if !reader.selected.layers.iter().any(|view|scope::overlay(&view.view)){return Ok(None);}
 let mut sources=BTreeMap::<(&str,&str),BTreeSet<&str>>::new();let mut files=BTreeSet::new();
 for node in nodes{if !reader.budget.visit()||!reader.budget.retain(node.unit_key.len()+node.node_id.len()+node.repository_id.as_str().len()+96){reader.gaps.push("unmapped_counts_incomplete");return Ok(None);}sources.entry((node.repository_id.as_str(),&node.node_id)).or_default().insert(&node.unit_key);files.insert((node.repository_id.as_str(),node.unit_key.as_str()));}
 let mut count=0;
 for unresolved in edges{
  if !reader.budget.visit(){reader.gaps.push("unmapped_counts_incomplete");return Ok(None);}
  let edge=&unresolved.original;let payload=&edge.edge.payload;
  let source=payload["sourceRepositoryId"].as_str().unwrap_or(edge.repository_id.as_str());let target=payload["targetRepositoryId"].as_str().unwrap_or(edge.repository_id.as_str());
  if !reader.scope().views.iter().any(|view|view.repository_id.as_str()==source)||!reader.scope().views.iter().any(|view|view.repository_id.as_str()==target)||!impact::formerly_mapped(edge,&reader.selected.layers,source,target){continue;}
  let from=sources.get(&(source,edge.edge.source_node_id.as_str())).is_some_and(|units|payload["sourceUnit"].as_str().map_or(units.len()==1,|unit|units.contains(unit)));
  let file=payload["targetUnit"].as_str().is_some_and(|unit|files.contains(&(target,unit)));
  let name=match selection{Some(CodeNodeSelection::Name{name,repository_id})=>repository_id.as_ref().is_none_or(|repo|repo.as_str()==target)&&["targetName","targetQualifiedName"].iter().any(|key|payload[key].as_str().is_some_and(|value|value.eq_ignore_ascii_case(&name.0))),_=>false};
  if from||file||name{count+=1;}
 }
 if count>0{reader.gaps.push("unmapped_inherited_edges");}
 Ok(Some(count))
}
pub(super) fn identities(nodes:&[GraphNode])->Vec<GraphNodeIdentity>{nodes.iter().map(dataset::identity).collect()}
