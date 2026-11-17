//! `skills.discover`, `.read`, `.resources`, `.history` and `roles.read`, `.history`.
use super::{
    HARNESS_ROOT_ID, InstructionService, SHARED_ROOT_ID, config_scope, invalid, item_visible,
    record, record_with_approval, revision_visible, visible_current, visible_item,
    visible_revision,
};
use crate::context::InvocationContext;
use crate::operations::{roles as role_ops, skills as skill_ops};
use crate::ports::storage::{
    ConfigScope, InstructionFilter, InstructionKind, InstructionOrigin, InstructionRead,
    ReadSnapshot, StorageError, StoredInstructionItem,
};
use crate::protocol::records::{
    InstructionRecord, PageRequest, SkillResource, SkillResourcePage, SkillResourcePageResult,
};
use crate::protocol::results::OperationResult;
use crate::protocol::support::Page;
use crate::protocol::{Id, ProjectId};
use crate::services::cursor::{self, CursorBinding};
use crate::services::sessions::Failure;
use std::collections::BTreeMap;

/// How a skill item stands among the items sharing its (case-insensitive) name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Standing {
    /// The one in effect: a present, enabled, active harness skill over a shared one.
    Effective,
    /// A higher-precedence item of the same name is in effect.
    Overridden,
    /// Two or more items share the highest precedence (names that differ only in case on a
    /// case-sensitive filesystem): a material conflict. None of them is in effect until the
    /// user resolves it by renaming or removing one.
    Conflict,
    /// Not usable: nothing approved yet, or the directory is missing or disabled.
    Inactive,
}
impl Standing {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Effective => "effective",
            Self::Overridden => "overridden",
            Self::Conflict => "conflict",
            Self::Inactive => "inactive",
        }
    }
}

/// Every skill item an agent working in `project` can use (the workspace's and that project's)
/// with its standing. Another project's skills are never read.
pub(crate) fn skill_items_in(
    read: &dyn InstructionRead,
    project: Option<&ProjectId>,
) -> Result<Vec<(StoredInstructionItem, Standing)>, StorageError> {
    let mut scopes = vec![ConfigScope::Workspace];
    scopes.extend(project.map(|p| ConfigScope::Project(p.clone())));
    let mut all = Vec::new();
    let mut after: Option<Id> = None;
    loop {
        let page = read.instruction_items(
            &InstructionFilter {
                kind: Some(InstructionKind::Skill),
                scopes: scopes.clone(),
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
    // A project's skill overrides a workspace skill of the same name, which overrides a shared
    // one, so the most specific skill is the one in effect.
    let rank = |i: &StoredInstructionItem| match (&i.scope, &i.origin) {
        (ConfigScope::Project(_), _) => 0,
        (_, InstructionOrigin::Harness) => 1,
        _ => 2,
    };
    // Per folded name: the best rank and every usable item holding it.
    let mut best: BTreeMap<String, (u8, Vec<Id>)> = BTreeMap::new();
    for item in all.iter().filter(|i| usable(i)) {
        let r = rank(item);
        let entry = best
            .entry(item.name.to_lowercase())
            .or_insert((r, Vec::new()));
        if r < entry.0 {
            *entry = (r, Vec::new());
        }
        if r == entry.0 {
            entry.1.push(item.id.clone());
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
            let standing = match best.get(&i.name.to_lowercase()) {
                _ if !usable(&i) => Standing::Inactive,
                Some((_, ids)) if ids.len() > 1 && ids.contains(&i.id) => Standing::Conflict,
                Some((_, ids)) if ids.len() == 1 && ids[0] == i.id => Standing::Effective,
                _ => Standing::Overridden,
            };
            (i, standing)
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
                // Workspace skills are visible from every project; a project's own skills are
                // listed with them when its scope is named (and only within that project).
                let scope = config_scope(ctx, &i.scope)?;
                let roots: Vec<&str> = i.root_ids.0.iter().map(Id::as_str).collect();
                if roots
                    .iter()
                    .any(|r| *r != SHARED_ROOT_ID && *r != HARNESS_ROOT_ID)
                {
                    return Err(invalid("skill_root"));
                }
                let mut records = Vec::new();
                let mut fence = 0i64;
                // Standing is computed for the project the caller works in even when the
                // workspace list is asked for, so a workspace skill a project's skill shadows
                // is shown as overridden.
                let working = scope.project_id().or(ctx.project_id());
                for (item, standing) in skill_items_in(instructions, working)? {
                    if scope == ConfigScope::Workspace && item.scope != ConfigScope::Workspace {
                        continue;
                    }
                    let root = if item.origin == InstructionOrigin::Shared {
                        SHARED_ROOT_ID
                    } else {
                        HARNESS_ROOT_ID
                    };
                    // Another project's confined draft is not listed.
                    if (!roots.is_empty() && !roots.contains(&root)) || !item_visible(ctx, &item) {
                        continue;
                    }
                    // A harness item with nothing approved yet (an unproposed, pending or
                    // rejected first draft) is listed with its latest revision and state, so it
                    // can be selected and edited with its own revision. It is never effective.
                    fence = fence.max(item.version);
                    let revision = visible_current(instructions, ctx, &item)?;
                    records.push(record(&item, &revision, Some(standing))?);
                }
                let listed = crate::fingerprint::sha256_hex(
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
                        value: cursor::slice(
                            &CursorBinding::new(ctx, "skills.discover", &[&listed], 1),
                            records,
                            &i.page,
                        )?,
                    },
                    fence.max(1),
                ))
            }
            skill_ops::Input::Read(i) => {
                let (record, revision) = self.read_record(
                    read,
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
                let revision = visible_revision(instructions, ctx, &item, i.revision.get())?;
                // Each resource keeps its skill-relative path; the gateway projects revision 1
                // callers to the artifacts alone.
                let resources: Vec<SkillResource> = revision
                    .resources
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|r| {
                        Ok::<_, serde_json::Error>(SkillResource {
                            path: serde_json::from_value(r["path"].clone())?,
                            artifact: serde_json::from_value(r["artifact"].clone())?,
                            executable: r["executable"].as_bool().unwrap_or(false),
                        })
                    })
                    .collect::<Result<_, _>>()
                    .map_err(|e| StorageError::InvalidRecord(format!("skill resource: {e}")))?;
                let binding = CursorBinding::new(
                    ctx,
                    "skills.resources",
                    &[item.id.as_str(), &revision.revision.to_string()],
                    1,
                );
                let page = cursor::slice(&binding, resources, &i.page)?;
                Ok((
                    OperationResult::SkillsResources {
                        value: SkillResourcePageResult::Current(SkillResourcePage {
                            items: page.items,
                            next_cursor: page.next_cursor,
                            has_more: page.has_more,
                        }),
                    },
                    item.version,
                ))
            }
            skill_ops::Input::History(i) => {
                let (page, revision) = self.history(
                    read,
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
        match input {
            role_ops::Input::Read(i) => {
                let (record, revision) = self.read_record(
                    read,
                    ctx,
                    &i.instruction_id,
                    InstructionKind::Role,
                    i.revision.as_ref().map(|r| r.get()),
                )?;
                Ok((OperationResult::RolesRead { value: record }, revision))
            }
            role_ops::Input::History(i) => {
                let (page, revision) =
                    self.history(read, ctx, &i.instruction_id, InstructionKind::Role, &i.page)?;
                Ok((OperationResult::RolesHistory { value: page }, revision))
            }
            _ => Err(invalid("instruction_operation")),
        }
    }

    fn read_record(
        &self,
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        id: &Id,
        kind: InstructionKind,
        revision: Option<i64>,
    ) -> Result<(InstructionRecord, i64), Failure> {
        let read = snapshot.instructions();
        let item = visible_item(read, ctx, id, kind)?;
        let r = match revision {
            Some(n) => visible_revision(read, ctx, &item, n)?,
            None => visible_current(read, ctx, &item)?,
        };
        let effective = if kind == InstructionKind::Skill {
            skill_items_in(read, item.scope.project_id().or(ctx.project_id()))?
                .into_iter()
                .find(|(i, _)| i.id == item.id)
                .map(|(_, standing)| standing)
        } else {
            None
        };
        Ok((
            record_with_approval(snapshot.decisions(), &item, &r, effective)?,
            item.version,
        ))
    }

    fn history(
        &self,
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        id: &Id,
        kind: InstructionKind,
        page: &PageRequest,
    ) -> Result<(Page<InstructionRecord>, i64), Failure> {
        let read = snapshot.instructions();
        let item = visible_item(read, ctx, id, kind)?;
        let binding = CursorBinding::new(ctx, "instructions.history", &[item.id.as_str()], 1);
        // Another project's confined drafts are skipped; read on until the page fills.
        let rows = cursor::page(
            &binding,
            page,
            |before, n| {
                let before = before
                    .map(|b| b.parse::<i64>().map_err(|_| StorageError::CursorExpired))
                    .transpose()?;
                cursor::scan(
                    before,
                    n,
                    |before, batch| read.instruction_revisions(&item.id, *before, batch),
                    |r| Some(r.revision),
                    |r| Ok(revision_visible(ctx, r)),
                )
            },
            |r| r.revision.to_string(),
        )?;
        Ok((
            rows.try_into_page(|r| record_with_approval(snapshot.decisions(), &item, &r, None))?,
            item.version,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::storage::{InstructionRevisionState, StoredInstructionRevision};
    use crate::protocol::ActorId;
    use serde_json::json;

    struct Items(Vec<StoredInstructionItem>);
    impl InstructionRead for Items {
        fn instruction_item(&self, id: &Id) -> Result<Option<StoredInstructionItem>, StorageError> {
            Ok(self.0.iter().find(|i| i.id == *id).cloned())
        }
        fn instruction_items(
            &self,
            _: &InstructionFilter,
            after: Option<&Id>,
            _: u16,
        ) -> Result<Vec<StoredInstructionItem>, StorageError> {
            Ok(if after.is_some() {
                Vec::new()
            } else {
                self.0.clone()
            })
        }
        fn instruction_revision(
            &self,
            id: &Id,
            revision: i64,
        ) -> Result<Option<StoredInstructionRevision>, StorageError> {
            Ok(Some(StoredInstructionRevision {
                instruction_id: id.clone(),
                revision,
                content: json!({}),
                content_digest: "a".repeat(64),
                state: InstructionRevisionState::Approved,
                author: ActorId(Id::new("host").unwrap()),
                origin: json!({}),
                resources: json!([]),
                applicability: vec![],
                approval_id: None,
                created_at_ms: 0,
                decided_at_ms: None,
                confined_project: None,
            }))
        }
        fn instruction_revisions(
            &self,
            _: &Id,
            _: Option<i64>,
            _: u16,
        ) -> Result<Vec<StoredInstructionRevision>, StorageError> {
            Ok(Vec::new())
        }
        fn instruction_revision_by_approval(
            &self,
            _: &Id,
        ) -> Result<Option<StoredInstructionRevision>, StorageError> {
            Ok(None)
        }
    }
    fn item(id: &str, name: &str, origin: InstructionOrigin) -> StoredInstructionItem {
        StoredInstructionItem {
            id: Id::new(id).unwrap(),
            kind: InstructionKind::Skill,
            name: name.into(),
            scope: ConfigScope::Workspace,
            origin,
            root_path: Some("/r".into()),
            source_path: Some(format!("/r/{name}")),
            latest_revision: 1,
            active_revision: Some(1),
            enabled: true,
            materialized_digest: None,
            observed_digest: None,
            present: true,
            version: 2,
            created_at_ms: 0,
            updated_at_ms: 0,
            confined_project: None,
        }
    }

    #[test]
    fn same_precedence_name_collisions_are_conflicts_and_never_effective() {
        let read = Items(vec![
            item("s-alpha", "alpha", InstructionOrigin::Shared),
            item("s-alpha-upper", "Alpha", InstructionOrigin::Shared),
            item("s-beta", "beta", InstructionOrigin::Shared),
            item("h-beta", "Beta", InstructionOrigin::Harness),
        ]);
        let standing: BTreeMap<String, Standing> = skill_items_in(&read, None)
            .unwrap()
            .into_iter()
            .map(|(i, s)| (i.id.as_str().to_owned(), s))
            .collect();
        assert_eq!(standing["s-alpha"], Standing::Conflict);
        assert_eq!(standing["s-alpha-upper"], Standing::Conflict);
        // A higher-precedence harness skill resolves the case collision with its shared source.
        assert_eq!(standing["h-beta"], Standing::Effective);
        assert_eq!(standing["s-beta"], Standing::Overridden);
        let effective = crate::services::instructions::effective_skills(&read).unwrap();
        assert_eq!(effective.skills.len(), 1);
        assert_eq!(effective.conflicts.len(), 1);
        assert_eq!(effective.conflicts[0].name, "alpha");
        assert_eq!(effective.conflicts[0].instruction_ids.len(), 2);
    }
}
