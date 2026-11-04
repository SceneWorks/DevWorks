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
use crate::operations::{OperationInput, git};
use crate::ports::AuthorityRequest;
use crate::ports::storage::{
    ConfigScope, ConfigurationRead, GrantKind, PrincipalBinding, StorageError,
};
use crate::protocol::ProjectId;
use crate::protocol::data::{GitCommand, PushMode};
use crate::registry::{AuthorityClass, EffectClass, Mode};
use crate::settings::catalog::{AgentRole, descriptor};
use crate::settings::model::{chain, current_chain, scope_of};

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
pub const NATIVE_EXECUTION: [&str; 8] = [
    "process.execute",
    "process.install_project_dependencies",
    "containers.build",
    "containers.pull",
    "containers.create",
    "containers.start",
    "containers.exec",
    "mcp.connect",
];

/// Commands a non-human principal may run only with an explicit approval for the exact request:
/// a system install, and the destructive project and repository-index changes (archiving fences
/// and stops a project's work at once, deletion tombstones it, index removal drops a shared
/// registration's index for every project). The user's own command is its confirmation.
pub const APPROVAL_REQUIRED: [&str; 4] = [
    "process.request_system_install",
    "projects.archive",
    "projects.delete",
    "repositories.remove_index",
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
}
impl EffectivePolicy {
    /// Stable digest of every value that changes an authorization; a fence compares it.
    pub fn digest(&self) -> String {
        sha256_hex(
            format!(
                "{}|{}|{}|{}",
                self.profile.map_or("unsupported", AccessProfile::as_str),
                self.shell_tools,
                self.web_tools,
                self.web_fetch
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
    let kind = ctx.principal_kind();
    let op = request.descriptor.name;
    let fam = family(op);
    let class = request.binding.authority;
    let command = request.binding.mode == Mode::Command;

    // Mandatory for every principal, including the user and the host.
    if let Some(target) = force_push_target(request.input)
        && protected_ref(target, protected_branches)
    {
        return Verdict::Deny(DenialReason::ProtectedBranch);
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
        && !selected.iter().any(|f| f == fam)
    {
        return Verdict::Deny(DenialReason::ToolNotSelected);
    }
    let read_only_role = role.is_some_and(AgentRole::read_only);
    if read_only_role {
        if command || request.effect != EffectClass::ReadOnly {
            return Verdict::Deny(DenialReason::ReadOnlyRole);
        }
        if UNVERIFIED_READ_FAMILIES.contains(&fam) {
            return Verdict::Deny(DenialReason::UnverifiedRead);
        }
    }
    if kind == PrincipalKind::Agent {
        if fam == "process" && !policy.shell_tools {
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
    let Some(profile) = policy.profile else {
        return Verdict::Deny(DenialReason::UnsupportedProfile);
    };
    if !command {
        return Verdict::Allow;
    }

    // Commands from here on.
    if matches!(kind, PrincipalKind::Package | PrincipalKind::McpClient)
        && class == AuthorityClass::Configuration
    {
        return Verdict::Deny(DenialReason::HumanOnly);
    }
    if force_push_target(request.input).is_some() || APPROVAL_REQUIRED.contains(&op) {
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
