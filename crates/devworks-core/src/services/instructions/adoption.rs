//! What running agents adopt at their next safe step: the effective skill set with exact
//! revisions and digests. Only active (approved) revisions are ever part of it, so a pending or
//! rejected proposal can never appear here; a change of the set's digest is the adoption
//! boundary that a session records (SKILL-03, SKILL-05).
use super::queries::{Standing, skill_items};
use super::{HARNESS_ROOT_ID, SHARED_ROOT_ID};
use crate::fingerprint::sha256_hex;
use crate::ports::storage::{InstructionOrigin, InstructionRead, StorageError};
use crate::protocol::Id;
use serde::Serialize;
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EffectiveSkill {
    pub instruction_id: Id,
    pub name: String,
    pub root_id: &'static str,
    pub source_path: Option<String>,
    pub revision: i64,
    pub content_digest: String,
    /// The instruction text artifact (`ArtifactRef`).
    pub content: Value,
    /// `[{path, artifact, sha256, byteCount, script}]` resolved relative to the skill directory.
    pub resources: Value,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EffectiveSkills {
    /// SHA-256 over the ordered `(name, instruction, revision, digest)` tuples and conflicts.
    pub digest: String,
    pub skills: Vec<EffectiveSkill>,
    /// Material same-precedence conflicts, visible for reconciliation: each entry lists the
    /// conflicting instruction items; none of them is in effect.
    pub conflicts: Vec<SkillConflict>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillConflict {
    /// The case-folded name the items share.
    pub name: String,
    pub instruction_ids: Vec<Id>,
}
impl EffectiveSkills {
    /// The stamp a provider context binds; `None` when there is nothing to bind.
    pub fn stamp(&self) -> Option<crate::providers::instructions::SkillStamp> {
        if self.skills.is_empty() && self.conflicts.is_empty() {
            return None;
        }
        Some(crate::providers::instructions::SkillStamp {
            digest: self.digest.clone(),
            skills: self
                .skills
                .iter()
                .map(|s| crate::providers::instructions::SkillStampEntry {
                    name: s.name.clone(),
                    instruction_id: s.instruction_id.clone(),
                    root_id: s.root_id.to_owned(),
                    revision: s.revision,
                    content_digest: s.content_digest.clone(),
                })
                .collect(),
            conflicts: self.conflicts.iter().map(|c| c.name.clone()).collect(),
        })
    }
    /// Names whose effective revision differs from `adopted` (added, changed or removed).
    pub fn transitions(&self, adopted: &EffectiveSkills) -> Vec<String> {
        let mut changed: Vec<String> = Vec::new();
        for s in &self.skills {
            match adopted.skills.iter().find(|a| a.name == s.name) {
                Some(a) if a.instruction_id == s.instruction_id && a.revision == s.revision => {}
                _ => changed.push(s.name.clone()),
            }
        }
        for a in &adopted.skills {
            if !self.skills.iter().any(|s| s.name == a.name) {
                changed.push(a.name.clone());
            }
        }
        changed.sort();
        changed.dedup();
        changed
    }
}

/// The current effective skill set, one per name, harness overrides first.
pub fn effective_skills(read: &dyn InstructionRead) -> Result<EffectiveSkills, StorageError> {
    let mut skills = Vec::new();
    let mut conflicts: Vec<SkillConflict> = Vec::new();
    for (item, standing) in skill_items(read)? {
        if standing == Standing::Conflict {
            let name = item.name.to_lowercase();
            match conflicts.iter_mut().find(|c| c.name == name) {
                Some(c) => c.instruction_ids.push(item.id.clone()),
                None => conflicts.push(SkillConflict {
                    name,
                    instruction_ids: vec![item.id.clone()],
                }),
            }
            continue;
        }
        if standing != Standing::Effective {
            continue;
        }
        let Some(active) = item.active_revision else {
            continue;
        };
        let Some(revision) = read.instruction_revision(&item.id, active)? else {
            continue;
        };
        skills.push(EffectiveSkill {
            instruction_id: item.id.clone(),
            name: item.name.clone(),
            root_id: if item.origin == InstructionOrigin::Shared {
                SHARED_ROOT_ID
            } else {
                HARNESS_ROOT_ID
            },
            source_path: item.source_path.clone(),
            revision: revision.revision,
            content_digest: revision.content_digest,
            content: revision.content,
            resources: revision.resources,
        });
    }
    let digest = sha256_hex(
        serde_json::to_string(
            &skills
                .iter()
                .map(|s| {
                    (
                        &s.name,
                        s.instruction_id.as_str(),
                        s.revision,
                        &s.content_digest,
                    )
                })
                .collect::<Vec<_>>(),
        )
        .expect("digest input")
        .chain_conflicts(&conflicts)
        .as_bytes(),
    );
    Ok(EffectiveSkills {
        digest,
        skills,
        conflicts,
    })
}

trait ChainConflicts {
    fn chain_conflicts(self, conflicts: &[SkillConflict]) -> String;
}
impl ChainConflicts for String {
    fn chain_conflicts(mut self, conflicts: &[SkillConflict]) -> String {
        if !conflicts.is_empty() {
            self.push_str(&serde_json::to_string(conflicts).expect("conflicts"));
        }
        self
    }
}
