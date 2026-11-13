//! Exact tariff arithmetic. Sum all applicable charges before rounding to micro-USD.
//! Missing quantities/rates belong to the caller's untracked coverage, never to this zero sum.
//! Tariff/quantity pages bound the component count. Big rational intermediates preserve legal
//! coprime unit denominators and cancelling factors; only the final integer ledger is i64.
use num_rational::BigRational;

const MICROS_PER_USD: i128 = 1_000_000;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MoneyError {
    InvalidDecimal,
    Overflow,
    /// A positive amount that rounds to zero micro-USD. Recording it as zero would hide spend
    /// from a budget, so the caller must keep it as an untracked component instead.
    SubMicro,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rounding {
    /// Incurred estimates round once, with exact half-micro ties upward.
    Nearest,
    /// Reservations never discard a positive fractional micro-USD.
    Up,
}
/// Nonnegative decimal, preserving the supported twelve fractional places without floats.
fn decimal(value: &str) -> Result<BigRational, MoneyError> {
    parse_decimal(value, 64, 12)
}
/// Longest provider-reported amount `reported_usd_micros` accepts. The decoder expands an
/// exponent to at most 4,096 places beside at most 1,024 mantissa digits.
pub const REPORTED_AMOUNT_MAX_LEN: usize = 8192;
fn parse_decimal(
    value: &str,
    max_len: usize,
    max_fraction: usize,
) -> Result<BigRational, MoneyError> {
    if value.is_empty() || value.len() > max_len {
        return Err(MoneyError::InvalidDecimal);
    }
    let mut parts = value.split('.');
    let whole = parts.next().ok_or(MoneyError::InvalidDecimal)?;
    let fraction = parts.next();
    if whole.is_empty()
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || parts.next().is_some()
        || fraction.is_some_and(|f| {
            f.is_empty() || f.len() > max_fraction || !f.bytes().all(|b| b.is_ascii_digit())
        })
    {
        return Err(MoneyError::InvalidDecimal);
    }
    let mut amount =
        BigRational::from_integer(whole.parse().map_err(|_| MoneyError::InvalidDecimal)?);
    if let Some(fraction) = fraction {
        amount += BigRational::new(
            fraction.parse().map_err(|_| MoneyError::InvalidDecimal)?,
            format!("1{}", "0".repeat(fraction.len()))
                .parse()
                .map_err(|_| MoneyError::InvalidDecimal)?,
        );
    }
    Ok(amount)
}
/// Round a provider-reported USD amount of any fractional length to whole micro-USD, half
/// up (an exact half micro rounds to the next micro), the unit the ledger stores. A
/// positive amount that rounds to zero is `SubMicro`, never a silent zero.
pub fn reported_usd_micros(amount: &str) -> Result<i64, MoneyError> {
    let micros = parse_decimal(amount, REPORTED_AMOUNT_MAX_LEN, REPORTED_AMOUNT_MAX_LEN)?
        * BigRational::from_integer(MICROS_PER_USD.into());
    let exact = CostAccumulator {
        micros: micros.clone(),
    };
    let rounded = exact.micro_usd(Rounding::Nearest)?;
    if rounded == 0 && micros != BigRational::from_integer(0.into()) {
        return Err(MoneyError::SubMicro);
    }
    Ok(rounded)
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CostAccumulator {
    micros: BigRational,
}
impl Default for CostAccumulator {
    fn default() -> Self {
        Self {
            micros: BigRational::from_integer(0.into()),
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
        let charge = decimal(rate)?
            * BigRational::new(quantity.into(), units.into())
            * BigRational::from_integer(multiplier.into());
        self.micros += charge;
        Ok(())
    }
    /// USD per million tokens is numerically micro-USD per token.
    pub fn tokens(&mut self, quantity: i64, usd_per_million: &str) -> Result<(), MoneyError> {
        self.add(quantity, usd_per_million, 1, 1)
    }
    /// The exact published denominator survives until the complete charge is rounded.
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
    /// The provider's original decimal/currency is retained separately; only USD is converted.
    pub fn amount_usd(&mut self, amount: &str) -> Result<(), MoneyError> {
        self.units(1, amount)
    }
    pub fn compare(&self, other: &Self) -> Result<std::cmp::Ordering, MoneyError> {
        Ok(self.micros.cmp(&other.micros))
    }
    pub fn micro_usd(&self, rounding: Rounding) -> Result<i64, MoneyError> {
        let whole = self.micros.numer() / self.micros.denom();
        let remainder = self.micros.numer() % self.micros.denom();
        let extra = match rounding {
            Rounding::Nearest => {
                (&remainder * 2_u8).cmp(self.micros.denom()) != std::cmp::Ordering::Less
            }
            Rounding::Up => remainder != 0.into(),
        };
        i64::try_from(whole + u8::from(extra)).map_err(|_| MoneyError::Overflow)
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
    fn reported_amounts_round_half_up_and_never_become_a_silent_zero() {
        assert_eq!(reported_usd_micros("0.000035549999999999995"), Ok(36));
        assert_eq!(reported_usd_micros("0.0000005"), Ok(1));
        assert_eq!(
            reported_usd_micros("0.0000004999999999999999999"),
            Err(MoneyError::SubMicro)
        );
        assert_eq!(reported_usd_micros("0"), Ok(0));
        assert_eq!(
            reported_usd_micros(&format!("0.{}1", "0".repeat(199))),
            Err(MoneyError::SubMicro)
        );
        assert_eq!(reported_usd_micros("1e3"), Err(MoneyError::InvalidDecimal));
        assert_eq!(
            reported_usd_micros(&format!("1{}", "0".repeat(400))),
            Err(MoneyError::Overflow)
        );
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
        cost.amount_usd("999999999999999999999999999999999999999999")
            .unwrap();
        assert_eq!(cost.micro_usd(Rounding::Nearest), Err(MoneyError::Overflow));
        let mut cost = CostAccumulator::default();
        cost.amount_usd("9223372036854.775808").unwrap();
        assert_eq!(cost.micro_usd(Rounding::Nearest), Err(MoneyError::Overflow));
    }
    #[test]
    fn legal_coprime_units_round_once_inside_the_ledger_range() {
        let mut cost = CostAccumulator::default();
        for units in [
            1_000_000_007,
            1_000_000_009,
            1_000_000_021,
            1_000_000_033,
            1_000_000_087,
        ] {
            cost.measure(1_000_000_000, "1", units).unwrap();
        }
        // Independent Python Fraction reference, rounded after the complete exact sum.
        assert_eq!(cost.micro_usd(Rounding::Nearest), Ok(5_000_000));
        assert_eq!(cost.micro_usd(Rounding::Up), Ok(5_000_000));
    }
    #[test]
    fn exact_comparison_cannot_overflow_cross_products() {
        let mut a = CostAccumulator::default();
        let mut b = CostAccumulator::default();
        for units in [1_000_000_007, 1_000_000_009, 1_000_000_021] {
            a.measure(1_000_000_000, "1", units).unwrap();
        }
        for units in [1_000_000_021, 1_000_000_033, 1_000_000_087] {
            b.measure(1_000_000_000, "1", units).unwrap();
        }
        assert_eq!(a.compare(&b), Ok(std::cmp::Ordering::Greater));
    }
    #[test]
    fn cancelling_large_legal_factors_never_rejects_a_representable_charge() {
        let mut cost = CostAccumulator::default();
        cost.measure(i64::MAX, "999999999999", i64::MAX).unwrap();
        assert_eq!(
            cost.micro_usd(Rounding::Nearest),
            Ok(999_999_999_999_000_000)
        );
    }
}
