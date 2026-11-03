//! Strict ingress framing applied before any envelope, version or operation interpretation.
//!
//! The common envelope bound is 1 MiB. A frame must be valid UTF-8 JSON consisting of exactly one
//! value, with no duplicate object member names (a later duplicate could otherwise silently
//! replace a validated member), no number outside IEEE-754 finite range and at most
//! [`MAX_NESTING`] nested arrays/objects. JSON text cannot spell NaN or Infinity; overflowing
//! numerals such as `1e400` are rejected rather than saturated.
use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Number, Value};
use std::cell::Cell;
use std::fmt;

/// Initial common envelope bound; capability-specific limits may be tighter.
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;
/// Maximum container nesting of a whole frame. Bounded dynamic JSON values (depth 64) sit a
/// few levels inside an envelope, so the frame limit leaves headroom above that inner limit
/// while staying below the parser's own recursion guard.
pub const MAX_NESTING: usize = 100;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrameError {
    TooLarge {
        bytes: usize,
    },
    InvalidUtf8,
    DuplicateMember(String),
    TooDeep,
    NonFiniteNumber,
    /// Syntax error, incomplete frame or trailing data.
    Syntax(String),
}
impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge { bytes } => {
                write!(f, "frame of {bytes} bytes exceeds {MAX_FRAME_BYTES}")
            }
            Self::InvalidUtf8 => f.write_str("frame is not valid UTF-8"),
            Self::DuplicateMember(name) => write!(f, "duplicate JSON member {name:?}"),
            Self::TooDeep => write!(f, "nesting exceeds {MAX_NESTING}"),
            Self::NonFiniteNumber => f.write_str("non-finite number"),
            Self::Syntax(e) => write!(f, "invalid JSON frame: {e}"),
        }
    }
}

/// Parses one bounded, strictly framed JSON value.
pub fn parse(raw: &[u8]) -> Result<Value, FrameError> {
    if raw.len() > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge { bytes: raw.len() });
    }
    std::str::from_utf8(raw).map_err(|_| FrameError::InvalidUtf8)?;
    let failure = Cell::new(None);
    let mut de = serde_json::Deserializer::from_slice(raw);
    let value = Strict {
        depth: 0,
        failure: &failure,
    }
    .deserialize(&mut de)
    .map_err(|e| {
        failure
            .take()
            .unwrap_or_else(|| FrameError::Syntax(e.to_string()))
    })?;
    de.end().map_err(|e| FrameError::Syntax(e.to_string()))?;
    Ok(value)
}

struct Strict<'a> {
    depth: usize,
    failure: &'a Cell<Option<FrameError>>,
}
impl Strict<'_> {
    fn fail<E: de::Error>(&self, error: FrameError) -> E {
        let message = error.to_string();
        self.failure.set(Some(error));
        E::custom(message)
    }
    fn nested<E: de::Error>(&self) -> Result<Strict<'_>, E> {
        if self.depth + 1 > MAX_NESTING {
            return Err(self.fail(FrameError::TooDeep));
        }
        Ok(Strict {
            depth: self.depth + 1,
            failure: self.failure,
        })
    }
}
impl<'de> DeserializeSeed<'de> for Strict<'_> {
    type Value = Value;
    fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> Result<Value, D::Error> {
        d.deserialize_any(self)
    }
}
impl<'de> Visitor<'de> for Strict<'_> {
    type Value = Value;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("strict JSON value")
    }
    fn visit_unit<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_bool<E: de::Error>(self, v: bool) -> Result<Value, E> {
        Ok(Value::Bool(v))
    }
    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Value, E> {
        Ok(Value::Number(v.into()))
    }
    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Value, E> {
        Ok(Value::Number(v.into()))
    }
    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Value, E> {
        Number::from_f64(v)
            .map(Value::Number)
            .ok_or_else(|| self.fail(FrameError::NonFiniteNumber))
    }
    fn visit_str<E: de::Error>(self, v: &str) -> Result<Value, E> {
        Ok(Value::String(v.to_owned()))
    }
    fn visit_string<E: de::Error>(self, v: String) -> Result<Value, E> {
        Ok(Value::String(v))
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let inner = self.nested()?;
        let mut out = Vec::new();
        while let Some(v) = seq.next_element_seed(Strict {
            depth: inner.depth,
            failure: inner.failure,
        })? {
            out.push(v);
        }
        Ok(Value::Array(out))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let inner = self.nested()?;
        let mut out = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if out.contains_key(&key) {
                return Err(self.fail(FrameError::DuplicateMember(key)));
            }
            let value = map.next_value_seed(Strict {
                depth: inner.depth,
                failure: inner.failure,
            })?;
            out.insert(key, value);
        }
        Ok(Value::Object(out))
    }
}
