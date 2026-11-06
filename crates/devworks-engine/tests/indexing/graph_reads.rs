//! Real registered read operators, retained source pages and exact trust mutations.
use super::*;
use devworks_core::protocol::data::{GraphPage,GraphResult};
fn page(app:&Runtime,ctx:&InvocationContext,operation:&str,query:Value,view:Option<Value>,cursor:Option<&str>)->GraphPage{
 let mut frame=json!({"protocol":"1.0-draft.1","schemaRevision":3,"queryId":"reads-native","scope":{"kind":"Workspace"},"query":query,"limits":{"maxResults":100,"maxVisited":100000,"deadlineMs":30000}});
 if let Some(view)=view{frame["view"]=view;}else{frame["current"]=json!(true);}if let Some(cursor)=cursor{frame["cursor"]=json!(cursor);}
 let reply=app.query(ctx,operation,frame.to_string().as_bytes()).unwrap();let value=match reply{DomainQueryReply::Result{result,..}=>serde_json::to_value(result).unwrap(),other=>panic!("{operation}: {other:?}")};serde_json::from_value(value["value"].clone()).unwrap()
}
#[test]
fn native_graph_reads_reuse_one_pin_and_stream_exact_source_after_checkout_removal(){
 let h=Harness::new();let executor=runtime();let store=h.open();let src=sources();let repo=src.join("reads");std::fs::create_dir_all(repo.join("src")).unwrap();
 std::fs::write(repo.join("src/lib.rs"),"pub fn target() {}\npub fn caller(){target();}\npub fn outer(){caller();}\n").unwrap();
 let large=format!("{}\0end\n","Unicode 世界\n".repeat(10000));std::fs::write(repo.join("README"),&large).unwrap();std::fs::write(repo.join("empty.txt"),"").unwrap();
 git(&repo,&["init","-q"]);git(&repo,&["add","-A"]);git(&repo,&["commit","-q","-m","read operators"]);
 let app=executor.block_on(boot(composition(&h,store.clone(),"git"))).unwrap();let ctx=principal(&*store,"user",PrincipalKind::Human);base_directory(&store,&ctx,&src);let repositories=discovered(&app,&ctx);let run=index(&app,&ctx,&repositories[0],"reads-index");executor.block_on(within("read source publishes",async{loop{let state=run_state(&store,&run);if state.state==RUN_SUCCEEDED{break;}assert!(!run_terminal(&state.state),"{state:?}");tokio::time::sleep(Duration::from_millis(20)).await;}}));
 let found=page(&app,&ctx,"code.search",json!({"kind":"Search","selection":{"mode":"NodeList","text":"target","repositoryId":repositories[0]}}),None,None);
 let GraphResult::CodeSearch{value}= &found.result else{panic!("{found:?}")};let target=value.items.0.iter().find_map(|hit|match hit{devworks_core::protocol::records::CodeSearchHit::Node{node}if node.name.0=="target"=>Some(node.clone()),_=>None}).unwrap();
 let pinned=json!({"kind":"Pinned","pinId":found.view.as_ref().unwrap().pin_id});
 std::fs::remove_dir_all(&repo).unwrap();
 let detail=page(&app,&ctx,"code.node_detail",json!({"kind":"Node","nodeId":target.node_id}),Some(pinned.clone()),None);let GraphResult::NodeDetail{value}=detail.result else{panic!("detail")};assert_eq!(value.incoming.0.iter().filter(|e|e.edge_kind.0=="CALLS").count(),1);
 for(operation,query)in[
 ("code.nodes_by_file",json!({"kind":"NodesByFile","selection":{"repositoryId":repositories[0],"path":"src/lib.rs","line":"1"}})),
 ("code.call_path",json!({"kind":"Path","selection":{"target":{"kind":"Node","nodeId":target.node_id},"direction":"Inbound","depth":3}})),
 ("code.traverse",json!({"kind":"Traverse","startIds":[target.node_id],"direction":"Reverse","edgeKinds":["CALLS"],"maxDepth":3})),
 ("code.consumers",json!({"kind":"Consumers","selection":{"kind":"Node","nodeId":target.node_id}})),
 ("code.publishers",json!({"kind":"Publishers","selection":{"kind":"Node","nodeId":target.node_id}})),
 ("code.data_lineage",json!({"kind":"DataLineage","selection":{"kind":"Node","nodeId":target.node_id}})),
 ("code.graph_scope_status",json!({"kind":"ScopeStatus"})),
 ]{let answer=page(&app,&ctx,operation,query,Some(pinned.clone()),None);assert_eq!(answer.view.as_ref().unwrap().pin_id,found.view.as_ref().unwrap().pin_id,"{operation}");}
 let query=json!({"kind":"Source","selection":{"kind":"File","repositoryId":repositories[0],"path":"README","startLine":"0","endLine":"0"}});let mut cursor=None;let mut bytes=Vec::<u8>::new();let mut pages=0;loop{let answer=page(&app,&ctx,"code.source",query.clone(),Some(pinned.clone()),cursor.as_deref());let GraphResult::SourceText{value}=answer.result else{panic!("source")};assert_eq!(value.start_byte.get()as usize,bytes.len());assert!(value.text.0.len()<=65536);bytes.extend_from_slice(value.text.0.as_bytes());pages+=1;cursor=answer.next_cursor;if cursor.is_none(){break;}}assert!(pages>1);assert_eq!(bytes,large.as_bytes());
 let empty=page(&app,&ctx,"code.snippet",json!({"kind":"Source","selection":{"kind":"File","repositoryId":repositories[0],"path":"empty.txt","startLine":"0","endLine":"0"}}),Some(pinned.clone()),None);let GraphResult::SourceText{value}=empty.result else{panic!("empty")};assert_eq!(value.total_bytes.get(),0);assert_eq!(value.first_line.get(),0);
 let command=json!({"protocol":"1.0-draft.1","schemaRevision":3,"requestKey":"trust-native","command":{"kind":"SetTrust","repositoryId":target.repository_id,"nodeId":target.node_id,"expectedRevision":target.trust_revision,"viewId":target.view_id,"trusted":false,"unitKey":target.unit_key,"expectedViewDigest":target.view_digest,"expectedContentIdentity":target.content_identity,"scope":{"kind":"Workspace"},"pinId":found.view.unwrap().pin_id}});
 let reply=app.command(&ctx,"code.set_trust",command.to_string().as_bytes()).unwrap();assert!(matches!(&reply,CommandReply::Recorded{receipt,..}if matches!(receipt.decision,DomainDecision::Applied{..})),"{reply:?}");
 let after=page(&app,&ctx,"code.node_detail",json!({"kind":"Node","nodeId":target.node_id}),Some(pinned),None);let GraphResult::NodeDetail{value}=after.result else{panic!("trust detail")};assert!(value.node.do_not_trust);assert_eq!(value.node.trust_revision.get(),2);
 executor.block_on(app.shutdown());let _=std::fs::remove_dir_all(src);
}
