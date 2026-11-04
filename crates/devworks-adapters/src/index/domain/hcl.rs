//! Terraform: a port of the retained `HclParser` (a regex best-effort parser of `.tf` and
//! `.tfvars` files), with the same expressions, block-body scanner, qualified names
//! (`resource.{type}.{name}`, `data.{type}.{name}`, `module.{name}`, `var.{name}`,
//! `output.{name}`, `local.{name}`, `tfvar.{name}`), properties and edges (`DEPENDS_ON`,
//! `INCLUDES_MODULE`, `CALLS` from outputs, the cross-repository `DEPLOYS`/`CONFIGURES` and the
//! URL/host/connection-string references).
use super::{Lines, Out, file_stem, props, utf16_len};
use devworks_core::ports::indexing::ExtractFault;
use regex::Regex;
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex};

fn re(p: &str) -> Regex {
    Regex::new(p).expect("a valid retained expression")
}
static RESOURCE: LazyLock<Regex> =
    LazyLock::new(|| re(r#"(?m)^\s*resource\s+"(?<type>[^"]+)"\s+"(?<name>[^"]+)"\s*\{"#));
static DATA: LazyLock<Regex> =
    LazyLock::new(|| re(r#"(?m)^\s*data\s+"(?<type>[^"]+)"\s+"(?<name>[^"]+)"\s*\{"#));
static MODULE: LazyLock<Regex> = LazyLock::new(|| re(r#"(?m)^\s*module\s+"(?<name>[^"]+)"\s*\{"#));
static VARIABLE: LazyLock<Regex> =
    LazyLock::new(|| re(r#"(?m)^\s*variable\s+"(?<name>[^"]+)"\s*\{"#));
static OUTPUT: LazyLock<Regex> = LazyLock::new(|| re(r#"(?m)^\s*output\s+"(?<name>[^"]+)"\s*\{"#));
static LOCALS: LazyLock<Regex> = LazyLock::new(|| re(r"(?m)^\s*locals\s*\{"));
static LOCAL_ASSIGNMENT: LazyLock<Regex> = LazyLock::new(|| re(r"(?m)^\s*(?<name>\w+)\s*="));
static TFVAR: LazyLock<Regex> = LazyLock::new(|| re(r"(?m)^\s*(?<name>\w+)\s*=\s*(?<value>.+)"));
static DEPENDS_ON: LazyLock<Regex> = LazyLock::new(|| re(r"depends_on\s*=\s*\[(?<deps>[^\]]*)\]"));
static RESOURCE_REF: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?<type>[a-z][a-z0-9_]+)\.(?<name>[a-z][a-z0-9_]+)"));
static MODULE_REF: LazyLock<Regex> = LazyLock::new(|| re(r"module\.(?<name>[a-z][a-z0-9_]+)"));
static STRING_LITERAL: LazyLock<Regex> =
    LazyLock::new(|| re(r#""(?<value>[^"\\]*(?:\\.[^"\\]*)*)""#));
static MAP_ENTRY: LazyLock<Regex> = LazyLock::new(|| re(r#"(?<key>\w+)\s*=\s*"(?<value>[^"]*?)""#));
static URL: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?i)https?://[\w\-\.]+(?::\d+)?(?:/[\w\-\./%]*)?"));
static HOST_PORT: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"(?i)(?<host>[\w\-\.]+\.(?:internal|local|corp|svc|cluster\.local|com|net|io))(?::(?<port>\d+))?",
    )
});
static CONNECTION_STRING: LazyLock<Regex> =
    LazyLock::new(|| re(r#"(?i)(?:Server|Data Source|Host|endpoint)=(?<host>[^;"]+)"#));

/// The retained per-key caches of `key = "value"` and `key = {` expressions.
static STRING_VALUE: LazyLock<Mutex<HashMap<String, Regex>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// `ExtractStringValue`: `key = "value"` or `key = value` on a line of `body`.
fn string_value(body: &str, key: &str) -> Option<String> {
    let r = {
        let mut cache = STRING_VALUE.lock().unwrap_or_else(|e| e.into_inner());
        cache
            .entry(key.to_owned())
            .or_insert_with(|| {
                re(&format!(
                    r#"(?:^|\n)\s*{}\s*=\s*"?(?<value>[^"\n]+?)"?\s*(?:\n|$)"#,
                    regex::escape(key)
                ))
            })
            .clone()
    };
    r.captures(body)
        .map(|c| dotnet_trim(&c["value"]).to_owned())
}

/// .NET `string.Trim()` (Unicode white space).
fn dotnet_trim(s: &str) -> &str {
    s.trim()
}

/// `ExtractBlockBody`: from the first `{` at or after `start`, the body up to the matching `}`
/// (strings and `#` comments skipped); the rest of the content when unbalanced.
fn block_body(content: &str, start: usize) -> &str {
    let b = content.as_bytes();
    let Some(open) = content[start.min(content.len())..]
        .find('{')
        .map(|i| i + start)
    else {
        // Empty, but still a slice of `content` (spans are computed from its position).
        return &content[content.len()..];
    };
    let mut depth = 1usize;
    let mut pos = open + 1;
    while pos < b.len() && depth > 0 {
        match b[pos] {
            b'{' => depth += 1,
            b'}' => depth -= 1,
            b'"' => {
                pos += 1;
                while pos < b.len() && b[pos] != b'"' {
                    if b[pos] == b'\\' {
                        pos += 1;
                    }
                    pos += 1;
                }
            }
            b'#' => {
                while pos < b.len() && b[pos] != b'\n' {
                    pos += 1;
                }
            }
            _ => {}
        }
        pos += 1;
    }
    if depth != 0 {
        return &content[open + 1..];
    }
    &content[open + 1..pos - 1]
}

/// `ExtractMapValues`: the `key = "value"` entries of the map assigned to `key`.
fn map_values(body: &str, key: &str) -> Vec<(String, String)> {
    let Ok(r) = Regex::new(&format!(r"{}\s*=\s*\{{", regex::escape(key))) else {
        return Vec::new();
    };
    let Some(m) = r.find(body) else {
        return Vec::new();
    };
    let map = block_body(body, m.end() - 1);
    MAP_ENTRY
        .captures_iter(map)
        .map(|c| (c["key"].to_owned(), c["value"].to_owned()))
        .collect()
}

/// The retained `provider`: the resource type up to its first `_`.
fn provider(t: &str) -> &str {
    t.split_once('_').map_or(t, |(p, _)| p)
}

pub(crate) fn extract(path: &str, source: &[u8], out: &mut Out<'_>) -> Result<(), ExtractFault> {
    out.text_scanner();
    let content = String::from_utf8_lossy(source);
    if content.trim().is_empty() {
        return Ok(());
    }
    let lines = Lines::new(&content);
    let mut p = Hcl {
        content: &content,
        lines: &lines,
        out,
        discovered: HashSet::new(),
    };
    if path.to_ascii_lowercase().ends_with(".tfvars") {
        p.tfvars()?;
    } else {
        p.resources()?;
        p.data_sources()?;
        p.modules()?;
        p.variables()?;
        p.outputs()?;
        p.locals()?;
    }
    p.scan_references(path)?;
    Ok(())
}

struct Hcl<'c, 'o, 'a> {
    content: &'c str,
    lines: &'c Lines,
    out: &'o mut Out<'a>,
    discovered: HashSet<String>,
}

impl Hcl<'_, '_, '_> {
    /// The span of a block from its match to its closing brace.
    fn block_span(&self, start: usize, body: &str) -> devworks_core::ports::indexing::Span {
        let body_start = body.as_ptr() as usize - self.content.as_ptr() as usize;
        let end = (body_start + body.len() + 1).min(self.content.len());
        // The match may begin with the white space before the keyword.
        let first = start
            + self.content[start..]
                .find(|c: char| !c.is_whitespace())
                .unwrap_or(0);
        self.lines.span(first, end)
    }

    fn resources(&mut self) -> Result<(), ExtractFault> {
        let content = self.content;
        for m in RESOURCE.captures_iter(content) {
            self.out.tick()?;
            let whole = m.get(0).expect("a match");
            let t = &m["type"];
            let n = &m["name"];
            let body = block_body(content, whole.start());
            let qn = format!("resource.{t}.{n}");
            let mut p = props(&[
                ("resource_type", json!(t)),
                ("provider", json!(provider(t))),
                ("confidence", json!("high")),
            ]);
            resource_properties(body, t, &mut p);
            let span = self.block_span(whole.start(), body);
            self.out
                .node("TerraformResource", &format!("{t}.{n}"), &qn, span, None, p);
            self.depends_on(body, &qn);
            self.cross_repo_edges(t, n, body, &qn);
        }
        Ok(())
    }

    fn data_sources(&mut self) -> Result<(), ExtractFault> {
        let content = self.content;
        for m in DATA.captures_iter(content) {
            self.out.tick()?;
            let whole = m.get(0).expect("a match");
            let (t, n) = (&m["type"], &m["name"]);
            let body = block_body(content, whole.start());
            let span = self.block_span(whole.start(), body);
            self.out.node(
                "TerraformDataSource",
                &format!("data.{t}.{n}"),
                &format!("data.{t}.{n}"),
                span,
                None,
                props(&[
                    ("data_type", json!(t)),
                    ("provider", json!(provider(t))),
                    ("confidence", json!("high")),
                ]),
            );
        }
        Ok(())
    }

    fn modules(&mut self) -> Result<(), ExtractFault> {
        let content = self.content;
        for m in MODULE.captures_iter(content) {
            self.out.tick()?;
            let whole = m.get(0).expect("a match");
            let name = &m["name"];
            let body = block_body(content, whole.start());
            let qn = format!("module.{name}");
            let source = string_value(body, "source");
            let version = string_value(body, "version");
            let mut p = props(&[("confidence", json!("high"))]);
            if let Some(s) = &source {
                p.insert("source".into(), json!(s));
            }
            if let Some(v) = &version {
                p.insert("version".into(), json!(v));
            }
            let span = self.block_span(whole.start(), body);
            self.out.node("TerraformModule", name, &qn, span, None, p);
            if let Some(source) = source {
                let local = source.trim_start_matches(['.', '/']);
                let target = if source.starts_with("./") || source.starts_with("../") {
                    format!("module_source.{}", sanitize(local))
                } else {
                    format!("terraform_registry:{source}")
                };
                self.out.edge(
                    "INCLUDES_MODULE",
                    &qn,
                    &target,
                    props(&[("source", json!(source))]),
                );
            }
            self.depends_on(body, &qn);
        }
        Ok(())
    }

    fn variables(&mut self) -> Result<(), ExtractFault> {
        let content = self.content;
        for m in VARIABLE.captures_iter(content) {
            self.out.tick()?;
            let whole = m.get(0).expect("a match");
            let name = &m["name"];
            let body = block_body(content, whole.start());
            let mut p = props(&[("confidence", json!("high"))]);
            for (key, prop) in [
                ("type", "type"),
                ("default", "default_value"),
                ("description", "description"),
            ] {
                if let Some(v) = string_value(body, key) {
                    p.insert(prop.into(), json!(v));
                }
            }
            let span = self.block_span(whole.start(), body);
            self.out.node(
                "TerraformVariable",
                name,
                &format!("var.{name}"),
                span,
                None,
                p,
            );
        }
        Ok(())
    }

    fn outputs(&mut self) -> Result<(), ExtractFault> {
        let content = self.content;
        for m in OUTPUT.captures_iter(content) {
            self.out.tick()?;
            let whole = m.get(0).expect("a match");
            let name = &m["name"];
            let body = block_body(content, whole.start());
            let qn = format!("output.{name}");
            let mut p = props(&[("confidence", json!("high"))]);
            let description = string_value(body, "description");
            let value = string_value(body, "value");
            if let Some(d) = &description {
                p.insert("description".into(), json!(d));
            }
            if let Some(v) = &value {
                p.insert("value_expression".into(), json!(v));
            }
            let span = self.block_span(whole.start(), body);
            self.out.node("TerraformOutput", name, &qn, span, None, p);
            if let Some(value) = value {
                for r in RESOURCE_REF.captures_iter(&value) {
                    let target = format!("resource.{}.{}", &r["type"], &r["name"]);
                    self.out.edge("CALLS", &qn, &target, Map::new());
                }
            }
        }
        Ok(())
    }

    fn locals(&mut self) -> Result<(), ExtractFault> {
        let content = self.content;
        for m in LOCALS.find_iter(content) {
            let body = block_body(content, m.start());
            let body_start = body.as_ptr() as usize - content.as_ptr() as usize;
            for a in LOCAL_ASSIGNMENT.captures_iter(body) {
                self.out.tick()?;
                let n = a.name("name").expect("a name");
                let name = n.as_str();
                let span = self
                    .lines
                    .span(body_start + n.start(), body_start + n.end());
                self.out.node(
                    "TerraformVariable",
                    name,
                    &format!("local.{name}"),
                    span,
                    None,
                    props(&[("scope", json!("local")), ("confidence", json!("high"))]),
                );
            }
        }
        Ok(())
    }

    fn tfvars(&mut self) -> Result<(), ExtractFault> {
        let content = self.content;
        for m in TFVAR.captures_iter(content) {
            self.out.tick()?;
            let n = m.name("name").expect("a name");
            let name = n.as_str();
            let value = dotnet_trim(&m["value"]).trim_matches('"');
            let mut p = props(&[("scope", json!("tfvar")), ("confidence", json!("high"))]);
            if utf16_len(value) <= 200 {
                p.insert("value_preview".into(), json!(value));
            }
            if value.contains("://")
                || value.contains(":5")
                || value.contains(":8")
                || value.contains(":3")
            {
                p.insert("likely_service_ref".into(), json!(true));
            }
            let whole = m.get(0).expect("a match");
            let span = self.lines.span(n.start(), whole.end());
            self.out.node(
                "TerraformVariable",
                name,
                &format!("tfvar.{name}"),
                span,
                None,
                p,
            );
        }
        Ok(())
    }

    /// `ExtractDependsOn`: the first `depends_on = [...]` of a block.
    fn depends_on(&mut self, body: &str, source: &str) {
        let Some(m) = DEPENDS_ON.captures(body) else {
            return;
        };
        let deps = &m["deps"];
        for r in RESOURCE_REF.captures_iter(deps) {
            let target = format!("resource.{}.{}", &r["type"], &r["name"]);
            self.out.edge("DEPENDS_ON", source, &target, Map::new());
        }
        for r in MODULE_REF.captures_iter(deps) {
            let target = format!("module.{}", &r["name"]);
            self.out.edge("DEPENDS_ON", source, &target, Map::new());
        }
    }

    /// `ExtractCrossRepoEdges`: deployment and configuration targets by resource type.
    fn cross_repo_edges(&mut self, t: &str, name: &str, body: &str, qn: &str) {
        let sv = |k: &str| string_value(body, k);
        let medium = json!("medium");
        let (kind, target, p): (&str, String, Map<String, Value>) =
            if matches!(t, "aws_ecs_service" | "aws_ecs_task_definition") {
                let image = sv("image");
                let container = sv("container_name")
                    .or_else(|| sv("name"))
                    .unwrap_or_else(|| name.to_owned());
                let mut p = props(&[("container", json!(container)), ("confidence", medium)]);
                if let Some(i) = image {
                    p.insert("image".into(), json!(i));
                }
                ("DEPLOYS", container, p)
            } else if t == "aws_lambda_function" {
                let f = sv("function_name").unwrap_or_else(|| name.to_owned());
                let p = props(&[("function_name", json!(f)), ("confidence", medium)]);
                ("DEPLOYS", f, p)
            } else if t == "aws_sqs_queue" {
                let q = sv("name").unwrap_or_else(|| name.to_owned());
                let p = props(&[("queue_name", json!(q)), ("confidence", medium)]);
                ("CONFIGURES", q, p)
            } else if t.contains("db_instance") || t.contains("rds") || t.contains("aurora") {
                let d = sv("db_name")
                    .or_else(|| sv("name"))
                    .unwrap_or_else(|| name.to_owned());
                let p = props(&[("database", json!(d)), ("confidence", medium)]);
                ("CONFIGURES", d, p)
            } else if t.contains("elasticache") {
                let c = sv("cluster_id")
                    .or_else(|| sv("replication_group_id"))
                    .unwrap_or_else(|| name.to_owned());
                let p = props(&[("cache_cluster", json!(c)), ("confidence", medium)]);
                ("CONFIGURES", c, p)
            } else if matches!(
                t,
                "aws_lb" | "aws_alb" | "aws_lb_target_group" | "aws_alb_target_group"
            ) {
                let l = sv("name").unwrap_or_else(|| name.to_owned());
                let p = props(&[("load_balancer", json!(l)), ("confidence", json!("low"))]);
                ("DEPLOYS", l, p)
            } else if t == "aws_s3_bucket" {
                let b = sv("bucket").unwrap_or_else(|| name.to_owned());
                let p = props(&[("bucket", json!(b)), ("confidence", medium)]);
                ("CONFIGURES", b, p)
            } else if t.contains("route53_record") {
                let d = sv("name").unwrap_or_else(|| name.to_owned());
                let p = props(&[("dns_record", json!(d)), ("confidence", medium)]);
                ("CONFIGURES", d, p)
            } else if t.contains("web_app") || t.contains("app_service") {
                let a = sv("name").unwrap_or_else(|| name.to_owned());
                let p = props(&[("app_service", json!(a)), ("confidence", medium)]);
                ("DEPLOYS", a, p)
            } else if t.contains("container_group") || t.contains("kubernetes") {
                let c = sv("name").unwrap_or_else(|| name.to_owned());
                let image = sv("image");
                let mut p = props(&[("container", json!(c)), ("confidence", medium)]);
                if let Some(i) = image {
                    p.insert("image".into(), json!(i));
                }
                ("DEPLOYS", c, p)
            } else {
                return;
            };
        self.out.name_edge(kind, qn, &target, p);
    }

    /// `ScanForReferences`: URLs, hosts and connection strings in every string literal.
    fn scan_references(&mut self, path: &str) -> Result<(), ExtractFault> {
        let parent = format!("file.{}", file_stem(path));
        self.out.alias(&parent, 0);
        let content = self.content;
        for m in STRING_LITERAL.captures_iter(content) {
            self.out.tick()?;
            let value = &m["value"];
            if value.trim().is_empty() {
                continue;
            }
            for u in URL.find_iter(value) {
                if self.discovered.insert(format!("url:{}", u.as_str())) {
                    self.out
                        .unresolved_call(&parent, u.as_str(), "terraform_url_ref", 0.4);
                }
            }
            for h in HOST_PORT.captures_iter(value) {
                let host = &h["host"];
                if self.discovered.insert(format!("host:{host}")) {
                    self.out
                        .unresolved_call(&parent, host, "terraform_host_ref", 0.3);
                }
            }
            for c in CONNECTION_STRING.captures_iter(value) {
                let host = &c["host"];
                if self.discovered.insert(format!("connstr:{host}")) {
                    self.out
                        .unresolved_call(&parent, host, "terraform_connection_ref", 0.5);
                }
            }
        }
        Ok(())
    }
}

/// `ExtractResourceProperties`: identifying properties of a resource body.
fn resource_properties(body: &str, t: &str, p: &mut Map<String, Value>) {
    if let Some(n) = string_value(body, "name") {
        p.insert("display_name".into(), json!(n));
    }
    // The retained map is case-insensitive: the last `Name` key (any case) wins.
    if let Some((_, v)) = map_values(body, "tags")
        .into_iter()
        .rev()
        .find(|(k, _)| k.eq_ignore_ascii_case("Name"))
    {
        p.insert("tag_name".into(), json!(v));
    }
    let mut put = |prop: &str, v: Option<String>| {
        if let Some(v) = v {
            p.insert(prop.into(), json!(v));
        }
    };
    if t.contains("ecs_service") || t.contains("ecs_task") {
        put("image", string_value(body, "image"));
        put("container_name", string_value(body, "container_name"));
    } else if t.contains("lambda") {
        put("handler", string_value(body, "handler"));
        put("runtime", string_value(body, "runtime"));
    } else if t.contains("db_instance") || t.contains("rds") {
        put("engine", string_value(body, "engine"));
        put(
            "database_name",
            string_value(body, "db_name").or_else(|| string_value(body, "name")),
        );
    } else if t.contains("sqs_queue") {
        put("queue_name", string_value(body, "name"));
    }
}

/// The retained `Sanitize` (lowercase; space, `-`, `/` and `.` to `_`).
fn sanitize(name: &str) -> String {
    name.to_lowercase()
        .chars()
        .map(|c| {
            if matches!(c, ' ' | '-' | '/' | '.') {
                '_'
            } else {
                c
            }
        })
        .collect()
}
