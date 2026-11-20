use crate::indexstore::{Occurrence, Symbol};
use crate::source::Source;
use crate::workspace::Module;
use devworks_core::ports::indexing::{
    EdgeTarget, Enrichment, ExtractedEdge, ExtractedNode, Resolution, Span,
};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

const INVALID: &str = "swift-indexstore-invalid";
const DECLARATION: u64 = 1;
const DEFINITION: u64 = 1 << 1;
const REFERENCE: u64 = 1 << 2;
const CALL: u64 = 1 << 5;
const IMPLICIT: u64 = 1 << 8;
const CHILD_OF: u64 = 1 << 9;
const BASE_OF: u64 = 1 << 10;
const OVERRIDE_OF: u64 = 1 << 11;
const CALLED_BY: u64 = 1 << 13;
const EXTENDED_BY: u64 = 1 << 14;
const ACCESSOR_OF: u64 = 1 << 15;
const CONTAINED_BY: u64 = 1 << 16;

fn definition(item: &Occurrence) -> bool {
    item.roles & (DECLARATION | DEFINITION) != 0
        && !item.symbol.name.is_empty()
        && !item.symbol.usr.is_empty()
        && symbol_kind(&item.symbol).is_some()
}

fn symbol_kind(symbol: &Symbol) -> Option<&'static str> {
    Some(match symbol.kind {
        1 => "Module",
        2 => "Namespace",
        4 => "Macro",
        5 => "Enum",
        6 => "Struct",
        7 => "Class",
        8 => "Protocol",
        9 => "Extension",
        11 => "TypeAlias",
        12 => "Function",
        13 => "Variable",
        14 => "Field",
        15 => "EnumCase",
        16..=18 | 22..=24 => "Method",
        19..=21 => "Property",
        25 => "Parameter",
        _ => return None,
    })
}

fn owner(item: &Occurrence, flags: u64) -> Option<&Symbol> {
    item.relations
        .iter()
        .find(|(roles, symbol)| roles & flags != 0 && !symbol.usr.is_empty())
        .map(|(_, symbol)| symbol)
}

fn use_owner(item: &Occurrence) -> Option<&Symbol> {
    owner(item, CALLED_BY).or_else(|| owner(item, CONTAINED_BY))
}

fn metadata(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(fields) => fields,
        _ => Map::new(),
    }
}

struct FileSpan<'a> {
    text: &'a str,
    offsets: Vec<usize>,
}

impl<'a> FileSpan<'a> {
    fn new(text: &'a str) -> Self {
        let offsets = std::iter::once(0)
            .chain(
                text.bytes()
                    .enumerate()
                    .filter_map(|(offset, byte)| (byte == b'\n').then_some(offset + 1)),
            )
            .collect();
        Self { text, offsets }
    }

    fn full(&self) -> Span {
        let last = self.offsets.last().copied().unwrap_or(0);
        Span {
            start_line: 1,
            start_column: 1,
            start_byte: 0,
            end_line: self.offsets.len() as u32,
            end_column: (self.text.len() - last + 1) as u32,
            end_byte: self.text.len() as u32,
        }
    }

    fn at(&self, line: u32, column: u32) -> Result<Span, &'static str> {
        let row = line.checked_sub(1).ok_or(INVALID)? as usize;
        let start = *self.offsets.get(row).ok_or(INVALID)?;
        let end = self
            .offsets
            .get(row + 1)
            .map_or(self.text.len(), |start| start - 1);
        let offset = start + column.checked_sub(1).ok_or(INVALID)? as usize;
        let rest = self.text.get(offset..end).ok_or(INVALID)?;
        let first = rest.chars().next();
        let width = if let Some(text) = rest.strip_prefix('`') {
            text.find('`').map_or(1, |end| end + 2)
        } else if first.is_some_and(identifier) {
            rest.chars()
                .take_while(|character| identifier(*character))
                .map(char::len_utf8)
                .sum()
        } else if first.is_some_and(operator) {
            rest.chars()
                .take_while(|character| operator(*character))
                .map(char::len_utf8)
                .sum()
        } else {
            first.map_or(0, char::len_utf8)
        };
        Ok(Span {
            start_line: line,
            start_column: column,
            start_byte: offset as u32,
            end_line: line,
            end_column: column + width as u32,
            end_byte: (offset + width) as u32,
        })
    }
}

fn identifier(character: char) -> bool {
    character.is_ascii_alphanumeric()
        || character == '_'
        || !character.is_ascii() && !character.is_whitespace()
}

fn operator(character: char) -> bool {
    "/=-+!*%<>&|^~?.".contains(character)
}

#[derive(Clone)]
struct Definition {
    unit: String,
    index: usize,
    kind: String,
    qualified_name: String,
}

struct Graph<'a> {
    repository: &'a str,
    modules: BTreeMap<&'a str, &'a Module>,
    units: BTreeMap<String, Enrichment>,
    definitions: BTreeMap<String, Vec<Definition>>,
}

fn target(definition: &Definition, unit: &str) -> EdgeTarget {
    if definition.unit == unit {
        EdgeTarget::Local(definition.index)
    } else {
        EdgeTarget::Node {
            unit: definition.unit.clone(),
            kind: definition.kind.clone(),
            qualified_name: definition.qualified_name.clone(),
        }
    }
}

impl Graph<'_> {
    fn unique(&self, usr: &str) -> Option<&Definition> {
        self.definitions
            .get(usr)
            .filter(|definitions| definitions.len() == 1)
            .and_then(|items| items.first())
    }

    fn module_target(&self, symbol: &Symbol) -> Option<EdgeTarget> {
        let module = self.modules.get(symbol.name.as_str())?;
        let manifest = module.manifest.as_ref()?;
        Some(EdgeTarget::Node {
            unit: manifest.clone(),
            kind: "Module".into(),
            qualified_name: format!(
                "{}:{}#target:{}",
                self.repository, manifest, module.target_name
            ),
        })
    }

    fn edge(
        &self,
        unit: &str,
        source: usize,
        kind: &str,
        symbol: &Symbol,
        item: &Occurrence,
        span: Span,
    ) -> ExtractedEdge {
        let known = if kind == "IMPORTS" {
            self.module_target(symbol)
        } else {
            None
        }
        .or_else(|| {
            self.unique(&symbol.usr)
                .map(|definition| target(definition, unit))
        });
        let mut properties = metadata(json!({
            "semanticBasis": "swift-indexstore", "usr": symbol.usr,
            "displayName": symbol.name, "symbolKind":symbol.kind, "roles":item.roles,
            "useSpan":span,
        }));
        let (target, resolution) = if let Some(known) = known {
            (known, Resolution::Resolved)
        } else {
            properties.insert("compiler_bound_unpublished".into(), json!(true));
            let identity = if symbol.usr.is_empty() {
                &symbol.name
            } else {
                &symbol.usr
            };
            (EdgeTarget::Symbol(identity.clone()), Resolution::Unresolved)
        };
        ExtractedEdge {
            kind: kind.into(),
            source,
            target,
            resolution,
            confidence: 1.0,
            properties,
        }
    }
}

pub(crate) fn project(
    source: &Source,
    groups: &[(Module, BTreeMap<String, Vec<Occurrence>>)],
    gaps: &BTreeMap<String, String>,
) -> Result<Vec<Enrichment>, &'static str> {
    let mut graph = Graph {
        repository: &source.repository_id,
        modules: groups
            .iter()
            .map(|(module, _)| (module.name.as_str(), module))
            .collect(),
        units: BTreeMap::new(),
        definitions: BTreeMap::new(),
    };
    let files: BTreeMap<_, _> = source
        .files
        .iter()
        .filter(|file| file.language == "swift")
        .map(|file| (file.unit.as_str(), file))
        .collect();
    let mut by_path = BTreeMap::new();
    for (module, records) in groups {
        for path in &module.files {
            if by_path
                .insert(path.as_str(), (module, records.get(path)))
                .is_some()
            {
                return Err(INVALID);
            }
        }
    }
    for (&path, file) in &files {
        let spans = FileSpan::new(&file.text);
        let mut properties = metadata(
            json!({"sha256":file.sha256,"semanticBasis":"swift-indexstore",
            "indexStoreFormat":5,"indexStoreApi":16}),
        );
        if let Some(gap) = gaps.get(path) {
            properties.insert("coverageGap".into(), json!(gap));
        } else if !by_path.contains_key(path) {
            return Err(INVALID);
        }
        let mut unit = Enrichment {
            unit_key: path.into(),
            nodes: vec![ExtractedNode {
                kind: "File".into(),
                name: path.rsplit('/').next().ok_or(INVALID)?.into(),
                qualified_name: path.into(),
                span: spans.full(),
                parent: None,
                properties,
            }],
            edges: Vec::new(),
        };
        if let Some((module, Some(items))) = by_path.get(path) {
            let mut by_usr = BTreeMap::<&str, &Occurrence>::new();
            for item in items.iter().filter(|item| definition(item)) {
                if item.line == 0 || item.column == 0 {
                    if item.roles & IMPLICIT != 0 {
                        continue;
                    }
                    return Err(INVALID);
                }
                by_usr
                    .entry(&item.symbol.usr)
                    .and_modify(|previous| {
                        if item.roles & DEFINITION != 0 && previous.roles & DEFINITION == 0 {
                            *previous = item;
                        }
                    })
                    .or_insert(item);
            }
            let mut definitions: Vec<_> = by_usr.into_values().collect();
            definitions.sort_by(|a, b| {
                (a.line, a.column, &a.symbol.usr).cmp(&(b.line, b.column, &b.symbol.usr))
            });
            let mut locals = BTreeMap::<&str, usize>::new();
            for item in definitions {
                let symbol = &item.symbol;
                let name = symbol.name.split('(').next().unwrap_or(&symbol.name);
                let name = if name.is_empty() { &symbol.name } else { name };
                let kind = symbol_kind(symbol).ok_or(INVALID)?;
                let qualified_name = format!("swift:{}:{}", module.id, symbol.usr);
                if qualified_name.len() > 4096 || unit.nodes.len() >= 50_000 {
                    return Err("swift-output-budget");
                }
                let index = unit.nodes.len();
                let parent = owner(item, CHILD_OF | ACCESSOR_OF)
                    .and_then(|parent| locals.get(parent.usr.as_str()))
                    .copied()
                    .unwrap_or(0);
                unit.nodes.push(ExtractedNode {
                    kind: kind.into(), name: name.into(), qualified_name: qualified_name.clone(),
                    span: spans.at(item.line, item.column)?, parent: Some(parent),
                    properties: metadata(json!({
                        "semanticBasis":"swift-indexstore","usr": symbol.usr,"displayName":symbol.name,
                        "module":module.name,"moduleIdentity":module.id,"sourceDigest":file.sha256,
                        "roles":item.roles,"properties":symbol.properties,"subkind":symbol.subkind,
                        "compilerTokenSpan":true, "is_test":symbol.properties & (1 << 3) != 0,
                    })),
                });
                locals.insert(&symbol.usr, index);
                graph
                    .definitions
                    .entry(symbol.usr.clone())
                    .or_default()
                    .push(Definition {
                        unit: path.into(),
                        index,
                        kind: kind.into(),
                        qualified_name,
                    });
                unit.edges.push(ExtractedEdge {
                    kind: if kind == "Method" {
                        "DEFINES_METHOD"
                    } else {
                        "DEFINES"
                    }
                    .into(),
                    source: parent,
                    target: EdgeTarget::Local(index),
                    resolution: Resolution::Resolved,
                    confidence: 1.0,
                    properties: metadata(json!({"semanticBasis":"swift-indexstore"})),
                });
            }
        }
        graph.units.insert(path.into(), unit);
    }

    for (&path, (module, items)) in &by_path {
        let Some(items) = items else {
            continue;
        };
        let spans = FileSpan::new(&files.get(path).ok_or(INVALID)?.text);
        let mut edges = Vec::new();
        for item in *items {
            if item.line == 0 || item.column == 0 {
                if item.roles & IMPLICIT != 0 {
                    continue;
                }
                return Err(INVALID);
            }
            let span = spans.at(item.line, item.column)?;
            let anchor = |symbol: &Symbol| {
                graph
                    .unique(&symbol.usr)
                    .filter(|definition| definition.unit == path)
                    .map(|definition| definition.index)
            };
            if item.roles & REFERENCE != 0 && !item.symbol.name.is_empty() {
                let kind = if item.symbol.kind == 1 {
                    "IMPORTS"
                } else if item.roles & CALL != 0 {
                    "CALLS"
                } else {
                    "REFERENCES"
                };
                let origin = use_owner(item).and_then(anchor).unwrap_or(0);
                edges.push(graph.edge(path, origin, kind, &item.symbol, item, span));
            }
            for (roles, related) in &item.relations {
                let relation = if roles & BASE_OF != 0 {
                    Some((
                        anchor(related),
                        &item.symbol,
                        if item.symbol.kind == 8 {
                            "IMPLEMENTS"
                        } else {
                            "INHERITS"
                        },
                    ))
                } else if roles & EXTENDED_BY != 0 {
                    Some((anchor(related), &item.symbol, "EXTENDS"))
                } else if roles & OVERRIDE_OF != 0 && definition(item) {
                    Some((anchor(&item.symbol), related, "OVERRIDES"))
                } else if roles & (CHILD_OF | ACCESSOR_OF) != 0
                    && definition(item)
                    && let Some(origin) = anchor(&item.symbol)
                    && graph.units[path].nodes[origin].parent == Some(0)
                {
                    Some((Some(origin), related, "CHILD_OF"))
                } else {
                    None
                };
                if let Some((Some(origin), target, kind)) = relation
                    && !target.name.is_empty()
                {
                    edges.push(graph.edge(path, origin, kind, target, item, span));
                }
            }
            if edges.len() > 100_000 {
                return Err("swift-output-budget");
            }
        }
        let unit = graph.units.get_mut(path).ok_or(INVALID)?;
        unit.nodes[0]
            .properties
            .insert("module".into(), json!(module.name));
        if unit.edges.len() + edges.len() > 100_000 {
            return Err("swift-output-budget");
        }
        unit.edges.extend(edges);
    }
    Ok(graph.units.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swift_compiler_spans_use_utf8_bytes_and_captured_lines() {
        let text = "// café 🚀\r\nfunc naïve() {}\r\n";
        let map = FileSpan::new(text);
        let span = map.at(2, 6).unwrap();
        assert_eq!(
            &text[span.start_byte as usize..span.end_byte as usize],
            "naïve"
        );
        assert_eq!(span.end_column, 12);
        assert!(map.at(50, 1).is_err());
        assert!(map.at(2, 9).is_err()); // middle of the UTF-8 sequence for ï
    }
}
