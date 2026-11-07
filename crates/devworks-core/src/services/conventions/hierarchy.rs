//! Bounded scans of a retained four-level hierarchy; no whole subtree is retained in memory.
use crate::{
    ports::storage::*,
    protocol::Id,
    services::{
        conventions::model,
        sessions::{Failure, Refusal},
    },
};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SubtreeWitness {
    pub digest: String,
    pub count: u64,
    pub height: usize,
}
fn invalid(message: &'static str) -> Failure {
    Refusal::new(
        crate::protocol::DomainErrorCode::InvalidArgument,
        message,
        None,
    )
    .into()
}
pub(super) fn parent_depth(
    read: &dyn ConventionRead,
    row: &StoredConvention,
    parent: Option<&Id>,
) -> Result<usize, Failure> {
    let mut parent = parent.cloned();
    let mut depth = 0;
    let mut seen = std::collections::HashSet::new();
    while let Some(id) = parent {
        if id == row.id || !seen.insert(id.clone()) || depth >= model::MAX_DEPTH {
            return Err(invalid("convention_parent"));
        }
        let p = read
            .convention(&id, None, std::slice::from_ref(&row.scope))?
            .filter(|r| r.lifecycle != "Deleted")
            .ok_or_else(|| invalid("convention_parent"))?;
        depth += 1;
        parent = p.parent_id;
    }
    Ok(depth)
}
fn visit(
    read: &dyn ConventionRead,
    row: &StoredConvention,
    depth: usize,
    hash: &mut Sha256,
    witness: &mut SubtreeWitness,
) -> Result<(), Failure> {
    if depth > model::MAX_DEPTH {
        return Err(invalid("convention_depth"));
    }
    let identity = serde_json::json!([
        row.id,
        row.revision,
        row.scope.key(),
        row.parent_id,
        row.content_ref,
        row.raw_content_ref
    ]);
    hash.update(identity.to_string().as_bytes());
    witness.count += 1;
    witness.height = witness.height.max(depth);
    let mut after = None;
    loop {
        let query = ConventionQuery {
            scopes: vec![row.scope.clone()],
            lifecycles: vec!["Active".into(), "Retired".into()],
            parent_id: Some(Some(row.id.clone())),
            ..Default::default()
        };
        let rows = read.conventions(&query, after.as_ref(), 32)?;
        if rows.is_empty() {
            break;
        }
        after = rows.last().map(StoredConvention::position);
        for child in rows {
            visit(read, &child, depth + 1, hash, witness)?;
        }
    }
    Ok(())
}
pub(super) fn witness(
    read: &dyn ConventionRead,
    row: &StoredConvention,
) -> Result<SubtreeWitness, Failure> {
    let mut result = SubtreeWitness {
        digest: String::new(),
        count: 0,
        height: 0,
    };
    let mut hash = Sha256::new();
    visit(read, row, 0, &mut hash, &mut result)?;
    result.digest = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
    Ok(result)
}

/// Portable parent components are literal scoped slugs, not filesystem paths or fuzzy names.
pub(crate) fn parent_path(
    tx: &mut dyn WriteTransaction,
    scope: &KnowledgeScope,
    path: &[String],
) -> Result<Option<StoredConvention>, Failure> {
    if path.len() > model::MAX_DEPTH {
        return Err(invalid("convention_depth"));
    }
    let mut parent = None;
    let mut result = None;
    for slug in path {
        let query = ConventionQuery {
            scopes: vec![scope.clone()],
            lifecycles: vec!["Active".into(), "Retired".into()],
            parent_id: Some(parent.clone()),
            slug: Some(slug.clone()),
            ..Default::default()
        };
        let mut rows = tx.conventions().conventions(&query, None, 2)?;
        if rows.len() != 1 {
            return Err(invalid("convention_parent"));
        }
        let row = rows.remove(0);
        parent = Some(row.id.clone());
        result = Some(row);
    }
    Ok(result)
}
