//! The SystemOne JSON wire. Model selection, credentials and dispatch belong to the router.
use super::{ProviderError, materialize::Materials};
use crate::operations::router::DecideInput;
use crate::ports::packages::SchemaValidator;
use crate::protocol::provider::SchemaRef;
use crate::protocol::records::{DecisionQuestion, MediaRef};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Map, Value, json};
use std::collections::BTreeSet;

/// Limit of the retained simple-router request, including base64 expansion.
pub const MAX_REQUEST_BYTES: usize = 40_000_000;
/// The result must also fit the native bounded JSON field.
pub const MAX_RESPONSE_BYTES: usize = 512 * 1024;
pub const MAX_TIMEOUT_MS: u64 = 120_000;

/// The built-in result envelope. Per-question compatibility is checked against the request.
pub fn result_document() -> Value {
    json!({
        "type":"object",
        "required":["model","answers"],
        "properties":{
            "model":{"type":"string","minLength":1,"maxLength":4096},
            "answers":{"type":"object","minProperties":1,"additionalProperties":{"type":"object","required":["type"],"properties":{"type":{"type":"string","minLength":1}}}},
            "usage":{"type":"object"}
        }
    })
}

pub fn result_reference() -> SchemaRef {
    let canonical = crate::jcs::canonicalize(&result_document()).expect("fixed decision schema");
    SchemaRef {
        schema_id: "systemone/result"
            .to_owned()
            .try_into()
            .expect("fixed schema ID"),
        revision: crate::protocol::Revision::new(1).expect("fixed schema revision"),
        sha256: crate::fingerprint::sha256_hex(canonical.as_bytes())
            .try_into()
            .expect("schema digest"),
    }
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Requirements {
    pub question_types: BTreeSet<String>,
    pub extension_schemas: Vec<SchemaRef>,
    pub images: bool,
    pub videos: bool,
    pub question_count: usize,
    pub bytes: usize,
    pub batch_bytes: usize,
}

pub fn media_supported(media_type: &str, video: bool) -> bool {
    if video {
        matches!(
            media_type,
            "video/mp4" | "video/webm" | "video/quicktime" | "video/x-matroska"
        )
    } else {
        matches!(media_type, "image/png" | "image/jpeg" | "image/webp")
    }
}

fn http_url(raw: &str) -> bool {
    url::Url::parse(raw).is_ok_and(|url| {
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
    })
}

fn check_nested(value: &Value, requirements: &mut Requirements) -> Result<(), ProviderError> {
    match value {
        Value::Object(fields) => {
            if let Some(kind @ ("image_url" | "video_url")) =
                fields.get("type").and_then(Value::as_str)
            {
                let video = kind == "video_url";
                let url = fields
                    .get(kind)
                    .and_then(|source| source.get("url"))
                    .and_then(Value::as_str)
                    .ok_or(ProviderError::InvalidInput("decision_media_source"))?;
                let data = url
                    .strip_prefix("data:")
                    .and_then(|data| data.split_once(";base64,"));
                if !http_url(url)
                    && !data.is_some_and(|(media, bytes)| {
                        media_supported(media, video) && STANDARD.decode(bytes).is_ok()
                    })
                {
                    return Err(ProviderError::InvalidInput("decision_media_url"));
                }
                requirements.images |= !video;
                requirements.videos |= video;
            }
            for child in fields.values() {
                check_nested(child, requirements)?;
            }
        }
        Value::Array(items) => {
            for child in items {
                check_nested(child, requirements)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn instructions(
    text: &str,
    parts: Option<&crate::protocol::support::DynamicJson>,
) -> Result<Value, ProviderError> {
    let Some(parts) = parts else {
        return Ok(json!(text));
    };
    let parts = parts
        .0
        .as_array()
        .filter(|items| !items.is_empty())
        .ok_or(ProviderError::InvalidInput("decision_instruction_parts"))?;
    let mut combined = Vec::with_capacity(parts.len() + usize::from(!text.is_empty()));
    if !text.is_empty() {
        combined.push(json!(text));
    }
    combined.extend(parts.iter().cloned());
    Ok(Value::Array(combined))
}

fn questions(input: &DecideInput, needs: &mut Requirements) -> Result<Value, ProviderError> {
    let mut items = Map::new();
    for entry in &input.questions.0 {
        let (kind, question) = match &entry.question {
            DecisionQuestion::Choice {
                instructions: text,
                criteria,
                instruction_parts,
            } => {
                let criteria = criteria
                    .0
                    .iter()
                    .map(|c| {
                        (
                            c.name.0.clone(),
                            json!(c.description.as_ref().map(|d| &d.0)),
                        )
                    })
                    .collect::<Map<_, _>>();
                (
                    "choice",
                    json!({"type":"choice", "instructions": instructions(&text.0, instruction_parts.as_ref())?, "criteria": criteria}),
                )
            }
            DecisionQuestion::Noul {
                instructions: text,
                instruction_parts,
            } => (
                "noul",
                json!({"type":"noul", "instructions": instructions(&text.0, instruction_parts.as_ref())?}),
            ),
            DecisionQuestion::Score {
                instructions: text,
                criteria,
                instruction_parts,
            } => (
                "score",
                json!({"type":"score", "instructions": instructions(&text.0, instruction_parts.as_ref())?, "criteria": criteria}),
            ),
            DecisionQuestion::Box {
                instructions: text,
                instruction_parts,
            } => (
                "box",
                json!({"type":"box", "instructions": instructions(&text.0, instruction_parts.as_ref())?}),
            ),
            DecisionQuestion::Extension { schema, payload } => {
                payload
                    .0
                    .get("type")
                    .and_then(Value::as_str)
                    .filter(|kind| {
                        !kind.is_empty() && !matches!(*kind, "choice" | "noul" | "score" | "box")
                    })
                    .ok_or(ProviderError::InvalidInput("decision_extension_type"))?;
                needs.extension_schemas.push(schema.clone());
                ("extension", payload.0.clone())
            }
        };
        if items.insert(entry.name.0.clone(), question).is_some() {
            return Err(ProviderError::InvalidInput("duplicate_decision_question"));
        }
        if kind != "extension" {
            needs.question_types.insert(kind.to_owned());
        }
    }
    needs.question_count = items.len();
    Ok(Value::Object(items))
}

fn media_length(media: &MediaRef, video: bool) -> Result<usize, ProviderError> {
    match media {
        MediaRef::Remote { url, media_type }
            if media_supported(&media_type.0, video) && http_url(&url.0) =>
        {
            Ok(url.0.len())
        }
        MediaRef::Artifact { artifact } if media_supported(&artifact.media_type, video) => {
            let n =
                usize::try_from(artifact.byte_length.get()).map_err(|_| ProviderError::TooLarge)?;
            if n == 0 || n > MAX_REQUEST_BYTES {
                return Err(ProviderError::TooLarge);
            }
            Ok(13 + artifact.media_type.len() + n.div_ceil(3) * 4)
        }
        _ => Err(ProviderError::InvalidInput("decision_media")),
    }
}

fn bytes(value: &Value) -> Result<usize, ProviderError> {
    serde_json::to_vec(value)
        .map(|body| body.len())
        .map_err(|_| ProviderError::InvalidInput("decision_json"))
}

pub fn requirements(input: &DecideInput) -> Result<Requirements, ProviderError> {
    if !input
        .timeout_ms
        .0
        .parse::<u64>()
        .is_ok_and(|value| value > 0 && value <= MAX_TIMEOUT_MS)
    {
        return Err(ProviderError::InvalidInput("decision_timeout"));
    }
    let mut needs = Requirements {
        images: !input.images.0.is_empty(),
        videos: !input.videos.0.is_empty(),
        ..Default::default()
    };
    let questions = questions(input, &mut needs)?;
    check_nested(&input.state.0, &mut needs)?;
    check_nested(&questions, &mut needs)?;
    let mut shared = bytes(&input.state.0)?;
    for (media, video) in [(&input.images.0, false), (&input.videos.0, true)] {
        for item in media {
            shared = shared.saturating_add(3 + media_length(item, video)?);
        }
    }
    let question_bytes = bytes(&questions)?;
    needs.bytes = shared.saturating_add(question_bytes).saturating_add(80);
    needs.batch_bytes = shared
        .saturating_mul(needs.question_count)
        .saturating_add(question_bytes);
    if needs.bytes > MAX_REQUEST_BYTES {
        return Err(ProviderError::TooLarge);
    }
    Ok(needs)
}

pub type RemoteMedia<'a> =
    dyn FnMut(&str, bool, Option<&str>, usize) -> Result<String, ProviderError> + 'a;

pub fn encode(
    input: &DecideInput,
    upstream: &str,
    materials: &dyn Materials,
    validator: &dyn SchemaValidator,
    remote: &mut RemoteMedia<'_>,
) -> Result<Vec<u8>, ProviderError> {
    let mut remaining = MAX_REQUEST_BYTES.saturating_sub(requirements(input)?.bytes);
    let mut resolve = |url: &str, video, declared: Option<&str>| {
        let limit = remaining.saturating_add(url.len());
        let materialized = remote(url, video, declared, limit)?;
        if materialized.len() > limit {
            return Err(ProviderError::TooLarge);
        }
        remaining = limit - materialized.len();
        Ok(materialized)
    };
    for entry in &input.questions.0 {
        if let DecisionQuestion::Extension { schema, payload } = &entry.question {
            validator
                .validate(&super::materialize::schema(materials, schema)?, &payload.0)
                .map_err(|_| ProviderError::InvalidInput("decision_extension_schema"))?;
        }
    }
    let questions = questions(input, &mut Requirements::default())?;
    let mut body = json!({"model":upstream,"state":input.state.0,"questions":questions});
    resolve_nested(&mut body, &mut resolve)?;
    for (list, field, video) in [
        (&input.images.0, "images", false),
        (&input.videos.0, "videos", true),
    ] {
        if list.is_empty() {
            continue;
        }
        let mut values = Vec::with_capacity(list.len());
        for item in list {
            match item {
                MediaRef::Remote { url, media_type } => {
                    values.push(resolve(&url.0, video, Some(&media_type.0))?)
                }
                MediaRef::Artifact { artifact } => {
                    let bytes = materials.artifact(artifact, artifact.byte_length.get() as u64)?;
                    values.push(format!(
                        "data:{};base64,{}",
                        artifact.media_type,
                        STANDARD.encode(bytes)
                    ));
                }
            }
        }
        body[field] = json!(values);
    }
    let body =
        serde_json::to_vec(&body).map_err(|_| ProviderError::InvalidInput("decision_json"))?;
    if body.len() > MAX_REQUEST_BYTES {
        return Err(ProviderError::TooLarge);
    }
    Ok(body)
}

type NestedMedia<'a> = dyn FnMut(&str, bool, Option<&str>) -> Result<String, ProviderError> + 'a;

fn resolve_nested(node: &mut Value, remote: &mut NestedMedia<'_>) -> Result<(), ProviderError> {
    match node {
        Value::Object(fields) => {
            let media = match fields.get("type").and_then(Value::as_str) {
                Some("image_url") => Some(("image_url", false)),
                Some("video_url") => Some(("video_url", true)),
                _ => None,
            };
            if let Some((key, video)) = media
                && let Some(url) = fields.get_mut(key).and_then(|source| source.get_mut("url"))
                && let Some(raw) = url.as_str().filter(|raw| !raw.starts_with("data:"))
            {
                *url = Value::String(remote(raw, video, None)?);
            }
            for child in fields.values_mut() {
                resolve_nested(child, remote)?;
            }
        }
        Value::Array(items) => {
            for child in items {
                resolve_nested(child, remote)?;
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(questions: Value) -> DecideInput {
        serde_json::from_value(json!({
            "usePool":true, "state":{"text":"look"}, "questions":questions,
            "images":[],"videos":[], "timeoutMs":"1000"
        }))
        .unwrap()
    }

    #[test]
    fn retained_question_types_and_nested_instruction_media_affect_admission() {
        let input = input(json!([
            {"name":"pick","question":{"kind":"Choice","instructions":"Look","criteria":[{"name":"a"},{"name":"b","description":"Other"}]}},
            {"name":"yes","question":{"kind":"Noul","instructions":"Decide"}},
            {"name":"grade","question":{"kind":"Score","instructions":"Rate","criteria":["low","high"]}},
            {"name":"locate","question":{"kind":"Box","instructions":"Locate","instructionParts":[{"type":"video_url","video_url":{"url":"data:video/mp4;base64,AAAA"}}]}}
        ]));
        let requirements = requirements(&input).unwrap();
        assert_eq!(
            requirements.question_types,
            ["box", "choice", "noul", "score"]
                .into_iter()
                .map(str::to_owned)
                .collect()
        );
        assert!(requirements.videos && !requirements.images);
        assert_eq!(requirements.question_count, 4);
        let wire = questions(&input, &mut Requirements::default()).unwrap();
        assert_eq!(wire["pick"]["criteria"], json!({"a":null,"b":"Other"}));
        assert_eq!(wire["grade"]["criteria"], json!(["low", "high"]));
        assert_eq!(wire["locate"]["instructions"][1]["type"], "video_url");
    }

    #[test]
    fn nested_media_has_the_same_source_checks_as_top_level_media() {
        let input = input(json!([
            {"name":"locate","question":{"kind":"Box","instructions":"Locate","instructionParts":[{"type":"image_url","image_url":{"url":"file:///etc/hosts"}}]}}
        ]));
        assert_eq!(
            requirements(&input).unwrap_err(),
            ProviderError::InvalidInput("decision_media_url")
        );
    }
}
