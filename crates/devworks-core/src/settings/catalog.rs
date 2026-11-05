//! The typed setting descriptor catalog: every key the ConfigurationService accepts, with its
//! category, label, Help ID, allowed scopes, value type, default, adoption boundary and whether it
//! is a harness-enforced policy rather than a user override value.
//!
//! Unknown keys fail closed. Each descriptor publishes an exact value schema reference
//! ([`SettingDescriptor::schema_ref`]): `devworks.settings/<snake_key>` at [`SCHEMA_REVISION`],
//! with the SHA-256 of the JCS-canonical schema document, so a `SchemaValue` written against a
//! different schema is detected instead of being reinterpreted.
//!
//! Defaults marked "engineering default" in descriptions are technical starting values, not
//! user-confirmed product decisions.
use crate::fingerprint::sha256_hex;
use crate::jcs;
use crate::protocol::Revision;
use crate::protocol::provider::{Digest, QualifiedId, SchemaRef};
use serde_json::{Value, json};

/// Revision of every value schema in this catalog build.
pub const SCHEMA_REVISION: i64 = 1;
/// Revision of the catalog (keys, defaults, Help IDs) recorded with every saved revision.
pub const CATALOG_REVISION: i64 = 7;

/// Settings categories, in navigation order. Labels follow the approved UI reference.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Category {
    General,
    Routing,
    Tools,
    Safety,
    GitHub,
    Delivery,
    Agents,
    Instructions,
    Containers,
    Knowledge,
    Power,
    Storage,
}
impl Category {
    pub const ALL: [Category; 12] = [
        Self::General,
        Self::Routing,
        Self::Tools,
        Self::Safety,
        Self::GitHub,
        Self::Delivery,
        Self::Agents,
        Self::Instructions,
        Self::Containers,
        Self::Knowledge,
        Self::Power,
        Self::Storage,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::General => "General & repository paths",
            Self::Routing => "Providers & routing",
            Self::Tools => "Agent tools",
            Self::Safety => "Access & safety",
            Self::GitHub => "GitHub account & sync",
            Self::Delivery => "Review & delivery",
            Self::Agents => "Agents & context",
            Self::Instructions => "Role instructions",
            Self::Containers => "Containers",
            Self::Knowledge => "Documents & retrieval",
            Self::Power => "Notifications & power",
            Self::Storage => "Storage & import",
        }
    }
    /// Local Help topic for the category's Settings section.
    pub fn help_id(self) -> &'static str {
        match self {
            Self::General => "settings-General",
            Self::Routing => "settings-Routing",
            Self::Tools => "settings-Tools",
            Self::Safety => "settings-Safety",
            Self::GitHub => "settings-GitHub",
            Self::Delivery => "settings-Delivery",
            Self::Agents => "settings-Agents",
            Self::Instructions => "settings-Instructions",
            Self::Containers => "settings-Containers",
            Self::Knowledge => "settings-Knowledge",
            Self::Power => "settings-Notifications",
            Self::Storage => "settings-Storage",
        }
    }
}

/// Scope kinds a setting may be configured at. Inheritance runs Workspace → Project.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ScopeKind {
    Workspace,
    Project,
}

/// When a saved change takes effect for already-running work (SET-04).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Adoption {
    /// Presentation/host preference applied as soon as it is saved.
    Immediate,
    /// Running agents adopt it at their next safe step; completed work keeps the revision it used.
    NextSafeStep,
    /// Applied at the next admission/scheduling decision that reads it.
    NextAdmission,
    /// Only new clones use it; existing repositories keep their recorded paths.
    NewClonesOnly,
}
impl Adoption {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Immediate => "Immediate",
            Self::NextSafeStep => "NextSafeStep",
            Self::NextAdmission => "NextAdmission",
            Self::NewClonesOnly => "NewClonesOnly",
        }
    }
}

/// Value type of a setting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueKind {
    Bool,
    Integer {
        min: i64,
        max: i64,
    },
    Choice(&'static [&'static str]),
    /// A local directory path (absolute, or `~`-relative to the user's home).
    Directory,
    /// Role instruction text. Text only: it can never carry tool grants or policy.
    Instructions,
    /// Ordered unique model IDs.
    ModelList,
    /// Signed package index sources: at most 16 `{"location", "publicKey"}` objects, the
    /// location an `https://` index URL or an absolute local directory, the key 64 hex digits
    /// of an Ed25519 public key.
    PackageIndexSources,
    /// A host-minted secret-store reference `{"secretRef": "dwsecret-<opaque id>"}` (see
    /// [`super::validate::is_secret_ref`]); never a credential value.
    SecretRef,
}

/// Whether a setting is a user value or a harness-enforced policy shown for transparency.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WritePolicy {
    Configurable,
    /// Mandatory protection (read-only reviewers, main protection, ...). Not an override value:
    /// every write is refused and the built-in value is always effective.
    HarnessEnforced,
}

/// Agent roles whose instructions are settings (SET-03).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AgentRole {
    WorkspaceCoordinator,
    FeatureCoordinator,
    IssueCoordinator,
    Worker,
    CodeReviewer,
    Dreaming,
    Scheduled,
}
impl AgentRole {
    pub const ALL: [AgentRole; 7] = [
        Self::WorkspaceCoordinator,
        Self::FeatureCoordinator,
        Self::IssueCoordinator,
        Self::Worker,
        Self::CodeReviewer,
        Self::Dreaming,
        Self::Scheduled,
    ];
    /// Harness policy of the role, independent of its instruction text: code reviewers are
    /// always read-only and can never write settings or propose changes.
    pub fn read_only(self) -> bool {
        matches!(self, Self::CodeReviewer)
    }
    /// Stable record name of the role (host authority bindings).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WorkspaceCoordinator => "workspaceCoordinator",
            Self::FeatureCoordinator => "featureCoordinator",
            Self::IssueCoordinator => "issueCoordinator",
            Self::Worker => "worker",
            Self::CodeReviewer => "codeReviewer",
            Self::Dreaming => "dreaming",
            Self::Scheduled => "scheduled",
        }
    }
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|r| r.as_str() == name)
    }
    pub fn instructions_key(self) -> &'static str {
        match self {
            Self::WorkspaceCoordinator => "roles.workspaceCoordinator.instructions",
            Self::FeatureCoordinator => "roles.featureCoordinator.instructions",
            Self::IssueCoordinator => "roles.issueCoordinator.instructions",
            Self::Worker => "roles.worker.instructions",
            Self::CodeReviewer => "roles.codeReviewer.instructions",
            Self::Dreaming => "roles.dreaming.instructions",
            Self::Scheduled => "roles.scheduled.instructions",
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::WorkspaceCoordinator => "Workspace coordinator instructions",
            Self::FeatureCoordinator => "Feature coordinator instructions",
            Self::IssueCoordinator => "Issue coordinator instructions",
            Self::Worker => "Implementation agent instructions",
            Self::CodeReviewer => "Code review agent instructions",
            Self::Dreaming => "Dreaming agent instructions",
            Self::Scheduled => "Scheduled agent instructions",
        }
    }
}

/// One setting.
#[derive(Clone, Copy, Debug)]
pub struct SettingDescriptor {
    pub key: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    pub category: Category,
    pub scopes: &'static [ScopeKind],
    pub kind: ValueKind,
    pub default: fn() -> Value,
    pub adoption: Adoption,
    pub policy: WritePolicy,
}
impl SettingDescriptor {
    pub fn allows(&self, scope: ScopeKind) -> bool {
        self.scopes.contains(&scope)
    }
    pub fn locked(&self) -> bool {
        self.policy == WritePolicy::HarnessEnforced
    }
    pub fn help_id(&self) -> &'static str {
        self.category.help_id()
    }
    pub fn default_value(&self) -> Value {
        (self.default)()
    }
    /// Qualified schema ID: `devworks.settings/` and the key in lower snake case.
    pub fn schema_id(&self) -> String {
        let mut out = String::from("devworks.settings/");
        for c in self.key.chars() {
            if c.is_ascii_uppercase() {
                out.push('_');
                out.push(c.to_ascii_lowercase());
            } else {
                out.push(c);
            }
        }
        out
    }
    /// The JSON Schema document of the configured value (a `null` assignment clears the scope's
    /// value and is not part of the value schema).
    pub fn schema_document(&self) -> Value {
        let body = match self.kind {
            ValueKind::Bool => json!({"type": "boolean"}),
            // Signed 64-bit bounds beyond 2^53 are not interoperable JSON numbers.
            ValueKind::Integer { min, max: i64::MAX } => {
                json!({"type": "integer", "minimum": min, "format": "int64"})
            }
            ValueKind::Integer { min, max } => {
                json!({"type": "integer", "minimum": min, "maximum": max})
            }
            ValueKind::Choice(options) => json!({"enum": options}),
            ValueKind::Directory => json!({"type": "string", "minLength": 1, "maxLength": 4096}),
            ValueKind::Instructions => json!({"type": "string", "maxLength": 65536}),
            ValueKind::ModelList => json!({
                "type": "array", "maxItems": 32, "uniqueItems": true,
                "items": {"type": "string", "pattern": "^[A-Za-z0-9_-]{1,128}$"}
            }),
            ValueKind::PackageIndexSources => json!({
                "type": "array", "maxItems": 16,
                "items": {"type": "object", "required": ["location", "publicKey"],
                    "additionalProperties": false,
                    "properties": {
                        "location": {"type": "string", "minLength": 1, "maxLength": 2048},
                        "publicKey": {"type": "string", "pattern": "^[0-9a-f]{64}$"}}}
            }),
            ValueKind::SecretRef => json!({
                "type": "object", "required": ["secretRef"], "additionalProperties": false,
                "properties": {"secretRef": {"type": "string", "pattern": "^dwsecret-[A-Za-z0-9]{16,96}$"}}
            }),
        };
        json!({"$id": self.schema_id(), "revision": SCHEMA_REVISION, "value": body})
    }
    pub fn schema_ref(&self) -> SchemaRef {
        let canonical = jcs::canonicalize(&self.schema_document()).expect("static schema");
        SchemaRef {
            schema_id: QualifiedId(self.schema_id()),
            revision: Revision::new(SCHEMA_REVISION).expect("positive"),
            sha256: Digest(sha256_hex(canonical.as_bytes())),
        }
    }
}

const WORKSPACE: &[ScopeKind] = &[ScopeKind::Workspace];
const INHERITED: &[ScopeKind] = &[ScopeKind::Workspace, ScopeKind::Project];

macro_rules! setting {
    ($key:expr, $label:expr, $desc:expr, $cat:ident, $scopes:expr, $kind:expr, $default:expr, $adopt:ident) => {
        setting!(
            $key,
            $label,
            $desc,
            $cat,
            $scopes,
            $kind,
            $default,
            $adopt,
            Configurable
        )
    };
    ($key:expr, $label:expr, $desc:expr, $cat:ident, $scopes:expr, $kind:expr, $default:expr, $adopt:ident, $policy:ident) => {
        SettingDescriptor {
            key: $key,
            label: $label,
            description: $desc,
            category: Category::$cat,
            scopes: $scopes,
            kind: $kind,
            default: || json!($default),
            adoption: Adoption::$adopt,
            policy: WritePolicy::$policy,
        }
    };
}

const POSITIVE: ValueKind = ValueKind::Integer {
    min: 1,
    max: i64::MAX,
};

fn role(role: AgentRole, default: fn() -> Value) -> SettingDescriptor {
    SettingDescriptor {
        key: role.instructions_key(),
        label: role.label(),
        description: "Editable role instructions. Harness policies and selected tools still apply; instructions cannot grant tools. Agent-drafted changes need your approval and adopt at the next safe step.",
        category: Category::Instructions,
        scopes: INHERITED,
        kind: ValueKind::Instructions,
        default,
        adoption: Adoption::NextSafeStep,
        policy: WritePolicy::Configurable,
    }
}

/// The catalog in display order.
pub fn catalog() -> &'static [SettingDescriptor] {
    use std::sync::OnceLock;
    static CATALOG: OnceLock<Vec<SettingDescriptor>> = OnceLock::new();
    CATALOG.get_or_init(build)
}

fn build() -> Vec<SettingDescriptor> {
    let mut all = vec![
        setting!(
            "repositories.baseDirectory",
            "Repository base directory",
            "Default destination for new repository clones. Changing it never moves existing repositories or changes their recorded paths. Separate from app data and worktree placement. Engineering default.",
            General,
            INHERITED,
            ValueKind::Directory,
            "~/DevWorks/Repositories",
            NewClonesOnly
        ),
        setting!(
            "routing.tierMax.members",
            "Tier Max members",
            "Configured models allocated round-robin for Tier Max.",
            Routing,
            WORKSPACE,
            ValueKind::ModelList,
            ["fable", "astra"],
            NextAdmission
        ),
        setting!(
            "routing.tierHigh.members",
            "Tier High members",
            "Configured models allocated round-robin for Tier High.",
            Routing,
            WORKSPACE,
            ValueKind::ModelList,
            ["opus", "sol"],
            NextAdmission
        ),
        setting!(
            "routing.tierMid.members",
            "Tier Mid members",
            "Configured models allocated round-robin for Tier Mid.",
            Routing,
            WORKSPACE,
            ValueKind::ModelList,
            ["sonnet", "terra"],
            NextAdmission
        ),
        setting!(
            "routing.tierLow.members",
            "Tier Low members",
            "Configured models allocated round-robin for Tier Low.",
            Routing,
            WORKSPACE,
            ValueKind::ModelList,
            ["haiku", "luna"],
            NextAdmission
        ),
        setting!(
            "tools.webSearch",
            "Web search",
            "Allow agents the shared web search tool (code review agents included). Searches go to the provider configured below.",
            Tools,
            WORKSPACE,
            ValueKind::Bool,
            true,
            NextSafeStep
        ),
        setting!(
            WEB_FETCH_TOOLS_KEY,
            "Web page fetching",
            "Allow agents the shared web page and documentation fetch tool (code review agents included). Fetched pages are kept as untrusted artifacts.",
            Tools,
            WORKSPACE,
            ValueKind::Bool,
            true,
            NextSafeStep
        ),
        setting!(
            WEB_SEARCH_PROVIDER_KEY,
            "Web search provider",
            "The external search service web search uses. None turns searching off: a search is refused as unsupported rather than answered with no results. Brave Search needs an API key, kept in the OS secret store.",
            Tools,
            WORKSPACE,
            ValueKind::Choice(&["none", "brave"]),
            "none",
            NextAdmission
        ),
        setting!(
            WEB_SEARCH_CREDENTIAL_KEY,
            "Web search API key",
            "Reference to the search provider's API key in the OS secret store. The key itself is never stored in configuration, history or logs, and is sent only to the provider.",
            Tools,
            WORKSPACE,
            ValueKind::SecretRef,
            Value::Null,
            NextAdmission
        ),
        setting!(
            WEB_FETCH_MAX_REDIRECTS_KEY,
            "Fetch redirect limit",
            "Most redirects one fetch follows. Every hop is checked against the network policy again and recorded. Five covers ordinary http-to-https, canonical-host and documentation-version redirects. Engineering default.",
            Tools,
            WORKSPACE,
            ValueKind::Integer { min: 0, max: 20 },
            5,
            NextAdmission
        ),
        setting!(
            WEB_TIMEOUT_KEY,
            "Web request time limit (ms)",
            "Longest a web fetch or search may take end to end, whatever the caller asks: name resolution, connection, TLS, every redirect and the body. Thirty seconds fits slow documentation hosts while keeping a stuck page from holding an agent. Engineering default.",
            Tools,
            WORKSPACE,
            ValueKind::Integer {
                min: 1_000,
                max: 120_000
            },
            30_000,
            NextAdmission
        ),
        setting!(
            WEB_FETCH_PRIVATE_KEY,
            "Fetch from private networks",
            "Allow fetching from loopback and private network addresses, for example a local documentation server. Off by default so a page or agent cannot reach services on this machine or network. Link-local and cloud metadata addresses stay blocked either way.",
            Tools,
            WORKSPACE,
            ValueKind::Bool,
            false,
            NextAdmission
        ),
        setting!(
            PACKAGE_INDEX_SOURCES_KEY,
            "Package index sources",
            "Signed publisher indexes the Add-ons screen lists available packs from: an https index URL or a local directory holding devworks-index.json, each with the publisher's Ed25519 public key (hex) the index must be signed with. Only a person applies a change (an agent's change is a proposal). None by default.",
            Tools,
            WORKSPACE,
            ValueKind::PackageIndexSources,
            Vec::<serde_json::Value>::new(),
            NextAdmission
        ),
        setting!(
            "tools.shell",
            "Shell commands",
            "Allow agents the authorized process execution tools.",
            Tools,
            WORKSPACE,
            ValueKind::Bool,
            true,
            NextSafeStep
        ),
        setting!(
            "safety.accessProfile",
            "Agent access",
            "Full access, the initial default, lets agents use their selected tools with your ordinary user rights; it is not an OS sandbox. Project scope keeps brokered work inside registered repositories and task worktrees and refuses native shell, package and MCP execution unless you grant a bounded exception, because no confined launcher is qualified. Reviewer restrictions, approvals, main-branch protection and the destructive-operation guard apply in both.",
            Safety,
            INHERITED,
            ValueKind::Choice(&["full", "projectScope"]),
            "full",
            Immediate
        ),
        setting!(
            "safety.reviewersReadOnly",
            "Code review agents are read-only",
            "Reviewers receive read tools only; write/edit and indirect mutations are always denied.",
            Safety,
            WORKSPACE,
            ValueKind::Bool,
            true,
            Immediate,
            HarnessEnforced
        ),
        setting!(
            "safety.protectMainBranch",
            "Main branch protection",
            "Never force-push main; feature branch force-push requests require approval.",
            Safety,
            WORKSPACE,
            ValueKind::Bool,
            true,
            Immediate,
            HarnessEnforced
        ),
        setting!(
            "safety.destructiveOperationGuard",
            "Destructive operation guard",
            "Check tool calls before dispatch to prevent whole-drive or repository deletion.",
            Safety,
            WORKSPACE,
            ValueKind::Bool,
            true,
            Immediate,
            HarnessEnforced
        ),
        setting!(
            "github.credential",
            "GitHub credential",
            "Reference to the GitHub credential in the OS secret store. The credential value itself is never stored in configuration.",
            GitHub,
            WORKSPACE,
            ValueKind::SecretRef,
            Value::Null,
            NextAdmission
        ),
        setting!(
            "github.pollForChanges",
            "Poll GitHub for changes",
            "Pushes, merges, reviews and checks notify agents through authenticated polling.",
            GitHub,
            INHERITED,
            ValueKind::Bool,
            true,
            NextAdmission
        ),
        setting!(
            "github.importOpenIssues",
            "Import open issues",
            "Import existing open issues when a repository is connected and newly reported ones thereafter.",
            GitHub,
            INHERITED,
            ValueKind::Bool,
            true,
            NextAdmission
        ),
        setting!(
            "github.syncLinkedIssueFields",
            "Synchronize linked issue fields",
            "Two-way synchronization of linked issue fields.",
            GitHub,
            INHERITED,
            ValueKind::Bool,
            true,
            NextAdmission
        ),
        setting!(
            "github.issueTriageIntervalSeconds",
            "Issue triage interval (seconds)",
            "App-wide minimum interval between automatic issue triage admissions.",
            GitHub,
            WORKSPACE,
            POSITIVE,
            1800,
            NextAdmission
        ),
        setting!(
            "delivery.autoMerge",
            "Auto-Merge",
            "Commit, push, address review feedback and merge when all required checks pass.",
            Delivery,
            INHERITED,
            ValueKind::Bool,
            true,
            NextAdmission
        ),
        setting!(
            "delivery.mandatoryReview",
            "Mandatory review",
            "Every delivery is reviewed before merge. A harness protection, not an override value.",
            Delivery,
            WORKSPACE,
            ValueKind::Bool,
            true,
            Immediate,
            HarnessEnforced
        ),
        setting!(
            "delivery.maxReviewFixCycles",
            "Review fix cycles",
            "Maximum review/fix cycles before asking for help. Engineering default.",
            Delivery,
            INHERITED,
            ValueKind::Integer { min: 1, max: 20 },
            3,
            NextAdmission
        ),
        setting!(
            "reviews.automationWaitSeconds",
            "Automated review wait (seconds)",
            "Finite wait for a registered review source before the review gate escalates. Engineering default.",
            Delivery,
            INHERITED,
            POSITIVE,
            900,
            NextAdmission
        ),
        setting!(
            "agents.maxSubagents",
            "Default maximum subagents",
            "Applies independently per feature and issue coordinator.",
            Agents,
            INHERITED,
            ValueKind::Integer { min: 1, max: 1000 },
            10,
            NextSafeStep
        ),
        setting!(
            "agents.compactionThresholdPercent",
            "Context compaction threshold (%)",
            "Context usage at which conversations are compacted.",
            Agents,
            WORKSPACE,
            ValueKind::Integer { min: 50, max: 95 },
            80,
            NextSafeStep
        ),
    ];
    all.extend([
        role(AgentRole::WorkspaceCoordinator, || {
            json!(
                "Coordinate work across projects. Answer status questions and delegate to feature and issue coordinators."
            )
        }),
        role(AgentRole::FeatureCoordinator, || {
            json!("Plan and coordinate one feature against its approved plan.")
        }),
        role(AgentRole::IssueCoordinator, || {
            json!("Triage, reproduce and coordinate one standalone issue.")
        }),
        role(AgentRole::Worker, || {
            json!("Implement the assigned task card and report verifiable evidence.")
        }),
        role(AgentRole::CodeReviewer, || {
            json!("Review the exact head for correctness and report findings.")
        }),
        role(AgentRole::Dreaming, || {
            json!("Consolidate Knowledge into checkpointed proposals.")
        }),
        role(AgentRole::Scheduled, || {
            json!("Run the scheduled job definition within its recorded scope.")
        }),
    ]);
    all.extend([
        setting!(
            "containers.preferredRuntime",
            "Preferred container runtime",
            "Detect availability and capabilities; never silently switch to a different backend.",
            Containers,
            INHERITED,
            ValueKind::Choice(&["docker", "podman", "apple"]),
            "docker",
            NextAdmission
        ),
        setting!(
            "knowledge.visionExtraction",
            "Vision extraction fallback",
            "Extract existing text first; use a vision-capable routed model for scanned pages.",
            Knowledge,
            WORKSPACE,
            ValueKind::Bool,
            true,
            NextAdmission
        ),
        setting!(
            "knowledge.lessonRecurrenceThreshold",
            "Lesson recurrence threshold",
            "Distinct source events after which a lesson an agent or reviewer inferred takes effect as a rule, without approval. A rule from your own words takes effect at once. Engineering default.",
            Knowledge,
            INHERITED,
            ValueKind::Integer { min: 2, max: 20 },
            3,
            NextAdmission
        ),
        setting!(
            "power.keepAwake",
            "Keep the computer awake while DevWorks is open",
            "While DevWorks is open, ask the operating system not to sleep when idle, even with no active work; the display may still turn off. It does not prevent explicit sleep, closing the lid, shutdown or every power policy. Closing DevWorks or turning this off releases the request.",
            Power,
            WORKSPACE,
            ValueKind::Bool,
            false,
            Immediate
        ),
        setting!(
            "storage.backupPolicy",
            "Backups",
            "Manual backup and restore only; there is no automatic backup.",
            Storage,
            WORKSPACE,
            ValueKind::Choice(&["manual"]),
            "manual",
            Immediate,
            HarnessEnforced
        ),
        setting!(
            ARTIFACT_LOCATION_KEY,
            "Artifact storage location",
            "Artifact contents are managed files under the application data root (artifacts/, with in-flight files in staging/), outside repositories and disposable worktrees, with their metadata in the workspace store. Moving them is an explicit data-root relocation, never a side effect of a settings edit.",
            Storage,
            WORKSPACE,
            ValueKind::Choice(&["applicationData"]),
            "applicationData",
            Immediate,
            HarnessEnforced
        ),
        setting!(
            ARTIFACT_RETENTION_KEY,
            "Artifact retention",
            "Artifacts are kept until they are explicitly deleted. Age, feature completion, worktree cleanup and storage pressure never remove them; only rebuildable previews are collected. Files still referenced elsewhere, being read or held by a backup stay until those references end.",
            Storage,
            WORKSPACE,
            ValueKind::Choice(&["untilExplicitDeletion"]),
            "untilExplicitDeletion",
            Immediate,
            HarnessEnforced
        ),
        setting!(
            ARTIFACT_QUOTA_KEY,
            "Artifact storage limit (bytes)",
            "Bytes of artifact content a project may hold; 0 means no limit. Reaching it refuses new saves with a visible status and never deletes anything to make room. Engineering default.",
            Storage,
            INHERITED,
            ValueKind::Integer {
                min: 0,
                max: i64::MAX
            },
            0,
            NextAdmission
        ),
        setting!(
            ARTIFACT_MAX_FILE_KEY,
            "Largest artifact file (bytes)",
            "Largest single file an artifact save or import reads. Larger sources are refused visibly. Engineering default.",
            Storage,
            WORKSPACE,
            ValueKind::Integer {
                min: 1,
                max: 1_073_741_824
            },
            67_108_864,
            NextAdmission
        ),
        setting!(
            LOG_RETENTION_DAYS_KEY,
            "Diagnostic log retention (days)",
            "Diagnostic and log records older than this are removed by the hourly retention pass and at startup; records survive restarts until then. Thirty days keeps a month of history for recurring failures and post-incident review while the log stays bounded. Evidence artifacts a record names are kept under their own retention. Engineering default.",
            Storage,
            WORKSPACE,
            ValueKind::Integer { min: 1, max: 365 },
            30,
            NextAdmission
        ),
        setting!(
            LOG_MAX_RECORDS_KEY,
            "Diagnostic log size (records)",
            "Most diagnostic and log records kept; the oldest beyond it are removed by the retention pass. With messages bounded to 8 KiB and helper output to 16 KiB per record, 50 000 records stay within about 1.2 GiB in the worst case and a few tens of megabytes typically. Engineering default.",
            Storage,
            WORKSPACE,
            ValueKind::Integer {
                min: 1_000,
                max: 1_000_000
            },
            50_000,
            NextAdmission
        ),
        setting!(
            OVERLAY_RECORD_RETENTION_DAYS_KEY,
            "Ended ref overlay retention (days)",
            "An ended ref overlay (expired, retired or failed) is pruned once it ended longer ago than this and is beyond the newest records kept per repository: its record, layers and graph data go, while its runs, history and logs stay. Live overlays and any still named by a view are never pruned. Separate from artifact retention. Engineering default.",
            Storage,
            WORKSPACE,
            ValueKind::Integer { min: 1, max: 365 },
            14,
            NextAdmission
        ),
        setting!(
            OVERLAY_RECORDS_KEPT_KEY,
            "Ended ref overlays kept per repository",
            "The most recently ended ref overlays each repository keeps whatever their age; older ones past the retention window are pruned. Engineering default.",
            Storage,
            WORKSPACE,
            ValueKind::Integer { min: 0, max: 10_000 },
            32,
            NextAdmission
        ),
    ]);
    all
}

/// Where artifact contents live (a harness policy shown for transparency).
pub const ARTIFACT_LOCATION_KEY: &str = "storage.artifactLocation";
/// Artifact retention: until explicit deletion (a harness policy; no automatic expiry).
pub const ARTIFACT_RETENTION_KEY: &str = "storage.artifactRetention";
/// Per-project artifact byte limit (0 = no limit).
pub const ARTIFACT_QUOTA_KEY: &str = "storage.artifactQuotaBytes";
/// Largest single artifact source file.
pub const ARTIFACT_MAX_FILE_KEY: &str = "storage.artifactMaxFileBytes";
/// Signed publisher index sources of available packages.
pub const PACKAGE_INDEX_SOURCES_KEY: &str = "packages.indexSources";
/// Days a diagnostic log record is retained.
pub const LOG_RETENTION_DAYS_KEY: &str = "diagnostics.logRetentionDays";
/// Most diagnostic log records retained.
pub const LOG_MAX_RECORDS_KEY: &str = "diagnostics.logMaxRecords";
/// Days an ended ref overlay's record is kept before record retention may prune it.
pub const OVERLAY_RECORD_RETENTION_DAYS_KEY: &str = "indexing.overlayRecordRetentionDays";
/// Ended ref overlays each repository keeps regardless of age.
pub const OVERLAY_RECORDS_KEPT_KEY: &str = "indexing.overlayRecordsKept";
/// Whether agents may use the shared web fetch tool.
pub const WEB_FETCH_TOOLS_KEY: &str = "tools.webFetch";
/// The configured external search provider (`none` or a provider ID).
pub const WEB_SEARCH_PROVIDER_KEY: &str = "web.searchProvider";
/// The secret-store reference of the search provider's credential.
pub const WEB_SEARCH_CREDENTIAL_KEY: &str = "web.searchCredential";
/// Most redirects one fetch follows.
pub const WEB_FETCH_MAX_REDIRECTS_KEY: &str = "web.fetchMaxRedirects";
/// Longest end-to-end web request (a fetch with its redirects, or a search), in milliseconds.
pub const WEB_TIMEOUT_KEY: &str = "web.timeoutMs";
/// Whether fetches may reach loopback and private network addresses.
pub const WEB_FETCH_PRIVATE_KEY: &str = "web.fetchAllowPrivateNetwork";

/// Looks up a descriptor by key. Unknown keys fail closed at every caller.
pub fn descriptor(key: &str) -> Option<&'static SettingDescriptor> {
    catalog().iter().find(|d| d.key == key)
}
