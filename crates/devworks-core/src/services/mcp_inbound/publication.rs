//! Reviewed operation and mode bindings for incoming MCP discovery and admission.

use super::OPERATIONS;
use crate::admission::frame_tool;
use crate::protocol::results::{DomainCommandReply, DomainQueryReply};
use crate::protocol::{Id, ProjectId, RequestKey};
use crate::registry::{
    self, AuthorityClass, Envelope, Mode, ModeBinding, OperationDescriptor, SchemaRevision,
};
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::OnceLock;

#[derive(Deserialize)]
struct Manifest {
    tools: Vec<Candidate>,
}

#[derive(Deserialize)]
struct Candidate {
    name: String,
    operation: String,
    mode: String,
}

/// A reviewed MCP name bound to one exact application operation, mode and schema revision.
pub struct PublishedTool {
    pub name: String,
    pub operation: &'static OperationDescriptor,
    pub binding: &'static ModeBinding,
    pub revision: SchemaRevision,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ToolArguments {
    project_id: ProjectId,
    input: Value,
    #[serde(default, deserialize_with = "provided_request_key")]
    request_key: Option<RequestKey>,
}

fn provided_request_key<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<RequestKey>, D::Error> {
    RequestKey::deserialize(deserializer).map(Some)
}

/// An exact gateway frame and the project it asks to use. The gateway still verifies the
/// caller's grant and every referenced source, worktree view and work item.
pub struct PublishedRequest {
    pub project: ProjectId,
    pub frame: Vec<u8>,
}

impl PublishedTool {
    pub fn has_target_authority(&self) -> bool {
        super::access::supported_target(self.operation.name, self.binding.mode)
    }

    /// Model-facing arguments with native envelope metadata and selectors supplied by the host.
    pub fn input_schema(&self) -> Value {
        let mut input = self.binding.input_schema(self.revision).to_value();
        input
            .as_object_mut()
            .expect("registered input schema")
            .remove("$schema");
        let definitions = input.as_object_mut().unwrap().remove("$defs");
        if let Envelope::Specialized { .. } = self.operation.envelope {
            for field in ["protocol", self.binding.mode.key_member(), "schemaRevision"] {
                remove_member(&mut input, field);
            }
            if let Some(selector) = self.operation.selector {
                let (object, field) = selector
                    .pointer
                    .trim_start_matches('/')
                    .split_once('/')
                    .expect("nested specialized selector");
                let property = &mut input["properties"][object];
                let schema = match property.get("$ref").and_then(Value::as_str) {
                    Some(reference) => definitions
                        .as_ref()
                        .and_then(|defs| defs.get(reference.strip_prefix("#/$defs/")?))
                        .expect("registered selector schema"),
                    None => property,
                };
                let mut selected = select_variant(schema, field, selector.equals).clone();
                remove_member(&mut selected, field);
                *property = selected;
            }
        }
        let mut id = schemars::schema_for!(Id).to_value();
        id.as_object_mut().unwrap().remove("$schema");
        let mut wrapper = json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["projectId", "input"],
            "properties": { "projectId": id, "input": input }
        });
        if self.binding.mode == Mode::Command {
            wrapper["properties"]["requestKey"] = id;
            wrapper["required"]
                .as_array_mut()
                .unwrap()
                .push(json!("requestKey"));
        }
        if let Some(definitions) = definitions {
            wrapper["$defs"] = definitions;
        }
        registry::prune_definitions(wrapper)
    }

    /// Native result, denial and recovery envelopes, narrowed to this selected operation's result.
    pub fn output_schema(&self) -> Value {
        let schema = match self.binding.mode {
            Mode::Query => schemars::schema_for!(DomainQueryReply),
            Mode::Command => schemars::schema_for!(DomainCommandReply),
        };
        let mut schema = registry::schema_at(schema, self.revision).to_value();
        let result = &mut schema["$defs"]["OperationResult"];
        let selected = select_variant(result, "kind", self.operation.name).clone();
        *result = selected;
        schema["type"] = json!("object");
        match self.operation.name {
            "artifacts.read" => {
                let content = &mut schema["$defs"]["ArtifactReadContent"];
                let variant = content["oneOf"]
                    .as_array_mut()
                    .expect("native artifact content")
                    .iter_mut()
                    .find(|variant| variant["properties"]["kind"]["const"] == "Binary")
                    .expect("native binary content");
                artifact_range(variant);
            }
            "artifacts.export" => artifact_range(&mut schema["$defs"]["ArtifactExport"]),
            _ => {}
        }
        registry::prune_definitions(schema)
    }

    /// Bind the selected operation and revision before current admission. The MCP request ID
    /// cannot choose a command's durable identity or stand in for its application request key.
    pub fn request(
        &self,
        query_id: &Id,
        arguments: Value,
    ) -> Result<PublishedRequest, &'static str> {
        let mut arguments: ToolArguments =
            serde_json::from_value(arguments).map_err(|_| "invalid MCP tool arguments")?;
        let identity = match (self.binding.mode, arguments.request_key.as_ref()) {
            (Mode::Query, None) => query_id.as_str(),
            (Mode::Command, Some(key)) => key.0.as_str(),
            (Mode::Query, Some(_)) => return Err("queries do not accept a command request key"),
            (Mode::Command, None) => return Err("commands require an application request key"),
        };
        if let Envelope::Specialized { .. } = self.operation.envelope {
            let object = arguments
                .input
                .as_object_mut()
                .ok_or("tool input must be an object")?;
            for field in ["protocol", self.binding.mode.key_member(), "schemaRevision"] {
                if object.contains_key(field) {
                    return Err("MCP tool input contains host-owned envelope metadata");
                }
            }
            if let Some(selector) = self.operation.selector {
                let (member, field) = selector
                    .pointer
                    .trim_start_matches('/')
                    .split_once('/')
                    .expect("nested specialized selector");
                let selected = object
                    .get_mut(member)
                    .and_then(Value::as_object_mut)
                    .ok_or("missing selected MCP tool input")?;
                if selected.contains_key(field) {
                    return Err("MCP tool input contains a host-owned selector");
                }
                selected.insert(field.into(), json!(selector.equals));
            }
        }
        let frame = frame_tool(
            identity.to_owned(),
            self.operation,
            self.binding.mode,
            self.revision,
            arguments.input,
        )?;
        Ok(PublishedRequest {
            project: arguments.project_id,
            frame,
        })
    }
}

fn artifact_range(schema: &mut Value) {
    for member in ["streamId", "expiresAtMs"] {
        remove_member(schema, member);
    }
    let position = json!({"type":"string","pattern":"^(0|[1-9][0-9]{0,18})$"});
    schema["properties"]["bytePage"] = json!({
        "type":"object",
        "additionalProperties":false,
        "required":["firstByte","nextByte","base64"],
        "properties":{
            "firstByte":position,
            "nextByte":position,
            "base64":{"type":"string","contentEncoding":"base64","maxLength":87384}
        }
    });
    schema["required"]
        .as_array_mut()
        .expect("native range fields")
        .push(json!("bytePage"));
}

fn remove_member(schema: &mut Value, field: &str) {
    schema["properties"]
        .as_object_mut()
        .expect("registered object schema")
        .remove(field);
    remove_required(schema, field);
}

fn remove_required(schema: &mut Value, field: &str) {
    if let Some(required) = schema.get_mut("required").and_then(Value::as_array_mut) {
        required.retain(|name| name != field);
    }
    for combinator in ["oneOf", "anyOf", "allOf"] {
        if let Some(alternatives) = schema.get_mut(combinator).and_then(Value::as_array_mut) {
            for alternative in alternatives {
                remove_required(alternative, field);
            }
        }
    }
}

fn select_variant<'a>(schema: &'a Value, field: &str, selected: &str) -> &'a Value {
    schema["oneOf"]
        .as_array()
        .expect("registered tagged union")
        .iter()
        .find(|variant| variant["properties"][field]["const"] == selected)
        .expect("registered selected variant")
}

/// The publication inventory shared by transport discovery and client grant validation.
/// A listener must verify that each underlying owner is installed before advertising it.
pub fn published_tools() -> &'static [PublishedTool] {
    static TOOLS: OnceLock<Vec<PublishedTool>> = OnceLock::new();
    TOOLS.get_or_init(|| {
        let manifest: Manifest = serde_json::from_str(include_str!(
            "../../../../../docs/architecture/mcp-publication-candidates.json"
        ))
        .expect("checked-in MCP publication manifest");
        let mut names = HashSet::new();
        let mut modes = HashSet::new();
        manifest
            .tools
            .into_iter()
            .map(|candidate| {
                let descriptor =
                    registry::find(&candidate.operation).expect("registered MCP operation");
                let binding = descriptor
                    .modes
                    .iter()
                    .find(|binding| binding.mode.as_str() == candidate.mode)
                    .expect("registered MCP binding");
                assert!(
                    !descriptor.human_only
                        && !OPERATIONS.contains(&descriptor.name)
                        && !matches!(
                            binding.authority,
                            AuthorityClass::Configuration
                                | AuthorityClass::HumanConfiguration
                                | AuthorityClass::HumanDecision
                                | AuthorityClass::HumanMaintenance
                                | AuthorityClass::HostMaintenance
                        ),
                    "private MCP operation: {}",
                    descriptor.name
                );
                assert!(names.insert(candidate.name.clone()), "duplicate MCP name");
                assert!(
                    modes.insert((descriptor.name, binding.mode)),
                    "duplicate MCP operation and mode"
                );
                PublishedTool {
                    name: candidate.name,
                    operation: descriptor,
                    binding,
                    revision: *descriptor
                        .revisions
                        .last()
                        .expect("registered schema revision"),
                }
            })
            .collect()
    })
}

pub(crate) fn selected_operation(operation: &str, mode: Mode) -> bool {
    published_tools()
        .iter()
        .any(|tool| tool.operation.name == operation && tool.binding.mode == mode)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_publication_binds_one_mode_and_revision_to_each_reviewed_name() {
        let tools = published_tools();
        let search = tools
            .iter()
            .find(|tool| tool.name == "devworks_code_search")
            .expect("retained code search");
        assert_eq!(search.operation.name, "code.search");
        assert_eq!(search.binding.mode, Mode::Query);
        assert!(search.operation.supports(search.revision));
        assert!(selected_operation("code.search", Mode::Query));
        assert!(!selected_operation("code.search", Mode::Command));
        for operation in ["mcp_server.configure", "documents.share", "documents.move"] {
            assert!(!tools.iter().any(|tool| tool.operation.name == operation));
        }
    }

    #[test]
    fn mcp_requests_use_the_reviewed_binding_and_refuse_caller_identity() {
        use serde_json::json;
        let tool = published_tools()
            .iter()
            .find(|tool| tool.name == "devworks_projects_get")
            .unwrap();
        let id = Id::new("native-query").unwrap();
        let input = json!({"projectId":"first","input":{"projectId":"first"}});
        let request = tool.request(&id, input.clone()).unwrap();
        assert_eq!(request.project.0.as_str(), "first");
        let frame: Value = serde_json::from_slice(&request.frame).unwrap();
        assert_eq!(frame["queryId"], "native-query");
        assert_eq!(frame["body"]["kind"], "projects.get");
        let mut spoofed = input.clone();
        spoofed["actor"] = json!("human");
        assert!(tool.request(&id, spoofed).is_err());
        spoofed = input;
        spoofed["input"]["kind"] = json!("documents.move");
        assert!(tool.request(&id, spoofed).is_err());
    }

    #[test]
    fn each_mcp_schema_projects_only_its_selected_input_and_result() {
        for tool in published_tools() {
            assert!(tool.has_target_authority(), "{}", tool.name);
            let input = tool.input_schema();
            let output = tool.output_schema();
            assert_eq!(input["additionalProperties"], false, "{}", tool.name);
            assert_eq!(
                input["properties"].get("requestKey").is_some(),
                tool.binding.mode == Mode::Command,
                "{}",
                tool.name
            );
            assert_eq!(
                output["$defs"]["OperationResult"]["properties"]["kind"]["const"],
                tool.operation.name
            );
        }

        let search = published_tools()
            .iter()
            .find(|tool| tool.name == "devworks_code_search")
            .unwrap();
        let schema = search.input_schema();
        let properties = &schema["properties"]["input"]["properties"];
        assert!(properties.get("protocol").is_none());
        assert!(properties.get("queryId").is_none());
        assert!(properties["query"]["properties"].get("kind").is_none());
        assert!(properties["query"]["properties"].get("text").is_some());
    }
}
