//! RFC 8785 JSON Canonicalization Scheme (JCS) over a parsed `serde_json::Value`.
//!
//! * Object members are sorted by their names' UTF-16 code units; arrays keep their order.
//! * Strings use the minimal JSON escaping of ECMAScript `JSON.stringify` (only `"`, `\\` and
//!   C0 controls are escaped; everything else is emitted as UTF-8).
//! * Numbers are serialized like ECMAScript `Number.prototype.toString` applied to the IEEE-754
//!   double. Integers outside the I-JSON safe range `±(2^53 − 1)` are rejected instead of
//!   silently rounded (two different inputs would otherwise canonicalize identically); this
//!   matches the authoring `rfc8785` library used for the recorded evidence. Exact signed-64
//!   values travel as canonical decimal strings, never as JSON numbers.
use serde_json::{Number, Value};
use std::fmt;

/// Largest integer magnitude every IEEE-754 double consumer represents exactly.
pub const MAX_SAFE_INTEGER: i128 = (1 << 53) - 1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JcsError {
    /// An integer outside the I-JSON safe range.
    NonInteroperableNumber(String),
}
impl fmt::Display for JcsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonInteroperableNumber(n) => {
                write!(f, "integer {n} exceeds the I-JSON safe range ±(2^53 - 1)")
            }
        }
    }
}

/// Returns the canonical UTF-8 text of `value`.
pub fn canonicalize(value: &Value) -> Result<String, JcsError> {
    let mut out = String::new();
    write_value(value, &mut out)?;
    Ok(out)
}

fn write_value(value: &Value, out: &mut String) -> Result<(), JcsError> {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(n) => out.push_str(&number(n)?),
        Value::String(s) => write_string(s, out),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(item, out)?;
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut members: Vec<_> = map.iter().collect();
            members.sort_by(|(a, _), (b, _)| a.encode_utf16().cmp(b.encode_utf16()));
            out.push('{');
            for (i, (name, item)) in members.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(name, out);
                out.push(':');
                write_value(item, out)?;
            }
            out.push('}');
        }
    }
    Ok(())
}

fn write_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{9}' => out.push_str("\\t"),
            '\u{a}' => out.push_str("\\n"),
            '\u{c}' => out.push_str("\\f"),
            '\u{d}' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

fn number(n: &Number) -> Result<String, JcsError> {
    let integer = n
        .as_i64()
        .map(i128::from)
        .or_else(|| n.as_u64().map(i128::from));
    let x = if let Some(wide) = integer {
        if wide.abs() > MAX_SAFE_INTEGER {
            return Err(JcsError::NonInteroperableNumber(n.to_string()));
        }
        wide as f64
    } else {
        n.as_f64()
            .ok_or_else(|| JcsError::NonInteroperableNumber(n.to_string()))?
    };
    Ok(ecmascript_number(x))
}

/// ECMAScript `Number::toString(x)` for a finite double (ECMA-262, Number::toString).
pub fn ecmascript_number(x: f64) -> String {
    if x == 0.0 {
        return "0".to_owned();
    }
    let (digits, n) = shortest_digits(x.abs());
    let k = digits.len() as i32;
    let mut out = String::new();
    if x < 0.0 {
        out.push('-');
    }
    if k <= n && n <= 21 {
        out.push_str(&digits);
        out.extend(std::iter::repeat_n('0', (n - k) as usize));
    } else if 0 < n && n <= 21 {
        out.push_str(&digits[..n as usize]);
        out.push('.');
        out.push_str(&digits[n as usize..]);
    } else if -6 < n && n <= 0 {
        out.push_str("0.");
        out.extend(std::iter::repeat_n('0', (-n) as usize));
        out.push_str(&digits);
    } else {
        let e = n - 1;
        out.push_str(&digits[..1]);
        if k > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push('e');
        out.push(if e < 0 { '-' } else { '+' });
        out.push_str(&e.unsigned_abs().to_string());
    }
    out
}

/// Shortest round-tripping decimal digits `s` and exponent `n` with `x = 0.s × 10^n`, for a
/// positive finite `x`. Rust's `{:e}` already yields a shortest round-tripping digit string; when
/// `x` lies exactly halfway between two shortest candidates, ECMAScript requires the even one,
/// which Rust does not guarantee (RFC 8785 vector 0x43143ff3c1cb0959).
fn shortest_digits(x: f64) -> (String, i32) {
    let (digits, n) = split_scientific(&format!("{x:e}"));
    // Every finite double has an exact decimal expansion of at most 767 significant digits.
    let (exact, exact_n) = split_scientific(&format!("{x:.800e}"));
    let exact = exact.trim_end_matches('0');
    if exact.len() == digits.len() + 1 && exact.ends_with('5') && exact_n == n {
        let down = exact[..digits.len()].to_owned();
        if let Some(up) = increment(&down) {
            let round_trips = |d: &str| format!("0.{d}e{n}").parse::<f64>() == Ok(x);
            let even = |d: &str| {
                d.bytes()
                    .last()
                    .is_some_and(|b| (b - b'0').is_multiple_of(2))
            };
            let chosen = match (round_trips(&down), round_trips(&up)) {
                (true, true) => {
                    if even(&down) {
                        down
                    } else {
                        up
                    }
                }
                (true, false) => down,
                (false, true) => up,
                (false, false) => digits,
            };
            return (chosen, n);
        }
    }
    (digits, n)
}

/// Splits Rust `{:e}` output `d.ddde±x` into digits and `x + 1`.
fn split_scientific(sci: &str) -> (String, i32) {
    let (mantissa, exponent) = sci.split_once('e').unwrap_or((sci, "0"));
    let digits = mantissa.chars().filter(char::is_ascii_digit).collect();
    (digits, exponent.parse::<i32>().unwrap_or(0) + 1)
}

/// Decimal increment without changing length; `None` on carry out of the leading digit.
fn increment(digits: &str) -> Option<String> {
    let mut bytes = digits.as_bytes().to_vec();
    for b in bytes.iter_mut().rev() {
        if *b == b'9' {
            *b = b'0';
        } else {
            *b += 1;
            return String::from_utf8(bytes).ok();
        }
    }
    None
}
