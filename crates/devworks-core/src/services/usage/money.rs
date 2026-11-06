//! Exact tariff arithmetic. Sum all applicable charges before rounding to micro-USD.
//! Missing quantities/rates belong to the caller's untracked coverage, never to this zero sum.

const SCALE: i128 = 1_000_000_000_000;
const MICROS_PER_USD: i128 = 1_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MoneyError {
    InvalidDecimal,
    Overflow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rounding {
    /// Incurred estimates round once, with exact half-micro ties upward.
    Nearest,
    /// Reservations never discard a positive fractional micro-USD.
    Up,
}

/// Twelve decimal places of a nonnegative amount, parsed without a floating point value.
fn decimal(value: &str) -> Result<i128, MoneyError> {
    if value.is_empty() || value.len() > 64 {
        return Err(MoneyError::InvalidDecimal);
    }
    let mut parts = value.split('.');
    let whole = parts.next().ok_or(MoneyError::InvalidDecimal)?;
    let fraction = parts.next();
    if whole.is_empty()
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || parts.next().is_some()
        || fraction
            .is_some_and(|f| f.is_empty() || f.len() > 12 || !f.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(MoneyError::InvalidDecimal);
    }
    let digits = |s: &str| {
        s.bytes().try_fold(0_i128, |n, b| {
            n.checked_mul(10)
                .and_then(|n| n.checked_add(i128::from(b - b'0')))
                .ok_or(MoneyError::Overflow)
        })
    };
    let mut amount = digits(whole)?
        .checked_mul(SCALE)
        .ok_or(MoneyError::Overflow)?;
    if let Some(fraction) = fraction {
        let scale = 10_i128.pow(12 - fraction.len() as u32);
        amount = amount
            .checked_add(
                digits(fraction)?
                    .checked_mul(scale)
                    .ok_or(MoneyError::Overflow)?,
            )
            .ok_or(MoneyError::Overflow)?;
    }
    Ok(amount)
}

fn gcd(mut a: i128, mut b: i128) -> i128 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CostAccumulator {
    numerator: i128,
    denominator: i128,
}
impl Default for CostAccumulator {
    fn default() -> Self {
        Self {
            numerator: 0,
            denominator: 1,
        }
    }
}

impl CostAccumulator {
    fn add(
        &mut self,
        quantity: i64,
        rate: &str,
        units: i64,
        multiplier: i128,
    ) -> Result<(), MoneyError> {
        if quantity < 0 || units <= 0 {
            return Err(MoneyError::InvalidDecimal);
        }
        let numerator = decimal(rate)?
            .checked_mul(i128::from(quantity))
            .and_then(|value| value.checked_mul(multiplier))
            .ok_or(MoneyError::Overflow)?;
        let denominator = SCALE
            .checked_mul(i128::from(units))
            .ok_or(MoneyError::Overflow)?;
        let reduction = gcd(numerator, denominator);
        let (numerator, denominator) = (numerator / reduction, denominator / reduction);
        let shared = gcd(self.denominator, denominator);
        let left = denominator / shared;
        let right = self.denominator / shared;
        let sum = self
            .numerator
            .checked_mul(left)
            .and_then(|v| numerator.checked_mul(right).and_then(|n| v.checked_add(n)))
            .ok_or(MoneyError::Overflow)?;
        let denominator = self
            .denominator
            .checked_mul(left)
            .ok_or(MoneyError::Overflow)?;
        let reduction = gcd(sum, denominator);
        self.numerator = sum / reduction;
        self.denominator = denominator / reduction;
        Ok(())
    }

    /// USD per million tokens is numerically micro-USD per token.
    pub fn tokens(&mut self, quantity: i64, usd_per_million: &str) -> Result<(), MoneyError> {
        self.add(quantity, usd_per_million, 1, 1)
    }

    /// An exact rate per `units` (for example seconds at a price per 60 seconds).
    /// The rational denominator is retained until the complete charge is rounded.
    pub fn measure(
        &mut self,
        quantity: i64,
        amount_usd: &str,
        units: i64,
    ) -> Result<(), MoneyError> {
        self.add(quantity, amount_usd, units, MICROS_PER_USD)
    }

    pub fn units(&mut self, quantity: i64, usd_per_unit: &str) -> Result<(), MoneyError> {
        self.measure(quantity, usd_per_unit, 1)
    }

    /// Preserve the provider's original decimal/currency separately; only USD is converted here.
    pub fn amount_usd(&mut self, amount: &str) -> Result<(), MoneyError> {
        self.units(1, amount)
    }

    /// Compare before rounding: two fractional charges may round to the same micro-USD.
    pub fn compare(&self, other: &Self) -> Result<std::cmp::Ordering, MoneyError> {
        let shared = gcd(self.denominator, other.denominator);
        let left = self
            .numerator
            .checked_mul(other.denominator / shared)
            .ok_or(MoneyError::Overflow)?;
        let right = other
            .numerator
            .checked_mul(self.denominator / shared)
            .ok_or(MoneyError::Overflow)?;
        Ok(left.cmp(&right))
    }

    pub fn micro_usd(&self, rounding: Rounding) -> Result<i64, MoneyError> {
        let whole = self.numerator / self.denominator;
        let fraction = self.numerator % self.denominator;
        let extra = match rounding {
            Rounding::Nearest => fraction >= self.denominator / 2 + self.denominator % 2,
            Rounding::Up => fraction > 0,
        };
        i64::try_from(whole + i128::from(extra)).map_err(|_| MoneyError::Overflow)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rates_and_non_token_units_keep_exact_decimal_units() {
        let mut cost = CostAccumulator::default();
        cost.tokens(1_000_000, "2.5").unwrap();
        cost.units(2, "0.001").unwrap();
        assert_eq!(cost.micro_usd(Rounding::Nearest), Ok(2_502_000));
    }

    #[test]
    fn sum_before_rounding_and_reserve_conservatively() {
        let mut cost = CostAccumulator::default();
        cost.tokens(1, "0.3").unwrap();
        assert_eq!(cost.micro_usd(Rounding::Nearest), Ok(0));
        assert_eq!(cost.micro_usd(Rounding::Up), Ok(1));
        cost.tokens(1, "0.3").unwrap();
        assert_eq!(cost.micro_usd(Rounding::Nearest), Ok(1));
        let mut tie = CostAccumulator::default();
        tie.amount_usd("0.0000005").unwrap();
        assert_eq!(tie.micro_usd(Rounding::Nearest), Ok(1));
    }

    #[test]
    fn per_minute_rates_retain_the_rational_remainder_across_components() {
        let mut cost = CostAccumulator::default();
        for _ in 0..60 {
            cost.measure(1, "0.000001", 60).unwrap();
        }
        assert_eq!(cost.micro_usd(Rounding::Nearest), Ok(1));
        assert_eq!(cost.micro_usd(Rounding::Up), Ok(1));
        assert_eq!(cost.measure(1, "1", 0), Err(MoneyError::InvalidDecimal));
    }

    #[test]
    fn reported_zero_stays_zero_and_a_tiny_reservation_stays_positive() {
        let mut cost = CostAccumulator::default();
        cost.amount_usd("0.000000000000").unwrap();
        assert_eq!(cost.micro_usd(Rounding::Up), Ok(0));
        cost.amount_usd("0.000000000001").unwrap();
        assert_eq!(cost.micro_usd(Rounding::Up), Ok(1));
    }

    #[test]
    fn invalid_or_overflowing_inputs_cannot_become_a_free_charge() {
        for value in [
            "",
            "-1",
            "+1",
            "1e3",
            " 1",
            ".1",
            "1.",
            "1.2.3",
            "0.1234567890123",
        ] {
            assert_eq!(
                CostAccumulator::default().amount_usd(value),
                Err(MoneyError::InvalidDecimal)
            );
        }
        let mut cost = CostAccumulator::default();
        assert_eq!(cost.tokens(-1, "1"), Err(MoneyError::InvalidDecimal));
        assert_eq!(
            cost.amount_usd("999999999999999999999999999999999999999999"),
            Err(MoneyError::Overflow)
        );
        cost.amount_usd("9223372036854.775808").unwrap();
        assert_eq!(cost.micro_usd(Rounding::Nearest), Err(MoneyError::Overflow));
    }
}
