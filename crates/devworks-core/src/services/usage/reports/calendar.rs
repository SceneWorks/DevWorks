//! UTC report boundaries preserve the retained service's calendar and half-open ranges.
use super::{SResult, invalid};
use jiff::{Span, ToSpan, civil::Date};
pub(super) const DAY: i64 = 86_400_000;
// UTC civil arithmetic covers the entire retained DateTime range. Zoned Timestamp
// reserves offset headroom and therefore excludes the last 26 hours of year 9999.
fn date(ms: i64) -> SResult<Date> {
    Date::constant(1970, 1, 1)
        .checked_add(
            Span::new()
                .try_days(ms.div_euclid(DAY))
                .map_err(|_| invalid("usage_range"))?,
        )
        .map_err(|_| invalid("usage_range"))
}
fn midnight(date: Date) -> SResult<i64> {
    i64::from(
        date.since(Date::constant(1970, 1, 1))
            .map_err(|_| invalid("usage_range"))?
            .get_days(),
    )
    .checked_mul(DAY)
    .ok_or_else(|| invalid("usage_range"))
}
#[derive(Clone, Copy, Debug, serde::Serialize)]
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
        let date = date(ms)?;
        let date = match self {
            Self::Day => date,
            Self::Week => date
                .checked_sub(i64::from(date.weekday().to_monday_zero_offset()).days())
                .map_err(|_| invalid("usage_range"))?,
            Self::Month => {
                Date::new(date.year(), date.month(), 1).map_err(|_| invalid("usage_range"))?
            }
        };
        midnight(date)
    }
    /// Number of boundaries in the half-open range, without walking synthetic buckets.
    pub fn count(self, first: i64, end: i64) -> SResult<usize> {
        let last = self.floor(end - 1)?;
        let n = match self {
            Self::Day => (last - first) / DAY,
            Self::Week => (last - first) / (7 * DAY),
            Self::Month => {
                let (a, b) = (date(first)?, date(last)?);
                i64::from(b.year() - a.year()) * 12 + i64::from(b.month() - a.month())
            }
        };
        usize::try_from(n + 1).map_err(|_| invalid("usage_range"))
    }
    pub fn at(self, first: i64, index: usize) -> SResult<i64> {
        let n = i64::try_from(index).map_err(|_| invalid("usage_range"))?;
        match self {
            Self::Day | Self::Week => first
                .checked_add(
                    n.checked_mul(if matches!(self, Self::Day) {
                        DAY
                    } else {
                        7 * DAY
                    })
                    .ok_or_else(|| invalid("usage_range"))?,
                )
                .ok_or_else(|| invalid("usage_range")),
            Self::Month => midnight(
                date(first)?
                    .checked_add(
                        Span::new()
                            .try_months(n)
                            .map_err(|_| invalid("usage_range"))?,
                    )
                    .map_err(|_| invalid("usage_range"))?,
            ),
        }
    }
    pub fn index(self, first: i64, bucket: i64) -> SResult<usize> {
        self.count(first, bucket + 1).map(|n| n - 1)
    }
}

/// Frozen DateTime accepts years 1..=9999. Signed epoch milliseconds preserve its real legal
/// pre-1970 dates, while rejecting dates that the retained controller could never bind.
pub(super) fn valid_source_date(ms: i64) -> SResult<()> {
    let year = date(ms)?.year();
    if !(1..=9999).contains(&year) {
        return Err(invalid("usage_range"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn calendar_indices_reach_the_last_legal_year_and_keep_leap_boundaries() {
        let first = -62_135_596_800_000;
        let end = 253_402_300_799_999;
        assert_eq!(Interval::Month.count(first, end).unwrap(), 119_988);
        assert_eq!(
            Interval::Month.at(first, 119_987).unwrap(),
            253_399_622_400_000
        );
        assert_eq!(Interval::Day.floor(end).unwrap(), 253_402_214_400_000);
        assert_eq!(Interval::Week.floor(-86_400_000).unwrap(), -259_200_000);
        // 2000-02-01 + one calendar month crosses the leap day exactly.
        assert_eq!(
            Interval::Month.at(949_363_200_000, 1).unwrap(),
            951_868_800_000
        );
        assert!(valid_source_date(end).is_ok());
        assert!(valid_source_date(end + 1).is_err());
        assert!(valid_source_date(first - 1).is_err());
    }
}
