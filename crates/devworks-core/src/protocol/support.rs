//! Bounded wire values, the `Check` trait and the closed domain fault model adopted from the
//! recorded candidates (tools/contracts-eval/src/domain_support.rs). No authority decisions.
use super::shared::*;
use schemars::JsonSchema;
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
    super::provider::Digest,
    super::provider::SchemaRef,
    super::provider::Attribution,
    super::data::DataScope,
    super::data::ViewSelection,
    super::data::Coverage
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
macro_rules! int_bound {($n:ident,$lo:expr,$hi:expr)=>{
 #[derive(Clone,Debug,Serialize)]#[serde(transparent)]pub struct $n(u32);
 impl<'de> Deserialize<'de> for $n{fn deserialize<D:Deserializer<'de>>(d:D)->Result<Self,D::Error>{let x=u32::deserialize(d)?;if !($lo..=$hi).contains(&x){return Err(D::Error::custom("integer bound"))}Ok(Self(x))}}
 impl JsonSchema for $n{fn schema_name()->Cow<'static,str>{stringify!($n).into()}fn json_schema(_: &mut SchemaGenerator)->Schema{json_schema!({"type":"integer","minimum":$lo,"maximum":$hi})}}
 impl Check for $n{fn check(&self)->Result<(),String>{Ok(())}}
 impl $n{pub fn get(&self)->u32{self.0}}
};}
int_bound!(Limit, 1, 500);
int_bound!(Depth, 0, 32);
int_bound!(Port, 1, 65535);
int_bound!(ByteLimit, 1, 1048576);
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
text_rule!(
    RelativePath,
    r"^(?:(?!\.{1,2}(?:/|$))[^/\u0000]+)(?:/(?:(?!\.{1,2}(?:/|$))[^/\u0000]+))*$",
    |s: &str| s.len() <= 4096
        && !s.starts_with('/')
        && !s.contains('\0')
        && s.split('/').all(|x| !x.is_empty() && x != "." && x != "..")
);
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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
    super::provider::ArtifactRef,
    super::provider::TranscriptPage,
    super::data::GitOid,
    super::data::GraphPage,
    super::data::RetrievalPage,
    super::data::DeliveryBarrier,
    super::data::DeliverySnapshot
);
