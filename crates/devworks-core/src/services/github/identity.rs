//! Account and repository identity observed through the authenticated GitHub.com API.
use super::api::{ApiPage, Endpoint, Validators};
use crate::ports::storage::ApplicationStore;
use crate::services::git::github::{Fault, GitHubClient, Repository};
use serde_json::{Map, Value};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountIdentity {
    pub remote_id: String,
    pub login: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepositoryIdentity {
    pub remote_id: String,
    pub numeric_id: String,
    pub repository: Repository,
    pub owner_kind: String,
    pub web_url: String,
    pub default_branch: String,
    pub permissions: Map<String, Value>,
    pub archived: bool,
    pub disabled: bool,
}

impl GitHubClient {
    /// Identity comes from /user, independently of any local Git transport credentials.
    pub fn observe_account(
        &self,
        store: &dyn ApplicationStore,
        now_ms: i64,
    ) -> Result<ApiPage, Fault> {
        let endpoint = Endpoint::new("https://api.github.com/user", None)?;
        self.observe_sign_in(store, &endpoint, now_ms)
    }

    /// Verify a particular registered remote against GitHub's authenticated repository API.
    pub fn observe_repository(
        &self,
        store: &dyn ApplicationStore,
        repository: &Repository,
        now_ms: i64,
    ) -> Result<RepositoryIdentity, Fault> {
        let endpoint = Endpoint::new(
            &format!(
                "https://api.github.com/repos/{}/{}",
                repository.owner, repository.name,
            ),
            None,
        )?;
        let page = self.observe_page(store, &endpoint, None, None, now_ms, 0)?;
        let observed = RepositoryIdentity::decode(successful_value(&page)?)?;
        if !observed
            .repository
            .owner
            .eq_ignore_ascii_case(&repository.owner)
            || !observed
                .repository
                .name
                .eq_ignore_ascii_case(&repository.name)
        {
            return Err(Fault::InvalidResponse);
        }
        Ok(observed)
    }

    /// Preserve opaque pagination while selecting the personal and organization repositories
    /// the authenticated account can see, including collaborator access.
    pub fn observe_account_repositories(
        &self,
        store: &dyn ApplicationStore,
        next: Option<&str>,
        validators: Option<&Validators>,
        limit: u16,
        now_ms: i64,
    ) -> Result<ApiPage, Fault> {
        let endpoint = Endpoint::new(
            &format!(
                "https://api.github.com/user/repos?visibility=all&affiliation=owner,collaborator,organization_member&per_page={}",
                limit.clamp(1, 100)
            ),
            None,
        )?;
        self.observe_page(store, &endpoint, next, validators, now_ms, 0)
    }
}

impl AccountIdentity {
    pub fn decode(page: &ApiPage) -> Result<Self, Fault> {
        let value = successful_value(page)?;
        let login = value["login"]
            .as_str()
            .filter(|name| {
                !name.is_empty()
                    && name.len() <= 100
                    && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            })
            .ok_or(Fault::InvalidResponse)?;
        Ok(Self {
            remote_id: remote_id(&value["id"])?,
            login: login.to_owned(),
        })
    }
}

impl RepositoryIdentity {
    pub fn decode_page(page: &ApiPage) -> Result<Vec<Self>, Fault> {
        successful_value(page)?
            .as_array()
            .filter(|rows| rows.len() <= 100)
            .ok_or(Fault::InvalidResponse)?
            .iter()
            .map(Self::decode)
            .collect()
    }

    pub fn decode(value: &Value) -> Result<Self, Fault> {
        let web_url = value["html_url"].as_str().ok_or(Fault::InvalidResponse)?;
        let repository = Repository::from_remote(web_url).ok_or(Fault::InvalidResponse)?;
        let full_name = format!("{}/{}", repository.owner, repository.name);
        // Reject lookalike links, URL credentials, origin aliases and mismatched owner names.
        let expected = format!("https://github.com/{full_name}");
        if !web_url.eq_ignore_ascii_case(&expected)
            || !matches!(value["full_name"].as_str(), Some(name) if name.eq_ignore_ascii_case(&full_name))
            || !matches!(value["name"].as_str(), Some(name) if name.eq_ignore_ascii_case(&repository.name))
            || !matches!(value["owner"]["login"].as_str(), Some(owner) if owner.eq_ignore_ascii_case(&repository.owner))
        {
            return Err(Fault::InvalidResponse);
        }
        let owner_kind = value["owner"]["type"]
            .as_str()
            .filter(|kind| matches!(*kind, "User" | "Organization"))
            .ok_or(Fault::InvalidResponse)?;
        let default_branch = value["default_branch"]
            .as_str()
            .filter(|branch| {
                !branch.is_empty() && branch.len() <= 1024 && !branch.chars().any(char::is_control)
            })
            .ok_or(Fault::InvalidResponse)?;
        let mut permissions = Map::new();
        if let Some(raw) = value.get("permissions") {
            let raw = raw.as_object().ok_or(Fault::InvalidResponse)?;
            for key in ["pull", "triage", "push", "maintain", "admin"] {
                if let Some(observed) = raw.get(key) {
                    permissions.insert(
                        key.to_owned(),
                        Value::Bool(observed.as_bool().ok_or(Fault::InvalidResponse)?),
                    );
                }
            }
        }
        let numeric_id = remote_id(&value["id"])?;
        let remote_id = match value.get("node_id") {
            Some(node) => node
                .as_str()
                .filter(|id| {
                    !id.is_empty() && id.len() <= 256 && id.bytes().all(|b| b.is_ascii_graphic())
                })
                .ok_or(Fault::InvalidResponse)?
                .to_owned(),
            None => numeric_id.clone(),
        };
        Ok(Self {
            remote_id,
            numeric_id,
            repository,
            owner_kind: owner_kind.to_owned(),
            web_url: expected,
            default_branch: default_branch.to_owned(),
            permissions,
            archived: flag(value, "archived")?,
            disabled: flag(value, "disabled")?,
        })
    }
}

fn successful_value(page: &ApiPage) -> Result<&Value, Fault> {
    match page.status {
        200 => page.value.as_ref().ok_or(Fault::InvalidResponse),
        status => Err(Fault::Rejected(status)),
    }
}

fn remote_id(value: &Value) -> Result<String, Fault> {
    value
        .as_u64()
        .filter(|id| *id != 0)
        .map(|id| id.to_string())
        .ok_or(Fault::InvalidResponse)
}

fn flag(value: &Value, field: &str) -> Result<bool, Fault> {
    value[field].as_bool().ok_or(Fault::InvalidResponse)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn repository() -> Value {
        json!({
            "id": 12, "node_id":"repo-node=", "owner": {"type": "Organization", "login":"Acme"},
            "html_url":"https://github.com/Acme/kit","full_name":"Acme/kit","name":"kit",
            "default_branch":"release/next","permissions":{"pull":true,"push":false},
            "archived":false,"disabled":false
        })
    }

    #[test]
    fn organization_identity_keeps_exact_repository_and_observed_permissions() {
        let row = RepositoryIdentity::decode(&repository()).unwrap();
        assert_eq!(row.remote_id, "repo-node=");
        assert_eq!(row.numeric_id, "12");
        assert_eq!(row.owner_kind, "Organization");
        assert_eq!(row.repository.owner, "Acme");
        assert_eq!(row.web_url, "https://github.com/Acme/kit");
        assert_eq!(row.default_branch, "release/next");
        assert_eq!(row.permissions["pull"], true);
        assert_eq!(row.permissions["push"], false);
        assert!(!row.permissions.contains_key("admin"));
    }

    #[test]
    fn repository_identity_refuses_cross_owner_links_and_malformed_permissions() {
        let mut row = repository();
        row["html_url"] = json!("https://github.com/Other/kit");
        assert_eq!(
            RepositoryIdentity::decode(&row),
            Err(Fault::InvalidResponse)
        );
        row = repository();
        row["permissions"]["push"] = json!("true");
        assert_eq!(
            RepositoryIdentity::decode(&row),
            Err(Fault::InvalidResponse)
        );
    }
}
