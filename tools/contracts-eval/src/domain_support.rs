//! Bounded wire values for architecture DTOs. No application authority implementation.
use crate::*;
use schemars::{Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer, Serialize, de::Error};
use std::borrow::Cow;
pub trait Check {
    fn check(&self) -> Result<(), String>;
}
macro_rules! checked_leaf {($($t:ty),*)=>{$(impl Check for $t{fn check(&self)->Result<(),String>{Ok(())}})*};}
checked_leaf!(
    bool,
    i32,
    Id,
    Revision,
    Counter,
    ProjectId,
    RootId,
    ActorId,
    AssignmentId,
    OperationId,
    QueryId,
    RequestKey,
    CommandId,
    EventId,
    StoreId,
    ResourceId,
    CorrelationId,
    ReadStamp,
    OperationState,
    RootControl,
    Activation,
    Blockers,
    EligibilityTarget,
    Purpose,
    Event,
    crate::provider::Digest,
    crate::provider::SchemaRef,
    crate::provider::Attribution,
    crate::data_delivery::DataScope,
    crate::data_delivery::ViewSelection,
    crate::data_delivery::Coverage
);
impl<T: Check> Check for Option<T> {
    fn check(&self) -> Result<(), String> {
        if let Some(x) = self {
            x.check()?
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize)]
#[serde(transparent)]
pub struct BoundedText<const MIN: usize, const MAX: usize>(pub String);
impl<'de, const MIN: usize, const MAX: usize> Deserialize<'de> for BoundedText<MIN, MAX> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        let n = s.chars().count();
        if n < MIN || n > MAX || s.contains('\0') {
            return Err(D::Error::custom("text length or NUL outside contract"));
        }
        Ok(Self(s))
    }
}
impl<const MIN: usize, const MAX: usize> JsonSchema for BoundedText<MIN, MAX> {
    fn schema_name() -> Cow<'static, str> {
        format!("Text{MIN}To{MAX}").into()
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type":"string","minLength":MIN,"maxLength":MAX,"pattern":"^[^\\u0000]*$"})
    }
}
impl<const MIN: usize, const MAX: usize> Check for BoundedText<MIN, MAX> {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}
pub type Label = BoundedText<1, 256>;
pub type Text = BoundedText<1, 4096>;
pub type Content = BoundedText<0, 65536>;
pub type Cursor = BoundedText<1, 4096>;
#[derive(Clone, Debug, Serialize)]
#[serde(transparent)]
pub struct BoundedList<T, const MIN: usize, const MAX: usize>(pub Vec<T>);
impl<'de, T: Deserialize<'de>, const MIN: usize, const MAX: usize> Deserialize<'de>
    for BoundedList<T, MIN, MAX>
{
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = Vec::<T>::deserialize(d)?;
        if v.len() < MIN || v.len() > MAX {
            return Err(D::Error::custom("list length outside contract"));
        }
        Ok(Self(v))
    }
}
impl<T: JsonSchema, const MIN: usize, const MAX: usize> JsonSchema for BoundedList<T, MIN, MAX> {
    fn schema_name() -> Cow<'static, str> {
        format!("List_{}_{}_{}", T::schema_name(), MIN, MAX).into()
    }
    fn json_schema(g: &mut SchemaGenerator) -> Schema {
        let mut s = g.subschema_for::<Vec<T>>();
        s.insert("minItems".into(), MIN.into());
        s.insert("maxItems".into(), MAX.into());
        s
    }
}
impl<T: Check, const MIN: usize, const MAX: usize> Check for BoundedList<T, MIN, MAX> {
    fn check(&self) -> Result<(), String> {
        for v in &self.0 {
            v.check()?
        }
        Ok(())
    }
}
pub type PlanItems<T> = BoundedList<T, 0, 200>;
pub type PlanFeatures<T> = BoundedList<T, 1, 20>;
pub type Items<T> = BoundedList<T, 0, 128>;
pub type NonEmpty<T> = BoundedList<T, 1, 128>;
pub type Rows<T> = BoundedList<T, 0, 500>;
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Page<T> {
    pub items: Rows<T>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
    pub has_more: bool,
}
impl<T: Check> Check for Page<T> {
    fn check(&self) -> Result<(), String> {
        self.items.check()?;
        if self.has_more != self.next_cursor.is_some() {
            return Err("page continuation mismatch".into());
        }
        Ok(())
    }
}
// A page of committed history (`history.events`, `.work_lineage`, `.git_links`,
// `.evidence_links`). Revision 1 is exactly the recorded [`Page`]: `nextCursor` is present
// exactly when `hasMore`. Schema revision 2 adds `checkpoint`, the continuation after the last
// fact this page covers, returned on every page including the current tail (an empty page keeps
// the position it was asked from), so a poller resumes without replaying the history. While
// `hasMore`, the checkpoint is the `nextCursor`. It keeps the recorded `Page` schema name, so
// revision-1 documents are unchanged.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schemars(rename = "Page")]
pub struct HistoryPage<T> {
    pub items: Rows<T>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
    pub has_more: bool,
    /// Schema revision 2: the continuation checkpoint (present on every revision-2 page).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Cursor", extend("x-since" = 2))]
    pub checkpoint: Option<Cursor>,
}
impl<T: Check> Check for HistoryPage<T> {
    fn check(&self) -> Result<(), String> {
        self.items.check()?;
        if self.has_more != self.next_cursor.is_some() {
            return Err("page continuation mismatch".into());
        }
        if let (Some(next), Some(checkpoint)) = (&self.next_cursor, &self.checkpoint) {
            if next.0 != checkpoint.0 {
                return Err("a page with more facts continues at its checkpoint".into());
            }
        }
        Ok(())
    }
}
/// A value whose wire shape grew in a later operation schema revision (members annotated
/// `x-since`). `retain_revision(r)` clears every member introduced after revision `r`.
pub trait Revisioned {
    fn retain_revision(&mut self, revision: u32);
    fn check_revision(&self, _revision: u32) -> Result<(), String> { Ok(()) }
}
impl<T: Revisioned> Revisioned for Option<T> {
    fn retain_revision(&mut self, revision: u32) {
        if let Some(x) = self {
            x.retain_revision(revision)
        }
    }
}
impl<T: Revisioned> Revisioned for Vec<T> {
    fn retain_revision(&mut self, revision: u32) {
        for x in self {
            x.retain_revision(revision)
        }
    }
}
impl<T: Revisioned, const MIN: usize, const MAX: usize> Revisioned for BoundedList<T, MIN, MAX> {
    fn retain_revision(&mut self, revision: u32) {
        self.0.retain_revision(revision)
    }
}
impl<T: Revisioned> Revisioned for Page<T> {
    fn retain_revision(&mut self, revision: u32) {
        self.items.retain_revision(revision)
    }
}
impl<T: Revisioned> Revisioned for HistoryPage<T> {
    fn retain_revision(&mut self, revision: u32) {
        if revision < 2 {
            self.checkpoint = None;
        }
        self.items.retain_revision(revision)
    }
}
/// Events gate a union alternative (the command aggregate), which cannot be cleared from a
/// value: the producer never emits one below revision 2, and [`Event::validate_revision`] is the
/// earlier revision's refusal.
impl Revisioned for Event {
    fn retain_revision(&mut self, _: u32) {}
}
/// Deserializer of a member introduced in a later schema revision: it is either omitted or
/// carries a value. It has no explicit-null spelling, so its presence is always visible to the
/// strict reader of an earlier revision (which refuses it rather than discarding it).
pub fn gated_member<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> Result<Option<T>, D::Error> {
    Option::<T>::deserialize(d)?
        .map(Some)
        .ok_or_else(|| D::Error::custom("a revision-gated member is omitted, never null"))
}
macro_rules! int_bound {($n:ident,$lo:expr,$hi:expr)=>{
 #[derive(Clone,Debug,Serialize)]#[serde(transparent)]pub struct $n(u32);
 impl<'de> Deserialize<'de> for $n{fn deserialize<D:Deserializer<'de>>(d:D)->Result<Self,D::Error>{let x=u32::deserialize(d)?;if !($lo..=$hi).contains(&x){return Err(D::Error::custom("integer bound"))}Ok(Self(x))}}
 impl JsonSchema for $n{fn schema_name()->Cow<'static,str>{stringify!($n).into()}fn json_schema(_: &mut SchemaGenerator)->Schema{json_schema!({"type":"integer","minimum":$lo,"maximum":$hi})}}
 impl Check for $n{fn check(&self)->Result<(),String>{Ok(())}}
};}
int_bound!(Limit, 1, 500);
int_bound!(Depth, 0, 32);
int_bound!(Port, 1, 65535);
int_bound!(ByteLimit, 1, 1048576);
// A briefing's estimated-token cap (`sessions.briefing` `budgetTokens`, schema revision 2).
int_bound!(TokenBudget, 1, 1000000);
macro_rules! text_rule {($n:ident,$pattern:expr,$validate:expr)=>{
 #[derive(Clone,Debug,Serialize)]#[serde(transparent)]pub struct $n(pub String);
 impl<'de> Deserialize<'de> for $n{fn deserialize<D:Deserializer<'de>>(d:D)->Result<Self,D::Error>{let s=String::deserialize(d)?;if !($validate)(&s){return Err(D::Error::custom(concat!("invalid ",stringify!($n))))}Ok(Self(s))}}
 impl JsonSchema for $n{fn schema_name()->Cow<'static,str>{stringify!($n).into()}fn json_schema(_: &mut SchemaGenerator)->Schema{json_schema!({"type":"string","minLength":1,"maxLength":4096,"pattern":$pattern})}}
 impl Check for $n{fn check(&self)->Result<(),String>{Ok(())}}
};}
text_rule!(Url, "^https?://[^\\s]+$", |s: &str| s.len() <= 4096
    && (s.starts_with("https://") || s.starts_with("http://"))
    && !s.chars().any(char::is_whitespace)
    && s.split("://").nth(1).is_some_and(|x| !x.is_empty())
    && !s.contains('\0'));
// A repository-relative path. Both `/` and `\` separate segments, so no platform reads it as
// absolute or as escaping its root: no leading separator (`/x`, `\x`, `\\server`), no drive
// form (`C:`, `C:x`, `C:\x`), no empty, `.` or `..` segment (either separator), no NUL.
text_rule!(
    RelativePath,
    r"^(?![A-Za-z]:)(?:(?!\.{1,2}(?:[/\\]|$))[^/\\\u0000]+)(?:[/\\](?:(?!\.{1,2}(?:[/\\]|$))[^/\\\u0000]+))*$",
    |s: &str| s.len() <= 4096
        && !s.contains('\0')
        && !s.starts_with(['/', '\\'])
        && !(s.len() >= 2 && s.as_bytes()[1] == b':' && s.as_bytes()[0].is_ascii_alphabetic())
        && s.split(['/', '\\']).all(|x| !x.is_empty() && x != "." && x != "..")
);
/// Distinct repository-relative paths, 1-128 of them (a set: decoding refuses a duplicate).
#[derive(Clone, Debug, Serialize)]
#[serde(transparent)]
pub struct PathSet(pub Vec<RelativePath>);
impl<'de> Deserialize<'de> for PathSet {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = Vec::<RelativePath>::deserialize(d)?;
        if v.is_empty() || v.len() > 128 {
            return Err(D::Error::custom("path set length outside contract"));
        }
        let mut seen = std::collections::HashSet::new();
        if !v.iter().all(|p| seen.insert(p.0.clone())) {
            return Err(D::Error::custom("duplicate path"));
        }
        Ok(Self(v))
    }
}
impl JsonSchema for PathSet {
    fn schema_name() -> Cow<'static, str> {
        "PathSet".into()
    }
    fn json_schema(g: &mut SchemaGenerator) -> Schema {
        let item = g.subschema_for::<RelativePath>();
        json_schema!({"type":"array","items":item,"minItems":1,"maxItems":128,"uniqueItems":true})
    }
}
impl Check for PathSet {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}
text_rule!(
    Money,
    "^(0|[1-9][0-9]{0,11})(\\.[0-9]{1,12})?$",
    |s: &str| {
        let v: Vec<_> = s.split('.').collect();
        !v.is_empty()
            && v.len() <= 2
            && !v[0].is_empty()
            && v[0].len() <= 12
            && v[0].bytes().all(|x| x.is_ascii_digit())
            && (v[0] == "0" || !v[0].starts_with('0'))
            && (v.len() == 1
                || (!v[1].is_empty()
                    && v[1].len() <= 12
                    && v[1].bytes().all(|x| x.is_ascii_digit())))
    }
);
text_rule!(SignedCounter, "^(0|-?[1-9][0-9]{0,18})$", |s: &str| s
    .parse::<i64>()
    .is_ok()
    && s != "-0"
    && !s.starts_with('+')
    && (s == "0" || !s.trim_start_matches('-').starts_with('0')));
text_rule!(PositiveCounter, "^[1-9][0-9]{0,18}$", |s: &str| s
    .parse::<i64>()
    .is_ok_and(|x| x > 0)
    && !s.starts_with(['0', '+', '-']));
#[derive(Clone, Debug, Serialize)]
#[serde(transparent)]
pub struct Finite(f64);
impl<'de> Deserialize<'de> for Finite {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let x = f64::deserialize(d)?;
        if !x.is_finite() {
            return Err(D::Error::custom("nonfinite"));
        }
        Ok(Self(x))
    }
}
impl JsonSchema for Finite {
    fn schema_name() -> Cow<'static, str> {
        "Finite".into()
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type":"number"})
    }
}
impl Check for Finite {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize)]
#[serde(transparent)]
pub struct Probability(f64);
impl<'de> Deserialize<'de> for Probability {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let x = f64::deserialize(d)?;
        if !x.is_finite() || !(0.0..=1.0).contains(&x) {
            return Err(D::Error::custom("probability bound"));
        }
        Ok(Self(x))
    }
}
impl JsonSchema for Probability {
    fn schema_name() -> Cow<'static, str> {
        "Probability".into()
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type":"number","minimum":0,"maximum":1})
    }
}
impl Check for Probability {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}
/// Closed selected-command arguments retained for exact human review and re-admission.
/// The outer frame and actual selected operation enforce their own limits. In particular, a
/// legal inner DynamicJson retains its own byte/depth/node bounds, without applying those
/// narrower bounds again to the aggregate argument object.
#[derive(Clone, Debug, Serialize)]
#[serde(transparent)]
pub struct SavedCommandInput(pub serde_json::Value);
impl<'de> Deserialize<'de> for SavedCommandInput {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = Self(serde_json::Value::deserialize(d)?);
        value.check().map_err(D::Error::custom)?;
        Ok(value)
    }
}
impl JsonSchema for SavedCommandInput {
    fn schema_name() -> Cow<'static, str> { "SavedCommandInput".into() }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type":"object","not":{"required":["kind"]},"description":"Closed selected-operation argument object, bounded by the existing 1MiB/100-container frame ceiling; no aggregate DynamicJson node limit. The actual selected admission rechecks operation schema, inner JSON bounds and authority. Outer proposal framing remains unchanged."})
    }
}
impl Check for SavedCommandInput {
    fn check(&self) -> Result<(), String> {
        if !self.0.is_object() || self.0.get("kind").is_some() {
            return Err("saved command input is the closed argument object without operation kind".into());
        }
        let encoded = serde_json::to_vec(&self.0).map_err(|e| e.to_string())?;
        if encoded.len() > 1024 * 1024 { return Err("saved command input exceeds frame byte limit".into()); }
        fn within_depth(v: &serde_json::Value, depth: usize) -> bool {
            match v {
                serde_json::Value::Array(values) => depth < 100 && values.iter().all(|v| within_depth(v, depth + 1)),
                serde_json::Value::Object(values) => depth < 100 && values.values().all(|v| within_depth(v, depth + 1)),
                _ => true,
            }
        }
        if !within_depth(&self.0, 0) { return Err("saved command input exceeds frame nesting limit".into()); }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize)]
#[serde(transparent)]
pub struct DynamicJson(pub serde_json::Value);
impl<'de> Deserialize<'de> for DynamicJson {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = serde_json::Value::deserialize(d)?;
        let encoded = serde_json::to_vec(&v).map_err(D::Error::custom)?;
        if encoded.len() > 524288 {
            return Err(D::Error::custom("JSON byte limit"));
        }
        fn walk(v: &serde_json::Value, depth: usize, n: &mut usize) -> bool {
            *n += 1;
            if depth > 64 || *n > 10000 {
                return false;
            }
            match v {
                serde_json::Value::Array(a) => a.iter().all(|x| walk(x, depth + 1, n)),
                serde_json::Value::Object(o) => o.values().all(|x| walk(x, depth + 1, n)),
                _ => true,
            }
        }
        if !walk(&v, 0, &mut 0) {
            return Err(D::Error::custom("JSON depth/node limit"));
        }
        Ok(Self(v))
    }
}
impl JsonSchema for DynamicJson {
    fn schema_name() -> Cow<'static, str> {
        "BoundedUntrustedJson".into()
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"description":"Purpose-specific untrusted JSON: at most512KiB,depth64,nodes10000; descriptor payloads additionally require declared schema validation before use. Not an operation or authority escape hatch."})
    }
}
impl Check for DynamicJson {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum DomainErrorCode {
    NotFound,
    RevisionConflict,
    InvalidArgument,
    PermissionDenied,
    UnsupportedCapability,
    UnsupportedTarget,
    CredentialsRequired,
    ApprovalRequired,
    ContinueRequired,
    DependencyBlocked,
    CapacityWait,
    BudgetHold,
    SchemaMismatch,
    SourceChanged,
    ViewUnavailable,
    CursorExpired,
    LeaseLost,
    Conflict,
    Cancelled,
    ExternalFailure,
    OutcomeUnknown,
    StorageUnavailable,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub enum RevisionDimension {
    Work,
    Control,
    Blocker,
    Operation,
    Policy,
    Scope,
    Plan,
    Authority,
    Configuration,
    Registry,
    Workflow,
    Queue,
    Store,
    Source,
    Artifact,
    Reference,
    Review,
    Claim,
    Schedule,
    Connection,
    Package,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DomainFault {
    pub code: DomainErrorCode,
    pub retry: RetryAdvice,
    pub correlation_id: CorrelationId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_id: Option<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_kind: Option<Label>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision_dimension: Option<RevisionDimension>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<Revision>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_revision: Option<Revision>,
    pub blocker_ids: Items<HoldId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostic_id: Option<Id>,
}
impl Check for DomainFault {
    fn check(&self) -> Result<(), String> {
        use DomainErrorCode::*;
        if matches!(self.code, OutcomeUnknown)
            && !matches!(self.retry, RetryAdvice::ReconcileOperation)
        {
            return Err("unknown outcome requires reconciliation".into());
        }
        if matches!(self.code, RevisionConflict)
            && (self.target_id.is_none()
                || self.target_kind.is_none()
                || self.revision_dimension.is_none()
                || self.expected_revision.is_none()
                || self.current_revision.is_none()
                || !matches!(self.retry, RetryAdvice::RefreshAndNewRequest))
        {
            return Err("revision conflict needs current/expected and refresh".into());
        }
        if matches!(self.code, PermissionDenied) && matches!(self.retry, RetryAdvice::SameCommand) {
            return Err("permission rejection cannot automatic-retry".into());
        }
        Ok(())
    }
}

macro_rules! validated_leaf {($($t:ty),*)=>{$(impl Check for $t{fn check(&self)->Result<(),String>{self.validate().map_err(str::to_owned)}})*};}
validated_leaf!(
    crate::provider::ArtifactRef,
    crate::provider::TranscriptPage,
    crate::data_delivery::GitOid,
    crate::data_delivery::GraphPage,
    crate::data_delivery::RetrievalPage,
    crate::data_delivery::DeliveryBarrier,
    crate::data_delivery::DeliverySnapshot
);
/// Schema revision 2 adds the project scope of `history.events`.
impl Revisioned for QueryRequest {
    fn retain_revision(&mut self, revision: u32) {
        if let Query::Events { project_id, .. } = &mut self.query {
            if revision < 2 {
                *project_id = None;
            }
        }
    }
}
