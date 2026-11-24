//! Exact source observations and a single guarded merge submission to GitHub.
use super::{Fault, GitHubClient, Method, Repository, Value, json};
use crate::ports::storage::ApplicationStore;
use crate::protocol::data::MergeMethod;

const ASYNC_API: &str = "2026-03-10";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DeliveryPull {
    pub head_ref: String,
    pub head: String,
    pub base_ref: String,
    pub base: String,
    pub open: bool,
    pub draft: bool,
    pub mergeable: Option<bool>,
    pub mergeable_state: Option<String>,
    pub merged: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum MergeRequest {
    Accepted(String),
    Enqueued,
    Merged(String),
    Refused,
}

#[derive(Clone, Debug)]
pub(crate) struct MergeTarget<'a> {
    pub expected_head: &'a str,
    pub method: &'a MergeMethod,
    pub use_queue: bool,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct QueueAnchor {
    queue: String,
    last_event: Option<String>,
}

pub(crate) struct QueueObservation {
    pub active: bool,
    pub removal: Option<String>,
    pub merged_method: Option<String>,
}

const QUEUE_STATE: &str = r#"
query DeliveryMergeQueue($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    nameWithOwner
    pullRequest(number: $number) {
      number baseRefName headRefOid merged
      mergeQueue { id }
      mergeQueueEntry { headCommit { oid } mergeQueue { id } }
      autoMergeRequest { mergeMethod }
      timelineItems(last: 20, itemTypes: [ADDED_TO_MERGE_QUEUE_EVENT, REMOVED_FROM_MERGE_QUEUE_EVENT]) {
        pageInfo { hasPreviousPage }
        nodes {
          ... on AddedToMergeQueueEvent { id mergeQueue { id } }
          ... on RemovedFromMergeQueueEvent { id mergeQueue { id } beforeCommit { oid } reason }
          __typename
        }
      }
    }
  }
}
"#;

fn string(value: &Value, pointer: &str) -> Result<String, Fault> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or(Fault::InvalidResponse)
}
fn commit(value: &Value, pointer: &str) -> Result<String, Fault> {
    let sha = string(value, pointer)?;
    if sha.len() == 40 && super::super::client::oid_ok(&sha) {
        Ok(sha)
    } else {
        Err(Fault::InvalidResponse)
    }
}
fn ticket(value: &str) -> bool {
    let mut parts = value.split('-');
    [8, 4, 4, 4, 12].into_iter().all(|len| {
        parts.next().is_some_and(|part| {
            part.len() == len && part.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
    }) && parts.next().is_none()
}
pub(crate) fn method_name(method: &MergeMethod) -> &'static str {
    match method {
        MergeMethod::MergeCommit => "merge",
        MergeMethod::Squash => "squash",
        MergeMethod::Rebase => "rebase",
    }
}
fn decode(value: &Value, target: &MergeTarget<'_>) -> Result<MergeRequest, Fault> {
    match value["status"].as_str() {
        Some("pending") => {
            let details = &value["details"];
            let id = string(details, "/uuid")?;
            let action = if target.use_queue {
                "merge_queue"
            } else {
                "direct_merge"
            };
            if !ticket(&id)
                || details["expected_head_sha"] != target.expected_head
                || details["merge_action"] != action
                || details["bypass_rules"] != false
                || (!target.use_queue && details["merge_method"] != method_name(target.method))
            {
                return Err(Fault::Ambiguous);
            }
            Ok(MergeRequest::Accepted(id))
        }
        Some("enqueued") if target.use_queue => Ok(MergeRequest::Enqueued),
        Some("merged") => Ok(MergeRequest::Merged(commit(value, "/details/sha")?)),
        Some("failed") => Ok(MergeRequest::Refused),
        _ => Err(Fault::InvalidResponse),
    }
}

fn queue_events(pull: &Value) -> Result<&[Value], Fault> {
    pull.pointer("/timelineItems/nodes")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .ok_or(Fault::InvalidResponse)
}

impl GitHubClient {
    pub(crate) fn delivery_pull(
        &self,
        store: &dyn ApplicationStore,
        repository: &Repository,
        number: u64,
    ) -> Result<DeliveryPull, Fault> {
        let value = self.request(
            store,
            Method::Get,
            format!("{}/{number}", repository.api()),
            None,
        )?;
        let full_name = repository.full_name();
        let url = format!("https://github.com/{full_name}/pull/{number}");
        if value["number"].as_u64() != Some(number)
            || !string(&value, "/html_url")?.eq_ignore_ascii_case(&url)
            || !string(&value, "/head/repo/full_name")?.eq_ignore_ascii_case(&full_name)
            || !string(&value, "/base/repo/full_name")?.eq_ignore_ascii_case(&full_name)
        {
            return Err(Fault::InvalidResponse);
        }
        let open = match value["state"].as_str() {
            Some("open") => true,
            Some("closed") => false,
            _ => return Err(Fault::InvalidResponse),
        };
        let draft = value["draft"].as_bool().ok_or(Fault::InvalidResponse)?;
        let merged = match value["merged"].as_bool() {
            Some(true) if !open => Some(commit(&value, "/merge_commit_sha")?),
            Some(false) => None,
            _ => return Err(Fault::InvalidResponse),
        };
        Ok(DeliveryPull {
            head_ref: string(&value, "/head/ref")?,
            head: commit(&value, "/head/sha")?,
            base_ref: string(&value, "/base/ref")?,
            base: commit(&value, "/base/sha")?,
            open,
            draft,
            mergeable: value["mergeable"].as_bool(),
            mergeable_state: value["mergeable_state"].as_str().map(str::to_owned),
            merged,
        })
    }

    pub(crate) fn submit_merge(
        &self,
        store: &dyn ApplicationStore,
        repository: &Repository,
        number: u64,
        target: &MergeTarget<'_>,
    ) -> Result<MergeRequest, Fault> {
        let mut body = json!({"sha":target.expected_head,"bypass_rules":false,
            "merge_action":if target.use_queue {"merge_queue"} else {"direct_merge"}});
        if !target.use_queue {
            body["merge_method"] = json!(method_name(target.method));
        }
        let response = self.request_version(
            store,
            Method::Put,
            format!("{}/{number}/merge-async", repository.api()),
            Some(body),
            ASYNC_API,
        )?;
        match response.status {
            200 | 202 | 409 => {
                let value =
                    serde_json::from_slice(&response.body).map_err(|_| Fault::InvalidResponse)?;
                let result = decode(&value, target)?;
                if response.status != 200 && !matches!(result, MergeRequest::Accepted(_)) {
                    return Err(Fault::Ambiguous);
                }
                Ok(result)
            }
            // An unqualified or malformed response does not prove whether GitHub dispatched.
            status @ (400 | 403 | 404 | 422) => Err(Fault::Rejected(status)),
            _ => Err(Fault::Unavailable),
        }
    }

    pub(crate) fn observe_merge_request(
        &self,
        store: &dyn ApplicationStore,
        repository: &Repository,
        number: u64,
        request: &str,
        target: &MergeTarget<'_>,
    ) -> Result<MergeRequest, Fault> {
        if !ticket(request) {
            return Err(Fault::InvalidResponse);
        }
        let response = self.request_version(
            store,
            Method::Get,
            format!("{}/{number}/merge-async/{request}", repository.api()),
            None,
            ASYNC_API,
        )?;
        if !response.success() {
            return Err(Fault::Rejected(response.status));
        }
        let value = serde_json::from_slice(&response.body).map_err(|_| Fault::InvalidResponse)?;
        let result = decode(&value, target)?;
        if matches!(&result, MergeRequest::Accepted(actual) if actual != request) {
            return Err(Fault::Ambiguous);
        }
        Ok(result)
    }

    pub(crate) fn single_final_pull(
        &self,
        store: &dyn ApplicationStore,
        repository: &Repository,
        number: u64,
        head: &str,
    ) -> Result<bool, Fault> {
        let data = self.graphql_query(
            self.token(store)?,
            "query DeliverySinglePull($owner:String!, $name:String!, $number:Int!) { repository(owner:$owner, name:$name) { nameWithOwner pullRequest(number:$number) { number headRefOid baseRefName stack { id } } } }",
            json!({"owner":repository.owner,"name":repository.name,"number":number}),
        )?;
        let repo = &data["repository"];
        let pull = &repo["pullRequest"];
        if !string(repo, "/nameWithOwner")?.eq_ignore_ascii_case(&repository.full_name())
            || pull["number"].as_u64() != Some(number)
            || pull["headRefOid"] != head
            || pull["baseRefName"] != "main"
        {
            return Err(Fault::InvalidResponse);
        }
        Ok(pull.get("stack") == Some(&Value::Null))
    }

    fn merge_queue_state(
        &self,
        store: &dyn ApplicationStore,
        repository: &Repository,
        number: u64,
    ) -> Result<Value, Fault> {
        let data = self.graphql_query(
            self.token(store)?,
            QUEUE_STATE,
            json!({"owner":repository.owner,"name":repository.name,"number":number}),
        )?;
        if !string(&data, "/repository/nameWithOwner")?
            .eq_ignore_ascii_case(&repository.full_name())
        {
            return Err(Fault::InvalidResponse);
        }
        let pull = &data["repository"]["pullRequest"];
        if pull["number"].as_u64() != Some(number) || pull["baseRefName"] != "main" {
            return Err(Fault::InvalidResponse);
        }
        Ok(pull.clone())
    }

    pub(crate) fn merge_queue_anchor(
        &self,
        store: &dyn ApplicationStore,
        repository: &Repository,
        number: u64,
        head: &str,
    ) -> Result<QueueAnchor, Fault> {
        let pull = self.merge_queue_state(store, repository, number)?;
        if pull["headRefOid"] != head
            || pull["merged"] != false
            || pull.get("mergeQueueEntry") != Some(&Value::Null)
        {
            return Err(Fault::Ambiguous);
        }
        let queue = string(&pull, "/mergeQueue/id")?;
        let events = queue_events(&pull)?;
        Ok(QueueAnchor {
            queue,
            last_event: events
                .last()
                .map(|event| string(event, "/id"))
                .transpose()?,
        })
    }

    pub(crate) fn observe_merge_queue(
        &self,
        store: &dyn ApplicationStore,
        repository: &Repository,
        number: u64,
        head: &str,
        anchor: &QueueAnchor,
    ) -> Result<QueueObservation, Fault> {
        let pull = self.merge_queue_state(store, repository, number)?;
        let queued = &pull["mergeQueueEntry"];
        let queue_path = "/mergeQueue/id";
        let active = !queued.is_null()
            && queued.pointer("/headCommit/oid").and_then(Value::as_str) == Some(head)
            && string(queued, queue_path)? == anchor.queue;
        let events = queue_events(&pull)?;
        let index = match &anchor.last_event {
            Some(id) => events
                .iter()
                .position(|event| event["id"] == id.as_str())
                .map(|index| index + 1)
                .ok_or(Fault::Ambiguous)?,
            None if pull["timelineItems"]["pageInfo"]["hasPreviousPage"] == false => 0,
            None => return Err(Fault::Ambiguous),
        };
        let removal = match &events[index..] {
            [added, removed]
                if pull.get("mergeQueueEntry") == Some(&Value::Null)
                    && added["__typename"] == "AddedToMergeQueueEvent"
                    && removed["__typename"] == "RemovedFromMergeQueueEvent"
                    && string(added, queue_path)? == anchor.queue
                    && string(removed, queue_path)? == anchor.queue
                    && removed.pointer("/beforeCommit/oid").and_then(Value::as_str)
                        == Some(head) =>
            {
                Some(string(removed, "/id")?)
            }
            _ => None,
        };
        let merged = pull["merged"].as_bool().ok_or(Fault::InvalidResponse)?;
        // GitHub sets the auto-merge method when the queue merges. Read it only alongside
        // the exact completed queue lifecycle, as its pre-merge setting is not evidence.
        let merged_method = if merged && removal.is_some() && pull["headRefOid"] == head {
            pull.pointer("/autoMergeRequest/mergeMethod")
                .and_then(Value::as_str)
                .map(str::to_ascii_lowercase)
        } else {
            None
        };
        Ok(QueueObservation {
            active,
            removal: if merged { None } else { removal },
            merged_method,
        })
    }

    pub(crate) fn can_merge(
        &self,
        store: &dyn ApplicationStore,
        repository: &Repository,
        method: &MergeMethod,
    ) -> Result<bool, Fault> {
        let url = format!("https://api.github.com/repos/{}", repository.full_name());
        let value = self.request(store, Method::Get, url, None)?;
        if !string(&value, "/full_name")?.eq_ignore_ascii_case(&repository.full_name()) {
            return Err(Fault::InvalidResponse);
        }
        let key = match method {
            MergeMethod::MergeCommit => "allow_merge_commit",
            MergeMethod::Squash => "allow_squash_merge",
            MergeMethod::Rebase => "allow_rebase_merge",
        };
        Ok(value["permissions"]["push"] == true
            && value[key] == true
            && value["archived"] == false
            && value["disabled"] == false)
    }

    /// Read the current required checks from GitHub at this exact head. Do not rely on the
    /// repository's aggregate check status, which can include unrelated checks and omit
    /// expected GitHub App restrictions.
    pub(crate) fn required_checks_green(
        &self,
        store: &dyn ApplicationStore,
        repository: &Repository,
        head: &str,
        required: &[(String, Option<u64>)],
    ) -> Result<bool, Fault> {
        if required.is_empty() {
            return Ok(true);
        }
        if head.len() != 40 || !super::super::client::oid_ok(head) {
            return Err(Fault::InvalidResponse);
        }
        let prefix = format!(
            "https://api.github.com/repos/{}/commits/{head}",
            repository.full_name()
        );
        let mut matched = vec![(false, true); required.len()];
        for (endpoint, key) in [("check-runs", "check_runs"), ("status", "statuses")] {
            for page in 1..=100 {
                let value = self.request(
                    store,
                    Method::Get,
                    format!("{prefix}/{endpoint}?per_page=100&page={page}"),
                    None,
                )?;
                let runs = endpoint == "check-runs";
                if !runs && commit(&value, "/sha")? != head {
                    return Err(Fault::InvalidResponse);
                }
                let rows = value[key].as_array().ok_or(Fault::InvalidResponse)?;
                for row in rows {
                    if runs && commit(row, "/head_sha")? != head {
                        return Err(Fault::InvalidResponse);
                    }
                    let name = string(row, if runs { "/name" } else { "/context" })?;
                    let app = row["app"]["id"].as_u64();
                    let green = if runs {
                        row["status"] == "completed" && row["conclusion"] == "success"
                    } else {
                        row["state"] == "success"
                    };
                    for (index, (expected, source)) in required.iter().enumerate() {
                        if expected == &name && source.is_none_or(|source| app == Some(source)) {
                            matched[index].0 = true;
                            matched[index].1 &= green;
                        }
                    }
                }
                if rows.len() < 100 {
                    break;
                }
                if page == 100 {
                    return Err(Fault::Ambiguous);
                }
            }
        }
        Ok(matched.into_iter().all(|(found, green)| found && green))
    }

    pub(crate) fn commit_parents_on_github(
        &self,
        store: &dyn ApplicationStore,
        repository: &Repository,
        sha: &str,
    ) -> Result<Vec<String>, Fault> {
        if sha.len() != 40 || !super::super::client::oid_ok(sha) {
            return Err(Fault::InvalidResponse);
        }
        let value = self.request(
            store,
            Method::Get,
            format!(
                "https://api.github.com/repos/{}/git/commits/{sha}",
                repository.full_name()
            ),
            None,
        )?;
        if commit(&value, "/sha")? != sha {
            return Err(Fault::InvalidResponse);
        }
        value["parents"]
            .as_array()
            .ok_or(Fault::InvalidResponse)?
            .iter()
            .map(|parent| commit(parent, "/sha"))
            .collect()
    }

    pub(crate) fn commit_reaches_main(
        &self,
        store: &dyn ApplicationStore,
        repository: &Repository,
        sha: &str,
        main: &str,
    ) -> Result<bool, Fault> {
        if [sha, main]
            .into_iter()
            .any(|id| id.len() != 40 || !super::super::client::oid_ok(id))
        {
            return Err(Fault::InvalidResponse);
        }
        if sha == main {
            return Ok(true);
        }
        let value = self.request(
            store,
            Method::Get,
            format!(
                "https://api.github.com/repos/{}/compare/{sha}...{main}?per_page=1",
                repository.full_name()
            ),
            None,
        )?;
        if commit(&value, "/base_commit/sha")? != sha {
            return Err(Fault::InvalidResponse);
        }
        Ok(value["status"] == "ahead" && commit(&value, "/merge_base_commit/sha")? == sha)
    }
}
