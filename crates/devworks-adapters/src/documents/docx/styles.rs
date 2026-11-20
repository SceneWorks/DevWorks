use super::{
    DocxError, DocxLimits, WordList,
    numbering::Numbering,
    xml::{self, Element, WORD, WORD_STRICT, Xml},
};
use devworks_core::services::documents::extraction::ExtractionControl;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Default)]
pub(super) struct ParagraphStyle {
    pub name: Option<String>,
    pub outline: Option<u8>,
    pub number: Option<u32>,
    pub list_level: Option<u8>,
}

impl ParagraphStyle {
    pub(super) fn apply(&mut self, element: &Element<'_>, word: &str) -> Result<(), DocxError> {
        let value = element.attr(word, "val").ok_or(DocxError::InvalidStyles)?;
        match element.name.local {
            "pStyle" if !value.is_empty() && value.len() <= 128 => {
                self.name = Some(value.to_owned())
            }
            "outlineLvl" => self.outline = Some(number(value, 9)? as u8),
            "numId" => self.number = Some(number(value, i32::MAX as u32)?),
            "ilvl" => self.list_level = Some(number(value, 8)? as u8),
            _ => return Err(DocxError::InvalidStyles),
        }
        Ok(())
    }
}

pub(super) fn number(value: &str, max: u32) -> Result<u32, DocxError> {
    value
        .parse::<u32>()
        .ok()
        .filter(|value| *value <= max)
        .ok_or(DocxError::InvalidStyles)
}

#[derive(Default)]
struct Style {
    parent: Option<String>,
    properties: ParagraphStyle,
}

#[derive(Default)]
pub(super) struct Styles(BTreeMap<String, Style>);

impl Styles {
    pub(super) fn effective(
        &self,
        paragraph: &ParagraphStyle,
        definitions: &Numbering,
        steps: usize,
        control: &ExtractionControl<'_>,
    ) -> Result<(Option<u8>, Option<WordList>), DocxError> {
        let mut outline = paragraph.outline;
        let mut numbering = paragraph.number;
        let mut chain = Vec::new();
        let mut style_id = paragraph.name.as_deref();
        let mut visited = BTreeSet::new();
        while let Some(id) = style_id {
            control.check()?;
            if visited.len() == steps || !visited.insert(id) {
                return Err(DocxError::InvalidStyles);
            }
            chain.push(id);
            let Some(style) = self.0.get(id) else { break };
            outline = outline.or(style.properties.outline);
            numbering = numbering.or(style.properties.number);
            style_id = style.parent.as_deref();
        }
        let heading = match outline {
            Some(level @ 0..=8) => Some(level + 1),
            Some(_) => None,
            None => paragraph.name.as_deref().and_then(builtin_heading),
        };
        let list = numbering
            .filter(|number| *number != 0)
            .map(|numbering_id| WordList {
                numbering_id,
                level: paragraph
                    .list_level
                    .or_else(|| {
                        chain
                            .iter()
                            .find_map(|style| definitions.level_for(numbering_id, style))
                    })
                    .unwrap_or_default(),
            });
        Ok((heading, list))
    }
}

fn builtin_heading(name: &str) -> Option<u8> {
    name.to_ascii_lowercase()
        .strip_prefix("heading")?
        .trim()
        .parse::<u8>()
        .ok()
        .filter(|number| (1..=9).contains(number))
}

pub(super) fn read(
    bytes: &[u8],
    limits: &DocxLimits,
    control: &ExtractionControl<'_>,
) -> Result<Styles, DocxError> {
    let mut styles = Styles::default();
    let mut word = WORD;
    let mut style: Option<(String, Style)> = None;
    let mut properties = false;
    let mut list = false;
    xml::walk(bytes, limits.xml_depth, control, |depth, event| {
        match event {
            Xml::Start(element) if depth == 1 => {
                word = match element.name.ns {
                    WORD => WORD,
                    WORD_STRICT => WORD_STRICT,
                    _ => return Err(DocxError::InvalidStyles),
                };
                if element.name.local != "styles" {
                    return Err(DocxError::InvalidStyles);
                }
            }
            Xml::Start(element) if depth == 2 && element.name.is(word, "style") => {
                if element.attr(word, "type") == Some("paragraph") {
                    let id = element
                        .attr(word, "styleId")
                        .filter(|id| !id.is_empty() && id.len() <= 128)
                        .ok_or(DocxError::InvalidStyles)?;
                    style = Some((id.to_owned(), Style::default()));
                }
            }
            Xml::Start(element) if element.name.ns == word => {
                if let Some((_, style)) = style.as_mut() {
                    match (depth, element.name.local) {
                        (3, "basedOn") => {
                            let parent = element
                                .attr(word, "val")
                                .filter(|id| !id.is_empty() && id.len() <= 128)
                                .ok_or(DocxError::InvalidStyles)?;
                            style.parent = Some(parent.to_owned());
                        }
                        (3, "pPr") => properties = true,
                        (4, "numPr") if properties => list = true,
                        (4, "outlineLvl") if properties => {
                            style.properties.apply(&element, word)?
                        }
                        (5, "ilvl" | "numId") if list => style.properties.apply(&element, word)?,
                        _ => {}
                    }
                }
            }
            Xml::End if depth == 4 => list = false,
            Xml::End if depth == 3 => properties = false,
            Xml::End if depth == 2 => {
                if let Some((name, style)) = style.take()
                    && (styles.0.len() == limits.extraction.blocks
                        || styles.0.insert(name, style).is_some())
                {
                    return Err(DocxError::InvalidStyles);
                }
            }
            _ => {}
        }
        Ok(())
    })?;
    Ok(styles)
}
