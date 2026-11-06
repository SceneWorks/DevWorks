//! UTC report boundaries preserve the retained service's calendar and half-open ranges.
use super::{SResult, invalid};
use jiff::{Timestamp, ToSpan, civil::Date, tz::TimeZone};
pub(super) const DAY: i64 = 86_400_000;
#[derive(Clone, Copy, Debug)]
pub(super) enum Interval {
    Day,
    Week,
    Month,
}
impl Interval {
    pub fn parse(value: Option<&str>, start: i64, end: i64) -> SResult<Self> {
        match value.unwrap_or("auto").trim().to_ascii_lowercase().as_str() {
            "" | "auto" => Ok(
                if end
                    .checked_sub(start)
                    .ok_or_else(|| invalid("usage_range"))?
                    <= 45 * DAY
                {
                    Self::Day
                } else if end
                    .checked_sub(start)
                    .ok_or_else(|| invalid("usage_range"))?
                    <= 180 * DAY
                {
                    Self::Week
                } else {
                    Self::Month
                },
            ),
            "day" => Ok(Self::Day),
            "week" => Ok(Self::Week),
            "month" => Ok(Self::Month),
            _ => Err(invalid("usage_interval")),
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Day => "day",
            Self::Week => "week",
            Self::Month => "month",
        }
    }
    pub fn floor(self, ms: i64) -> SResult<i64> {
        let date = Timestamp::from_millisecond(ms)
            .map_err(|_| invalid("usage_range"))?
            .to_zoned(TimeZone::UTC)
            .date();
        let date = match self {
            Self::Day => date,
            Self::Week => date
                .checked_sub(i64::from(date.weekday().to_monday_zero_offset()).days())
                .map_err(|_| invalid("usage_range"))?,
            Self::Month => {
                Date::new(date.year(), date.month(), 1).map_err(|_| invalid("usage_range"))?
            }
        };
        let out = date
            .at(0, 0, 0, 0)
            .to_zoned(TimeZone::UTC)
            .map_err(|_| invalid("usage_range"))?
            .timestamp()
            .as_millisecond();
        Ok(out)
    }
    pub fn next(self, ms: i64) -> SResult<i64> {
        if matches!(self, Self::Day | Self::Week) {
            return ms
                .checked_add(if matches!(self, Self::Day) {
                    DAY
                } else {
                    7 * DAY
                })
                .ok_or_else(|| invalid("usage_range"));
        }
        Timestamp::from_millisecond(ms)
            .map_err(|_| invalid("usage_range"))?
            .to_zoned(TimeZone::UTC)
            .date()
            .checked_add(1.months())
            .and_then(|d| d.at(0, 0, 0, 0).to_zoned(TimeZone::UTC))
            .map(|z| z.timestamp().as_millisecond())
            .map_err(|_| invalid("usage_range"))
    }
}

/// Frozen DateTime accepts years 1..=9999. Signed epoch milliseconds preserve its real legal
/// pre-1970 dates, while rejecting dates that the retained controller could never bind.
pub(super) fn valid_source_date(ms: i64) -> SResult<()> {
    let year = Timestamp::from_millisecond(ms)
        .map_err(|_| invalid("usage_range"))?
        .to_zoned(TimeZone::UTC)
        .year();
    if !(1..=9999).contains(&year) {
        return Err(invalid("usage_range"));
    }
    Ok(())
}
