//! Capture a structural code graph once. Database timings exclude parsing and vector fixture creation.
use anyhow::{Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{collections::HashMap, path::{Path, PathBuf}, process::Command};

#[derive(Clone, Serialize)]
struct Node { id:u64, project:u32, repo:u32, name:String, kind:String, file:String, line:usize, vector:Vec<f32> }
#[derive(Clone, Serialize)]
struct Edge { src:u64, dst:u64, kind:String }
#[derive(Serialize)]
struct Fixture { manifest:serde_json::Value, nodes:Vec<Node>, edges:Vec<Edge> }
#[derive(Default)]
struct Capture { nodes:Vec<Node>, edges:Vec<Edge>, definitions:HashMap<String,Vec<u64>>, references:Vec<(u64,String)>, files:usize, parse_errors:usize }

fn vector(key:&str)->Vec<f32>{
    // Mathematical fixtures, not learned embeddings and not semantic-quality evidence.
    let mut out=Vec::with_capacity(384);
    for i in 0_u64..48 {let mut h=Sha256::new();h.update(key.as_bytes());h.update(i.to_le_bytes());for b in h.finalize().chunks_exact(4){out.push((u32::from_le_bytes(b.try_into().unwrap()) as f64/u32::MAX as f64*2.0-1.0) as f32);}}
    let norm=out.iter().map(|v|v*v).sum::<f32>().sqrt();for x in &mut out{*x/=norm;}out
}
impl Capture {
    fn node(&mut self,repo:u32,name:String,kind:&str,file:&str,line:usize)->u64{
        let id=self.nodes.len() as u64+1;let v=vector(&format!("{repo}:{file}:{kind}:{name}:{line}"));
        self.nodes.push(Node{id,project:0,repo,name,kind:kind.into(),file:file.into(),line,vector:v});id
    }
    fn edge(&mut self,src:u64,dst:u64,kind:&str){self.edges.push(Edge{src,dst,kind:kind.into()});}
    fn walk(&mut self,n:tree_sitter::Node,source:&[u8],repo:u32,file:&str,parent:u64){
        let declaration=matches!(n.kind(),"class_declaration"|"struct_declaration"|"record_declaration"|"interface_declaration"|"enum_declaration"|"method_declaration"|"constructor_declaration"|"function_item"|"struct_item"|"enum_item"|"trait_item"|"mod_item"|"impl_item"|"function_declaration"|"method_definition"|"type_alias_declaration");
        let mut scope=parent;
        if declaration{
            let name=n.child_by_field_name("name").and_then(|x|x.utf8_text(source).ok()).unwrap_or(n.kind()).to_string();
            scope=self.node(repo,name.clone(),n.kind(),file,n.start_position().row+1);self.edge(parent,scope,"Contains");
            self.definitions.entry(name).or_default().push(scope);
        }
        if matches!(n.kind(),"call_expression"|"invocation_expression"){
            if let Some(f)=n.child_by_field_name("function"){
                let target=f.child_by_field_name("field").or_else(||f.child_by_field_name("name")).unwrap_or(f);
                if let Ok(name)=target.utf8_text(source){if name.chars().all(|c|c.is_alphanumeric()||c=='_'){self.references.push((scope,name.to_owned()));}}
            }
        }
        let mut cursor=n.walk();for child in n.named_children(&mut cursor){self.walk(child,source,repo,file,scope);}
    }
}
fn git(root:&Path,args:&[&str])->Result<Vec<u8>>{let out=Command::new("git").arg("-C").arg(root).args(args).output()?;anyhow::ensure!(out.status.success(),"git operation failed in {}",root.display());Ok(out.stdout)}

fn main()->Result<()>{
    let mut args=std::env::args().skip(1);let output=PathBuf::from(args.next().context("Output JSON path required")?);let mut c=Capture::default();let mut repos=Vec::new();let mut input_hash=Sha256::new();
    for (index,arg) in args.enumerate(){
        let (name,path)=arg.split_once('=').context("Use name=/absolute/repository/path")?;let root=Path::new(path);let repo=index as u32;
        let repo_node=c.node(repo,name.into(),"Repository","",0);let head=String::from_utf8(git(root,&["rev-parse","HEAD"])?)?.trim().to_string();let dirty=!git(root,&["status","--porcelain"] )?.is_empty();let mut files:Vec<_>=git(root,&["ls-files","-z"] )?.split(|b|*b==0).filter_map(|p|std::str::from_utf8(p).ok()).filter(|p|!p.is_empty()).map(str::to_owned).collect();files.sort();let before=c.files;
        for file in files{
            if file.split('/').any(|p|matches!(p,"node_modules"|"target"|"bin"|"obj"|"vendor"|".claude"|".codex")){continue;}
            let ext=Path::new(&file).extension().and_then(|s|s.to_str()).unwrap_or("");
            let language=match ext{"rs"=>tree_sitter_rust::LANGUAGE.into(),"cs"=>tree_sitter_c_sharp::LANGUAGE.into(),"ts"=>tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),"tsx"=>tree_sitter_typescript::LANGUAGE_TSX.into(),_=>continue};
            let Ok(source)=std::fs::read(root.join(&file))else{continue;};if source.len()>2_000_000{continue;}
            input_hash.update(name.as_bytes());input_hash.update(file.as_bytes());input_hash.update(&source);let id=c.node(repo,file.clone(),"File",&file,1);c.edge(repo_node,id,"Contains");
            let mut parser=tree_sitter::Parser::new();parser.set_language(&language)?;let tree=parser.parse(&source,None).context("Parser produced no tree")?;c.files+=1;if tree.root_node().has_error(){c.parse_errors+=1;}c.walk(tree.root_node(),&source,repo,&file,id);
        }
        repos.push(serde_json::json!({"name":name,"head":head,"working_tree_dirty":dirty,"parsed_files":c.files-before}));
    }
    let mut unresolved=0;for (from,target) in c.references.clone(){match c.definitions.get(&target){Some(ids) if ids.len()==1&&ids[0]!=from=>c.edge(from,ids[0],"SyntaxReference"),_=>unresolved+=1,}}
    let real_nodes=c.nodes.len();let real_edges=c.edges.len();
    // Deliberately duplicated vectors in an isolated project test scope leakage.
    let controls=real_nodes.min(2500);let mut mapping=HashMap::new();for node in c.nodes[..controls].to_vec(){let original=node.id;let mut clone=node;clone.id=c.nodes.len() as u64+1;clone.project=1;mapping.insert(original,clone.id);c.nodes.push(clone);}
    for e in c.edges[..real_edges].to_vec(){if let(Some(src),Some(dst))=(mapping.get(&e.src),mapping.get(&e.dst)){c.edge(*src,*dst,&e.kind);}}
    c.edges.sort_by(|a,b|(a.src,a.dst,&a.kind).cmp(&(b.src,b.dst,&b.kind)));c.edges.dedup_by(|a,b|a.src==b.src&&a.dst==b.dst&&a.kind==b.kind);
    let f=Fixture{manifest:serde_json::json!({"format":1,"repositories":repos,"source_sha256":format!("{:x}",input_hash.finalize()),"parsed_files":c.files,"files_with_parse_errors":c.parse_errors,"source_nodes":real_nodes,"isolation_control_nodes":controls,"unresolved_or_ambiguous_syntax_references":unresolved,"edge_semantics":"Tree-sitter containment plus unique-name syntax candidates, not compiler-resolved calls","vectors":"384-dimensional normalized deterministic SHA-256 numeric fixtures; no inference or semantic RAG quality claim","control_project":"Duplicate content in project 1 for isolation tests; source multi-repository project is 0"}),nodes:c.nodes,edges:c.edges};
    if let Some(p)=output.parent(){std::fs::create_dir_all(p)?;}serde_json::to_writer(std::fs::File::create(&output)?,&f)?;eprintln!("{}",serde_json::json!({"nodes":f.nodes.len(),"edges":f.edges.len(),"files":f.manifest["parsed_files"],"output":output}));Ok(())
}
