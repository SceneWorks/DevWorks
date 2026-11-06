//! Exact captured UTF-8 pages, prepared with bounded buffers outside the publication snapshot.
use super::{model,read::{Position,Reader}};
use crate::ports::storage::*;
use crate::protocol::{records::*,data::GraphResult,DomainErrorCode};
use crate::services::{artifacts::ArtifactService,sessions::{Failure,Refusal}};
use serde_json::json;
const PAGE:usize=64*1024;
pub(super) struct Source{capture:StoredGraphSourceCapture,unit:StoredUnit,repository:crate::protocol::Id,view:crate::protocol::Id,node:Option<GraphNode>,text:String,start:usize,end:usize,total:usize,first_line:usize,last_line:usize,more:bool}
fn unavailable()->Failure{Refusal::new(DomainErrorCode::ViewUnavailable,"captured_source",None).into()}
fn invalid()->Failure{Refusal::new(DomainErrorCode::InvalidArgument,"source_line_range",None).into()}
pub(super) fn prepare(reader:&mut Reader<'_>,artifacts:&ArtifactService,selection:&CodeSourceSelection)->Result<Source,Failure>{
 let (repository,path,node)=match selection{CodeSourceSelection::Node{node_id,..}=>{let node=reader.find_id(node_id)?.ok_or_else(||crate::services::sessions::not_found("node",node_id))?;(node.repository_id.clone(),node.node.unit_key.clone(),Some(node))},CodeSourceSelection::File{repository_id,path,..}=>(repository_id.clone(),path.0.clone(),None)};
 reader.scoped_repository(&repository)?;let view=reader.selected.layers.iter().find(|v|v.view.repository_id==repository).ok_or_else(unavailable)?;let(unit,capture)=model::capture(reader.read,view,&path)?.ok_or_else(unavailable)?;
 if unit.payload["sourceCapture"]["textEncoding"].as_str()!=Some("Utf8"){return Err(Refusal::new(DomainErrorCode::ViewUnavailable,"source_not_utf8",None).into());}
 let lines=unit.payload["sourceCapture"]["lineCount"].as_u64().ok_or_else(unavailable)? as usize;
 let total=usize::try_from(capture.byte_count).map_err(|_|unavailable())?;
 let (first,last)=match selection{
 CodeSourceSelection::File{start_line,end_line,..}=>{let start=(start_line.get() as usize).max(1);let end=if end_line.get()==0{lines}else{(end_line.get() as usize).min(lines)};if lines>0&&(start>lines||end<start){return Err(invalid());}(start,end)},
 CodeSourceSelection::Node{mode:CodeSourceMode::NodeWindow,..}if lines>200=>{let node=node.as_ref().expect("node selection");let start=node.node.payload["span"]["startLine"].as_u64().ok_or_else(unavailable)? as usize;let end=node.node.payload["span"]["endLine"].as_u64().ok_or_else(unavailable)? as usize;(start.saturating_sub(5).max(1),end.saturating_add(5).min(lines))},_=>(1,lines)};
 let requested=reader.position.source_byte;let mut output=Vec::with_capacity(PAGE+4);let(mut offset,mut line)=(0usize,1usize);let mut start=None;let mut range_end=0;let mut first_line=0;let mut last_line=0;
 artifacts.scan_index_source(reader.read,&capture,|bytes|{
 if !reader.budget.visit(){return false;}
 for byte in bytes{
   let selected=line>=first&&line<=last;let after=requested.is_none_or(|requested|offset>=requested);
   if selected&&after {if start.is_none(){start=Some(offset);first_line=line;}if output.len()<PAGE+4{output.push(*byte);last_line=line;}range_end=offset+1;}
   if *byte==b'\n'{line+=1;}offset+=1;
 }true}).map_err(|_|if reader.budget.exhausted==Some("graph_cancelled"){StorageError::Cancelled.into()}else{unavailable()})?;
 let start=start.unwrap_or(if total==0{0}else{range_end});if requested.is_some_and(|r|r!=start){return Err(StorageError::CursorExpired.into());}
 let mut count=output.len().min(PAGE);while count>0&&std::str::from_utf8(&output[..count]).is_err(){let error=std::str::from_utf8(&output[..count]).expect_err("invalid boundary");if error.error_len().is_some(){return Err(unavailable());}count=error.valid_up_to();}
 let text=String::from_utf8(output[..count].to_vec()).map_err(|_|unavailable())?;let end=start+count;
 // The final retained block can contain a few lookahead bytes; derive the exact page's line.
 if !text.is_empty(){last_line=first_line+text.as_bytes()[..text.len()-1].iter().filter(|b|**b==b'\n').count();}
 if !reader.budget.retain(text.len()){return Err(unavailable());}
 Ok(Source{capture,unit,repository,view:view.view.view_id.clone(),node,text,start,end,total,first_line,last_line,more:end<range_end})
}
pub(super) fn query(reader:&mut Reader<'_>,source:&Source)->Result<GraphResult,Failure>{
 let view=reader.selected.layers.iter().find(|v|v.view.view_id==source.view&&v.view.repository_id==source.repository).ok_or_else(unavailable)?;let current=model::capture(reader.read,view,&source.unit.unit_key)?.ok_or_else(unavailable)?;if current.1!=source.capture||current.0!=source.unit{return Err(unavailable());}
 let mut location=model::location(&view.view,&source.unit,&source.capture,&serde_json::Value::Null)?.ok_or_else(unavailable)?;location.start_line=model::count(source.first_line as u64)?;location.end_line=model::count(source.last_line as u64)?;location.start_byte=Some(model::count(source.start as u64)?);location.end_byte=Some(model::count(source.end as u64)?);
 if source.more{reader.next=Some(Position{source_byte:Some(source.end),..Default::default()});}
 let mut value=json!({"source":location,"text":source.text,"startByte":source.start.to_string(),"endByte":source.end.to_string(),"totalBytes":source.total.to_string(),"firstLine":source.first_line.to_string(),"lastLine":source.last_line.to_string()});
 if let Some(node)=&source.node{value["nodeId"]=json!(node.node.node_id);for(key,field)in[("nodeStartLine","startLine"),("nodeEndLine","endLine")]{if let Some(line)=node.node.payload["span"][field].as_u64(){value[key]=json!(line.to_string());}}}
 Ok(GraphResult::SourceText{value:model::wire(value)?})
}
