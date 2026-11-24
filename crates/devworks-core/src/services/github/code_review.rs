//! Read a configured bot's terminal review from the existing exact-head GitHub poll ledger.
use super::{GitHubService, issue_poll, reads::ObservationKind};
use crate::ports::storage::{
    GithubObservationFilter, GithubScope, ReadOptions, ReadSnapshot, StorageError,
    StoredGithubObservation, read_snapshot,
};
use crate::protocol::{Id, ProjectId};
use crate::services::kernel::Failure;
use std::collections::HashMap;

pub struct CodeReviewQuery<'a> {
    pub project: &'a ProjectId,
    pub repository: &'a Id,
    pub pull_number: &'a str,
    pub head: &'a str,
    pub bot_id: u64,
}

#[derive(Debug)]
pub enum CodeReviewObservation {
    Waiting,
    Failed,
    Unavailable,
    Stale,
    Completed {
        review: Box<StoredGithubObservation>,
        comments: Vec<StoredGithubObservation>,
    },
}

impl GitHubService {
    /// Uses the existing account-scoped request budget, conditional polling and page checkpoints.
    pub(crate) fn poll_code_review(
        &self,
        query: CodeReviewQuery<'_>,
    ) -> Result<CodeReviewObservation, Failure> {
        self.poll_pull_evidence(
            query.repository,
            query.pull_number,
            query.head,
            ObservationKind::Reviews,
        )?;
        Ok(read_snapshot(
            self.store.as_ref(),
            &ReadOptions::default(),
            |snapshot| code_review_observation(snapshot, &query),
        )?)
    }
}

pub(crate) fn code_review_observation(
    snapshot: &dyn ReadSnapshot,
    query: &CodeReviewQuery<'_>,
) -> Result<CodeReviewObservation, StorageError> {
    if !snapshot
        .projects()
        .members(query.project)?
        .contains(query.repository)
    {
        return Err(StorageError::ScopeDenied);
    }
    let Some(target) = issue_poll::target(snapshot, query.repository)? else {
        return Ok(CodeReviewObservation::Unavailable);
    };
    let scope = GithubScope {
        repositories: vec![query.repository.clone()],
        project_id: Some(query.project.clone()),
    };
    let filter = GithubObservationFilter {
        scope: scope.clone(),
        kinds: vec!["PullRequest".into()],
        subject_id: Some(query.pull_number.to_owned()),
        head: None,
    };
    let pull = latest_observations(snapshot, &filter)?
        .into_values()
        .max_by_key(|row| (row.source_updated_at_ms, row.observed_at_ms));
    let Some(pull) = pull else {
        return Ok(CodeReviewObservation::Waiting);
    };
    if pull.head.as_deref() != Some(query.head) || pull.payload["state"] != "open" {
        return Ok(CodeReviewObservation::Stale);
    }

    let mut complete_through = i64::MAX;
    for kind in ["Review", "ReviewComment"] {
        let key = format!("pull:{}:{}:{kind}", query.pull_number, query.head);
        let Some(cursor) = snapshot
            .github()
            .github_poll_cursor(&target.repository.id, &key)?
        else {
            return Ok(CodeReviewObservation::Waiting);
        };
        if cursor.watermark.as_ref().and_then(|value| value.as_i64())
            != Some(target.account.permission_revision)
            || cursor.next_page.is_some()
            || !cursor.bootstrap_complete
        {
            return Ok(CodeReviewObservation::Waiting);
        }
        if cursor.last_error.is_some() {
            return Ok(CodeReviewObservation::Failed);
        }
        complete_through = complete_through.min(cursor.last_polled_at_ms.unwrap_or(0));
    }

    let filter = GithubObservationFilter {
        scope,
        kinds: vec!["Review".into(), "ReviewComment".into()],
        subject_id: Some(query.pull_number.to_owned()),
        head: Some(query.head.to_owned()),
    };
    let rows = latest_observations(snapshot, &filter)?;
    let authored = |row: &&StoredGithubObservation| {
        row.payload["authorId"].as_u64() == Some(query.bot_id) && row.payload["authorType"] == "Bot"
    };
    let review = rows
        .values()
        .filter(|row| row.kind == "Review")
        .filter(authored)
        .filter(|row| {
            row.payload["submittedAt"]
                .as_str()
                .and_then(|value| value.parse::<jiff::Timestamp>().ok())
                .is_some_and(|at| at.as_millisecond() <= complete_through)
        })
        .max_by_key(|row| (row.payload["submittedAt"].as_str(), row.observed_at_ms));
    let Some(review) = review else {
        return Ok(CodeReviewObservation::Waiting);
    };
    if review.payload["state"] == "DISMISSED" {
        return Ok(CodeReviewObservation::Failed);
    }
    let declared_comments = match review.payload["state"].as_str() {
        Some("APPROVED" | "CHANGES_REQUESTED") => None,
        Some("COMMENTED") => {
            match explicit_comment_count(review.payload["body"].as_str().unwrap_or("")) {
                Some(count) => Some(count),
                None => return Ok(CodeReviewObservation::Waiting),
            }
        }
        _ => return Ok(CodeReviewObservation::Waiting),
    };

    let Some(run) = review.payload["resourceId"]
        .as_str()
        .and_then(|value| value.parse::<u64>().ok())
    else {
        return Err(StorageError::InvalidRecord("GitHub review identity".into()));
    };
    let mut comments = rows
        .values()
        .filter(|row| row.kind == "ReviewComment")
        .filter(authored)
        .filter(|row| row.payload["reviewId"].as_u64() == Some(run))
        .cloned()
        .collect::<Vec<_>>();
    if declared_comments.is_some_and(|count| count != comments.len()) {
        return Ok(CodeReviewObservation::Waiting);
    }
    comments.sort_unstable_by(|left, right| left.id.as_str().cmp(right.id.as_str()));
    Ok(CodeReviewObservation::Completed {
        review: Box::new(review.clone()),
        comments,
    })
}

/// A comment-form automation review may explicitly declare the complete number of actionable
/// inline comments. A comment with no such declaration does not qualify as completion.
fn explicit_comment_count(body: &str) -> Option<usize> {
    let mut counts = body.lines().filter_map(|line| {
        line.trim()
            .trim_matches('*')
            .strip_prefix("Actionable comments posted:")
            .map(str::trim)
            .filter(|count| !count.is_empty() && count.bytes().all(|byte| byte.is_ascii_digit()))
            .and_then(|count| count.parse::<usize>().ok())
    });
    let count = counts.next()?;
    counts.next().is_none().then_some(count)
}

#[cfg(test)]
mod tests {
    #[test]
    fn comment_reviews_need_an_explicit_unambiguous_completion_count() {
        assert_eq!(
            super::explicit_comment_count("**Actionable comments posted: 0**"),
            Some(0)
        );
        assert_eq!(
            super::explicit_comment_count("No actionable comments were posted"),
            None
        );
        assert_eq!(
            super::explicit_comment_count(
                "Actionable comments posted: 0\nActionable comments posted: 2"
            ),
            None
        );
    }
}
/// Retain the latest server revision of each remote resource before interpreting its state.
/// Never decide a complete review from a truncated page.
fn latest_observations(
    snapshot: &dyn ReadSnapshot,
    filter: &GithubObservationFilter,
) -> Result<HashMap<(String, String), StoredGithubObservation>, StorageError> {
    const LIMIT: u16 = 200;
    let mut result = HashMap::new();
    let mut after = None;
    for _ in 0..20 {
        let page = snapshot
            .github()
            .github_observations(filter, after.as_ref(), LIMIT)?;
        for row in &page {
            let id = row.payload["resourceId"]
                .as_str()
                .map(str::to_owned)
                .or_else(|| {
                    row.payload["resourceId"]
                        .as_u64()
                        .map(|value| value.to_string())
                })
                .ok_or_else(|| {
                    StorageError::InvalidRecord("GitHub review observation identity".into())
                })?;
            let key = (row.kind.clone(), id);
            let held = result.entry(key).or_insert_with(|| row.clone());
            if (row.source_updated_at_ms, row.observed_at_ms)
                >= (held.source_updated_at_ms, held.observed_at_ms)
            {
                *held = row.clone();
            }
        }
        if page.len() < usize::from(LIMIT) {
            return Ok(result);
        }
        after = page.last().map(|row| row.id.clone());
    }
    Err(StorageError::InvalidRecord(
        "GitHub review observations exceed the qualified review limit".into(),
    ))
}

/// Current GitHub branch policy. It is evidence for the review exception only, never a grant.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct DeliveryReviewRules {
    pub digest: String,
    pub fresh_review: bool,
    pub required_checks: Vec<(String, Option<u64>)>,
    #[serde(default)]
    pub merge_queue_method: Option<String>,
    #[serde(default)]
    pub required_approvals: bool,
    #[serde(default)]
    pub requires_clean: bool,
    #[serde(default)]
    pub requires_linear_history: bool,
}

impl GitHubService {
    pub(crate) fn delivery_review_rules(
        &self,
        project: &ProjectId,
        repository: &Id,
    ) -> Result<DeliveryReviewRules, Failure> {
        use super::api::Endpoint;
        use crate::services::git::github::Fault;
        use serde_json::{Value, json};

        let target = read_snapshot(self.store.as_ref(), &ReadOptions::default(), |snapshot| {
            if !snapshot.projects().members(project)?.contains(repository) {
                return Err(StorageError::ScopeDenied);
            }
            issue_poll::target(snapshot, repository)
        })?
        .ok_or_else(|| super::repositories::api_fault(Fault::Unavailable))?;
        let read = |path: &str| {
            let page = Endpoint::new(
                &format!(
                    "https://api.github.com/repos/{}/{}{}",
                    target.repository.owner, target.repository.name, path
                ),
                Some(&format!("/repositories/{}{path}", target.numeric_id)),
            )
            .map_err(super::repositories::api_fault)?;
            self.client
                .observe_authenticated_page(
                    &page,
                    None,
                    None,
                    self.clock.now_ms().get(),
                    60_000,
                    self.client
                        .token_for_account(&*self.store, &target.account)
                        .map_err(super::repositories::api_fault)?,
                )
                .map_err(super::repositories::api_fault)
        };
        let protection = read("/branches/main/protection")?;
        let invalid = || super::repositories::api_fault(Fault::InvalidResponse);
        let legacy = match protection.status {
            200 => protection
                .value
                .filter(Value::is_object)
                .ok_or_else(invalid)?,
            404 => {
                let branch = read("/branches/main")?;
                if branch.status != 200 {
                    return Err(invalid());
                }
                let branch = branch
                    .value
                    .filter(|branch| branch["name"] == "main" && branch["protected"].is_boolean());
                if branch.is_none() {
                    return Err(invalid());
                }
                if branch
                    .as_ref()
                    .is_some_and(|branch| branch["protected"] == true)
                {
                    // Classic protection can be unreadable even when rulesets are visible.
                    // Confirm the exact ref has no classic protection before relying on rulesets.
                    let token = self
                        .client
                        .token_for_account(&*self.store, &target.account)
                        .map_err(super::repositories::api_fault)?;
                    let data = self.client.graphql_query(
                        token,
                        "query DeliveryClassicProtection($owner:String!, $name:String!) { repository(owner:$owner, name:$name) { nameWithOwner ref(qualifiedName:\"refs/heads/main\") { name prefix branchProtectionRule { id } } } }",
                        json!({"owner":target.repository.owner,"name":target.repository.name}),
                    ).map_err(super::repositories::api_fault)?;
                    let repo = &data["repository"];
                    let exact = repo["nameWithOwner"].as_str().is_some_and(|name| {
                        name.eq_ignore_ascii_case(&format!(
                            "{}/{}",
                            target.repository.owner, target.repository.name
                        ))
                    });
                    let branch_ref = &repo["ref"];
                    if !exact
                        || branch_ref["name"] != "main"
                        || branch_ref["prefix"] != "refs/heads/"
                        || branch_ref.get("branchProtectionRule") != Some(&Value::Null)
                    {
                        return Err(super::repositories::api_fault(Fault::Unavailable));
                    }
                }
                Value::Null
            }
            other => return Err(super::repositories::api_fault(Fault::Rejected(other))),
        };
        let rules_page = read("/rules/branches/main")?;
        if rules_page.status != 200 || rules_page.next_page.is_some() {
            return Err(invalid());
        }
        let rules = rules_page
            .value
            .as_ref()
            .and_then(Value::as_array)
            .ok_or_else(invalid)?;
        let approvals = &legacy["required_pull_request_reviews"];
        let mut required_approvals = approvals["required_approving_review_count"]
            .as_u64()
            .is_some_and(|count| count > 0)
            || approvals["require_code_owner_reviews"] == true;
        let mut merge_queue_method = None;
        let mut fresh_review = approvals["dismiss_stale_reviews"] == true
            || approvals["require_last_push_approval"] == true;
        let mut requires_linear_history = legacy["required_linear_history"]["enabled"] == true;
        let mut requires_clean = legacy["required_conversation_resolution"]["enabled"] == true;
        let mut required_checks = Vec::new();
        if let Some(checks) = legacy["required_status_checks"]["checks"].as_array() {
            for check in checks {
                let context = check["context"].as_str().ok_or_else(invalid)?;
                required_checks.push((context.to_owned(), check["app_id"].as_u64()));
            }
        } else if let Some(checks) = legacy["required_status_checks"]["contexts"].as_array() {
            for check in checks {
                required_checks.push((check.as_str().ok_or_else(invalid)?.to_owned(), None));
            }
        }
        for rule in rules {
            let parameters = &rule["parameters"];
            match rule["type"].as_str() {
                Some("merge_queue") => {
                    let method = parameters["merge_method"]
                        .as_str()
                        .ok_or_else(invalid)?
                        .to_ascii_lowercase();
                    if !matches!(method.as_str(), "merge" | "squash" | "rebase")
                        || merge_queue_method
                            .replace(method.clone())
                            .is_some_and(|old| old != method)
                    {
                        return Err(invalid());
                    }
                }
                Some("pull_request") => {
                    required_approvals |= parameters["required_approving_review_count"]
                        .as_u64()
                        .is_some_and(|count| count > 0)
                        || parameters["require_code_owner_review"] == true;
                    fresh_review |= parameters["dismiss_stale_reviews_on_push"] == true
                        || parameters["require_last_push_approval"] == true;
                }
                Some("required_linear_history") => requires_linear_history = true,
                Some(
                    "workflows"
                    | "required_workflow_status_checks"
                    | "required_deployments"
                    | "required_review_thread_resolution"
                    | "code_scanning"
                    | "code_quality",
                ) => {
                    requires_clean = true;
                }
                Some("required_status_checks") => {
                    for check in parameters["required_status_checks"]
                        .as_array()
                        .ok_or_else(invalid)?
                    {
                        required_checks.push((
                            check["context"].as_str().ok_or_else(invalid)?.to_owned(),
                            check["integration_id"].as_u64(),
                        ));
                    }
                }
                _ => {}
            }
        }
        required_checks.sort_unstable();
        required_checks.dedup();
        let digest = crate::fingerprint::canonical_digest(&json!({
            "statusChecks":legacy["required_status_checks"],
            "reviewRules":approvals, "rulesets":rules,
        }))
        .map_err(|_| invalid())?;
        Ok(DeliveryReviewRules {
            digest,
            fresh_review,
            required_checks,
            merge_queue_method,
            required_approvals,
            requires_clean,
            requires_linear_history,
        })
    }
}
