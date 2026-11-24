//! Bind driver slots to typed values. The parser captures tokens outside quoted data and
//! the driver uses native bindings, even when a named slot is reused.
use super::statement::SqlRefusal;
use crate::protocol::records::{DbEngine, DbParameter};
use std::collections::{BTreeMap, BTreeSet};

pub fn order(
    engine: &DbEngine,
    uses: &[String],
    parameters: &[DbParameter],
) -> Result<Vec<usize>, SqlRefusal> {
    let supplied: BTreeMap<_, _> = parameters
        .iter()
        .enumerate()
        .map(|(index, parameter)| (parameter.name.0.as_str(), index))
        .collect();
    if supplied.len() != parameters.len() {
        return Err(SqlRefusal::InvalidBinding);
    }
    let ordered = match engine {
        DbEngine::MariaDb | DbEngine::MySql => {
            if uses.len() != parameters.len() || uses.iter().any(|slot| slot != "?") {
                return Err(SqlRefusal::InvalidBinding);
            }
            (0..parameters.len()).collect()
        }
        DbEngine::Postgres => numbered(uses, parameters.len(), "$", "")?,
        DbEngine::SqlServer => numbered(uses, parameters.len(), "@", "p")?,
        DbEngine::Sqlite if uses.iter().all(|slot| slot == "?") => {
            if uses.len() != parameters.len() {
                return Err(SqlRefusal::InvalidBinding);
            }
            (0..parameters.len()).collect()
        }
        DbEngine::Sqlite if uses.iter().all(|slot| slot.starts_with('?')) => {
            numbered(uses, parameters.len(), "?", "")?
        }
        DbEngine::Sqlite => {
            let mut seen = BTreeSet::new();
            let mut ordered = Vec::new();
            for slot in uses {
                let name = slot
                    .strip_prefix([':', '@', '$'])
                    .ok_or(SqlRefusal::InvalidBinding)?;
                if seen.insert(slot) {
                    ordered.push(*supplied.get(name).ok_or(SqlRefusal::InvalidBinding)?);
                }
            }
            if ordered.iter().copied().collect::<BTreeSet<_>>().len() != parameters.len() {
                return Err(SqlRefusal::InvalidBinding);
            }
            ordered
        }
    };
    Ok(ordered)
}

fn numbered(
    uses: &[String],
    count: usize,
    sigil: &str,
    prefix: &str,
) -> Result<Vec<usize>, SqlRefusal> {
    let mut seen = BTreeSet::new();
    for use_site in uses {
        let slot = use_site.to_ascii_lowercase();
        let number = slot
            .strip_prefix(sigil)
            .and_then(|rest| rest.strip_prefix(prefix))
            .filter(|rest| !rest.starts_with('0'))
            .and_then(|rest| rest.parse::<usize>().ok())
            .filter(|number| (1..=count).contains(number))
            .ok_or(SqlRefusal::InvalidBinding)?;
        seen.insert(number);
    }
    if seen.len() != count {
        return Err(SqlRefusal::InvalidBinding);
    }
    Ok((0..count).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::records::DbScalar;
    use crate::protocol::support::BoundedText;

    #[test]
    fn named_slots_bind_once_each_and_unused_values_are_refused() {
        let parameter = |name: &str| DbParameter {
            name: BoundedText(name.to_owned()),
            value: DbScalar::Null {},
        };
        let values = [parameter("first"), parameter("second")];
        let slots = |names: &[&str]| {
            names
                .iter()
                .map(|name| name.to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            order(
                &DbEngine::Sqlite,
                &slots(&[":second", ":first", ":second"]),
                &values
            )
            .unwrap(),
            [1, 0]
        );
        assert_eq!(
            order(&DbEngine::Postgres, &slots(&["$2", "$1", "$2"]), &values).unwrap(),
            [0, 1]
        );
        assert!(order(&DbEngine::Postgres, &slots(&["$2"]), &values).is_err());
        assert!(order(&DbEngine::MySql, &slots(&["?"]), &values).is_err());
    }
}
