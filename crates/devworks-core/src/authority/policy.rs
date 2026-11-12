//! Pure authority policy: access profiles, effective user policy and the role/tool/profile
//! evaluation every caller shares.
//!
//! Effective authority is the intersection of the principal's host binding (kind, role, project,
//! selected tools), the operation's recorded authority/effect class, the effective access policy
//! (Workspace → Project settings) and mandatory restrictions that no profile waives. A denial
//! dominates. Nothing here reads request text for authority: the typed input is inspected only
//! to classify the selected effect (for example a force push and its target ref).
use crate::context::{InvocationContext, PrincipalKind};
use crate::fingerprint::sha256_hex;
use crate::operations::{OperationInput, git, watch};
use crate::ports::AuthorityRequest;
use crate::ports::storage::{
    ConfigScope, ConfigurationRead, GrantKind, PrincipalBinding, StorageError,
};
use crate::protocol::ProjectId;
use crate::protocol::data::{GitCommand, PushMode};
use crate::registry::{AuthorityClass, EffectClass, Mode};
use crate::settings::catalog::{AgentRole, TOOL_OVERRIDES_KEY, descriptor};
use crate::settings::model::{chain, current_chain, scope_of};
use std::collections::BTreeMap;

/// Setting that selects the access profile (Full is the initial default for new projects).
pub const ACCESS_PROFILE_KEY: &str = "safety.accessProfile";
/// Setting that allows the authorized process execution tools.
pub const SHELL_TOOLS_KEY: &str = "tools.shell";
/// Setting that allows the shared web search tool.
pub const WEB_TOOLS_KEY: &str = "tools.webSearch";
/// Setting that allows the shared web fetch tool.
pub use crate::settings::catalog::WEB_FETCH_TOOLS_KEY;

/// Refs no principal may force-push, under any profile or approval.
pub const PROTECTED_REFS: [&str; 2] = ["refs/heads/main", "refs/heads/master"];

/// Families whose reads are not verified reads (opaque remote tools, package code, unqualified
/// database query surfaces and live browser pages): never in a reviewer's read set, whatever the
/// profile or tool selection.
///
/// The shared web tools are not among them (TOOL-04/05/08): `web.search` and `web.fetch` are
/// host-native bounded reads that change nothing the reviewer owns. The host performs them under
/// its own egress policy, sends none of the user's credentials and records what it observed as
/// untrusted evidence, so a read-only reviewer keeps the same selected read tools as every agent.
pub const UNVERIFIED_READ_FAMILIES: [&str; 4] = ["mcp", "addons", "database", "browser"];

/// Commands that run arbitrary native code with the desktop user's rights. Under Project scope
/// they need a reviewed native grant, because no confined launcher is qualified; there is no
/// fallback to Full.
pub const NATIVE_EXECUTION: [&str; 18] = [
    "git.worktree",
    "git.commit",
    "git.push",
    "process.execute",
    // A process watcher launches through process.execute.
    "watch.create",
    "process.install_project_dependencies",
    "containers.build",
    "containers.pull",
    "containers.create",
    "containers.start",
    "containers.exec",
    "mcp.connect",
    "mcp.discover",
    "addons.install",
    "addons.update",
    "addons.enable",
    "addons.rollback",
    "addons.configure",
];

/// Commands a non-human principal may run only with an explicit approval for the exact request:
/// destructive project and repository-index changes and skill/role activation (archiving fences
/// and stops a project's work at once, deletion tombstones it, index removal drops a shared
/// registration's index for every project). `process.request_system_install` only stages a
/// proposal and performs no installation; its later approval is the human decision.
/// The user's own command is its confirmation.
pub const APPROVAL_REQUIRED: [&str; 5] = [
    "projects.archive",
    "projects.delete",
    "repositories.remove_index",
    // Activation of a skill or role revision: agent content takes effect only by replay of an
    // exact human approval of the proposed revision (SKILL-07, SET-05).
    "skills.rollback",
    "roles.rollback",
];

/// The selectable access profiles.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AccessProfile {
    /// Broad user-level local execution with selected tools; mandatory controls still apply.
    /// Not OS confinement.
    Full,
    /// Brokered work stays in registered repositories/task worktrees; native execution needs a
    /// reviewed native grant.
    ProjectScope,
}
impl AccessProfile {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::ProjectScope => "projectScope",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "full" => Some(Self::Full),
            "projectScope" => Some(Self::ProjectScope),
            _ => None,
        }
    }
    /// The enforcement boundary this profile actually provides (shown with effective
    /// permissions; never stronger than what is implemented).
    pub fn guarantee(self) -> &'static str {
        match self {
            Self::Full => {
                "Selected tools run with your ordinary user rights. Harness checks inspect known destructive intent; they are not OS confinement against arbitrary code."
            }
            Self::ProjectScope => {
                "Brokered work is limited to the project. Native shell, package and MCP execution is refused unless you grant a bounded native exception, which runs with your user rights."
            }
        }
    }
}

/// Where an effective value came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Provenance {
    pub scope: ConfigScope,
    /// Configuration revision of that scope (1 is the built-in default).
    pub revision: i64,
}

/// The access-relevant effective user policy of one scope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectivePolicy {
    /// `None` when the stored profile is not supported by this build (fails closed).
    pub profile: Option<AccessProfile>,
    pub profile_source: Option<Provenance>,
    pub shell_tools: bool,
    /// The shared web search tool.
    pub web_tools: bool,
    /// The shared web fetch tool.
    pub web_fetch: bool,
    /// Explicit shared choices. An invalid stored value refuses agent tools until repaired.
    pub tool_overrides: Option<BTreeMap<String, bool>>,
}
impl EffectivePolicy {
    pub fn tool_selected(&self, capability: &str, default: bool) -> bool {
        self.tool_overrides
            .as_ref()
            .is_some_and(|choices| choices.get(capability).copied().unwrap_or(default))
    }
    /// Stable digest of every value that changes an authorization; a fence compares it.
    pub fn digest(&self) -> String {
        sha256_hex(
            format!(
                "{}|{}|{}|{}|{}",
                self.profile.map_or("unsupported", AccessProfile::as_str),
                self.shell_tools,
                self.web_tools,
                self.web_fetch,
                serde_json::to_string(&self.tool_overrides).expect("tool choices serialize")
            )
            .as_bytes(),
        )
    }
}

/// Resolves the effective access policy of `project` (or the workspace) from current settings.
/// Unknown or schema-mismatched stored values fail closed: an unsupported profile and disabled
/// tools.
pub fn resolve_policy(
    read: &dyn ConfigurationRead,
    project: Option<&ProjectId>,
) -> Result<EffectivePolicy, StorageError> {
    let scope = project.map_or(ConfigScope::Workspace, |p| ConfigScope::Project(p.clone()));
    let layers = current_chain(read, &scope)?;
    let effective = |key: &str| {
        let d = descriptor(key).expect("catalog key");
        chain(d, &layers).ok().and_then(|c| c.into_iter().next())
    };
    let (profile, profile_source) = match effective(ACCESS_PROFILE_KEY) {
        Some(e) => (
            e.value.value.0.as_str().and_then(AccessProfile::parse),
            Some(Provenance {
                scope: scope_of(&e.source_scope),
                revision: e.revision.get(),
            }),
        ),
        None => (None, None),
    };
    let flag = |key| {
        effective(key)
            .and_then(|e| e.value.value.0.as_bool())
            .unwrap_or(false)
    };
    Ok(EffectivePolicy {
        profile,
        profile_source,
        shell_tools: flag(SHELL_TOOLS_KEY),
        web_tools: flag(WEB_TOOLS_KEY),
        web_fetch: flag(WEB_FETCH_TOOLS_KEY),
        tool_overrides: effective(TOOL_OVERRIDES_KEY).and_then(|entry| {
            let value = entry.value.value.0;
            let descriptor = descriptor(TOOL_OVERRIDES_KEY).expect("catalog key");
            if crate::settings::validate::check_value(descriptor, &value)
                .iter()
                .any(|issue| issue.blocking)
            {
                None
            } else {
                serde_json::from_value(value).ok()
            }
        }),
    })
}

/// Why authority refused (or needs a grant for) a request. Exposed to the UI and history so a
/// blocked request carries its rationale.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DenialReason {
    /// The invocation is bound to another store or an earlier restore epoch.
    StaleInvocation,
    /// Current authority records could not be read; authority fails closed.
    AuthorityUnavailable,
    /// The principal is not registered with the kind the host bound.
    UnregisteredPrincipal,
    /// A non-human principal without a host authority binding.
    UnboundPrincipal,
    PrincipalRevoked,
    /// The binding is not a supported authority profile for its kind (for example an agent
    /// without a role, or a role on a non-agent).
    UnsupportedPrincipal,
    /// The stored access profile is not supported by this build.
    UnsupportedProfile,
    HumanOnly,
    HostOnly,
    /// Read-only roles (code reviewers) never write, under any profile.
    ReadOnlyRole,
    /// A reviewer may only use verified reads.
    UnverifiedRead,
    ToolNotSelected,
    ShellToolsDisabled,
    WebToolsDisabled,
    /// The shared web fetch tool is turned off.
    WebFetchDisabled,
    /// The invocation is outside the principal's bound project.
    OutOfProjectScope,
    /// A bound agent requested work outside its root or delegated task references.
    OutOfAssignmentScope,
    /// Project scope has no qualified confined launcher for native execution.
    EnforcementUnavailable,
    /// Force-pushing main/master is never allowed.
    ProtectedBranch,
    /// The operation needs an explicit approval for this exact request.
    ApprovalRequired,
    /// The principal acts only within a host-minted grant for this operation.
    GrantRequired,
}
impl DenialReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::StaleInvocation => "StaleInvocation",
            Self::AuthorityUnavailable => "AuthorityUnavailable",
            Self::UnregisteredPrincipal => "UnregisteredPrincipal",
            Self::UnboundPrincipal => "UnboundPrincipal",
            Self::PrincipalRevoked => "PrincipalRevoked",
            Self::UnsupportedPrincipal => "UnsupportedPrincipal",
            Self::UnsupportedProfile => "UnsupportedProfile",
            Self::HumanOnly => "HumanOnly",
            Self::HostOnly => "HostOnly",
            Self::ReadOnlyRole => "ReadOnlyRole",
            Self::UnverifiedRead => "UnverifiedRead",
            Self::ToolNotSelected => "ToolNotSelected",
            Self::ShellToolsDisabled => "ShellToolsDisabled",
            Self::WebToolsDisabled => "WebToolsDisabled",
            Self::WebFetchDisabled => "WebFetchDisabled",
            Self::OutOfProjectScope => "OutOfProjectScope",
            Self::OutOfAssignmentScope => "OutOfAssignmentScope",
            Self::EnforcementUnavailable => "EnforcementUnavailable",
            Self::ProtectedBranch => "ProtectedBranch",
            Self::ApprovalRequired => "ApprovalRequired",
            Self::GrantRequired => "GrantRequired",
        }
    }
    /// Plain-language rationale shown with the blocked request.
    pub fn rationale(self) -> &'static str {
        match self {
            Self::StaleInvocation => "The request belongs to another store or a restored epoch.",
            Self::AuthorityUnavailable => "Current permissions could not be checked.",
            Self::UnregisteredPrincipal => "The caller is not a registered principal.",
            Self::UnboundPrincipal => "The host has not granted this caller any authority.",
            Self::PrincipalRevoked => "This caller's authority was revoked.",
            Self::UnsupportedPrincipal => {
                "This caller's authority binding is not a supported profile."
            }
            Self::UnsupportedProfile => {
                "The configured access profile is not supported by this version."
            }
            Self::HumanOnly => "Only you can take this action, from the app itself.",
            Self::HostOnly => "Only the application performs this maintenance.",
            Self::ReadOnlyRole => "Code review agents are read-only, even with Full access.",
            Self::UnverifiedRead => {
                "Code review agents may only use verified reads; this tool cannot be verified read-only."
            }
            Self::ToolNotSelected => "This tool is not selected for this agent.",
            Self::ShellToolsDisabled => "Shell commands are turned off in Agent tools settings.",
            Self::WebToolsDisabled => "Web search is turned off in Agent tools settings.",
            Self::WebFetchDisabled => "Web page fetching is turned off in Agent tools settings.",
            Self::OutOfProjectScope => "The request is outside the agent's project.",
            Self::OutOfAssignmentScope => "The request is outside this agent's assigned work.",
            Self::EnforcementUnavailable => {
                "Project scope cannot confine native execution here; grant a bounded native exception to run it with your user rights."
            }
            Self::ProtectedBranch => "Main is never force-pushed.",
            Self::ApprovalRequired => "This action needs your explicit approval for this request.",
            Self::GrantRequired => "This caller acts only within a grant you created.",
        }
    }
}

/// The policy outcome before grants are resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    /// Allowed only by spending a matching grant of `kind`; `exact` grants must bind the exact
    /// request digest. Without one the request is refused with `reason`.
    NeedsGrant {
        kind: GrantKind,
        exact: bool,
        reason: DenialReason,
    },
    Deny(DenialReason),
}

fn family(op: &str) -> &str {
    op.split_once('.').map_or(op, |(f, _)| f)
}

/// A force push and the ref it targets, when the typed input is one.
fn force_push_target(input: &OperationInput) -> Option<&str> {
    match input {
        OperationInput::Git(git::Input::Push(request)) => match &request.command {
            GitCommand::Push {
                target_ref,
                mode: PushMode::ForceWithLease,
                ..
            } => Some(target_ref),
            _ => None,
        },
        _ => None,
    }
}

/// Whether the typed input is a watcher cancellation that also stops the watched process.
fn stops_watched_process(input: &OperationInput) -> bool {
    matches!(
        input,
        OperationInput::Watch(watch::Input::Cancel(watch::CancelInput {
            stop_process: true,
            ..
        }))
    )
}

/// Evaluates one request for a principal. `binding` is the principal's current host binding
/// (required for every kind except Human and Host). Mandatory restrictions are evaluated first
/// and hold under every profile.
/// Whether `target` names a protected branch: `main`, `master` or one of `extra` (for example a
/// remote default branch the Git adapter knows). Compared case-insensitively, because refs
/// collide on case-insensitive filesystems and a near-alias must not slip through.
pub fn protected_ref(target: &str, extra: &[String]) -> bool {
    let short = |r: &str| {
        r.strip_prefix("refs/heads/")
            .unwrap_or(r)
            .to_ascii_lowercase()
    };
    let name = short(target.trim_start_matches('+'));
    PROTECTED_REFS
        .iter()
        .map(|r| short(r))
        .chain(extra.iter().map(|r| short(r)))
        .any(|p| p == name)
}

pub fn evaluate(
    ctx: &InvocationContext,
    binding: Option<&PrincipalBinding>,
    policy: &EffectivePolicy,
    request: &AuthorityRequest<'_>,
    protected_branches: &[String],
) -> Verdict {
    evaluate_with_owned_task(ctx, binding, policy, request, protected_branches, false)
}

/// Whether a CURRENT registered tool may be presented to this invocation. This grants no
/// execution authority: target/effect refinements and exact grants still require full admission.
pub fn registered_tool_visibility(
    ctx: &InvocationContext,
    binding: Option<&PrincipalBinding>,
    policy: &EffectivePolicy,
    descriptor: &crate::registry::OperationDescriptor,
    selected: &crate::registry::ModeBinding,
    effect: EffectClass,
) -> Verdict {
    let kind = ctx.principal_kind();
    let op = descriptor.name;
    let fam = family(op);
    let class = selected.authority;
    let command = selected.mode == Mode::Command;
    // These project-wide operations have no target that can be narrowed to the running root.
    // Agents use addressed reads and writes for their root and delegated task references.
    if kind == PrincipalKind::Agent
        && ctx.root_id().is_some()
        && matches!(
            op,
            "features.list"
                | "features.create"
                | "features.prioritize"
                | "features.dependencies"
                | "issues.list"
                | "issues.create"
                | "issues.prioritize"
                | "tasks.list"
                | "tasks.prioritize"
                | "tasks.dependency_links"
        )
    {
        return Verdict::Deny(DenialReason::OutOfAssignmentScope);
    }
    if kind == PrincipalKind::Agent && ctx.assignment_id().is_some() && op == "artifacts.list" {
        return Verdict::Deny(DenialReason::OutOfAssignmentScope);
    }
    if kind == PrincipalKind::Agent && ctx.assignment_id().is_some() && op.starts_with("jobs.") {
        return Verdict::Deny(DenialReason::OutOfAssignmentScope);
    }
    if kind == PrincipalKind::Agent
        && ctx.assignment_id().is_some()
        && matches!(op, "approvals.list" | "approvals.review_queue")
    {
        return Verdict::Deny(DenialReason::OutOfAssignmentScope);
    }
    // Project-wide message views belong to the human UI. Agent sessions use their addressed
    // inbox; never advertise a project history or event feed they cannot read.
    if matches!(
        op,
        "agents.history"
            | "agents.events"
            | "mcp.configure"
            | "mcp.select_tools"
            | "mcp.disable"
            | "mcp.remove"
    ) && kind != PrincipalKind::Human
    {
        return Verdict::Deny(DenialReason::HumanOnly);
    }

    match kind {
        PrincipalKind::Human => return Verdict::Allow,
        PrincipalKind::Host => {
            return if matches!(
                class,
                AuthorityClass::HumanConfiguration
                    | AuthorityClass::HumanDecision
                    | AuthorityClass::HumanMaintenance
            ) {
                Verdict::Deny(DenialReason::HumanOnly)
            } else {
                Verdict::Allow
            };
        }
        _ => {}
    }

    let Some(binding) = binding else {
        return Verdict::Deny(DenialReason::UnboundPrincipal);
    };
    if binding.kind != kind {
        return Verdict::Deny(DenialReason::UnsupportedPrincipal);
    }
    if !binding.active {
        return Verdict::Deny(DenialReason::PrincipalRevoked);
    }
    let role = match (kind, binding.role) {
        (PrincipalKind::Agent, Some(role)) => Some(role),
        (PrincipalKind::Agent, None) | (_, Some(_)) => {
            return Verdict::Deny(DenialReason::UnsupportedPrincipal);
        }
        (_, None) => None,
    };
    // A task or feature assignment never grants project administration. Rootless agents retain
    // their existing project policy and approval path; humans retain direct authority above.
    if kind == PrincipalKind::Agent
        && ctx.root_id().is_some()
        && command
        && (matches!(fam, "projects" | "workflows" | "roles")
            || op == "repositories.remove_index"
            || op == "next_work.claim_project_queue"
            || class == AuthorityClass::Configuration)
        && (role != Some(AgentRole::WorkspaceCoordinator) || ctx.root_id().is_some())
    {
        return Verdict::Deny(DenialReason::OutOfAssignmentScope);
    }
    if ctx.root_id().is_some()
        && role == Some(AgentRole::Worker)
        && matches!(
            op,
            "runs.start"
                | "runs.pause"
                | "runs.continue"
                | "runs.redirect"
                | "runs.stop"
                | "tasks.create"
                | "tasks.sync_plan"
                | "features.edit"
                | "features.edit_plan"
                | "features.request_plan_approval"
                | "next_work.claim_feature_task"
        )
    {
        return Verdict::Deny(DenialReason::OutOfAssignmentScope);
    }
    match class {
        AuthorityClass::HumanConfiguration
        | AuthorityClass::HumanDecision
        | AuthorityClass::HumanMaintenance => return Verdict::Deny(DenialReason::HumanOnly),
        AuthorityClass::HostMaintenance => return Verdict::Deny(DenialReason::HostOnly),
        _ => {}
    }
    // Internal project IDs restrict brokered data under every profile.
    if let Some(bound) = &binding.project_id
        && ctx.project_id() != Some(bound)
    {
        return Verdict::Deny(DenialReason::OutOfProjectScope);
    }
    if let Some(selected) = &binding.tool_families
        && (!selected.iter().any(|f| f == fam)
            // A watcher launches through the process tools: it needs that family too.
            || (op == "watch.create" && !selected.iter().any(|f| f == "process")))
    {
        return Verdict::Deny(DenialReason::ToolNotSelected);
    }
    let default_selected = !matches!(fam, "desktop" | "browser");
    if kind == PrincipalKind::Agent
        && !policy.tool_selected(&format!("devworks/{op}"), default_selected)
    {
        return Verdict::Deny(DenialReason::ToolNotSelected);
    }
    // A watcher launches through process.execute: turning that tool off turns off launching
    // through a watcher too.
    if kind == PrincipalKind::Agent
        && op == "watch.create"
        && !policy.tool_selected("devworks/process.execute", true)
    {
        return Verdict::Deny(DenialReason::ToolNotSelected);
    }
    let read_only_role = role.is_some_and(AgentRole::read_only);
    if read_only_role {
        if command || effect != EffectClass::ReadOnly {
            return Verdict::Deny(DenialReason::ReadOnlyRole);
        }
        if UNVERIFIED_READ_FAMILIES.contains(&fam) {
            return Verdict::Deny(DenialReason::UnverifiedRead);
        }
    }
    if kind == PrincipalKind::Agent {
        // A process watcher launches and stops processes through the process tools.
        if (fam == "process" || matches!(op, "watch.create" | "watch.cancel"))
            && !policy.shell_tools
        {
            return Verdict::Deny(DenialReason::ShellToolsDisabled);
        }
        // Each web tool has its own switch; no other web operation is allowed by them.
        if fam == "web" {
            match op {
                "web.search" if policy.web_tools => {}
                "web.fetch" if policy.web_fetch => {}
                "web.fetch" => return Verdict::Deny(DenialReason::WebFetchDisabled),
                _ => return Verdict::Deny(DenialReason::WebToolsDisabled),
            }
        }
    }
    // An unsupported stored profile fails closed for reads and commands alike.
    let Some(_) = policy.profile else {
        return Verdict::Deny(DenialReason::UnsupportedProfile);
    };
    if !command {
        return Verdict::Allow;
    }

    Verdict::Allow
}

/// Visibility of a selected external MCP tool. A server's read-only annotation is advisory;
/// it cannot qualify a reviewer read or bypass the principal's current project and profile.
pub fn outgoing_mcp_tool_visibility(
    ctx: &InvocationContext,
    binding: Option<&PrincipalBinding>,
    policy: &EffectivePolicy,
    capability: &str,
) -> Verdict {
    if matches!(
        ctx.principal_kind(),
        PrincipalKind::Human | PrincipalKind::Host
    ) {
        return Verdict::Allow;
    }
    let Some(binding) = binding else {
        return Verdict::Deny(DenialReason::UnboundPrincipal);
    };
    if binding.kind != ctx.principal_kind() || !binding.active {
        return Verdict::Deny(DenialReason::PrincipalRevoked);
    }
    if (ctx.principal_kind() == PrincipalKind::Agent && binding.role.is_none())
        || (ctx.principal_kind() != PrincipalKind::Agent && binding.role.is_some())
    {
        return Verdict::Deny(DenialReason::UnsupportedPrincipal);
    }
    if let Some(project) = &binding.project_id
        && ctx.project_id() != Some(project)
    {
        return Verdict::Deny(DenialReason::OutOfProjectScope);
    }
    if let Some(families) = &binding.tool_families
        && !families.iter().any(|family| family == "mcp")
    {
        return Verdict::Deny(DenialReason::ToolNotSelected);
    }
    if binding.role.is_some_and(AgentRole::read_only) {
        return Verdict::Deny(DenialReason::UnverifiedRead);
    }
    match policy.profile {
        None => return Verdict::Deny(DenialReason::UnsupportedProfile),
        Some(AccessProfile::ProjectScope) => {
            return Verdict::Deny(DenialReason::EnforcementUnavailable);
        }
        Some(_) => {}
    }
    if ctx.principal_kind() != PrincipalKind::Agent {
        return Verdict::Deny(DenialReason::GrantRequired);
    }
    if !policy.tool_selected(capability, true) {
        return Verdict::Deny(DenialReason::ToolNotSelected);
    }
    Verdict::Allow
}

pub(crate) fn evaluate_with_owned_task(
    ctx: &InvocationContext,
    binding: Option<&PrincipalBinding>,
    policy: &EffectivePolicy,
    request: &AuthorityRequest<'_>,
    protected_branches: &[String],
    owned_task_force: bool,
) -> Verdict {
    let kind = ctx.principal_kind();
    let op = request.descriptor.name;
    let class = request.binding.authority;
    // Mandatory for every principal, including the user and the host.
    if let Some(target) = force_push_target(request.input)
        && protected_ref(target, protected_branches)
    {
        return Verdict::Deny(DenialReason::ProtectedBranch);
    }

    if kind == PrincipalKind::Host && force_push_target(request.input).is_some() {
        return Verdict::NeedsGrant {
            kind: GrantKind::Operation,
            exact: true,
            reason: DenialReason::ApprovalRequired,
        };
    }
    let visible = registered_tool_visibility(
        ctx,
        binding,
        policy,
        request.descriptor,
        request.binding,
        request.effect,
    );
    if !matches!(visible, Verdict::Allow) {
        return visible;
    }
    // Stopping a watched process is a process cancellation: it needs what process.cancel needs.
    if stops_watched_process(request.input)
        && !matches!(kind, PrincipalKind::Human | PrincipalKind::Host)
        && (binding
            .and_then(|b| b.tool_families.as_ref())
            .is_some_and(|selected| !selected.iter().any(|f| f == "process"))
            || (kind == PrincipalKind::Agent
                && !policy.tool_selected("devworks/process.cancel", true)))
    {
        return Verdict::Deny(DenialReason::ToolNotSelected);
    }
    if matches!(kind, PrincipalKind::Human | PrincipalKind::Host)
        || request.binding.mode != Mode::Command
    {
        return Verdict::Allow;
    }
    let profile = policy
        .profile
        .expect("tool visibility checked supported profile");
    // Commands from here on.
    if matches!(kind, PrincipalKind::Package | PrincipalKind::McpClient)
        && class == AuthorityClass::Configuration
    {
        return Verdict::Deny(DenialReason::HumanOnly);
    }
    if (force_push_target(request.input).is_some() && !owned_task_force)
        || APPROVAL_REQUIRED.contains(&op)
    {
        // Explicit approvals stay effective under Full access.
        return Verdict::NeedsGrant {
            kind: GrantKind::Operation,
            exact: true,
            reason: DenialReason::ApprovalRequired,
        };
    }
    if NATIVE_EXECUTION.contains(&op) && profile == AccessProfile::ProjectScope {
        return Verdict::NeedsGrant {
            kind: GrantKind::NativeExecution,
            exact: true,
            reason: DenialReason::EnforcementUnavailable,
        };
    }
    if matches!(
        kind,
        PrincipalKind::Package | PrincipalKind::McpClient | PrincipalKind::Schedule
    ) {
        // Declared package capabilities, MCP client labels and schedule definitions are not
        // authority: each command needs a host grant bound to this operation.
        return Verdict::NeedsGrant {
            kind: GrantKind::Operation,
            exact: false,
            reason: DenialReason::GrantRequired,
        };
    }
    Verdict::Allow
}
