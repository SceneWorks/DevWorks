use super::{InvocationKind, InvokeOutcome, InvokeRequest, PackageHost, contributions::scope_of};
use devworks_core::context::InvocationContext;
use devworks_core::fingerprint::canonical_digest;
use devworks_core::ports::IdKind;
use devworks_core::ports::packages::Inspection;
use devworks_core::protocol::provider::ArtifactRef;
use devworks_core::protocol::records::Severity;
use devworks_core::providers::context::{ContentBlock, MessageRole, ProviderContext};
use devworks_extension_protocol::package::{
    Contribution, ContributionEvent, ContributionMeta, HookPhase, InvocationOutcome,
};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

const MAX_HOOKS: usize = 16;
const HOOK_BUDGET: Duration = Duration::from_secs(8);
const HOOK_DEADLINE: Duration = Duration::from_secs(4);
const MAX_INPUT: usize = 1024 * 1024;
const MAX_TEXT: usize = 4096;

impl PackageHost {
    /// Collects source-bound context suggestions without passing on provider-native blocks,
    /// tool arguments, credentials or system instructions. Each output remains attributed data.
    pub fn context_suggestions(
        &self,
        principal: &InvocationContext,
        phase: HookPhase,
        context: &ProviderContext,
    ) -> Vec<String> {
        self.context_suggestions_until(principal, phase, context, &|| false)
    }

    pub(crate) fn context_suggestions_until(
        &self,
        principal: &InvocationContext,
        phase: HookPhase,
        context: &ProviderContext,
        stopped: &dyn Fn() -> bool,
    ) -> Vec<String> {
        let phase_name = |phase: &HookPhase| match phase {
            HookPhase::Assemble => "Assemble",
            HookPhase::Compact => "Compact",
            HookPhase::Decision => "Decision",
        };
        let Ok(installed) = self.active_contributions(principal, "ContextHook") else {
            return Vec::new();
        };
        let mut selected = Vec::new();
        for inspection in &installed {
            for entry in &inspection.contributions {
                let Some(Contribution::ContextHook {
                    meta,
                    phase: declared,
                    priority,
                }) = entry
                    .verified_descriptor()
                    .and_then(|value| serde_json::from_value(value.clone()).ok())
                else {
                    continue;
                };
                let access = format!("context-hook:{}", meta.id.as_str());
                if phase_name(&phase) == phase_name(&declared)
                    && inspection.access.contains(&access)
                {
                    selected.push((priority, inspection, meta));
                }
            }
        }
        selected.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| a.2.id.as_str().cmp(b.2.id.as_str()))
        });
        if selected.is_empty() {
            return Vec::new();
        }
        let snapshot: Vec<_> = context
            .messages
            .iter()
            .map(|message| {
                let text: Vec<_> = message
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect();
                json!({"role": message.role, "text": text})
            })
            .collect();
        let input = json!({"phase": phase_name(&phase), "messages": snapshot});
        let Ok(input_digest) = canonical_digest(&input) else {
            return Vec::new();
        };
        let input = json!({"contextDigest": input_digest, "context": input});
        let mut suggestions = Vec::new();
        let deadline = Instant::now() + HOOK_BUDGET;
        for (_, inspection, meta) in selected.into_iter().take(MAX_HOOKS) {
            if stopped() || Instant::now() >= deadline {
                break;
            }
            let hook_deadline = deadline.min(Instant::now() + HOOK_DEADLINE);
            match self.run_hook(principal, inspection, &meta, &input, hook_deadline, stopped) {
                Ok(suggestion) => suggestions.push(suggestion),
                Err(error) => self.service.record_log(
                    &inspection.package_id,
                    Severity::Warning,
                    "PackageContextHookRejected",
                    "The package did not supply a current context suggestion",
                    Some(error),
                ),
            }
        }
        suggestions
    }

    fn run_hook(
        &self,
        principal: &InvocationContext,
        inspection: &Inspection,
        meta: &ContributionMeta,
        input: &Value,
        deadline: Instant,
        stopped: &dyn Fn() -> bool,
    ) -> Result<String, String> {
        let input_digest = input["contextDigest"]
            .as_str()
            .ok_or("missing context digest")?;
        let bytes = serde_json::to_vec(input).map_err(|error| error.to_string())?;
        if bytes.len() > MAX_INPUT {
            return Err("the context snapshot exceeded its bound".into());
        }
        let package = &inspection.package_id;
        let manifest = &inspection.manifest_digest;
        let input_schema = self.contribution_schema(inspection, &meta.input_schema)?;
        let output_schema = self.contribution_schema(inspection, &meta.output_schema)?;
        self.service
            .validator()
            .validate(&input_schema, input)
            .map_err(|_| "the context does not meet the hook's input schema")?;
        let provenance = json!({"kind": "PackageContextHook", "contributionId": meta.id,
            "manifestDigest": manifest, "contextDigest": input_digest});
        let input =
            self.stage_contribution_input(principal, (package, manifest), &bytes, provenance)?;
        let input = serde_json::to_value(input)
            .and_then(serde_json::from_value)
            .map_err(|error| error.to_string())?;
        let handle = self
            .invoke(InvokeRequest {
                package_id: package.clone(),
                contribution_id: meta.id.as_str().to_owned(),
                scope: scope_of(principal)?,
                input,
                operation_id: self.ids.next_id(IdKind::Operation),
                deadline: deadline.saturating_duration_since(Instant::now()),
                allowed_operations: Vec::new(),
                kind: InvocationKind::ContextHook,
                principal: Some(principal.clone()),
                expected_digest: Some(manifest.clone()),
            })
            .map_err(|error| format!("context helper could not run: {error:?}"))?;
        let outcome = loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if stopped() || remaining.is_zero() {
                handle.cancel();
                return Err(
                    "the context helper did not complete within its active deadline".into(),
                );
            }
            if let Some(outcome) = handle.wait(remaining.min(Duration::from_millis(100))) {
                break outcome;
            }
        };
        if !matches!(
            outcome,
            InvokeOutcome::Finished(InvocationOutcome::Completed { .. })
        ) {
            return Err("the context helper did not complete".into());
        }
        let (events, dropped) = handle.events();
        let [
            ContributionEvent::ContextProposal {
                input_digest: proposed,
                proposal,
            },
        ] = events.as_slice()
        else {
            return Err("the context helper must produce exactly one proposal".into());
        };
        if dropped != 0
            || String::from(proposed.clone()) != input_digest
            || !handle.produced(proposal)
        {
            return Err("the context proposal is stale or not owned by the invocation".into());
        }
        let reference: ArtifactRef = serde_json::to_value(proposal)
            .and_then(serde_json::from_value)
            .map_err(|error| error.to_string())?;
        let ports = self
            .broker
            .get()
            .ok_or("package artifacts are not available")?;
        let (bytes, _) = ports
            .artifacts
            .read_exact(&*self.store, principal, &reference, MAX_TEXT as u64 * 2)
            .map_err(|_| "the context proposal is not readable")?;
        let value = devworks_core::framing::parse(&bytes).map_err(|error| error.to_string())?;
        self.service
            .validator()
            .validate(&output_schema, &value)
            .map_err(|_| "the context proposal does not meet its output schema")?;
        let proposal = value
            .as_object()
            .filter(|object| object.len() == 1)
            .and_then(|object| object.get("text"))
            .and_then(Value::as_str)
            .filter(|text| !text.trim().is_empty() && text.len() <= MAX_TEXT)
            .ok_or("the context proposal must carry bounded, nonempty text")?;
        if !self.contribution_current(principal, "ContextHook", (package, manifest))? {
            return Err("the context hook's version is no longer active".into());
        }
        Ok(format!(
            "Context supplied by {} ({}):\n{}",
            meta.id.as_str(),
            manifest,
            proposal
        ))
    }
}

pub(crate) fn append_context_suggestions(context: &mut ProviderContext, suggestions: Vec<String>) {
    if suggestions.is_empty() {
        return;
    }
    use devworks_core::providers::context::Message;
    let content = suggestions
        .into_iter()
        .map(|text| ContentBlock::Text { text, cache: None })
        .collect::<Vec<_>>();
    match context.messages.last_mut() {
        Some(last) if matches!(last.role, MessageRole::User) => last.content.extend(content),
        _ => context.messages.push(Message {
            role: MessageRole::User,
            content,
        }),
    }
}
