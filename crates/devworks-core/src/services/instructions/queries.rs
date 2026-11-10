//! `skills.discover`, `.read`, `.resources`, `.history` and `roles.read`, `.history`.
use super::{
    HARNESS_ROOT_ID, InstructionService, SHARED_ROOT_ID, config_scope, current_revision, invalid,
    not_found_failure, record, visible_item,
};
use crate::context::InvocationContext;
use crate::operations::{roles as role_ops, skills as skill_ops};
use crate::ports::storage::{
    ConfigScope, InstructionFilter, InstructionKind, InstructionOrigin, InstructionRead,
    ReadSnapshot, StorageError, StoredInstructionItem,
};
use crate::protocol::Id;
use crate::protocol::provider::ArtifactRef;
use crate::protocol::records::{InstructionRecord, PageRequest};
use crate::protocol::results::OperationResult;
use crate::protocol::support::{BoundedList, BoundedText, Page};
use crate::services::sessions::Failure;
use std::collections::BTreeMap;

fn offset(page: &PageRequest, binding: &str) -> Result<usize, Failure> {
    match &page.cursor {
        None => Ok(0),
        Some(c) => {
            c.0.strip_suffix(&format!(":{binding}"))
                .and_then(|n| n.parse::<usize>().ok())
                .ok_or(Failure::Storage(StorageError::CursorExpired))
        }
    }
}
fn paged<T>(all: Vec<T>, page: &PageRequest, binding: &str) -> Result<Page<T>, Failure> {
    let start = offset(page, binding)?;
    if start > all.len() {
        return Err(Failure::Storage(StorageError::CursorExpired));
    }
    let end = (start + page.limit.get() as usize).min(all.len());
    let has_more = end < all.len();
    Ok(Page {
        items: BoundedList(all.into_iter().skip(start).take(end - start).collect()),
        next_cursor: has_more.then(|| BoundedText(format!("{end}:{binding}"))),
        has_more,
    })
}

/// Every skill item, with which one of each name is effective (a present, enabled, active
/// harness skill over a shared skill of the same name).
pub(crate) fn skill_items(
    read: &dyn InstructionRead,
) -> Result<Vec<(StoredInstructionItem, bool)>, StorageError> {
    let mut all = Vec::new();
    let mut after: Option<Id> = None;
    loop {
        let page = read.instruction_items(
            &InstructionFilter {
                kind: Some(InstructionKind::Skill),
                scopes: vec![ConfigScope::Workspace],
                name: None,
            },
            after.as_ref(),
            crate::ports::storage::MAX_PAGE,
        )?;
        let done = page.len() < crate::ports::storage::MAX_PAGE as usize;
        after = page.last().map(|i| i.id.clone());
        all.extend(page);
        if done {
            break;
        }
    }
    let usable = |i: &StoredInstructionItem| i.present && i.enabled && i.active_revision.is_some();
    let mut winner: BTreeMap<String, (u8, Id)> = BTreeMap::new();
    for item in all.iter().filter(|i| usable(i)) {
        let rank = if item.origin == InstructionOrigin::Harness {
            0
        } else {
            1
        };
        let entry = winner
            .entry(item.name.clone())
            .or_insert((rank, item.id.clone()));
        if rank < entry.0 {
            *entry = (rank, item.id.clone());
        }
    }
    all.sort_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then(a.origin.as_str().cmp(b.origin.as_str()))
    });
    Ok(all
        .into_iter()
        .map(|i| {
            let effective = winner.get(&i.name).is_some_and(|(_, id)| *id == i.id);
            (i, effective)
        })
        .collect())
}

impl InstructionService {
    pub(crate) fn skill_query(
        &self,
        read: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        input: &skill_ops::Input,
    ) -> Result<(OperationResult, i64), Failure> {
        let instructions = read.instructions();
        match input {
            skill_ops::Input::Discover(i) => {
                // Skills are workspace resources visible from every project scope.
                config_scope(ctx, &i.scope)?;
                let roots: Vec<&str> = i.root_ids.0.iter().map(Id::as_str).collect();
                if roots
                    .iter()
                    .any(|r| *r != SHARED_ROOT_ID && *r != HARNESS_ROOT_ID)
                {
                    return Err(invalid("skill_root"));
                }
                let mut records = Vec::new();
                let mut fence = 0i64;
                for (item, effective) in skill_items(instructions)? {
                    let root = if item.origin == InstructionOrigin::Shared {
                        SHARED_ROOT_ID
                    } else {
                        HARNESS_ROOT_ID
                    };
                    if !roots.is_empty() && !roots.contains(&root) {
                        continue;
                    }
                    // A harness item with nothing approved yet (a pending agent draft) is not
                    // a discoverable skill.
                    if item.active_revision.is_none() {
                        continue;
                    }
                    fence = fence.max(item.version);
                    let revision = current_revision(instructions, &item)?;
                    records.push(record(&item, &revision, Some(effective))?);
                }
                let binding = crate::fingerprint::sha256_hex(
                    serde_json::to_string(
                        &records
                            .iter()
                            .map(|r| (r.id.as_str(), r.revision.get()))
                            .collect::<Vec<_>>(),
                    )
                    .expect("binding")
                    .as_bytes(),
                );
                Ok((
                    OperationResult::SkillsDiscover {
                        value: paged(records, &i.page, &binding[..16])?,
                    },
                    fence.max(1),
                ))
            }
            skill_ops::Input::Read(i) => {
                let (record, revision) = self.read_record(
                    instructions,
                    ctx,
                    &i.instruction_id,
                    InstructionKind::Skill,
                    i.revision.as_ref().map(|r| r.get()),
                )?;
                Ok((OperationResult::SkillsRead { value: record }, revision))
            }
            skill_ops::Input::Resources(i) => {
                let item =
                    visible_item(instructions, ctx, &i.instruction_id, InstructionKind::Skill)?;
                let revision = instructions
                    .instruction_revision(&item.id, i.revision.get())?
                    .ok_or_else(|| not_found_failure(InstructionKind::Skill, &item.id))?;
                let refs: Vec<ArtifactRef> = revision
                    .resources
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|r| serde_json::from_value(r["artifact"].clone()))
                    .collect::<Result<_, _>>()
                    .map_err(|e| StorageError::InvalidRecord(format!("skill resource: {e}")))?;
                let binding = format!("{}-{}", item.id.as_str(), revision.revision);
                Ok((
                    OperationResult::SkillsResources {
                        value: paged(refs, &i.page, &binding)?,
                    },
                    item.version,
                ))
            }
            skill_ops::Input::History(i) => {
                let (page, revision) = self.history(
                    instructions,
                    ctx,
                    &i.instruction_id,
                    InstructionKind::Skill,
                    &i.page,
                )?;
                Ok((OperationResult::SkillsHistory { value: page }, revision))
            }
            _ => Err(invalid("instruction_operation")),
        }
    }

    pub(crate) fn role_query(
        &self,
        read: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        input: &role_ops::Input,
    ) -> Result<(OperationResult, i64), Failure> {
        let instructions = read.instructions();
        match input {
            role_ops::Input::Read(i) => {
                let (record, revision) = self.read_record(
                    instructions,
                    ctx,
                    &i.instruction_id,
                    InstructionKind::Role,
                    i.revision.as_ref().map(|r| r.get()),
                )?;
                Ok((OperationResult::RolesRead { value: record }, revision))
            }
            role_ops::Input::History(i) => {
                let (page, revision) = self.history(
                    instructions,
                    ctx,
                    &i.instruction_id,
                    InstructionKind::Role,
                    &i.page,
                )?;
                Ok((OperationResult::RolesHistory { value: page }, revision))
            }
            _ => Err(invalid("instruction_operation")),
        }
    }

    fn read_record(
        &self,
        read: &dyn InstructionRead,
        ctx: &InvocationContext,
        id: &Id,
        kind: InstructionKind,
        revision: Option<i64>,
    ) -> Result<(InstructionRecord, i64), Failure> {
        let item = visible_item(read, ctx, id, kind)?;
        let r = match revision {
            Some(n) => read
                .instruction_revision(&item.id, n)?
                .ok_or_else(|| not_found_failure(kind, &item.id))?,
            None => current_revision(read, &item)?,
        };
        let effective = if kind == InstructionKind::Skill {
            skill_items(read)?
                .into_iter()
                .find(|(i, _)| i.id == item.id)
                .map(|(_, e)| e)
        } else {
            None
        };
        Ok((record(&item, &r, effective)?, item.version))
    }

    fn history(
        &self,
        read: &dyn InstructionRead,
        ctx: &InvocationContext,
        id: &Id,
        kind: InstructionKind,
        page: &PageRequest,
    ) -> Result<(Page<InstructionRecord>, i64), Failure> {
        let item = visible_item(read, ctx, id, kind)?;
        let before = match &page.cursor {
            None => None,
            Some(c) => Some(
                c.0.strip_suffix(&format!(":{}", item.id.as_str()))
                    .and_then(|n| n.parse::<i64>().ok())
                    .ok_or(Failure::Storage(StorageError::CursorExpired))?,
            ),
        };
        let limit = (page.limit.get() as u16).min(crate::ports::storage::MAX_PAGE - 1);
        let rows = read.instruction_revisions(&item.id, before, limit + 1)?;
        let has_more = rows.len() > limit as usize;
        let rows: Vec<_> = rows.into_iter().take(limit as usize).collect();
        let next = has_more
            .then(|| {
                rows.last()
                    .map(|r| BoundedText(format!("{}:{}", r.revision, item.id.as_str())))
            })
            .flatten();
        let items = rows
            .iter()
            .map(|r| record(&item, r, None))
            .collect::<Result<Vec<_>, _>>()?;
        Ok((
            Page {
                items: BoundedList(items),
                next_cursor: next,
                has_more,
            },
            item.version,
        ))
    }
}
