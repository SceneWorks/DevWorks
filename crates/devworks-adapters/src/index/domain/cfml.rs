//! ColdFusion: a port of the retained `ColdFusionParser` (a regex best-effort parser of `.cfm`
//! and `.cfc` files) with the same expressions: the `Component` of a `.cfc` (tag or CFScript,
//! `INHERITS` its `extends`), `Function`s (tag and CFScript, `DEFINES_METHOD` from the component),
//! `<cfinvoke>` calls, `createObject("component", ...)` uses, `<cfhttp>` outbound `Route`s with
//! `HTTP_CALLS`, `<cfquery>` table `QUERIES` and `<cfinclude>` `IMPORTS`.
use super::{Lines, Out, file_stem, props};
use devworks_core::ports::indexing::ExtractFault;
use regex::Regex;
use serde_json::json;
use std::sync::LazyLock;

fn re(p: &str) -> Regex {
    Regex::new(p).expect("a valid retained expression")
}
static COMPONENT_TAG: LazyLock<Regex> = LazyLock::new(|| re(r"(?is)<cfcomponent\b[^>]*>"));
static NAME_ATTRIBUTE: LazyLock<Regex> =
    LazyLock::new(|| re(r#"(?i)\bname\s*=\s*["'](?<value>[^"']+)["']"#));
static EXTENDS_ATTRIBUTE: LazyLock<Regex> =
    LazyLock::new(|| re(r#"(?i)\bextends\s*=\s*["'](?<value>[^"']+)["']"#));
static SCRIPT_COMPONENT: LazyLock<Regex> = LazyLock::new(|| {
    re(r#"(?im)^\s*component\b(?:\s+extends\s*=\s*["'](?<extends>[^"']+)["'])?\s*\{"#)
});
static FUNCTION_TAG: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r#"(?i)<cffunction\b[^>]*?\bname\s*=\s*["'](?<name>[^"']+)["'](?:[^>]*?\baccess\s*=\s*["'](?<access>[^"']+)["'])?(?:[^>]*?\breturntype\s*=\s*["'](?<returntype>[^"']+)["'])?[^>]*>"#,
    )
});
static SCRIPT_FUNCTION: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"(?i)(?:(?<access>public|private|remote|package)\s+)?(?:(?<returntype>\w+)\s+)?function\s+(?<name>\w+)\s*\(",
    )
});
static INVOKE: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r#"(?i)<cfinvoke\b[^>]*?\bcomponent\s*=\s*["'](?<component>[^"']+)["'][^>]*?\bmethod\s*=\s*["'](?<method>[^"']+)["'][^>]*>"#,
    )
});
static CREATE_OBJECT: LazyLock<Regex> = LazyLock::new(|| {
    re(r#"(?i)createObject\s*\(\s*["']component["']\s*,\s*["'](?<path>[^"']+)["']\s*\)"#)
});
static HTTP: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r#"(?i)<cfhttp\b[^>]*?\burl\s*=\s*["'](?<url>[^"']+)["'](?:[^>]*?\bmethod\s*=\s*["'](?<method>[^"']+)["'])?[^>]*>"#,
    )
});
static QUERY: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r#"(?i)<cfquery\b(?:[^>]*?\bname\s*=\s*["'](?<name>[^"']+)["'])?[^>]*>(?<sql>[\s\S]*?)</cfquery>"#,
    )
});
static TABLE_REFERENCE: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?i)(?:FROM|JOIN|INTO|UPDATE)\s+(?:\[?dbo\]?\.)?\[?(?<table>\w+)\]?"));
static INCLUDE: LazyLock<Regex> = LazyLock::new(|| {
    re(r#"(?i)<cfinclude\b[^>]*?\btemplate\s*=\s*["'](?<template>[^"']+)["'][^>]*>"#)
});

const SQL_KEYWORDS: &[&str] = &[
    "SELECT", "WHERE", "SET", "VALUES", "NULL", "NOT", "AND", "OR", "IN", "EXISTS", "CASE", "WHEN",
    "THEN", "ELSE", "END", "AS", "ON", "LEFT", "RIGHT", "INNER", "OUTER", "CROSS", "FULL",
];

pub(crate) fn extract(path: &str, source: &[u8], out: &mut Out<'_>) -> Result<(), ExtractFault> {
    out.text_scanner();
    let content = String::from_utf8_lossy(source);
    let lines = Lines::new(&content);
    let mut p = Cf {
        content: &content,
        lines: &lines,
        out,
        path,
    };
    let file_qn = p.file_qn();
    p.out.alias(&file_qn, 0);
    let component = if path.to_ascii_lowercase().ends_with(".cfc") {
        Some(p.component())
    } else {
        None
    };
    let component = component.as_deref();
    p.functions(component)?;
    p.invocations(component)?;
    p.http_calls(component)?;
    p.queries(component)?;
    p.includes(component)?;
    p.create_object(component)?;
    Ok(())
}

struct Cf<'c, 'o, 'a> {
    content: &'c str,
    lines: &'c Lines,
    out: &'o mut Out<'a>,
    path: &'c str,
}

impl Cf<'_, '_, '_> {
    /// The file's own qualified name (`BuildFileQualifiedName`: the path with `/` as `.`); it
    /// is the file node.
    fn file_qn(&self) -> String {
        self.path.replace(['/', '\\'], ".")
    }
    fn source_qn(&self, component: Option<&str>) -> String {
        component.map_or_else(|| self.file_qn(), str::to_owned)
    }

    /// `ExtractComponent`: the component's name and `extends`.
    fn component(&mut self) -> String {
        let content = self.content;
        let stem = file_stem(self.path).to_owned();
        let (name, extends) = if let Some(tag) = COMPONENT_TAG.find(content) {
            let tag = tag.as_str();
            (
                NAME_ATTRIBUTE
                    .captures(tag)
                    .map_or(stem, |c| c["value"].to_owned()),
                EXTENDS_ATTRIBUTE
                    .captures(tag)
                    .map(|c| c["value"].to_owned()),
            )
        } else if let Some(s) = SCRIPT_COMPONENT.captures(content) {
            (stem, s.name("extends").map(|e| e.as_str().to_owned()))
        } else {
            (stem, None)
        };
        let qn = name.clone();
        self.out.node(
            "Component",
            &name,
            &qn,
            self.lines.span(0, content.len()),
            None,
            props(&[
                ("confidence_band", json!("medium")),
                ("language", json!("coldfusion")),
            ]),
        );
        if let Some(e) = extends {
            self.out.edge(
                "INHERITS",
                &qn,
                &e,
                props(&[("confidence_band", json!("medium"))]),
            );
        }
        qn
    }

    fn function_qn(component: Option<&str>, name: &str) -> String {
        match component {
            Some(c) => format!("{c}.{name}"),
            None => name.to_owned(),
        }
    }

    fn functions(&mut self, component: Option<&str>) -> Result<(), ExtractFault> {
        let content = self.content;
        let parent = component.and_then(|c| self.out.index_of(c));
        for m in FUNCTION_TAG.captures_iter(content) {
            self.out.tick()?;
            let whole = m.get(0).expect("a match");
            let name = &m["name"];
            let access = m.name("access").map_or("public", |a| a.as_str());
            let returns = m.name("returntype").map_or("any", |a| a.as_str());
            let qn = Self::function_qn(component, name);
            self.out.node(
                "Function",
                name,
                &qn,
                self.lines.span(whole.start(), whole.end()),
                parent,
                props(&[
                    ("access", json!(access)),
                    ("return_type", json!(returns)),
                    ("confidence_band", json!("medium")),
                    ("language", json!("coldfusion")),
                ]),
            );
            if let Some(c) = component {
                self.out.contain("DEFINES_METHOD", c, &qn);
            }
        }
        for m in SCRIPT_FUNCTION.captures_iter(content) {
            self.out.tick()?;
            let whole = m.get(0).expect("a match");
            let name = &m["name"];
            let access = m.name("access").map_or("public", |a| a.as_str());
            let returns = m.name("returntype").map_or("any", |a| a.as_str());
            let qn = Self::function_qn(component, name);
            // Avoid duplicates if both tag and script forms appear.
            if self.out.has_qn(&qn) {
                continue;
            }
            self.out.node(
                "Function",
                name,
                &qn,
                self.lines.span(whole.start(), whole.end()),
                parent,
                props(&[
                    ("access", json!(access)),
                    ("return_type", json!(returns)),
                    ("confidence_band", json!("medium")),
                    ("language", json!("coldfusion")),
                    ("syntax", json!("cfscript")),
                ]),
            );
            if let Some(c) = component {
                self.out.contain("DEFINES_METHOD", c, &qn);
            }
        }
        Ok(())
    }

    fn invocations(&mut self, component: Option<&str>) -> Result<(), ExtractFault> {
        let source = self.source_qn(component);
        for m in INVOKE.captures_iter(self.content) {
            self.out.tick()?;
            let (c, method) = (&m["component"], &m["method"]);
            let target = format!("{c}.{method}");
            self.out.edge(
                "CALLS",
                &source,
                &target,
                props(&[("confidence_band", json!("medium"))]),
            );
            self.out.unresolved_call(&source, &target, c, 0.6);
        }
        Ok(())
    }

    fn create_object(&mut self, component: Option<&str>) -> Result<(), ExtractFault> {
        let source = self.source_qn(component);
        for m in CREATE_OBJECT.captures_iter(self.content) {
            self.out.tick()?;
            self.out.edge(
                "USES_TYPE",
                &source,
                &m["path"],
                props(&[("confidence_band", json!("medium"))]),
            );
        }
        Ok(())
    }

    fn http_calls(&mut self, component: Option<&str>) -> Result<(), ExtractFault> {
        let source = self.source_qn(component);
        let parent = component.and_then(|c| self.out.index_of(c));
        for m in HTTP.captures_iter(self.content) {
            self.out.tick()?;
            let whole = m.get(0).expect("a match");
            let url = &m["url"];
            let method = m
                .name("method")
                .map_or_else(|| "GET".to_owned(), |x| x.as_str().to_uppercase());
            let route = format!("{source}:HTTP:{method}:{url}");
            self.out.node(
                "Route",
                &format!("{method} {url}"),
                &route,
                self.lines.span(whole.start(), whole.end()),
                parent,
                props(&[
                    ("http_method", json!(method)),
                    ("url", json!(url)),
                    ("direction", json!("outbound")),
                    ("confidence_band", json!("medium")),
                    ("language", json!("coldfusion")),
                ]),
            );
            self.out.edge(
                "HTTP_CALLS",
                &source,
                &route,
                props(&[
                    ("http_method", json!(method)),
                    ("url", json!(url)),
                    ("confidence_band", json!("medium")),
                ]),
            );
        }
        Ok(())
    }

    fn queries(&mut self, component: Option<&str>) -> Result<(), ExtractFault> {
        let source = self.source_qn(component);
        for m in QUERY.captures_iter(self.content) {
            self.out.tick()?;
            let name = m.name("name").map_or("", |n| n.as_str());
            for t in TABLE_REFERENCE.captures_iter(&m["sql"]) {
                let table = &t["table"];
                if SQL_KEYWORDS.iter().any(|k| k.eq_ignore_ascii_case(table)) {
                    continue;
                }
                self.out.edge(
                    "QUERIES",
                    &source,
                    table,
                    props(&[
                        ("confidence_band", json!("low")),
                        ("query_name", json!(name)),
                        ("source", json!("cfquery")),
                    ]),
                );
            }
        }
        Ok(())
    }

    fn includes(&mut self, component: Option<&str>) -> Result<(), ExtractFault> {
        let source = self.source_qn(component);
        for m in INCLUDE.captures_iter(self.content) {
            self.out.tick()?;
            let template = &m["template"];
            let target = template.replace('/', ".");
            let target = target.trim_start_matches('.');
            self.out.edge(
                "IMPORTS",
                &source,
                target,
                props(&[
                    ("confidence_band", json!("medium")),
                    ("template_path", json!(template)),
                ]),
            );
        }
        Ok(())
    }
}
