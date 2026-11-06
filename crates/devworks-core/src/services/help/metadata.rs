use super::HelpService;
use crate::fingerprint::sha256_hex;
use crate::protocol::provider::Digest;
use crate::protocol::records::HelpCapabilityMetadata;
use crate::protocol::support::BoundedText;
use crate::services::sessions::rev;

impl HelpService {
    pub(super) fn capability_metadata(&self, operation: &str) -> Vec<HelpCapabilityMetadata> {
        let Some(descriptor) = crate::registry::find(operation) else {
            return Vec::new();
        };
        let revision = *descriptor
            .revisions
            .last()
            .expect("qualified catalog revision");
        let registered = self
            .registered
            .read()
            .unwrap_or_else(|lock| lock.into_inner());
        descriptor.modes.iter().map(|binding| {
            let input=serde_json::to_vec(&binding.input_schema(revision)).expect("registered schema serializes");
            let result=serde_json::to_vec(&descriptor.result_schema(revision)).expect("registered schema serializes");
            let available=registered.contains(&(operation.into(),binding.mode.as_str().into()));
            HelpCapabilityMetadata {
                operation:BoundedText(operation.into()),mode:BoundedText(binding.mode.as_str().into()),
                owner:BoundedText(descriptor.owner.into()),authority:BoundedText(format!("{:?}",binding.authority)),effect:BoundedText(format!("{:?}",binding.effect)),
                schema_revision:rev(i64::from(revision.0)),input_type:BoundedText(binding.input_type.into()),result_type:BoundedText(descriptor.result_type.into()),
                input_schema_digest:Digest(sha256_hex(&input)),result_schema_digest:Digest(sha256_hex(&result)),
                available,unavailable_reason:(!available).then(||BoundedText("This operation and mode have no handler in the current native composition.".into())),
            }
        }).collect()
    }
}

pub(super) const AUTO_START: &str = "<!-- AUTO:START - Do not edit between these markers -->";
pub(super) const AUTO_END: &str = "<!-- AUTO:END -->";

/// Preserve hand-authored material exactly as the retained documentation generator does.
pub(super) fn replace_auto(existing: &str, generated: &str) -> String {
    match (existing.find(AUTO_START), existing.find(AUTO_END)) {
        (Some(start), Some(end)) if end >= start => format!(
            "{}{}{}",
            &existing[..start],
            generated,
            &existing[end + AUTO_END.len()..]
        ),
        _ => format!("{generated}\n\n{existing}"),
    }
}

pub(super) fn render(metadata: &HelpCapabilityMetadata) -> String {
    let descriptor = crate::registry::find(&metadata.operation.0).expect("registered operation");
    let mode = if metadata.mode.0 == "Query" {
        crate::registry::Mode::Query
    } else {
        crate::registry::Mode::Command
    };
    let binding = descriptor.binding(mode).expect("registered mode");
    let revision = crate::registry::SchemaRevision(metadata.schema_revision.get() as u32);
    let schema = serde_json::to_value(binding.input_schema(revision)).expect("registered schema");
    let mut body = format!(
        "{AUTO_START}\n## {} ({})\n\nOwner: `{}`. Authority: `{}`. Effect: `{}`. Schema revision: `{}`.\n\nRegistration describes implementation availability. Every real request still passes current tool selection, scope, policy, target and approval checks; documentation grants no authority.\n\n### Parameters\n\n| Name | Required | Schema | Description |\n|---|---|---|---|\n",
        metadata.operation.0,
        metadata.mode.0,
        metadata.owner.0,
        metadata.authority.0,
        metadata.effect.0,
        metadata.schema_revision.get()
    );
    if let Some(properties) = schema["properties"].as_object() {
        for (name, property) in properties {
            let required = schema["required"]
                .as_array()
                .is_some_and(|required| required.iter().any(|value| value.as_str() == Some(name)));
            let description = property["description"]
                .as_str()
                .unwrap_or("")
                .replace('|', "\\|")
                .replace('\n', " ");
            body.push_str(&format!(
                "| `{name}` | {} | `{}` | {} |\n",
                if required { "Yes" } else { "No" },
                property.to_string().replace('|', "\\|"),
                description
            ));
        }
    }
    body.push_str(&format!("\n### Exact input contract\n\n```json\n{}\n```\n\n### Exact result contract\n\n```json\n{}\n```\n{AUTO_END}",serde_json::to_string_pretty(&schema).expect("schema"),serde_json::to_string_pretty(&descriptor.result_schema(revision)).expect("schema")));
    body
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn auto_generation_preserves_exact_manual_material_and_prepends_when_markers_are_absent() {
        let generated = format!("{AUTO_START}\ncurrent schema\n{AUTO_END}");
        let old =
            format!("Manual π prefix\n{AUTO_START}\nstale schema\n{AUTO_END}\nManual 尾 suffix");
        assert_eq!(
            replace_auto(&old, &generated),
            format!("Manual π prefix\n{generated}\nManual 尾 suffix")
        );
        let manual = "Entirely manual π page\n";
        assert_eq!(
            replace_auto(manual, &generated),
            format!("{generated}\n\n{manual}")
        );
        assert_eq!(
            replace_auto(&replace_auto(manual, &generated), &generated),
            format!("{generated}\n\n{manual}")
        );
    }
}
