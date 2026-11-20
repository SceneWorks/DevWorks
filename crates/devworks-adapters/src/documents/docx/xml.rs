use super::DocxError;
use devworks_core::services::documents::extraction::{ExtractionControl, decode_text};
use quick_xml::{
    NsReader, XmlVersion,
    events::Event,
    name::{NamespaceResolver, QName, ResolveResult},
};
use std::{borrow::Cow, collections::BTreeSet};

pub(super) const TYPES: &str = "http://schemas.openxmlformats.org/package/2006/content-types";
pub(super) const OPC_RELS: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
pub(super) const WORD: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
pub(super) const WORD_STRICT: &str = "http://purl.oclc.org/ooxml/wordprocessingml/main";
pub(super) const REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
pub(super) const REL_STRICT: &str = "http://purl.oclc.org/ooxml/officeDocument/relationships";

#[derive(Clone, Copy, PartialOrd, Ord, PartialEq, Eq)]
pub(super) struct Name<'a> {
    pub ns: &'a str,
    pub local: &'a str,
}

impl Name<'_> {
    pub(super) fn is(self, ns: &str, local: &str) -> bool {
        self.ns == ns && self.local == local
    }
}

pub(super) struct Element<'a> {
    pub name: Name<'a>,
    attributes: Vec<(Name<'a>, Cow<'a, str>)>,
}

impl Element<'_> {
    pub(super) fn attr(&self, ns: &str, local: &str) -> Option<&str> {
        self.attributes
            .iter()
            .find(|(name, _)| name.is(ns, local))
            .map(|(_, value)| value.as_ref())
    }
}

pub(super) enum Xml<'a> {
    Start(Element<'a>),
    End,
    Text(Cow<'a, str>),
}

pub(super) fn walk(
    bytes: &[u8],
    depth_limit: usize,
    control: &ExtractionControl<'_>,
    mut visit: impl FnMut(usize, Xml<'_>) -> Result<(), DocxError>,
) -> Result<(), DocxError> {
    use DocxError::MalformedXml as Bad;
    let decoded = decode_text(bytes, control).map_err(|err| match err {
        devworks_core::services::documents::extraction::ExtractionError::Cancelled
        | devworks_core::services::documents::extraction::ExtractionError::Deadline => err.into(),
        _ => Bad,
    })?;
    if decoded.chars().any(|c| !xml_char(c)) {
        return Err(Bad);
    }
    let mut reader = NsReader::from_str(&decoded);
    reader.config_mut().expand_empty_elements = true;
    reader.config_mut().check_comments = true;
    let mut at_start = true;
    let mut depth = 0usize;
    let mut root_seen = false;
    loop {
        control.check()?;
        let event = reader.read_event().map_err(|_| Bad)?;
        let first = std::mem::replace(&mut at_start, false);
        match event {
            Event::Start(start) => {
                if depth == 0 && std::mem::replace(&mut root_seen, true) {
                    return Err(Bad);
                }
                depth += 1;
                if depth > depth_limit {
                    return Err(DocxError::XmlDepthLimit);
                }
                let resolver = reader.resolver();
                let name = resolve(resolver, start.name(), false)?;
                let mut seen = BTreeSet::new();
                let mut attributes = Vec::new();
                for (attribute_index, item) in start.attributes().enumerate() {
                    control.check()?;
                    if attribute_index == 128 {
                        return Err(DocxError::XmlAttributeLimit);
                    }
                    let item = item.map_err(|_| Bad)?;
                    let attr_name = item.key.as_ref();
                    if attr_name == b"xmlns" || attr_name.starts_with(b"xmlns:") {
                        continue;
                    }
                    let key = resolve(resolver, item.key, true)?;
                    if !seen.insert(key) {
                        return Err(Bad);
                    }
                    let value = item
                        .decoded_and_normalized_value(XmlVersion::Implicit1_0, reader.decoder())
                        .map_err(|_| Bad)?;
                    if value.chars().any(|scalar| !xml_char(scalar)) {
                        return Err(Bad);
                    }
                    attributes.push((key, value));
                }
                visit(depth, Xml::Start(Element { name, attributes }))?;
            }
            Event::End(_) => {
                if depth == 0 {
                    return Err(Bad);
                }
                visit(depth, Xml::End)?;
                depth -= 1;
            }
            Event::Text(text) => {
                let text = text.xml10_content().map_err(|_| Bad)?;
                if depth == 0 && text.chars().all(xml_space) {
                    continue;
                }
                if depth == 0 {
                    return Err(Bad);
                }
                visit(depth, Xml::Text(text))?;
            }
            Event::CData(text) if depth > 0 => {
                visit(depth, Xml::Text(text.xml10_content().map_err(|_| Bad)?))?;
            }
            Event::GeneralRef(reference) if depth > 0 => {
                let name = reference.xml10_content().map_err(|_| Bad)?;
                if name.len() > 16 {
                    return Err(DocxError::ForbiddenXml);
                }
                let entity = format!("&{name};");
                let decoded =
                    quick_xml::escape::unescape(&entity).map_err(|_| DocxError::ForbiddenXml)?;
                if decoded.chars().any(|scalar| !xml_char(scalar)) {
                    return Err(Bad);
                }
                visit(depth, Xml::Text(decoded))?;
            }
            Event::Eof if root_seen && depth == 0 => return control.check().map_err(Into::into),
            Event::DocType(_) => return Err(DocxError::ForbiddenXml),
            Event::Decl(decl) if first => {
                if decl.version().map_err(|_| Bad)?.as_ref() != b"1.0" {
                    return Err(Bad);
                }
                if let Some(encoding) = decl.encoding() {
                    let encoding = encoding.map_err(|_| Bad)?;
                    let utf16 =
                        bytes.starts_with(&[0xfe, 0xff]) || bytes.starts_with(&[0xff, 0xfe]);
                    let expected = if utf16 {
                        b"utf-16".as_slice()
                    } else {
                        b"utf-8"
                    };
                    if !encoding.eq_ignore_ascii_case(expected)
                        && !(bytes.starts_with(&[0xff, 0xfe])
                            && encoding.eq_ignore_ascii_case(b"utf-16le"))
                        && !(bytes.starts_with(&[0xfe, 0xff])
                            && encoding.eq_ignore_ascii_case(b"utf-16be"))
                    {
                        return Err(Bad);
                    }
                }
            }
            Event::PI(_) | Event::Comment(_) => {}
            _ => return Err(Bad),
        }
    }
}

fn resolve<'a>(
    resolver: &'a NamespaceResolver,
    name: QName<'a>,
    attribute: bool,
) -> Result<Name<'a>, DocxError> {
    let (namespace, local) = if attribute {
        resolver.resolve_attribute(name)
    } else {
        resolver.resolve_element(name)
    };
    let ns = match namespace {
        ResolveResult::Unbound => "",
        ResolveResult::Bound(namespace) => {
            std::str::from_utf8(namespace.into_inner()).map_err(|_| DocxError::MalformedXml)?
        }
        ResolveResult::Unknown(_) => return Err(DocxError::MalformedXml),
    };
    let local = std::str::from_utf8(local.into_inner()).map_err(|_| DocxError::MalformedXml)?;
    Ok(Name { ns, local })
}

fn xml_space(scalar: char) -> bool {
    matches!(scalar, '\t' | '\n' | '\r' | ' ')
}

fn xml_char(scalar: char) -> bool {
    matches!(scalar, '\t' | '\n' | '\r' | ' '..='\u{d7ff}' | '\u{e000}'..='\u{fffd}' | '\u{10000}'..='\u{10ffff}')
}
