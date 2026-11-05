//! Backend endpoint registrations (CodeGraph `TreeSitterExtractor.BackendEndpoints`, its
//! `BackendRegistrationPass` and `Router`): Axum (`Router::new().route(..).nest(..).merge(..)`)
//! in Rust and Gin (`gin.New()`, `Group`, `GET`/`POST`/.../`Any`/`Handle`) in Go become `Route`
//! nodes (`GET /users/{id}`), each with a `HANDLES` relationship to its handler function.
//!
//! The registration tree is evaluated per file exactly as retained (router values flow through
//! `let`/`:=` bindings, `if` branches get a copy of the environment, closures and nested
//! functions are not entered; Axum's `any` is the fallback an explicit method wins over, Gin's
//! `Any` registers every method). Handler resolution is scoped as retained and finished by the
//! indexer's linker: the handler's name in this file (limited to the function definitions of the
//! registering module scope), in the module files a `crate::`/`self::`/`super::` path names, in
//! the Go package directory, or in the directory a module-relative Go import names through the
//! nearest `go.mod`. A unique candidate makes the route `resolved`; none, several, a dynamic path
//! or method, or a route registered by more than one file leaves it unresolved.
//!
//! A registration source with syntax errors yields no routes and records the pass incomplete (a
//! recovered tree could silently drop endpoints). The pass is bounded in work and depth.
//! The retained route identity is `route:{repository}:{method}:{path}`; units are repository
//! scoped here, so the qualified name is `route:{method}:{path}` and the repository is the
//! unit's.
use devworks_core::ports::indexing::{EdgeTarget, ExtractedEdge, ExtractedNode, Resolution, Span};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use tree_sitter::Node;

const WILDCARD: [&str; 9] = [
    "GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS", "TRACE", "CONNECT",
];
const GIN_METHODS: [&str; 11] = [
    "GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS", "CONNECT", "TRACE", "Any", "Handle",
];
/// Work and depth bounds of one file's pass.
const MAX_WORK: u64 = 50_000_000;
const MAX_DEPTH: usize = 512;

/// What one file's registrations contributed: route nodes (parent 0, the file) and edges whose
/// `source` indexes into `nodes` (offset by the caller).
#[derive(Default)]
pub struct Endpoints {
    pub nodes: Vec<ExtractedNode>,
    pub edges: Vec<ExtractedEdge>,
    pub incomplete: Option<String>,
}

/// Whether a file of this extension may hold registrations (the retained prefilter).
pub fn may_contain(rust: bool, content: &str) -> bool {
    let methods: &[&str] = if rust {
        &["route", "nest", "merge"]
    } else {
        &GIN_METHODS
    };
    let b = content.as_bytes();
    for (i, _) in content.match_indices('.') {
        let mut j = i + 1;
        while j < b.len() && (b[j] as char).is_whitespace() {
            j += 1;
        }
        let start = j;
        while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
            j += 1;
        }
        let ident = &content[start..j];
        if !methods.contains(&ident) {
            continue;
        }
        while j < b.len() && (b[j] as char).is_whitespace() {
            j += 1;
        }
        if b.get(j) == Some(&b'(') {
            return true;
        }
    }
    false
}

type Key = (usize, usize, u16);
fn key(n: Node<'_>) -> Key {
    (n.start_byte(), n.end_byte(), n.kind_id())
}

#[derive(Clone)]
struct Registration<'t> {
    path: Option<String>,
    verb: String,
    handler: Node<'t>,
    origin: Node<'t>,
    wildcard: bool,
    registration_method: Option<&'static str>,
}

#[derive(Default)]
struct Router<'t> {
    routes: Vec<Registration<'t>>,
    consumed: bool,
    /// `None`: a Gin router group whose prefix is not known (a parameter).
    prefix: Option<String>,
}

struct Limit;

struct Pass<'t, 's> {
    src: &'s [u8],
    rel: &'s str,
    rust: bool,
    imports: HashMap<String, String>,
    scope_imports: HashMap<String, HashMap<String, String>>,
    functions: Vec<Node<'t>>,
    rust_uses: Vec<Node<'t>>,
    go_imports: Vec<Node<'t>>,
    locations: HashMap<Key, (String, Option<Node<'t>>)>,
    function_lines: HashMap<(String, String), BTreeSet<u32>>,
    shadow_starts: HashMap<(Key, String), usize>,
    gin_aliases: BTreeSet<String>,
    routers: Vec<Router<'t>>,
    depth: usize,
    work: u64,
}

impl<'t, 's> Pass<'t, 's> {
    fn charge(&mut self, n: u64) -> Result<(), Limit> {
        self.work = self.work.saturating_add(n);
        if self.work > MAX_WORK {
            Err(Limit)
        } else {
            Ok(())
        }
    }
    fn text(&self, n: Option<Node<'_>>) -> String {
        n.map(|n| String::from_utf8_lossy(&self.src[n.start_byte()..n.end_byte()]).into_owned())
            .unwrap_or_default()
    }
    fn enter(&mut self) -> Result<(), Limit> {
        self.charge(1)?;
        if self.depth >= MAX_DEPTH {
            return Err(Limit);
        }
        self.depth += 1;
        Ok(())
    }
    fn leave(&mut self) {
        self.depth -= 1;
    }
    fn named(n: Node<'t>) -> Vec<Node<'t>> {
        let mut c = n.walk();
        n.named_children(&mut c).collect()
    }

    fn index(&mut self, root: Node<'t>) -> Result<(), Limit> {
        let mut pending: Vec<(Node<'t>, usize, String, Option<Node<'t>>)> =
            vec![(root, 0, String::new(), None)];
        while let Some((node, depth, scope, function)) = pending.pop() {
            if depth > 4096 {
                return Err(Limit);
            }
            self.locations.insert(key(node), (scope.clone(), function));
            let mut function = function;
            if matches!(
                node.kind(),
                "function_item" | "function_declaration" | "method_declaration"
            ) {
                self.functions.push(node);
                function = Some(node);
                let name_node = node.child_by_field_name("name");
                let name = self.text(name_node);
                self.charge((scope.len() + name.len()) as u64)?;
                let lines = self
                    .function_lines
                    .entry((scope.clone(), name))
                    .or_default();
                lines.insert(node.start_position().row as u32 + 1);
                if let Some(nn) = name_node {
                    lines.insert(nn.start_position().row as u32 + 1);
                }
            }
            if node.kind() == "use_declaration" {
                self.rust_uses.push(node);
            }
            if node.kind() == "import_spec" {
                self.go_imports.push(node);
            }
            if let Some(f) = function
                && matches!(
                    node.kind(),
                    "let_declaration"
                        | "short_var_declaration"
                        | "parameter"
                        | "parameter_declaration"
                )
            {
                let n = node
                    .child_by_field_name("pattern")
                    .or_else(|| node.child_by_field_name("name"))
                    .or_else(|| node.child_by_field_name("left"));
                let name = self.text(n);
                let e = self
                    .shadow_starts
                    .entry((key(f), name))
                    .or_insert(usize::MAX);
                *e = (*e).min(node.start_byte());
            }
            let mut scope = scope;
            if node.kind() == "mod_item" {
                let name = self.text(node.child_by_field_name("name"));
                scope = if scope.is_empty() {
                    name
                } else {
                    format!("{scope}::{name}")
                };
            }
            for child in Self::named(node) {
                self.charge(1)?;
                pending.push((child, depth + 1, scope.clone(), function));
            }
        }
        Ok(())
    }

    fn scope_of(&self, n: Node<'_>) -> String {
        self.locations
            .get(&key(n))
            .map(|l| l.0.clone())
            .unwrap_or_default()
    }

    fn expand_use(&mut self, text: &str, prefix: &str, depth: usize) -> Result<(), Limit> {
        if depth > MAX_DEPTH {
            return Err(Limit);
        }
        self.charge((text.len() + prefix.len()) as u64)?;
        if let Some(brace) = text.find('{')
            && text.ends_with('}')
        {
            let b = text.as_bytes();
            let mut nesting = 0i32;
            let first = brace + 1;
            let mut start = first;
            for i in first..b.len() {
                if b[i] == b'{' {
                    nesting += 1;
                }
                if (b[i] == b',' && nesting == 0) || i == b.len() - 1 {
                    let part = text[start..i].trim().to_owned();
                    let p = format!("{prefix}{}", &text[..brace]);
                    self.expand_use(&part, &p, depth + 1)?;
                    start = i + 1;
                }
                if b[i] == b'}' {
                    nesting -= 1;
                }
            }
            return Ok(());
        }
        let parts: Vec<&str> = split_as(text);
        let mut target = format!("{prefix}{}", parts[0].trim());
        if let Some(t) = target.strip_suffix("::self") {
            target = t.to_owned();
        }
        let alias = if parts.len() == 2 {
            parts[1].trim().to_owned()
        } else {
            target.rsplit("::").next().unwrap_or(&target).to_owned()
        };
        self.imports.insert(alias, target);
        Ok(())
    }

    fn literal(&self, n: Option<Node<'_>>) -> Option<String> {
        let n = n?;
        if !matches!(
            n.kind(),
            "string_literal" | "raw_string_literal" | "interpreted_string_literal"
        ) {
            return None;
        }
        let t = self.text(Some(n));
        if t.len() >= 2 && t.starts_with('`') && t.ends_with('`') {
            return Some(t[1..t.len() - 1].to_owned());
        }
        if t.starts_with('r') {
            let first = t.find('"')?;
            let last = t.rfind('"')?;
            return (last > first).then(|| t[first + 1..last].to_owned());
        }
        serde_json::from_str::<String>(&t).ok()
    }

    fn read_imports(&mut self) -> Result<(), Limit> {
        if self.rust {
            let mut groups: BTreeMap<String, Vec<Node<'t>>> = BTreeMap::new();
            for u in self.rust_uses.clone() {
                groups.entry(self.scope_of(u)).or_default().push(u);
            }
            for (scope, uses) in groups {
                self.imports = HashMap::new();
                for u in uses {
                    self.charge(1)?;
                    let mut text = self.text(Some(u)).trim().trim_end_matches(';').to_owned();
                    if let Some(i) = text.find("use ") {
                        text = text[i + 4..].trim().to_owned();
                    }
                    self.expand_use(&text, "", 0)?;
                }
                let imports = std::mem::take(&mut self.imports);
                self.scope_imports.insert(scope, imports);
            }
        } else {
            for n in self.go_imports.clone() {
                self.charge(1)?;
                if let Some(path) = self.literal(n.child_by_field_name("path")) {
                    let alias = match n.child_by_field_name("name") {
                        Some(a) => self.text(Some(a)),
                        None => path.rsplit('/').next().unwrap_or(&path).to_owned(),
                    };
                    self.imports.insert(alias, path);
                }
            }
            for (alias, path) in &self.imports {
                if path == "github.com/gin-gonic/gin" {
                    self.gin_aliases.insert(alias.clone());
                }
            }
        }
        Ok(())
    }

    fn set_rust_imports(&mut self, origin: Node<'_>) {
        let scope = self.scope_of(origin);
        self.imports = self.scope_imports.get(&scope).cloned().unwrap_or_default();
    }

    fn expand_path(&self, path: &str) -> String {
        let (head, tail) = match path.split_once("::") {
            Some((h, t)) => (h, Some(t)),
            None => (path, None),
        };
        match self.imports.get(head) {
            Some(imported) => match tail {
                Some(t) => format!("{imported}::{t}"),
                None => imported.clone(),
            },
            None => path.to_owned(),
        }
    }

    fn new_router(&mut self) -> usize {
        self.routers.push(Router {
            prefix: Some(String::new()),
            ..Router::default()
        });
        self.routers.len() - 1
    }

    fn add(&mut self, router: usize, r: Registration<'t>) -> Result<(), Limit> {
        self.charge(if r.wildcard { WILDCARD.len() as u64 } else { 1 })?;
        if r.wildcard {
            for m in WILDCARD {
                let mut x = r.clone();
                x.verb = m.into();
                x.wildcard = true;
                x.registration_method = Some("ANY");
                self.routers[router].routes.push(x);
            }
        } else {
            self.routers[router].routes.push(r);
        }
        Ok(())
    }

    fn process(
        &mut self,
        node: Option<Node<'t>>,
        env: &mut HashMap<String, usize>,
    ) -> Result<(), Limit> {
        let Some(node) = node else { return Ok(()) };
        self.enter()?;
        let r = self.process_inner(node, env);
        self.leave();
        r
    }
    fn process_inner(
        &mut self,
        node: Node<'t>,
        env: &mut HashMap<String, usize>,
    ) -> Result<(), Limit> {
        if matches!(
            node.kind(),
            "let_declaration"
                | "short_var_declaration"
                | "var_spec"
                | "assignment_expression"
                | "assignment_statement"
        ) {
            let mut value = node
                .child_by_field_name("value")
                .or_else(|| node.child_by_field_name("right"));
            if let Some(v) = value
                && v.kind() == "expression_list"
            {
                value = Self::named(v).into_iter().next();
            }
            let mut name = node
                .child_by_field_name("pattern")
                .or_else(|| node.child_by_field_name("left"))
                .or_else(|| node.child_by_field_name("name"));
            if let Some(n) = name
                && n.kind() == "expression_list"
            {
                name = Self::named(n).into_iter().next();
            }
            if let (Some(v), Some(n)) = (value, name) {
                let router = self.evaluate(v, env)?;
                let k = self.text(Some(n));
                match router {
                    Some(r) => {
                        env.insert(k, r);
                    }
                    None => {
                        env.remove(&k);
                    }
                }
            }
            return Ok(());
        }
        if node.kind() == "call_expression" {
            self.evaluate(node, env)?;
            return Ok(());
        }
        if matches!(
            node.kind(),
            "function_item"
                | "function_declaration"
                | "method_declaration"
                | "closure_expression"
                | "func_literal"
        ) {
            return Ok(());
        }
        let branch = matches!(node.kind(), "if_expression" | "if_statement");
        for child in Self::named(node) {
            if branch {
                self.charge(env.len() as u64)?;
                let mut copy = env.clone();
                self.process(Some(child), &mut copy)?;
            } else {
                self.process(Some(child), env)?;
            }
        }
        Ok(())
    }

    fn join(&mut self, prefix: Option<&str>, path: Option<&str>) -> Result<Option<String>, Limit> {
        let (Some(prefix), Some(path)) = (prefix, path) else {
            return Ok(None);
        };
        self.charge((prefix.len() + path.len()) as u64)?;
        let joined = format!(
            "{}/{}",
            prefix.trim_end_matches('/'),
            path.trim_start_matches('/')
        );
        Ok(Some(format!("/{}", joined.trim_start_matches('/'))))
    }

    fn evaluate(
        &mut self,
        node: Node<'t>,
        env: &HashMap<String, usize>,
    ) -> Result<Option<usize>, Limit> {
        self.enter()?;
        let r = self.evaluate_inner(node, env);
        self.leave();
        r
    }
    fn evaluate_inner(
        &mut self,
        node: Node<'t>,
        env: &HashMap<String, usize>,
    ) -> Result<Option<usize>, Limit> {
        if node.kind() == "identifier" {
            return Ok(env.get(&self.text(Some(node))).copied());
        }
        if matches!(
            node.kind(),
            "reference_expression" | "parenthesized_expression" | "try_expression"
        ) {
            for c in Self::named(node) {
                if let Some(r) = self.evaluate(c, env)? {
                    return Ok(Some(r));
                }
            }
            return Ok(None);
        }
        if node.kind() != "call_expression" {
            return Ok(None);
        }
        let Some(function) = node.child_by_field_name("function") else {
            return Ok(None);
        };
        let arguments: Vec<Node<'t>> = node
            .child_by_field_name("arguments")
            .map(Self::named)
            .unwrap_or_default();
        let receiver = function.child_by_field_name(if self.rust { "value" } else { "operand" });
        let field = function.child_by_field_name("field");
        let method = field.map(|f| self.text(Some(f)));
        let constructor = if self.rust && receiver.is_none() {
            let text = self.text(Some(function));
            text.strip_suffix("::new")
                .is_some_and(|base| self.expand_path(base) == "axum::Router")
        } else if !self.rust
            && let Some(r) = receiver.filter(|r| r.kind() == "identifier")
            && matches!(method.as_deref(), Some("New" | "Default"))
        {
            self.imports.get(&self.text(Some(r))).map(String::as_str)
                == Some("github.com/gin-gonic/gin")
        } else {
            false
        };
        if constructor {
            return Ok(Some(self.new_router()));
        }
        let (Some(receiver), Some(method)) = (receiver, method) else {
            return Ok(None);
        };
        let Some(parent) = self.evaluate(receiver, env)? else {
            return Ok(None);
        };
        let origin = field.unwrap_or(node);
        if !self.rust {
            if method == "Group" && !arguments.is_empty() {
                let lit = self.literal(Some(arguments[0]));
                let prefix = self.routers[parent].prefix.clone();
                let joined = self.join(prefix.as_deref(), lit.as_deref())?;
                self.routers.push(Router {
                    prefix: joined,
                    ..Router::default()
                });
                // A group is a value, not a registered router (the retained pass never lists it).
                let i = self.routers.len() - 1;
                self.routers[i].consumed = true;
                return Ok(Some(i));
            }
            if GIN_METHODS.contains(&method.as_str()) {
                let offset = usize::from(method == "Handle");
                if arguments.len() < offset + 2 {
                    return Ok(Some(parent));
                }
                let verb = if method == "Handle" {
                    self.literal(Some(arguments[0]))
                        .map(|v| v.to_uppercase())
                        .unwrap_or_else(|| "UNKNOWN".into())
                } else if method == "Any" {
                    "ANY".into()
                } else {
                    method.clone()
                };
                let lit = self.literal(Some(arguments[offset]));
                let prefix = self.routers[parent].prefix.clone();
                let path = self.join(prefix.as_deref(), lit.as_deref())?;
                let reg = Registration {
                    path,
                    verb,
                    handler: *arguments.last().expect("two arguments"),
                    origin,
                    wildcard: method == "Any",
                    registration_method: None,
                };
                let out = self.new_router();
                self.add(out, reg)?;
            }
            return Ok(Some(parent));
        }
        // Rust routers are values: the accumulated tree moves to the outer builder.
        let result = self.new_router();
        self.routers[parent].consumed = true;
        let inherited = self.routers[parent].routes.clone();
        self.charge(inherited.len() as u64)?;
        self.routers[result].routes.extend(inherited);
        if method == "route" && arguments.len() >= 2 {
            let path = self.literal(Some(arguments[0]));
            for (verb, handler) in self.method_handlers(arguments[1])? {
                let reg = Registration {
                    path: path.clone(),
                    wildcard: verb == "ANY",
                    verb,
                    handler,
                    origin,
                    registration_method: None,
                };
                self.add(result, reg)?;
            }
        } else if (method == "nest" && arguments.len() >= 2)
            || (method == "merge" && !arguments.is_empty())
        {
            let nest = method == "nest";
            if let Some(nested) = self.evaluate(arguments[usize::from(nest)], env)? {
                self.routers[nested].consumed = true;
                let routes = self.routers[nested].routes.clone();
                self.charge(routes.len() as u64)?;
                let prefix = nest.then(|| self.literal(Some(arguments[0])));
                for mut r in routes {
                    if let Some(p) = &prefix {
                        r.path = self.join(p.as_deref(), r.path.as_deref())?;
                    }
                    self.routers[result].routes.push(r);
                }
            }
        }
        Ok(Some(result))
    }

    fn method_handlers(&mut self, node: Node<'t>) -> Result<Vec<(String, Node<'t>)>, Limit> {
        self.enter()?;
        let r = self.method_handlers_inner(node);
        self.leave();
        r
    }
    fn method_handlers_inner(&mut self, node: Node<'t>) -> Result<Vec<(String, Node<'t>)>, Limit> {
        let mut out = Vec::new();
        if node.kind() != "call_expression" {
            return Ok(out);
        }
        let Some(function) = node.child_by_field_name("function") else {
            return Ok(out);
        };
        let args: Vec<Node<'t>> = node
            .child_by_field_name("arguments")
            .map(Self::named)
            .unwrap_or_default();
        if args.is_empty() {
            return Ok(out);
        }
        let receiver = function.child_by_field_name("value");
        let name = match function.child_by_field_name("field") {
            Some(f) => self.text(Some(f)),
            None => self
                .expand_path(&self.text(Some(function)))
                .rsplit("::")
                .next()
                .unwrap_or("")
                .to_owned(),
        };
        if let Some(r) = receiver {
            out.extend(self.method_handlers(r)?);
        } else if !self
            .expand_path(&self.text(Some(function)))
            .starts_with("axum::routing::")
        {
            return Ok(out);
        }
        if matches!(
            name.as_str(),
            "get"
                | "post"
                | "put"
                | "patch"
                | "delete"
                | "head"
                | "options"
                | "trace"
                | "connect"
                | "any"
        ) {
            out.push((name.to_uppercase(), args[0]));
        }
        Ok(out)
    }

    fn handler_shape(&self, expr: &str) -> bool {
        let sep = if self.rust { "::" } else { "." };
        !expr.is_empty()
            && expr
                .split(sep)
                .all(|p| !p.is_empty() && p.chars().all(|c| c.is_alphanumeric() || c == '_'))
    }

    /// The candidate scopes of a handler expression (retained `ResolveHandler`), as the
    /// linker's `candidateUnits` / `candidateLines` / `candidateDirs` / `goImport`.
    fn candidates(&mut self, expr: &str, origin: Node<'t>) -> Result<Map<String, Value>, Limit> {
        let mut out = Map::new();
        self.charge(expr.len() as u64 + 1)?;
        let none = || {
            let mut m = Map::new();
            m.insert("candidateUnits".into(), json!([]));
            m
        };
        if !self.handler_shape(expr) {
            return Ok(none());
        }
        let sep = if self.rust { "::" } else { "." };
        let parts: Vec<&str> = expr.split(sep).collect();
        let (scope, function) = self
            .locations
            .get(&key(origin))
            .cloned()
            .unwrap_or_default();
        if let Some(f) = function
            && let Some(shadow) = self.shadow_starts.get(&(key(f), parts[0].to_owned()))
            && *shadow < origin.start_byte()
        {
            // The handler names a local binding, not a function.
            return Ok(none());
        }
        if !self.rust {
            if parts.len() == 1 {
                out.insert("candidateDirs".into(), json!([dir_of(self.rel)]));
            } else if parts.len() == 2
                && let Some(import) = self.imports.get(parts[0])
            {
                out.insert("goImport".into(), json!(import));
            } else {
                return Ok(none());
            }
            return Ok(out);
        }
        let expanded = self.expand_path(expr);
        let parts: Vec<String> = expanded.split("::").map(str::to_owned).collect();
        let name = parts.last().cloned().unwrap_or_default();
        let lines_of = |p: &Self, scope: &str| -> Vec<u32> {
            p.function_lines
                .get(&(scope.to_owned(), name.clone()))
                .map(|l| l.iter().copied().collect())
                .unwrap_or_default()
        };
        let mut units: Vec<String> = Vec::new();
        let mut lines = Map::new();
        if parts.len() == 1 {
            units.push(self.rel.to_owned());
            lines.insert(self.rel.to_owned(), json!(lines_of(self, &scope)));
        } else {
            let rel = self.rel;
            let crate_root = match rel.rfind("/src/") {
                Some(i) => rel[..i + 5].to_owned(),
                None if rel.starts_with("src/") => "src/".to_owned(),
                None if dir_of(rel).is_empty() => String::new(),
                None => format!("{}/", dir_of(rel)),
            };
            let module = module_of(&rel[crate_root.len()..]);
            let file_modules: Vec<String> = if module.is_empty() {
                Vec::new()
            } else {
                module.split("::").map(str::to_owned).collect()
            };
            let mut segments = file_modules.clone();
            if !scope.is_empty() {
                segments.extend(scope.split("::").map(str::to_owned));
            }
            let head = expr.split("::").next().unwrap_or("");
            if self.imports.contains_key(head)
                && !matches!(parts[0].as_str(), "self" | "super" | "crate")
            {
                segments.clear();
            }
            self.charge((parts.len() + scope.len() + rel.len()) as u64)?;
            let mut start = 0;
            if parts[0] == "crate" {
                segments.clear();
                start += 1;
            } else if parts[0] == "self" {
                start += 1;
            }
            while start < parts.len() - 1 && parts[start] == "super" {
                segments.pop();
                start += 1;
            }
            segments.extend(parts[start..parts.len() - 1].iter().cloned());
            let target = format!("{crate_root}{}", segments.join("/"));
            let inline_scope = segments[file_modules.len().min(segments.len())..].join("::");
            if segments.len() >= file_modules.len()
                && segments[..file_modules.len()] == file_modules[..]
            {
                units.push(rel.to_owned());
                lines.insert(rel.to_owned(), json!(lines_of(self, &inline_scope)));
            }
            units.push(format!("{target}.rs"));
            units.push(format!("{target}/mod.rs"));
            if segments.is_empty() {
                units.push(format!("{crate_root}lib.rs"));
                units.push(format!("{crate_root}main.rs"));
            }
        }
        out.insert("candidateUnits".into(), json!(units));
        if !lines.is_empty() {
            out.insert("candidateLines".into(), Value::Object(lines));
        }
        Ok(out)
    }

    fn emit(&mut self, r: &Registration<'t>, out: &mut Endpoints) -> Result<(), Limit> {
        self.charge(1)?;
        if self.rust {
            self.set_rust_imports(r.origin);
        }
        let handler_text = if r.handler.kind() == "call_expression" {
            self.text(Some(
                r.handler
                    .child_by_field_name("function")
                    .unwrap_or(r.handler),
            ))
        } else {
            self.text(Some(r.handler))
        };
        let expanded = if self.rust {
            self.expand_path(&handler_text)
        } else {
            handler_text.clone()
        };
        let handler_scope = if self.rust {
            expanded.clone()
        } else {
            match handler_text
                .split_once('.')
                .and_then(|(p, _)| self.imports.get(p))
            {
                Some(pkg) if handler_text.contains('.') => pkg.clone(),
                _ => dir_of(self.rel).to_owned(),
            }
        };
        let row = r.origin.start_position();
        let identity = r
            .path
            .clone()
            .unwrap_or_else(|| format!("dynamic:{}:{}:{}", self.rel, row.row + 1, row.column + 1));
        let qn = format!("route:{}:{identity}", r.verb);
        let shaped = self.handler_shape(&handler_text);
        let handler_name = if shaped {
            expanded
                .rsplit(if self.rust { "::" } else { "." })
                .next()
                .unwrap_or("")
                .to_owned()
        } else {
            String::new()
        };
        let status = if r.path.is_none() {
            "dynamic_path"
        } else if r.verb == "UNKNOWN" {
            "dynamic_method"
        } else if handler_name.is_empty() {
            "unresolved_handler"
        } else {
            // Settled by the linker: `resolved`, `unresolved_handler` or `ambiguous_handler`.
            "pending_link"
        };
        let mut props = json!({
            "http_method": r.verb, "route_kind": "endpoint",
            "framework": if self.rust { "axum" } else { "gin" }, "source": "tree-sitter-registration",
            "origin_file": self.rel, "handler_expression": handler_text, "handler_name": handler_name,
            "semantic_owner": "http_endpoints", "handler_scope": handler_scope,
            "resolution_status": status,
            "confidence": "low",
        });
        if let Some(m) = r.registration_method {
            props["registration_method"] = json!(m);
        }
        if let Some(p) = &r.path {
            props["route_template"] = json!(p);
        }
        let name = format!("{} {}", r.verb, r.path.as_deref().unwrap_or("<dynamic>"));
        let span = Span {
            start_line: r.origin.start_position().row as u32 + 1,
            start_column: r.origin.start_position().column as u32 + 1,
            end_line: r.origin.end_position().row as u32 + 1,
            end_column: r.origin.end_position().column as u32 + 1,
            start_byte: r.origin.start_byte() as u32,
            end_byte: r.origin.end_byte() as u32,
        };
        out.nodes.push(ExtractedNode {
            kind: "Route".into(),
            name: name.clone(),
            qualified_name: qn,
            span,
            parent: Some(0),
            properties: match props {
                Value::Object(m) => m,
                _ => Map::new(),
            },
        });
        if status == "pending_link" && !handler_name.is_empty() {
            let mut ep = self.candidates(&handler_text, r.origin)?;
            ep.insert("routeName".into(), json!(name));
            ep.insert("origin_file".into(), json!(self.rel));
            ep.insert("semantic_owner".into(), json!("http_endpoints"));
            ep.insert(
                "framework".into(),
                json!(if self.rust { "axum" } else { "gin" }),
            );
            out.edges.push(ExtractedEdge {
                kind: "HANDLES".into(),
                source: out.nodes.len() - 1,
                target: EdgeTarget::Symbol(handler_name),
                resolution: Resolution::Unresolved,
                confidence: 0.9,
                properties: ep,
            });
        }
        Ok(())
    }

    fn run(&mut self, root: Node<'t>, content: &str) -> Result<Endpoints, Limit> {
        self.index(root)?;
        self.read_imports()?;
        self.charge(content.len() as u64)?;
        let framework = if self.rust {
            self.scope_imports
                .values()
                .any(|m| m.values().any(|v| v.starts_with("axum::")))
                || content.contains("axum::Router")
        } else {
            self.imports
                .values()
                .any(|v| v == "github.com/gin-gonic/gin")
        };
        let mut out = Endpoints::default();
        if !framework {
            return Ok(out);
        }
        for f in self.functions.clone() {
            if self.rust {
                self.set_rust_imports(f);
            }
            let mut env: HashMap<String, usize> = HashMap::new();
            if !self.rust {
                let params = f.child_by_field_name("parameters");
                let mut stack: Vec<Node<'t>> = params.into_iter().collect();
                while let Some(n) = stack.pop() {
                    self.charge(1)?;
                    if n.kind() == "parameter_declaration" {
                        let ty = self.text(n.child_by_field_name("type"));
                        if let Some(rest) = ty.strip_prefix('*')
                            && let Some((alias, kind)) = rest.rsplit_once('.')
                            && !alias.is_empty()
                            && self.gin_aliases.contains(alias)
                            && matches!(kind, "Engine" | "RouterGroup")
                        {
                            for id in Self::named(n)
                                .into_iter()
                                .filter(|c| c.kind() == "identifier")
                            {
                                self.routers.push(Router {
                                    prefix: (kind != "RouterGroup").then(String::new),
                                    consumed: true,
                                    ..Router::default()
                                });
                                env.insert(self.text(Some(id)), self.routers.len() - 1);
                            }
                        }
                    }
                    stack.extend(Self::named(n));
                }
            }
            self.process(f.child_by_field_name("body"), &mut env)?;
        }
        let routers: Vec<usize> = (0..self.routers.len())
            .filter(|i| !self.routers[*i].consumed)
            .collect();
        for i in routers {
            let mut regs = self.routers[i].routes.clone();
            for r in &regs {
                self.charge(6 * (1 + r.path.as_ref().map_or(0, String::len)) as u64)?;
            }
            if self.rust {
                // Axum tries explicit HEAD, then GET, before its `any` fallback.
                let wildcard_head: BTreeSet<Option<String>> = regs
                    .iter()
                    .filter(|r| r.wildcard && r.verb == "HEAD" && r.path.is_some())
                    .map(|r| r.path.clone())
                    .collect();
                let explicit_head: BTreeSet<Option<String>> = regs
                    .iter()
                    .filter(|r| !r.wildcard && r.verb == "HEAD")
                    .map(|r| r.path.clone())
                    .collect();
                let extra: Vec<Registration<'t>> = regs
                    .iter()
                    .filter(|r| {
                        !r.wildcard
                            && r.verb == "GET"
                            && wildcard_head.contains(&r.path)
                            && !explicit_head.contains(&r.path)
                    })
                    .map(|r| {
                        let mut x = r.clone();
                        x.verb = "HEAD".into();
                        x.registration_method = Some("GET");
                        x
                    })
                    .collect();
                regs.extend(extra);
            }
            let explicit: BTreeSet<(Option<String>, String)> = regs
                .iter()
                .filter(|r| !r.wildcard && r.path.is_some())
                .map(|r| (r.path.clone(), r.verb.clone()))
                .collect();
            for r in &regs {
                if !self.rust
                    || !r.wildcard
                    || !explicit.contains(&(r.path.clone(), r.verb.clone()))
                {
                    self.emit(r, &mut out)?;
                }
            }
        }
        // One route identity with different handlers in this file: ambiguous, no handler.
        let mut handlers: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for n in &out.nodes {
            let h = format!(
                "{}|{}",
                n.properties["handler_expression"].as_str().unwrap_or(""),
                n.properties["handler_scope"].as_str().unwrap_or("")
            );
            handlers
                .entry(n.qualified_name.clone())
                .or_default()
                .insert(h);
        }
        let ambiguous: BTreeSet<String> = handlers
            .into_iter()
            .filter(|(_, h)| h.len() > 1)
            .map(|(q, _)| q)
            .collect();
        for n in &mut out.nodes {
            if ambiguous.contains(&n.qualified_name) {
                n.properties
                    .insert("resolution_status".into(), json!("ambiguous_handler"));
                n.properties
                    .insert("registration_ambiguous".into(), json!(true));
                n.properties.insert("confidence".into(), json!("low"));
            }
        }
        // Distinct by qualified name (the first registration wins), edges by (route, handler).
        let mut keep: Vec<bool> = Vec::with_capacity(out.nodes.len());
        let mut seen = BTreeSet::new();
        for n in &out.nodes {
            keep.push(seen.insert(n.qualified_name.clone()));
        }
        let mut remap: Vec<Option<usize>> = Vec::with_capacity(keep.len());
        let mut next = 0usize;
        for k in &keep {
            remap.push(k.then(|| {
                next += 1;
                next - 1
            }));
        }
        let nodes: Vec<ExtractedNode> = out
            .nodes
            .into_iter()
            .zip(&keep)
            .filter(|(_, k)| **k)
            .map(|(n, _)| n)
            .collect();
        let mut edge_keys = BTreeSet::new();
        let edges: Vec<ExtractedEdge> = out
            .edges
            .into_iter()
            .filter_map(|mut e| {
                let q = &nodes.get(remap[e.source]?)?.qualified_name;
                if ambiguous.contains(q) {
                    return None;
                }
                let target = match &e.target {
                    EdgeTarget::Symbol(s) => s.clone(),
                    _ => String::new(),
                };
                if !edge_keys.insert((q.clone(), target)) {
                    return None;
                }
                e.source = remap[e.source]?;
                Some(e)
            })
            .collect();
        Ok(Endpoints {
            nodes,
            edges,
            incomplete: None,
        })
    }
}

/// `use a::{b, c as d}`'s `x as y` split.
fn split_as(text: &str) -> Vec<&str> {
    let mut chars = text.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c.is_whitespace() {
            while chars.peek().is_some_and(|(_, c)| c.is_whitespace()) {
                chars.next();
            }
            let j = chars.peek().map_or(text.len(), |(j, _)| *j);
            if let Some(alias) = text[j..].strip_prefix("as")
                && alias.starts_with(char::is_whitespace)
            {
                return vec![&text[..i], alias.trim_start()];
            }
        }
    }
    vec![text]
}

fn dir_of(p: &str) -> &str {
    p.rfind('/').map_or("", |i| &p[..i])
}

/// The Rust module path of a file below its crate root (`api/users.rs` → `api::users`;
/// `lib.rs`, `main.rs`, `mod.rs` name their directory).
fn module_of(rel: &str) -> String {
    let m = rel.replace('/', "::");
    let m = m
        .strip_suffix("::lib.rs")
        .or_else(|| m.strip_suffix("::main.rs"))
        .or_else(|| m.strip_suffix("::mod.rs"))
        .map(str::to_owned)
        .unwrap_or_else(|| {
            if matches!(m.as_str(), "lib.rs" | "main.rs" | "mod.rs") {
                String::new()
            } else {
                m.strip_suffix(".rs").unwrap_or(&m).to_owned()
            }
        });
    m.trim_matches(':').to_owned()
}

/// Extracts the registrations of one Rust or Go file (`rel`: its repository-relative path).
pub fn extract(root: Node<'_>, source: &[u8], rel: &str, rust: bool) -> Endpoints {
    let content = String::from_utf8_lossy(source);
    if !may_contain(rust, &content) {
        return Endpoints::default();
    }
    if root.has_error() {
        // A recovered tree could silently remove endpoints: none are claimed.
        return Endpoints {
            incomplete: Some("backend-endpoints: the registration source has syntax errors".into()),
            ..Endpoints::default()
        };
    }
    let mut pass = Pass {
        src: source,
        rel,
        rust,
        imports: HashMap::new(),
        scope_imports: HashMap::new(),
        functions: Vec::new(),
        rust_uses: Vec::new(),
        go_imports: Vec::new(),
        locations: HashMap::new(),
        function_lines: HashMap::new(),
        shadow_starts: HashMap::new(),
        gin_aliases: BTreeSet::new(),
        routers: Vec::new(),
        depth: 0,
        work: 0,
    };
    match pass.run(root, &content) {
        Ok(e) => e,
        Err(Limit) => Endpoints {
            incomplete: Some(
                "backend-endpoints: the analysis work or depth limit was reached".into(),
            ),
            ..Endpoints::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modules_and_use_aliases_follow_the_retained_rules() {
        assert_eq!(module_of("api/users.rs"), "api::users");
        assert_eq!(module_of("lib.rs"), "");
        assert_eq!(module_of("api/mod.rs"), "api");
        assert_eq!(split_as("axum::Router as R"), ["axum::Router", "R"]);
        assert_eq!(split_as("axum::routing::get"), ["axum::routing::get"]);
        assert_eq!(split_as("crate::pà as x"), ["crate::pà", "x"]);
        assert_eq!(split_as("crate::àas as handler"), ["crate::àas", "handler"]);
        assert_eq!(split_as("crate::pà\u{2003}as\u{2003}x"), ["crate::pà", "x"]);
        assert_eq!(split_as("crate::àas"), ["crate::àas"]);
        assert!(may_contain(
            true,
            "let app = Router::new() . route(\"/\", get(h));"
        ));
        assert!(!may_contain(true, "fn main() {}"));
        assert!(may_contain(false, "r.GET(\"/x\", h)"));
    }
}
