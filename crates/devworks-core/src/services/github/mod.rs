//! GitHub response observations are data, never local execution authority.
mod account;
pub mod api;
mod binding;
mod comments;
pub(crate) mod credentials;
pub mod identity;
pub mod indexing;
pub mod issue_poll;
mod issues;
pub mod notices;
pub mod publish;
mod pull_observations;
mod pulls;
mod reads;
pub mod references;
mod repositories;
mod resolution;
pub mod response;
pub mod schedule;
mod sync;
pub mod triage;
pub mod triage_case;
pub mod verification;
pub(crate) mod watch;

pub use account::GitHubService;
pub use publish::CONSUMER as EFFECT_CONSUMER;
pub use resolution::settle_effect;

impl crate::services::effect::JournaledEffect for GitHubService {
    fn resumes_after_observation(&self) -> bool {
        true
    }

    fn execute(
        &self,
        operation: &crate::ports::storage::OperationRecord,
    ) -> crate::services::effect::EffectOutcome {
        match operation.capability.as_str() {
            publish::CAPABILITY => self.publish(operation, false),
            sync::CAPABILITY => self.sync_issues(operation),
            resolution::COMMENT | resolution::CLOSE => self.resolution_effect(operation, false),
            _ => crate::services::effect::EffectOutcome::Failed("Unknown GitHub operation".into()),
        }
    }

    fn observe(
        &self,
        operation: &crate::ports::storage::OperationRecord,
    ) -> crate::services::effect::EffectOutcome {
        match operation.capability.as_str() {
            publish::CAPABILITY => self.publish(operation, true),
            sync::CAPABILITY => self.sync_issues(operation),
            resolution::COMMENT | resolution::CLOSE => self.resolution_effect(operation, true),
            _ => crate::services::effect::EffectOutcome::Failed("Unknown GitHub operation".into()),
        }
    }
}
