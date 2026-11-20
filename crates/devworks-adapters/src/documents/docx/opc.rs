use super::{
    DocxError, DocxLimits,
    package::{Package, part_name, relationship_part, resolve_part},
    xml::{self, OPC_RELS, REL, REL_STRICT, TYPES, Xml},
};
use devworks_core::services::documents::extraction::ExtractionControl;
use std::collections::BTreeMap;

const MAIN: &str =
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml";
const RELS: &str = "application/vnd.openxmlformats-package.relationships+xml";
const STYLES: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml";

pub(super) struct Manifest {
    defaults: BTreeMap<String, String>,
    overrides: BTreeMap<String, String>,
}

impl Manifest {
    pub(super) fn read(
        package: &mut Package<'_>,
        limits: &DocxLimits,
        control: &ExtractionControl<'_>,
    ) -> Result<Self, DocxError> {
        use DocxError::InvalidContentTypes as Bad;
        let raw = package.read("[content_types].xml", control)?;
        let mut manifest = Self {
            defaults: BTreeMap::new(),
            overrides: BTreeMap::new(),
        };
        xml::walk(&raw, limits.xml_depth, control, |depth, item| {
            match item {
                Xml::Start(root) if depth == 1 && root.name.is(TYPES, "Types") => {}
                Xml::Start(element) if depth == 2 && element.name.ns == TYPES => {
                    let media = element
                        .attr("", "ContentType")
                        .filter(|media| {
                            media.len() <= 256 && media.contains('/') && media.is_ascii()
                        })
                        .ok_or(Bad)?
                        .to_ascii_lowercase();
                    if media.contains("macroenabled") || media.contains("vbaproject") {
                        return Err(DocxError::UnsupportedVariant);
                    }
                    let replaced = match element.name.local {
                        "Default" => {
                            let ext = element
                                .attr("", "Extension")
                                .filter(|ext| {
                                    !ext.is_empty()
                                        && ext.len() <= 64
                                        && ext.bytes().all(|byte| {
                                            byte.is_ascii_alphanumeric()
                                                || matches!(byte, b'-' | b'_')
                                        })
                                })
                                .ok_or(Bad)?;
                            manifest.defaults.insert(ext.to_ascii_lowercase(), media)
                        }
                        "Override" => {
                            let raw = element
                                .attr("", "PartName")
                                .and_then(|name| name.strip_prefix('/'))
                                .ok_or(Bad)?;
                            let name = part_name(raw)?;
                            if !package.names.contains_key(&name) {
                                return Err(Bad);
                            }
                            manifest.overrides.insert(name, media)
                        }
                        _ => return Err(Bad),
                    };
                    if replaced.is_some() {
                        return Err(Bad);
                    }
                }
                Xml::End => {}
                Xml::Text(text) if text.trim().is_empty() => {}
                _ => return Err(Bad),
            }
            Ok(())
        })?;
        for name in package.names.keys() {
            if name != "[content_types].xml" && manifest.media(name).is_none() {
                return Err(Bad);
            }
        }
        Ok(manifest)
    }

    fn media(&self, name: &str) -> Option<&str> {
        self.overrides
            .get(name)
            .or_else(|| {
                name.rsplit_once('.')
                    .and_then(|(_, extension)| self.defaults.get(extension))
            })
            .map(String::as_str)
    }

    pub(super) fn expected(&self, name: &str, expected: &str) -> Result<(), DocxError> {
        let media = self.media(name);
        if media.is_some_and(|media| media == expected) {
            Ok(())
        } else {
            Err(DocxError::InvalidContentTypes)
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Target {
    Internal(String),
    External(String),
}

pub(super) struct Relationship {
    pub kind: String,
    pub target: Target,
}

pub(super) type Relationships = BTreeMap<String, Relationship>;

pub(super) fn read_relationships(
    package: &mut Package<'_>,
    manifest: &Manifest,
    source: &str,
    limits: &DocxLimits,
    control: &ExtractionControl<'_>,
) -> Result<Relationships, DocxError> {
    use DocxError::InvalidRelationships as Bad;
    let name = relationship_part(source);
    if !package.names.contains_key(&name) && !source.is_empty() {
        return Ok(BTreeMap::new());
    }
    manifest.expected(&name, RELS)?;
    let raw = package.read(&name, control)?;
    let mut output = BTreeMap::new();
    xml::walk(&raw, limits.xml_depth, control, |depth, item| {
        match item {
            Xml::Start(root) if depth == 1 && root.name.is(OPC_RELS, "Relationships") => {}
            Xml::Start(row) if depth == 2 && row.name.is(OPC_RELS, "Relationship") => {
                let id = row
                    .attr("", "Id")
                    .filter(|id| !id.is_empty() && id.len() <= 128)
                    .ok_or(Bad)?;
                let kind = row
                    .attr("", "Type")
                    .filter(|kind| !kind.is_empty() && kind.len() <= 512)
                    .ok_or(Bad)?;
                let target = row
                    .attr("", "Target")
                    .filter(|target| !target.is_empty() && target.len() <= 4096)
                    .ok_or(Bad)?;
                let target = match row.attr("", "TargetMode") {
                    Some("External") => Target::External(target.to_owned()),
                    None | Some("Internal") => Target::Internal(resolve_part(source, target)?),
                    _ => return Err(Bad),
                };
                if output.len() == limits.parts
                    || output
                        .insert(
                            id.to_owned(),
                            Relationship {
                                kind: kind.to_owned(),
                                target,
                            },
                        )
                        .is_some()
                {
                    return Err(Bad);
                }
            }
            Xml::End => {}
            Xml::Text(text) if text.trim().is_empty() => {}
            _ => return Err(Bad),
        }
        Ok(())
    })?;
    Ok(output)
}

pub(super) fn is_kind(kind: &str, expected: &str) -> bool {
    [REL, REL_STRICT].iter().any(|namespace| {
        kind.strip_prefix(namespace)
            .and_then(|tail| tail.strip_prefix('/'))
            == Some(expected)
    })
}

pub(super) fn unique_internal<'a>(
    relationships: &'a Relationships,
    kind: &str,
) -> Result<Option<&'a str>, DocxError> {
    let mut matching = relationships
        .values()
        .filter(|item| is_kind(&item.kind, kind));
    match (matching.next(), matching.next()) {
        (None, None) => Ok(None),
        (
            Some(Relationship {
                target: Target::Internal(name),
                ..
            }),
            None,
        ) => Ok(Some(name)),
        _ => Err(DocxError::InvalidRelationships),
    }
}

pub(super) fn main_part<'a>(
    manifest: &Manifest,
    root: &'a Relationships,
) -> Result<&'a str, DocxError> {
    let name = unique_internal(root, "officeDocument")?.ok_or(DocxError::MissingPart)?;
    manifest.expected(name, MAIN)?;
    Ok(name)
}

pub(super) fn styles_part<'a>(
    manifest: &Manifest,
    main: &'a Relationships,
) -> Result<Option<&'a str>, DocxError> {
    let name = unique_internal(main, "styles")?;
    if let Some(name) = name {
        manifest.expected(name, STYLES)?;
    }
    Ok(name)
}
