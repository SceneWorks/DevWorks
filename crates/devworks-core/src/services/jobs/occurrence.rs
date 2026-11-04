//! Schedule occurrences (JOB-02/03): standard five-field cron expressions in an IANA time zone,
//! fixed intervals and manual schedules, with timezone-correct occurrence identities.
//!
//! # Calendar expressions
//!
//! The retained CodeGraph scheduler parsed `CronFormat.Standard` with Cronos, so the same
//! grammar and matching rules apply here: `minute hour day-of-month month day-of-week`, with
//! `*`/`?`, lists, ranges (reversed ranges wrap, `23-01` is `23,0,1`), steps (`*/5`, `1-15/3`,
//! `5/15` from 5 to the field maximum), `JAN`–`DEC` and `SUN`–`SAT` (case-insensitive, `0` and
//! `7` are Sunday), `L`, `L-n`, `nW`, `LW`, `L-nW` in day-of-month, `nL` and `n#k` in
//! day-of-week, and the macros `@every_minute`, `@hourly`, `@daily`, `@midnight`, `@weekly`,
//! `@monthly`, `@yearly` and `@annually`. When both day fields are restricted a day must match
//! both (Cronos uses AND, unlike Vixie cron's OR).
//!
//! # Daylight saving time (Cronos semantics)
//!
//! * **Gap** (clocks move forward): a local time that does not exist runs once at the instant the
//!   clock jumps (02:30 on the spring-forward day runs at 03:00 new time); it is never skipped.
//! * **Fold** (clocks move back): an *interval* expression (a `*`, `?`, range or step in the
//!   minute or hour field) runs at both instants of a repeated local time, in UTC order; any other
//!   expression runs only at the first (earlier-offset) instant, so a daily job runs once.
//!
//! Every occurrence is a UTC instant, and its identity is that instant (`at-<utc ms>`): the two
//! instants of a folded local time are two distinct occurrences, and a gap-adjusted occurrence
//! and the regular one at the same instant are one.
use jiff::Timestamp;
use jiff::civil::DateTime;
use jiff::tz::{AmbiguousOffset, TimeZone};

const MINUTE_MS: i64 = 60_000;
const DAY_MS: i64 = 86_400_000;
/// Wider than any offset change a zone makes around a future instant (DST shifts are 30 min to
/// 2 h): the local wall time of any later instant is at least the local time of `after` minus
/// this, and a local time more than this past the best candidate names a later instant.
const SHIFT_BOUND_MS: i64 = 3 * 3_600_000;
/// Days searched for a next occurrence before an expression is declared to have none (Feb 30,
/// or a combination that recurs less often than this).
const HORIZON_DAYS: i64 = 366 * 30;
/// The shortest fixed interval a schedule may use (a resource bound: a schedule cannot hammer
/// the store or a provider).
pub const MIN_INTERVAL_SECONDS: i64 = 60;
/// Missed occurrences counted one by one before the count is reported as a lower bound.
pub const MISSED_COUNT_CAP: u64 = 10_000;

/// A parsed standard (five-field) cron expression.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CronExpression {
    minutes: u64,
    hours: u32,
    dom: DaySpec,
    months: u16,
    dow: WeekSpec,
    /// Interval-based for DST folds (see the module documentation).
    interval: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum DaySpec {
    Any,
    Days(u32),
    /// `L`, `L-n` and their `W` forms.
    Last {
        offset: u32,
        weekday: bool,
    },
    /// `nW`.
    Nearest(u32),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum WeekSpec {
    Any,
    /// Bits 0 (Sunday) ..= 6 (Saturday).
    Days(u8),
    /// `nL`: the last such weekday of the month.
    Last(u8),
    /// `n#k`: the k-th such weekday of the month.
    Nth(u8, u8),
}

const MONTHS: [&str; 12] = [
    "JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC",
];
const WEEKDAYS: [&str; 7] = ["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"];

fn number(s: &str, what: &str) -> Result<u32, String> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) || s.len() > 4 {
        return Err(format!("{what}: '{s}' is not a number"));
    }
    s.parse()
        .map_err(|_| format!("{what}: '{s}' is not a number"))
}

/// A value of a field: a number or (months, weekdays) a three-letter name.
fn value(
    s: &str,
    min: u32,
    max: u32,
    names: &[&str],
    name_base: u32,
    what: &str,
) -> Result<u32, String> {
    let upper = s.to_ascii_uppercase();
    if let Some(i) = names.iter().position(|n| *n == upper) {
        return Ok(name_base + i as u32);
    }
    let n = number(s, what)?;
    if n < min || n > max {
        return Err(format!("{what}: {n} is outside {min}-{max}"));
    }
    Ok(n)
}

/// The bit set of a regular field (`*`, `?`, lists, ranges, steps, names).
fn field(
    text: &str,
    min: u32,
    max: u32,
    names: &[&str],
    name_base: u32,
    what: &str,
) -> Result<u64, String> {
    let mut bits = 0u64;
    if text.is_empty() {
        return Err(format!("{what} is empty"));
    }
    for part in text.split(',') {
        if part.is_empty() {
            return Err(format!("{what}: empty list member"));
        }
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => {
                let step = number(s, what)?;
                if step == 0 || step > max {
                    return Err(format!("{what}: step {step} is outside 1-{max}"));
                }
                (r, Some(step))
            }
            None => (part, None),
        };
        let (start, end) = if range == "*" || range == "?" {
            (min, max)
        } else if let Some((a, b)) = range.split_once('-') {
            (
                value(a, min, max, names, name_base, what)?,
                value(b, min, max, names, name_base, what)?,
            )
        } else {
            let a = value(range, min, max, names, name_base, what)?;
            (a, if step.is_some() { max } else { a })
        };
        // A reversed range wraps from the maximum back to the minimum.
        let span = if start <= end {
            end - start
        } else {
            (max - start) + 1 + (end - min)
        };
        let step = step.unwrap_or(1);
        let mut k = 0;
        while k <= span {
            let v = start + k;
            let v = if v > max { v - (max - min + 1) } else { v };
            bits |= 1u64 << v;
            k += step;
        }
    }
    Ok(bits)
}

fn is_star(text: &str) -> bool {
    text == "*" || text == "?"
}

fn day_spec(text: &str) -> Result<DaySpec, String> {
    const WHAT: &str = "day-of-month";
    if is_star(text) {
        return Ok(DaySpec::Any);
    }
    let upper = text.to_ascii_uppercase();
    if let Some(rest) = upper.strip_prefix('L') {
        let (rest, weekday) = match rest.strip_suffix('W') {
            Some(r) => (r, true),
            None => (rest, false),
        };
        let offset = match rest.strip_prefix('-') {
            Some(n) => {
                let n = number(n, WHAT)?;
                if n > 30 {
                    return Err(format!("{WHAT}: L-{n} is outside L-0..L-30"));
                }
                n
            }
            None if rest.is_empty() => 0,
            None => return Err(format!("{WHAT}: '{text}' is not a last-day form")),
        };
        return Ok(DaySpec::Last { offset, weekday });
    }
    if let Some(day) = upper.strip_suffix('W') {
        return Ok(DaySpec::Nearest(value(day, 1, 31, &[], 0, WHAT)?));
    }
    Ok(DaySpec::Days(field(text, 1, 31, &[], 0, WHAT)? as u32))
}

fn week_spec(text: &str) -> Result<WeekSpec, String> {
    const WHAT: &str = "day-of-week";
    let weekday = |s: &str| value(s, 0, 7, &WEEKDAYS, 0, WHAT).map(|v| (v % 7) as u8);
    if is_star(text) {
        return Ok(WeekSpec::Any);
    }
    let upper = text.to_ascii_uppercase();
    if let Some((day, nth)) = upper.split_once('#') {
        let k = number(nth, WHAT)?;
        if !(1..=5).contains(&k) {
            return Err(format!("{WHAT}: #{k} is outside #1-#5"));
        }
        return Ok(WeekSpec::Nth(weekday(day)?, k as u8));
    }
    if let Some(day) = upper.strip_suffix('L')
        && !day.is_empty()
    {
        return Ok(WeekSpec::Last(weekday(day)?));
    }
    let raw = field(text, 0, 7, &WEEKDAYS, 0, WHAT)?;
    // 7 is Sunday.
    let bits = (raw & 0x7f) | ((raw >> 7) & 1);
    Ok(WeekSpec::Days(bits as u8))
}

impl CronExpression {
    /// Parses a standard five-field expression or macro.
    pub fn parse(expression: &str) -> Result<Self, String> {
        let trimmed = expression.trim();
        let expanded = match trimmed.to_ascii_lowercase().as_str() {
            "@every_minute" => "* * * * *".to_owned(),
            "@hourly" => "0 * * * *".to_owned(),
            "@daily" | "@midnight" => "0 0 * * *".to_owned(),
            "@weekly" => "0 0 * * 0".to_owned(),
            "@monthly" => "0 0 1 * *".to_owned(),
            "@yearly" | "@annually" => "0 0 1 1 *".to_owned(),
            m if m.starts_with('@') => {
                return Err(format!("'{trimmed}' is not a supported macro"));
            }
            _ => trimmed.to_owned(),
        };
        let fields: Vec<&str> = expanded.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(format!(
                "a schedule needs five fields (minute hour day-of-month month day-of-week), found {}",
                fields.len()
            ));
        }
        let interval = fields[..2].iter().any(|f| f.contains(['*', '?', '-', '/']));
        Ok(Self {
            minutes: field(fields[0], 0, 59, &[], 0, "minute")?,
            hours: field(fields[1], 0, 23, &[], 0, "hour")? as u32,
            dom: day_spec(fields[2])?,
            months: field(fields[3], 1, 12, &MONTHS, 1, "month")? as u16,
            dow: week_spec(fields[4])?,
            interval,
        })
    }

    /// Whether a repeated (folded) local time runs at both of its instants.
    pub fn is_interval(&self) -> bool {
        self.interval
    }

    fn day_matches(&self, days: i64) -> bool {
        let (y, m, d) = civil_from_days(days);
        if self.months & (1 << m) == 0 {
            return false;
        }
        let last = days_in_month(y, m);
        let weekday = weekday_of(days);
        let dom = match &self.dom {
            DaySpec::Any => true,
            DaySpec::Days(bits) => bits & (1 << d) != 0,
            DaySpec::Last { offset, weekday: w } => {
                *offset < last && {
                    let target = last - offset;
                    let target = if *w {
                        nearest_weekday(y, m, target)
                    } else {
                        target
                    };
                    d == target
                }
            }
            DaySpec::Nearest(n) => *n <= last && d == nearest_weekday(y, m, *n),
        };
        let dow = match &self.dow {
            WeekSpec::Any => true,
            WeekSpec::Days(bits) => bits & (1 << weekday) != 0,
            WeekSpec::Last(w) => weekday == *w && d + 7 > last,
            WeekSpec::Nth(w, k) => weekday == *w && (d - 1) / 7 + 1 == u32::from(*k),
        };
        dom && dow
    }

    /// The UTC instants a local wall time of a matching minute names in `tz` (see the module
    /// documentation for gaps and folds).
    fn instants(&self, tz: &TimeZone, days: i64, hour: u32, minute: u32) -> Vec<i64> {
        let (y, m, d) = civil_from_days(days);
        let naive = days * DAY_MS + i64::from(hour) * 3_600_000 + i64::from(minute) * MINUTE_MS;
        let Ok(dt) = DateTime::new(y as i16, m as i8, d as i8, hour as i8, minute as i8, 0, 0)
        else {
            return Vec::new();
        };
        let at = |offset: jiff::tz::Offset| naive - i64::from(offset.seconds()) * 1000;
        match tz.to_ambiguous_timestamp(dt).offset() {
            AmbiguousOffset::Unambiguous { offset } => vec![at(offset)],
            AmbiguousOffset::Gap { before, after } => {
                // The jump happens in (naive - after, naive - before]: run at that instant.
                let low = at(after);
                let jump = Timestamp::from_millisecond(low)
                    .ok()
                    .and_then(|t| tz.following(t).next())
                    .map(|t| t.timestamp().as_millisecond())
                    .filter(|t| *t > low && *t <= at(before));
                vec![jump.unwrap_or(at(before))]
            }
            AmbiguousOffset::Fold { before, after } => {
                if self.interval {
                    vec![at(before), at(after)]
                } else {
                    vec![at(before)]
                }
            }
        }
    }

    /// The first occurrence strictly after `after_ms` (UTC) in `tz`, or `None` when the
    /// expression produces none within the search horizon.
    pub fn next_after(&self, tz: &TimeZone, after_ms: i64) -> Option<i64> {
        // Local wall times as if UTC: an instant `t` reads `t + offset(t)`. Any instant after
        // `after_ms` reads after `after_ms + offset(after) - SHIFT_BOUND`.
        let offset = Timestamp::from_millisecond(after_ms)
            .ok()
            .map_or(0, |t| i64::from(tz.to_offset(t).seconds()) * 1000);
        let start = after_ms.checked_add(offset)?.checked_sub(SHIFT_BOUND_MS)?;
        let reach = offset + SHIFT_BOUND_MS;
        let first_day = start.div_euclid(DAY_MS);
        let start_minute = start.div_euclid(MINUTE_MS);
        let mut best: Option<i64> = None;
        for days in first_day..first_day.saturating_add(HORIZON_DAYS) {
            if let Some(b) = best
                && days * DAY_MS - reach > b
            {
                return best;
            }
            if !self.day_matches(days) {
                continue;
            }
            for hour in (0..24).filter(|h| self.hours & (1 << h) != 0) {
                for minute in (0..60).filter(|m| self.minutes & (1u64 << m) != 0) {
                    let naive =
                        days * DAY_MS + i64::from(hour) * 3_600_000 + i64::from(minute) * MINUTE_MS;
                    if naive.div_euclid(MINUTE_MS) < start_minute {
                        continue;
                    }
                    if let Some(b) = best
                        && naive - reach > b
                    {
                        return best;
                    }
                    for at in self.instants(tz, days, hour, minute) {
                        if at > after_ms && best.is_none_or(|b| at < b) {
                            best = Some(at);
                        }
                    }
                }
            }
        }
        best
    }
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        _ => 28,
    }
}
/// 0 = Sunday.
fn weekday_of(days: i64) -> u8 {
    // 1970-01-01 was a Thursday.
    (days + 4).rem_euclid(7) as u8
}
/// The weekday Cronos's `W` selects for `day`: Saturday moves to Friday (or to Monday the 3rd
/// when it is the 1st), Sunday to Monday (or to Friday when it is the last day).
fn nearest_weekday(y: i64, m: u32, day: u32) -> u32 {
    let w = weekday_of(days_from_civil(y, m, day));
    let last = days_in_month(y, m);
    match w {
        6 if day == 1 => 3,
        6 => day - 1,
        0 if day == last => day - 2,
        0 => day + 1,
        _ => day,
    }
}
/// Days since 1970-01-01 of a proleptic Gregorian date.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(m);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// The first instant `anchor + k·period` (k ≥ 0) strictly after `after_ms`.
pub fn next_interval(anchor_ms: i64, period_ms: i64, after_ms: i64) -> Option<i64> {
    if period_ms <= 0 {
        return None;
    }
    if anchor_ms > after_ms {
        return Some(anchor_ms);
    }
    let k = (after_ms - anchor_ms).div_euclid(period_ms) + 1;
    k.checked_mul(period_ms)?.checked_add(anchor_ms)
}

/// Resolves an IANA time zone name (`UTC` included).
pub fn zone(name: &str) -> Result<TimeZone, String> {
    if name.eq_ignore_ascii_case("UTC") {
        return Ok(TimeZone::UTC);
    }
    TimeZone::get(name).map_err(|_| format!("unknown time zone '{name}'"))
}

/// The occurrence identity of a scheduled instant.
pub fn occurrence_key(at_ms: i64) -> String {
    format!("at-{at_ms}")
}

/// A validated schedule as the scheduler evaluates it.
#[derive(Clone, Debug)]
pub enum Plan {
    Manual,
    Interval { period_ms: i64 },
    Calendar { cron: CronExpression, tz: TimeZone },
}
impl Plan {
    /// The first occurrence strictly after `after_ms`; `anchor_ms` anchors an interval.
    pub fn next_after(&self, anchor_ms: i64, after_ms: i64) -> Option<i64> {
        match self {
            Self::Manual => None,
            Self::Interval { period_ms } => next_interval(anchor_ms, *period_ms, after_ms),
            Self::Calendar { cron, tz } => cron.next_after(tz, after_ms),
        }
    }
    /// How many occurrences fall in `(after_ms, until_ms]` (`first` is the first of them, when
    /// known), counted up to [`MISSED_COUNT_CAP`]; `true` when the count is exact.
    pub fn count_between(
        &self,
        anchor_ms: i64,
        first: Option<i64>,
        after_ms: i64,
        until_ms: i64,
    ) -> (u64, bool) {
        match self {
            Self::Manual => (0, true),
            Self::Interval { period_ms } => {
                let Some(first) = first.or_else(|| next_interval(anchor_ms, *period_ms, after_ms))
                else {
                    return (0, true);
                };
                if first > until_ms {
                    return (0, true);
                }
                let n = (until_ms - first) / period_ms + 1;
                (u64::try_from(n).unwrap_or(0), true)
            }
            Self::Calendar { cron, tz } => {
                let mut n = 0;
                let mut at = first.or_else(|| cron.next_after(tz, after_ms));
                while let Some(t) = at
                    && t <= until_ms
                {
                    n += 1;
                    if n >= MISSED_COUNT_CAP {
                        return (n, false);
                    }
                    at = cron.next_after(tz, t);
                }
                (n, true)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(s: &str) -> i64 {
        s.parse::<Timestamp>().unwrap().as_millisecond()
    }
    fn ny() -> TimeZone {
        zone("America/New_York").unwrap()
    }
    fn after(expr: &str, tz: &TimeZone, from: &str, n: usize) -> Vec<String> {
        let cron = CronExpression::parse(expr).unwrap();
        let mut at = ms(from);
        let mut out = Vec::new();
        for _ in 0..n {
            at = cron.next_after(tz, at).expect("an occurrence");
            out.push(
                Timestamp::from_millisecond(at)
                    .unwrap()
                    .to_zoned(tz.clone())
                    .strftime("%Y-%m-%d %H:%M%:z")
                    .to_string(),
            );
        }
        out
    }

    #[test]
    fn spring_forward_runs_a_skipped_local_time_once_at_the_jump() {
        // 2026-03-08: 02:00 EST becomes 03:00 EDT in New York.
        assert_eq!(
            after("30 2 * * *", &ny(), "2026-03-07T12:00:00Z", 3),
            [
                "2026-03-08 03:00-04:00",
                "2026-03-09 02:30-04:00",
                "2026-03-10 02:30-04:00"
            ]
        );
        // An interval expression is not skipped either: 02:00 and 02:30 both map to the jump,
        // which is one occurrence.
        assert_eq!(
            after("*/30 * * * *", &ny(), "2026-03-08T06:15:00Z", 3),
            [
                "2026-03-08 01:30-05:00",
                "2026-03-08 03:00-04:00",
                "2026-03-08 03:30-04:00"
            ]
        );
    }

    #[test]
    fn fall_back_repeats_interval_expressions_but_not_daily_ones() {
        // 2026-11-01: 02:00 EDT becomes 01:00 EST in New York.
        assert_eq!(
            after("*/30 * * * *", &ny(), "2026-11-01T04:15:00Z", 6),
            [
                "2026-11-01 00:30-04:00",
                "2026-11-01 01:00-04:00",
                "2026-11-01 01:30-04:00",
                "2026-11-01 01:00-05:00",
                "2026-11-01 01:30-05:00",
                "2026-11-01 02:00-05:00"
            ]
        );
        assert_eq!(
            after("30 1 * * *", &ny(), "2026-10-31T12:00:00Z", 3),
            [
                "2026-11-01 01:30-04:00",
                "2026-11-02 01:30-05:00",
                "2026-11-03 01:30-05:00"
            ]
        );
        // The two instants of a folded local time are distinct occurrence identities.
        let cron = CronExpression::parse("*/30 * * * *").unwrap();
        let first = cron.next_after(&ny(), ms("2026-11-01T05:15:00Z")).unwrap();
        let second = cron.next_after(&ny(), ms("2026-11-01T06:15:00Z")).unwrap();
        assert_ne!(occurrence_key(first), occurrence_key(second));
        assert_eq!(second - first, 3_600_000);
    }

    #[test]
    fn utc_and_offsets_of_other_zones_are_respected() {
        assert_eq!(
            after("0 9 * * MON-FRI", &TimeZone::UTC, "2026-11-06T10:00:00Z", 2),
            ["2026-11-09 09:00+00:00", "2026-11-10 09:00+00:00"]
        );
        let tokyo = zone("Asia/Tokyo").unwrap();
        assert_eq!(
            after("0 0 * * *", &tokyo, "2026-11-03T14:59:00Z", 1),
            ["2026-11-04 00:00+09:00"]
        );
        assert!(zone("Mars/Olympus_Mons").is_err());
    }

    #[test]
    fn cronos_special_characters_and_and_semantics() {
        let utc = TimeZone::UTC;
        // Last day, last weekday, nearest weekday, nth and last weekday of the month.
        assert_eq!(
            after("0 0 L * *", &utc, "2026-02-01T00:00:00Z", 1),
            ["2026-02-28 00:00+00:00"]
        );
        assert_eq!(
            after("0 0 L-1 * *", &utc, "2026-02-01T00:00:00Z", 1),
            ["2026-02-27 00:00+00:00"]
        );
        // 2026-05-31 is a Sunday: LW is Friday the 29th.
        assert_eq!(
            after("0 0 LW * *", &utc, "2026-05-01T00:00:00Z", 1),
            ["2026-05-29 00:00+00:00"]
        );
        // 2026-08-01 is a Saturday: 1W is Monday the 3rd.
        assert_eq!(
            after("0 0 1W * *", &utc, "2026-07-15T00:00:00Z", 1),
            ["2026-08-03 00:00+00:00"]
        );
        assert_eq!(
            after("0 0 * * 6#3", &utc, "2026-11-01T00:00:00Z", 1),
            ["2026-11-21 00:00+00:00"]
        );
        assert_eq!(
            after("0 0 * * FRIL", &utc, "2026-11-01T00:00:00Z", 1),
            ["2026-11-27 00:00+00:00"]
        );
        // Friday the 13th: both day fields must match.
        assert_eq!(
            after("0 0 13 * 5", &utc, "2026-01-01T00:00:00Z", 2),
            ["2026-02-13 00:00+00:00", "2026-03-13 00:00+00:00"]
        );
        // Reversed ranges wrap; 7 is Sunday; names are case-insensitive.
        assert_eq!(
            after("0 23-1 * * *", &utc, "2026-11-03T22:00:00Z", 3),
            [
                "2026-11-03 23:00+00:00",
                "2026-11-04 00:00+00:00",
                "2026-11-04 01:00+00:00"
            ]
        );
        assert_eq!(
            after("0 0 * dec-jan 7", &utc, "2026-11-03T00:00:00Z", 1),
            ["2026-12-06 00:00+00:00"]
        );
        assert_eq!(
            after("@hourly", &utc, "2026-11-03T22:10:00Z", 1),
            ["2026-11-03 23:00+00:00"]
        );
        assert_eq!(
            after("5/20 * * * *", &utc, "2026-11-03T22:06:00Z", 3),
            [
                "2026-11-03 22:25+00:00",
                "2026-11-03 22:45+00:00",
                "2026-11-03 23:05+00:00"
            ]
        );
    }

    #[test]
    fn invalid_expressions_and_impossible_dates_are_refused() {
        for bad in [
            "",
            "* * * *",
            "* * * * * *",
            "60 * * * *",
            "* 24 * * *",
            "*/0 * * * *",
            "1-x * * * *",
            "@every_second",
            "@reboot",
            "* * 32 * *",
            "* * * 13 *",
            "* * * * 8",
            "* * L-31 * *",
            "* * * * 1#6",
            "* * ,1 * *",
        ] {
            assert!(CronExpression::parse(bad).is_err(), "{bad:?} parsed");
        }
        let feb30 = CronExpression::parse("0 0 30 2 *").unwrap();
        assert_eq!(
            feb30.next_after(&TimeZone::UTC, ms("2026-01-01T00:00:00Z")),
            None
        );
        assert!(CronExpression::parse("30 1 * * *").is_ok_and(|c| !c.is_interval()));
        assert!(CronExpression::parse("0,45 1,2 * * *").is_ok_and(|c| !c.is_interval()));
        assert!(CronExpression::parse("30 * * * *").is_ok_and(|c| c.is_interval()));
        assert!(CronExpression::parse("0 1-3 * * *").is_ok_and(|c| c.is_interval()));
    }

    #[test]
    fn intervals_and_missed_counts_never_catch_up() {
        assert_eq!(next_interval(1_000, 60_000, 500), Some(1_000));
        assert_eq!(next_interval(1_000, 60_000, 1_000), Some(61_000));
        assert_eq!(next_interval(1_000, 60_000, 120_999), Some(121_000));
        let plan = Plan::Interval { period_ms: 60_000 };
        // Due at 61 000; the app reopens at 300 500: 61k, 121k, 181k, 241k were missed.
        assert_eq!(
            plan.count_between(1_000, Some(61_000), 1_000, 300_500),
            (4, true)
        );
        assert_eq!(plan.next_after(1_000, 300_500), Some(301_000));
        let daily = Plan::Calendar {
            cron: CronExpression::parse("0 3 * * *").unwrap(),
            tz: ny(),
        };
        let from = ms("2026-11-01T00:00:00Z");
        assert_eq!(
            daily.count_between(0, None, from, from + 10 * DAY_MS),
            (10, true)
        );
        let dense = Plan::Calendar {
            cron: CronExpression::parse("* * * * *").unwrap(),
            tz: TimeZone::UTC,
        };
        assert_eq!(
            dense.count_between(0, None, from, from + 30 * DAY_MS),
            (MISSED_COUNT_CAP, false)
        );
    }

    #[test]
    fn civil_day_arithmetic_round_trips() {
        for days in [-800_000, -1, 0, 1, 19_000, 20_759, 2_000_000] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(weekday_of(days_from_civil(2026, 11, 3)), 2);
    }
}
