//! Lossless property fragments; large strings and nested JSON are not replaced by previews.
use crate::protocol::Counter;
use crate::protocol::records::CodeProperty;
use crate::protocol::support::{BoundedList, Content, Rows};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const FRAGMENT_BYTES: usize = 8 * 1024;
const PAGE_BYTES: usize = 48 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct Position {
    pub index: usize,
    pub offset: usize,
}

pub(super) struct Properties {
    pub items: Rows<CodeProperty>,
    pub next: Option<Position>,
}

pub(super) fn page(value: &Value, after: Option<&Position>) -> Result<Properties, String> {
    let Some(object) = value.as_object() else {
        return Ok(Properties {
            items: BoundedList(Vec::new()),
            next: None,
        });
    };
    let mut keys: Vec<_> = object.keys().collect();
    keys.sort();
    let mut output = Vec::new();
    let mut used = 0;
    for (index, key) in keys.into_iter().enumerate() {
        if after.is_some_and(|p| index < p.index) {
            continue;
        }
        let json = serde_json::to_string(&object[key]).map_err(|e| e.to_string())?;
        let mut offset = after.filter(|p| p.index == index).map_or(0, |p| p.offset);
        if offset > json.len() || !json.is_char_boundary(offset) {
            return Err("invalid property cursor".into());
        }
        while offset < json.len() {
            if used >= PAGE_BYTES {
                return Ok(Properties {
                    items: BoundedList(output),
                    next: Some(Position { index, offset }),
                });
            }
            let mut end = json.len().min(offset + FRAGMENT_BYTES);
            while !json.is_char_boundary(end) {
                end -= 1;
            }
            let fragment = &json[offset..end];
            used += key.len() + fragment.len();
            output.push(CodeProperty {
                name: serde_json::from_value::<Content>(Value::String(key.clone()))
                    .map_err(|e| e.to_string())?,
                json_text: serde_json::from_value::<Content>(Value::String(fragment.to_owned()))
                    .map_err(|e| e.to_string())?,
                offset: Counter::new(offset as i64).map_err(|e| e.to_string())?,
                total_bytes: Counter::new(json.len() as i64).map_err(|e| e.to_string())?,
                next_offset: if end < json.len() {
                    Some(Counter::new(end as i64).map_err(|e| e.to_string())?)
                } else {
                    None
                },
            });
            offset = end;
        }
    }
    Ok(Properties {
        items: BoundedList(output),
        next: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn large_unicode_json_is_reconstructable_without_cutting_a_character() {
        let value = serde_json::json!({"large": "𐀀".repeat(65_536), "next": {"nested": [1, 2, 3]}});
        let mut after = None;
        let mut reconstructed = String::new();
        let mut seen_next = false;
        loop {
            let properties = page(&value, after.as_ref()).expect("property page");
            for property in properties.items.0 {
                let name = property.name.0;
                let text = property.json_text.0;
                if name == "large" {
                    assert_eq!(property.offset.get() as usize, reconstructed.len());
                    reconstructed.push_str(&text);
                } else {
                    seen_next = true;
                }
            }
            after = properties.next;
            if after.is_none() {
                break;
            }
        }
        assert_eq!(
            serde_json::from_str::<Value>(&reconstructed).unwrap(),
            value["large"]
        );
        assert!(seen_next);
    }
}
