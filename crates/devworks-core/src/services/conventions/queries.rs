//! Scoped metadata pages. Selection, ordering and source generations precede pagination.
use super::{ConventionService, access, model};
use crate::{
    context::InvocationContext,
    ports::storage::*,
    protocol::{
        Id, Revision,
        data::DataScope,
        records::*,
        support::{BoundedList, BoundedText},
    },
    services::{
        cursor::CursorBinding,
        sessions::{Failure, Refusal, rev},
    },
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

pub(super) fn lifecycles(
    values: Option<&crate::protocol::support::Items<ConventionLifecycle>>,
) -> Vec<String> {
    values
        .map(|v| {
            v.0.iter()
                .map(|s| {
                    serde_json::to_value(s)
                        .expect("closed enum")
                        .as_str()
                        .expect("enum string")
                        .to_owned()
                })
                .collect()
        })
        .unwrap_or_else(|| vec!["Active".into()])
}
pub(super) fn scope_revision(
    read: &dyn ReadSnapshot,
    scopes: &[KnowledgeScope],
) -> Result<i64, StorageError> {
    scopes
        .iter()
        .try_fold(0_i64, |total, scope| {
            total
                .checked_add(read.conventions().convention_scope_revision(scope)?)
                .ok_or_else(|| StorageError::InvalidRecord("scope revision overflow".into()))
        })
        .map(|n| n.max(1))
}
pub(super) fn binding<T: Serialize>(
    ctx: &InvocationContext,
    op: &str,
    input: &T,
    revision: i64,
) -> Result<CursorBinding, StorageError> {
    let mut value = serde_json::to_value(input).map_err(invalid)?;
    if let Some(v) = value.get_mut("page").and_then(Value::as_object_mut) {
        v.remove("cursor");
    }
    if let Some(v) = value.as_object_mut() {
        v.remove("cursor");
    }
    let normalized = serde_json::to_string(&value).map_err(invalid)?;
    Ok(CursorBinding::new(ctx, op, &[&normalized], revision))
}
fn invalid(error: impl std::fmt::Display) -> StorageError {
    StorageError::InvalidRecord(error.to_string())
}
pub(super) fn parse<T: DeserializeOwned>(value: Value) -> Result<T, StorageError> {
    serde_json::from_value(value).map_err(invalid)
}
fn page_budget<T: Serialize>(rows: &mut Vec<T>) -> Result<bool, StorageError> {
    // Keep room for the actual envelope, stamp, cursor and metadata. Never truncate a field.
    let ceiling = crate::framing::MAX_FRAME_BYTES - 16_384;
    let mut bytes = 0_usize;
    let mut retained = 0;
    for row in rows.iter() {
        let size = serde_json::to_vec(row).map_err(invalid)?.len() + 1;
        if bytes + size > ceiling {
            break;
        }
        bytes += size;
        retained += 1;
    }
    if !rows.is_empty() && retained == 0 {
        return Err(invalid(
            "one convention metadata row exceeds the response bound",
        ));
    }
    let more = retained < rows.len();
    rows.truncate(retained);
    Ok(more)
}
impl ConventionService {
    pub(super) fn catalog_page(
        &self,
        read: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        input: &crate::operations::conventions::ListInput,
        op: &str,
    ) -> Result<ConventionPage, Failure> {
        let scope = &input.scope;
        let page = &input.page;
        let applicability = &input.applicability;
        let states = input.lifecycles.as_ref();
        let order = input.order.as_ref();
        let scoped = access::scope(read, ctx, Some(scope), false)?;
        let revision = scope_revision(read, &scoped.readable)?;
        let cursor = binding(ctx, op, input, revision)?;
        let query = ConventionQuery {
            scopes: scoped.readable.clone(),
            order: match order {
                Some(crate::protocol::records::ConventionOrder::Organization) => {
                    crate::ports::storage::ConventionOrder::Organization
                }
                _ => crate::ports::storage::ConventionOrder::Title,
            },
            lifecycles: lifecycles(states),
            applicability: applicability.0.iter().map(|s| s.0.clone()).collect(),
            ..Default::default()
        };
        let after = page
            .cursor
            .as_ref()
            .map(|c| cursor.decode(&c.0))
            .transpose()?
            .map(|s| Id::new(s).map_err(|_| StorageError::CursorExpired))
            .transpose()?;
        let after = after
            .as_ref()
            .map(|id| {
                read.conventions()
                    .convention(id, None, &scoped.readable)?
                    .map(|r| r.position())
                    .ok_or(StorageError::CursorExpired)
            })
            .transpose()?;
        let selected =
            read.conventions()
                .conventions(&query, after.as_ref(), page.limit.get() as u16)?;
        let mut rows = selected
            .iter()
            .map(|row| model::record(row, self.index.status(read, row)?))
            .collect::<Result<Vec<_>, StorageError>>()?;
        let clipped = page_budget(&mut rows)?;
        let last = rows.last().map(|r| r.id.clone());
        let more = clipped
            || if let Some(id) = last.as_ref() {
                let row = selected.iter().find(|r| &r.id == id).expect("selected row");
                !read
                    .conventions()
                    .conventions(&query, Some(&row.position()), 1)?
                    .is_empty()
            } else {
                false
            };
        Ok(ConventionPage {
            items: BoundedList(rows),
            next_cursor: if more {
                last.map(|id| BoundedText(cursor.encode(id.as_str())))
            } else {
                None
            },
            has_more: more,
            scope_revision: rev(revision),
            manifest_schema: model::manifest_schema(),
        })
    }
    pub(super) fn versions(
        &self,
        read: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        input: &crate::operations::conventions::VersionInput,
    ) -> Result<ConventionPage, Failure> {
        let scoped = access::scope(read, ctx, input.scope.as_ref(), false)?;
        let owner = access::owner(read, ctx, &input.convention_id, None, input.scope.as_ref())?;
        let revision = scope_revision(read, &scoped.readable)?;
        let cursor = binding(ctx, "conventions.version", input, revision)?;
        let before = input
            .page
            .cursor
            .as_ref()
            .map(|c| cursor.decode(&c.0))
            .transpose()?
            .map(|s| s.parse::<i64>().map_err(|_| StorageError::CursorExpired))
            .transpose()?;
        let selected = read.conventions().convention_versions(
            &owner.id,
            &scoped.readable,
            before,
            input.page.limit.get() as u16,
        )?;
        let mut rows = selected
            .iter()
            .map(|row| model::record(row, self.index.status(read, row)?))
            .collect::<Result<Vec<_>, StorageError>>()?;
        let clipped = page_budget(&mut rows)?;
        let last = rows.last().map(|r| r.revision.get());
        let more = clipped
            || if let Some(n) = last {
                !read
                    .conventions()
                    .convention_versions(&owner.id, &scoped.readable, Some(n), 1)?
                    .is_empty()
            } else {
                false
            };
        Ok(ConventionPage {
            items: BoundedList(rows),
            next_cursor: if more {
                last.map(|n| BoundedText(cursor.encode(&n.to_string())))
            } else {
                None
            },
            has_more: more,
            scope_revision: rev(revision),
            manifest_schema: model::manifest_schema(),
        })
    }
    pub(super) fn attachments(
        &self,
        read: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        input: &crate::operations::conventions::ResourcesInput,
        id: &Id,
        owner_revision: Option<&Revision>,
        states: Option<&crate::protocol::support::Items<ConventionLifecycle>>,
    ) -> Result<ConventionResourcePage, Failure> {
        let scoped = access::scope(read, ctx, Some(&input.scope), false)?;
        let owner = access::owner(
            read,
            ctx,
            id,
            owner_revision.map(Revision::get),
            Some(&input.scope),
        )?;
        let revision = scope_revision(read, &scoped.readable)?;
        let cursor = binding(ctx, "conventions.resources", input, revision)?;
        let after = input
            .page
            .cursor
            .as_ref()
            .map(|c| cursor.decode(&c.0))
            .transpose()?
            .map(|s| Id::new(s).map_err(|_| StorageError::CursorExpired))
            .transpose()?;
        let query = ConventionResourceQuery {
            scopes: scoped.readable,
            convention_id: Some(id.clone()),
            convention_revision: Some(owner.revision),
            lifecycles: lifecycles(states),
        };
        let selected = read.conventions().convention_resources(
            &query,
            after.as_ref(),
            input.page.limit.get() as u16,
        )?;
        let mut rows = selected
            .iter()
            .map(|row| model::resource(row, &owner))
            .collect::<Result<Vec<_>, _>>()?;
        let clipped = page_budget(&mut rows)?;
        let last = rows.last().map(|r| r.id.clone());
        let more = clipped
            || if let Some(id) = last.as_ref() {
                !read
                    .conventions()
                    .convention_resources(&query, Some(id), 1)?
                    .is_empty()
            } else {
                false
            };
        Ok(ConventionResourcePage {
            items: BoundedList(rows),
            next_cursor: if more {
                last.map(|id| BoundedText(cursor.encode(id.as_str())))
            } else {
                None
            },
            has_more: more,
            scope_revision: rev(revision),
        })
    }
    /// Tool lookup preserves exact hierarchical paths, then unique source-compatible fuzzy
    /// slug/title matching. Virtual resource lookup passes exact_root and never falls back.
    pub(super) fn resolve_slug(
        &self,
        read: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        scope: &DataScope,
        slug: &str,
        exact_root: bool,
    ) -> Result<StoredConvention, Failure> {
        let scoped = access::scope(read, ctx, Some(scope), false)?;
        let exact = ConventionQuery {
            scopes: scoped.readable.clone(),
            lifecycles: vec!["Active".into()],
            parent_id: Some(None),
            slug: Some(slug.to_owned()),
            ..Default::default()
        };
        let mut roots = read.conventions().conventions(&exact, None, 2)?;
        if roots.len() == 1 {
            return Ok(roots.remove(0));
        }
        if roots.len() > 1 {
            return Err(Refusal::new(
                crate::protocol::DomainErrorCode::InvalidArgument,
                "ambiguous_convention",
                None,
            )
            .into());
        }
        let mut parent = None;
        let mut found = None;
        let segments: Vec<_> = if exact_root {
            vec![slug]
        } else {
            slug.split('/').filter(|s| !s.is_empty()).collect()
        };
        for segment in &segments {
            let q = ConventionQuery {
                scopes: scoped.readable.clone(),
                lifecycles: vec!["Active".into()],
                parent_id: Some(parent.clone()),
                slug: Some((*segment).to_owned()),
                ..Default::default()
            };
            let rows = read.conventions().conventions(&q, None, 2)?;
            if rows.len() != 1 {
                found = None;
                break;
            }
            let row = rows.into_iter().next().expect("one");
            parent = Some(row.id.clone());
            found = Some(row);
        }
        if let Some(row) = found {
            return Ok(row);
        }
        if !exact_root {
            let q = ConventionQuery {
                scopes: scoped.readable,
                lifecycles: vec!["Active".into()],
                name_pattern: Some(format!("%{slug}%")),
                ..Default::default()
            };
            let mut rows = read.conventions().conventions(&q, None, 2)?;
            if rows.len() == 1 {
                return Ok(rows.remove(0));
            }
            if rows.len() > 1 {
                return Err(Refusal::new(
                    crate::protocol::DomainErrorCode::InvalidArgument,
                    "ambiguous_convention",
                    None,
                )
                .into());
            }
        }
        Err(Refusal::new(
            crate::protocol::DomainErrorCode::NotFound,
            "convention",
            None,
        )
        .into())
    }
}
