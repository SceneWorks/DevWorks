//! Host-declared usage semantics. A codec supplies quantities, never billing authority.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ObservationSemantics {
    Cumulative,
    Delta,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputSemantics {
    /// The input count already contains cache reads/writes as subsets.
    Inclusive,
    /// Messages-style input, cache reads and cache writes are disjoint categories.
    Disjoint,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeasuredQuantity {
    pub modality: String,
    pub charge_kind: String,
    pub unit: String,
    pub cache_write_ttl_ms: Option<i64>,
    pub quantity: i64,
}
impl MeasuredQuantity {
    fn same_kind(&self, other: &Self) -> bool {
        self.modality == other.modality
            && self.charge_kind == other.charge_kind
            && self.unit == other.unit
            && self.cache_write_ttl_ms == other.cache_write_ttl_ms
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quantities {
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cached_read_tokens: Option<i64>,
    pub cache_write_tokens: Option<i64>,
    /// Reasoning is an output subset; it is observable but never added to output again.
    pub reasoning_tokens: Option<i64>,
    pub additional: Vec<MeasuredQuantity>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MeterError {
    NegativeQuantity,
    InvalidUnit,
    DuplicateUnit,
    Overflow,
    InvalidSubset,
    StaleSequence,
    SemanticsChanged,
}

impl Quantities {
    pub fn validate(&self) -> Result<(), MeterError> {
        if [
            self.input_tokens,
            self.output_tokens,
            self.cached_read_tokens,
            self.cache_write_tokens,
            self.reasoning_tokens,
        ]
        .into_iter()
        .flatten()
        .any(|n| n < 0)
        {
            return Err(MeterError::NegativeQuantity);
        }
        if self.additional.len() > 128 {
            return Err(MeterError::InvalidUnit);
        }
        for (index, value) in self.additional.iter().enumerate() {
            if value.quantity < 0 {
                return Err(MeterError::NegativeQuantity);
            }
            if [&value.modality, &value.charge_kind, &value.unit]
                .into_iter()
                .any(|s| s.is_empty() || s.len() > 128 || s.chars().any(char::is_control))
                || value.cache_write_ttl_ms.is_some_and(|n| n <= 0)
            {
                return Err(MeterError::InvalidUnit);
            }
            if self.additional[..index].iter().any(|v| v.same_kind(value)) {
                return Err(MeterError::DuplicateUnit);
            }
        }
        if matches!((self.reasoning_tokens, self.output_tokens), (Some(reasoning),Some(output)) if reasoning > output)
        {
            return Err(MeterError::InvalidSubset);
        }
        Ok(())
    }

    /// An absent component remains unknown. A reported zero stays a known zero.
    pub fn total_input(&self, semantics: InputSemantics) -> Result<Option<i64>, MeterError> {
        match semantics {
            InputSemantics::Inclusive => Ok(self.input_tokens),
            InputSemantics::Disjoint => match (
                self.input_tokens,
                self.cached_read_tokens,
                self.cache_write_tokens,
            ) {
                (Some(input), Some(read), Some(write)) => input
                    .checked_add(read)
                    .and_then(|n| n.checked_add(write))
                    .map(Some)
                    .ok_or(MeterError::Overflow),
                _ => Ok(None),
            },
        }
    }

    pub fn uncached_input(&self, semantics: InputSemantics) -> Result<Option<i64>, MeterError> {
        match semantics {
            InputSemantics::Disjoint => Ok(self.input_tokens),
            InputSemantics::Inclusive => match (
                self.input_tokens,
                self.cached_read_tokens,
                self.cache_write_tokens,
            ) {
                (Some(input), Some(read), Some(write)) => input
                    .checked_sub(read)
                    .and_then(|n| n.checked_sub(write))
                    .filter(|n| *n >= 0)
                    .map(Some)
                    .ok_or(MeterError::InvalidSubset),
                _ => Ok(None),
            },
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meter {
    pub quantities: Quantities,
    pub last_sequence: i64,
    pub semantics: Option<ObservationSemantics>,
}
impl Meter {
    /// The storage observation key provides durable duplicate detection before this method.
    /// Provider event sequences may contain text between usage events, so gaps are valid.
    pub fn observe(
        &mut self,
        sequence: i64,
        semantics: ObservationSemantics,
        value: &Quantities,
    ) -> Result<(), MeterError> {
        value.validate()?;
        if sequence <= self.last_sequence {
            return Err(MeterError::StaleSequence);
        }
        if self.semantics.is_some_and(|before| before != semantics) {
            return Err(MeterError::SemanticsChanged);
        }
        let mut merged = self.quantities.clone();
        for (current, incoming) in [
            (&mut merged.input_tokens, value.input_tokens),
            (&mut merged.output_tokens, value.output_tokens),
            (&mut merged.cached_read_tokens, value.cached_read_tokens),
            (&mut merged.cache_write_tokens, value.cache_write_tokens),
            (&mut merged.reasoning_tokens, value.reasoning_tokens),
        ] {
            if let Some(incoming) = incoming {
                *current = Some(match semantics {
                    ObservationSemantics::Cumulative => incoming,
                    ObservationSemantics::Delta => current
                        .unwrap_or(0)
                        .checked_add(incoming)
                        .ok_or(MeterError::Overflow)?,
                });
            }
        }
        for incoming in &value.additional {
            match merged.additional.iter_mut().find(|v| v.same_kind(incoming)) {
                Some(current) => {
                    current.quantity = match semantics {
                        ObservationSemantics::Cumulative => incoming.quantity,
                        ObservationSemantics::Delta => current
                            .quantity
                            .checked_add(incoming.quantity)
                            .ok_or(MeterError::Overflow)?,
                    }
                }
                None => merged.additional.push(incoming.clone()),
            }
        }
        merged.validate()?;
        self.quantities = merged;
        self.last_sequence = sequence;
        self.semantics = Some(semantics);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cumulative_fields_replace_instead_of_double_counting_and_missing_stays_unknown() {
        let mut meter = Meter::default();
        meter
            .observe(
                2,
                ObservationSemantics::Cumulative,
                &Quantities {
                    input_tokens: Some(100),
                    cached_read_tokens: Some(40),
                    ..Default::default()
                },
            )
            .unwrap();
        meter
            .observe(
                9,
                ObservationSemantics::Cumulative,
                &Quantities {
                    input_tokens: Some(120),
                    output_tokens: Some(0),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(meter.quantities.input_tokens, Some(120));
        assert_eq!(meter.quantities.cached_read_tokens, Some(40));
        assert_eq!(meter.quantities.output_tokens, Some(0));
        assert_eq!(
            meter.quantities.uncached_input(InputSemantics::Inclusive),
            Ok(None)
        );
        meter
            .observe(
                10,
                ObservationSemantics::Cumulative,
                &Quantities {
                    cache_write_tokens: Some(0),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            meter.quantities.uncached_input(InputSemantics::Inclusive),
            Ok(Some(80))
        );
    }

    #[test]
    fn disjoint_inputs_and_reasoning_subsets_do_not_double_count() {
        let q = Quantities {
            input_tokens: Some(20),
            cached_read_tokens: Some(30),
            cache_write_tokens: Some(10),
            output_tokens: Some(15),
            reasoning_tokens: Some(5),
            additional: vec![],
        };
        assert_eq!(q.total_input(InputSemantics::Disjoint), Ok(Some(60)));
        assert_eq!(q.uncached_input(InputSemantics::Disjoint), Ok(Some(20)));
        assert_eq!(q.output_tokens, Some(15));
        assert_eq!(
            q.uncached_input(InputSemantics::Inclusive),
            Err(MeterError::InvalidSubset)
        );
    }

    #[test]
    fn delta_counts_add_only_observed_fields_and_refused_changes_are_atomic() {
        let mut meter = Meter::default();
        meter
            .observe(
                1,
                ObservationSemantics::Delta,
                &Quantities {
                    output_tokens: Some(3),
                    ..Default::default()
                },
            )
            .unwrap();
        meter
            .observe(
                2,
                ObservationSemantics::Delta,
                &Quantities {
                    output_tokens: Some(4),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(meter.quantities.output_tokens, Some(7));
        assert_eq!(meter.quantities.input_tokens, None);
        let before = meter.clone();
        assert_eq!(
            meter.observe(2, ObservationSemantics::Delta, &Quantities::default()),
            Err(MeterError::StaleSequence)
        );
        assert_eq!(
            meter.observe(3, ObservationSemantics::Cumulative, &Quantities::default()),
            Err(MeterError::SemanticsChanged)
        );
        assert_eq!(
            meter.observe(
                3,
                ObservationSemantics::Delta,
                &Quantities {
                    output_tokens: Some(i64::MAX),
                    ..Default::default()
                }
            ),
            Err(MeterError::Overflow)
        );
        assert_eq!(meter, before);
    }
}
