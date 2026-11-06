//! Native shared-owner regressions: no test handler, direct catalog inserts or model execution.
use super::*;
fn imported_order(f: &Fx, name: &str, order: Option<i32>) -> StoredConvention {
    let mut value = manifest(0);
    value["slug"] = json!(name);
    value["title"] = json!(name);
    match order {
        Some(order) => value["sortOrder"] = json!(order),
        None => {
            value.as_object_mut().unwrap().remove("sortOrder");
        }
    }
    let source = f.source(&serde_json::to_vec(&value).unwrap());
    let op = f.admit(&source, &format!("create-{name}"));
    let result = f.dispatch(&op);
    assert_eq!(result.state, OperationState::ObservedComplete, "{result:?}");
    let draft = read(f.store.as_ref(), |r| r.conventions().convention_import(&op)).unwrap();
    read(f.store.as_ref(), |r| {
        r.conventions()
            .convention(&draft.convention_id, None, &[KnowledgeScope::Workspace])
    })
    .unwrap()
}
#[test]
fn omitted_order_appends_but_explicit_zero_remains_an_explicit_choice() {
    let f = Fx::new();
    assert_eq!(imported_order(&f, "first", None).sort_order, 0);
    assert_eq!(imported_order(&f, "append", None).sort_order, 1);
    assert_eq!(imported_order(&f, "explicit-zero", Some(0)).sort_order, 0);
    assert_eq!(
        imported_order(&f, "append-after-explicit", None).sort_order,
        2
    );
}
#[test]
fn omitted_order_retains_frozen_unchecked_i32_maximum_wrapping() {
    let f = Fx::new();
    assert_eq!(
        imported_order(&f, "maximum", Some(i32::MAX)).sort_order,
        i64::from(i32::MAX)
    );
    assert_eq!(
        imported_order(&f, "wrapped", None).sort_order,
        i64::from(i32::MIN)
    );
    assert_eq!(imported_order(&f, "still-explicit", Some(0)).sort_order, 0);
}
