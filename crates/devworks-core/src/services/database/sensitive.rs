//! Sensitive decisions bind to each physical column. Aliases never participate in a policy
//! match, and an override on a different relation cannot neutralize a denied column.
use crate::ports::database::data::{DataFailure, FailureKind, PhysicalColumn};
use crate::ports::storage::{SensitivePolicy, SensitiveRule};

pub fn enforce(
    global: &SensitivePolicy,
    source: &SensitivePolicy,
    columns: &[PhysicalColumn],
) -> Result<(), DataFailure> {
    for column in columns {
        let mut denied = false;
        let mut allowed = false;
        for rule in global.rules.iter().chain(&source.rules) {
            if !matches(rule, column) {
                continue;
            }
            if rule.allow {
                allowed = true;
            } else {
                denied = true;
            }
        }
        if denied && !allowed {
            return Err(DataFailure::Refused(FailureKind::PermissionDenied));
        }
    }
    Ok(())
}

pub fn matches(rule: &SensitiveRule, column: &PhysicalColumn) -> bool {
    let selector =
        |rule: Option<&str>, actual: &str| rule.is_none_or(|name| name == "*" || name == actual);
    selector(rule.catalog.as_deref(), &column.catalog)
        && selector(rule.schema.as_deref(), &column.schema)
        && selector(rule.table.as_deref(), &column.table)
        && rule.column == column.column
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Id;

    #[test]
    fn allow_on_one_physical_relation_does_not_change_another_relations_denial() {
        let rule = |id: &str, table: Option<&str>, allow| SensitiveRule {
            id: Id::new(id).unwrap(),
            catalog: None,
            schema: None,
            table: table.map(str::to_owned),
            column: "internal".into(),
            allow,
            reason: None,
        };
        let global = SensitivePolicy {
            rules: vec![rule("default", None, false)],
            ..Default::default()
        };
        let source = SensitivePolicy {
            rules: vec![rule("one-table", Some("public_items"), true)],
            ..Default::default()
        };
        let column = |table: &str| PhysicalColumn {
            catalog: "fixture".into(),
            schema: "public".into(),
            table: table.into(),
            column: "internal".into(),
        };
        assert!(enforce(&global, &source, &[column("public_items")]).is_ok());
        assert!(
            enforce(
                &global,
                &source,
                &[column("public_items"), column("private_items")]
            )
            .is_err()
        );
    }
}
