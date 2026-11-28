use super::{
    DocxError, DocxLimits,
    xml::{self, Element, WORD, WORD_STRICT, Xml},
};
use devworks_core::services::documents::extraction::ExtractionControl;
use std::collections::BTreeMap;

#[derive(Default)]
pub(super) struct Numbering {
    instances: BTreeMap<u32, u32>,
    abstract_levels: BTreeMap<(u32, u8), String>,
    instance_levels: BTreeMap<(u32, u8), Option<String>>,
}

impl Numbering {
    pub(super) fn level_for(&self, instance: u32, style: &str) -> Option<u8> {
        let abstract_id = *self.instances.get(&instance)?;
        (0..=8).find(|&level| {
            let association = self.instance_levels.get(&(instance, level)).map_or_else(
                || self.abstract_levels.get(&(abstract_id, level)),
                Option::as_ref,
            );
            association.is_some_and(|candidate| candidate == style)
        })
    }
}

#[derive(Clone, Copy)]
enum Owner {
    Abstract(u32),
    Instance(u32),
}

pub(super) fn read(
    bytes: &[u8],
    limits: &DocxLimits,
    control: &ExtractionControl<'_>,
) -> Result<Numbering, DocxError> {
    let mut result = Numbering::default();
    let mut word = WORD;
    let mut owner = None;
    let mut level = None;
    let mut override_level = None;
    let mut records = 0;
    xml::walk(bytes, limits.xml_depth, control, |depth, event| {
        match event {
            Xml::Start(root) if depth == 1 => {
                word = match root.name.ns {
                    WORD => WORD,
                    WORD_STRICT => WORD_STRICT,
                    _ => return Err(DocxError::InvalidNumbering),
                };
                if root.name.local != "numbering" {
                    return Err(DocxError::InvalidNumbering);
                }
            }
            Xml::Start(element) if element.name.ns == word => {
                match (owner, depth, element.name.local) {
                    (_, 2, "abstractNum") => {
                        owner = Some(Owner::Abstract(value(
                            &element,
                            word,
                            "abstractNumId",
                            i32::MAX as u32,
                        )?));
                    }
                    (_, 2, "num") => {
                        owner = Some(Owner::Instance(value(
                            &element,
                            word,
                            "numId",
                            i32::MAX as u32,
                        )?));
                    }
                    (Some(Owner::Instance(id)), 3, "abstractNumId") => {
                        let abstract_id = value(&element, word, "val", i32::MAX as u32)?;
                        if result.instances.insert(id, abstract_id).is_some() {
                            return Err(DocxError::InvalidNumbering);
                        }
                    }
                    (Some(Owner::Abstract(_)), 3, "lvl") => {
                        level = Some(value(&element, word, "ilvl", 8)? as u8);
                    }
                    (Some(Owner::Instance(_)), 3, "lvlOverride") => {
                        override_level = Some(value(&element, word, "ilvl", 8)? as u8);
                    }
                    (Some(Owner::Instance(id)), 4, "lvl") if override_level.is_some() => {
                        let slot = value(&element, word, "ilvl", 8)? as u8;
                        if Some(slot) != override_level
                            || result.instance_levels.insert((id, slot), None).is_some()
                        {
                            return Err(DocxError::InvalidNumbering);
                        }
                        level = Some(slot);
                    }
                    (Some(Owner::Abstract(id)), 4, "pStyle") if let Some(level) = level => {
                        if result
                            .abstract_levels
                            .insert((id, level), style(&element, word)?)
                            .is_some()
                        {
                            return Err(DocxError::InvalidNumbering);
                        }
                    }
                    (Some(Owner::Instance(id)), 5, "pStyle") if let Some(level) = level => {
                        let entry = result
                            .instance_levels
                            .get_mut(&(id, level))
                            .ok_or(DocxError::InvalidNumbering)?;
                        if entry.replace(style(&element, word)?).is_some() {
                            return Err(DocxError::InvalidNumbering);
                        }
                    }
                    _ => return Ok(()),
                }
                records += 1;
                if records > limits.extraction.blocks {
                    return Err(DocxError::StructureLimit);
                }
            }
            Xml::End if depth == 2 => owner = None,
            Xml::End if depth == 3 => {
                level = None;
                override_level = None;
            }
            Xml::End if depth == 4 && override_level.is_some() => level = None,
            _ => {}
        }
        Ok(())
    })?;
    Ok(result)
}

fn value(element: &Element<'_>, word: &str, name: &str, max: u32) -> Result<u32, DocxError> {
    element
        .attr(word, name)
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|value| *value <= max)
        .ok_or(DocxError::InvalidNumbering)
}

fn style(element: &Element<'_>, word: &str) -> Result<String, DocxError> {
    element
        .attr(word, "val")
        .filter(|id| !id.is_empty() && id.len() <= 128)
        .map(str::to_owned)
        .ok_or(DocxError::InvalidNumbering)
}
