//! Bounded GitHub.com PR client, over the shared no-redirect HTTP and OS-secret ports.
//! Transport credentials remain Git's concern; API authorization uses github.credential.
use crate::authority::redact::SecretRef;
use crate::ports::http::{HttpRequest, HttpTransport, Method};
use crate::ports::secrets::{Secret, SecretResolver};
use crate::ports::storage::{ApplicationStore, ConfigScope, ReadOptions, read_snapshot};
use crate::settings::{
    catalog::descriptor,
    model::{chain, current_layer},
};
use serde_json::{Value, json};
use std::sync::Arc;

pub struct GitHubClient {
    pub http: Arc<dyn HttpTransport>,
    pub secrets: Arc<dyn SecretResolver>,
}
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Repository {
    pub owner: String,
    pub name: String,
}
impl Repository {
    pub fn from_remote(remote: &str) -> Option<Self> {
        let path = if let Some(path) = remote.strip_prefix("git@github.com:") {
            path.to_owned()
        } else {
            let url = url::Url::parse(remote).ok()?;
            if url.host_str()? != "github.com"
                || !matches!(url.scheme(), "https" | "ssh")
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
                || (url.port().is_some()
                    && url.port() != Some(if url.scheme() == "ssh" { 22 } else { 443 }))
                || !(url.username().is_empty()
                    || (url.scheme() == "ssh" && url.username() == "git"))
            {
                return None;
            }
            url.path().trim_start_matches('/').to_owned()
        };
        let path = path.trim_end_matches('/');
        let path = path.strip_suffix(".git").unwrap_or(path);
        let (owner, name) = path.split_once('/')?;
        let valid = |s: &str| {
            !s.is_empty()
                && s.len() <= 100
                && s != "."
                && s != ".."
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        };
        (valid(owner) && valid(name)).then(|| Self {
            owner: owner.into(),
            name: name.into(),
        })
    }
    fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
    fn api(&self) -> String {
        format!(
            "https://api.github.com/repos/{}/{}/pulls",
            self.owner, self.name
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fault {
    CredentialsRequired,
    Unavailable,
    Rejected(u16),
    InvalidResponse,
    Ambiguous,
}
#[derive(Clone, Debug)]
pub struct PullRequest {
    pub number: u64,
    pub url: String,
    pub head: String,
    pub base: String,
    pub sha: String,
    pub title: String,
    pub body: String,
}
fn parse(repository: &Repository, value: &Value) -> Result<PullRequest, Fault> {
    let string = |pointer: &str| {
        value
            .pointer(pointer)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or(Fault::InvalidResponse)
    };
    let number = value["number"]
        .as_u64()
        .filter(|n| *n > 0)
        .ok_or(Fault::InvalidResponse)?;
    let expected = format!(
        "https://github.com/{}/{}/pull/{number}",
        repository.owner, repository.name
    );
    if !string("/head/repo/full_name")?.eq_ignore_ascii_case(&repository.full_name())
        || !string("/base/repo/full_name")?.eq_ignore_ascii_case(&repository.full_name())
        || !string("/html_url")?.eq_ignore_ascii_case(&expected)
        || value["state"] != "open"
    {
        return Err(Fault::InvalidResponse);
    }
    Ok(PullRequest {
        number,
        url: expected,
        head: string("/head/ref")?,
        base: string("/base/ref")?,
        sha: string("/head/sha")?,
        title: string("/title")?,
        body: value["body"].as_str().unwrap_or("").to_owned(),
    })
}
impl GitHubClient {
    pub(crate) fn token(&self, store: &dyn ApplicationStore) -> Result<Secret, Fault> {
        let reference = read_snapshot(store, &ReadOptions::default(), |s| {
            let layer = current_layer(s.configuration(), &ConfigScope::Workspace)?;
            let d = descriptor("github.credential").expect("catalog credential");
            let entries = chain(d, &[layer]).map_err(|_| {
                crate::ports::storage::StorageError::InvalidRecord(
                    "GitHub credential configuration".into(),
                )
            })?;
            Ok(entries
                .into_iter()
                .next()
                .and_then(|e| SecretRef::from_setting(&e.value.value.0)))
        })
        .map_err(|_| Fault::Unavailable)?
        .ok_or(Fault::CredentialsRequired)?;
        let value = self
            .secrets
            .resolve(&reference)
            .map_err(|_| Fault::CredentialsRequired)?;
        Ok(Secret::new(format!("Bearer {}", value.expose())))
    }
    fn request(
        &self,
        store: &dyn ApplicationStore,
        method: Method,
        url: String,
        body: Option<Value>,
    ) -> Result<Value, Fault> {
        let mut request = HttpRequest::get(url)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .secret_header("Authorization", self.token(store)?);
        request.method = method;
        request.timeout_ms = 15_000;
        request.max_response_bytes = 2 * 1024 * 1024;
        if let Some(body) = body {
            request = request.header("Content-Type", "application/json");
            request.body = Some(serde_json::to_vec(&body).map_err(|_| Fault::InvalidResponse)?);
        }
        let response = self.http.send(&request).map_err(|_| Fault::Unavailable)?;
        if !response.success() {
            return Err(Fault::Rejected(response.status));
        }
        serde_json::from_slice(&response.body).map_err(|_| Fault::InvalidResponse)
    }
    pub fn default_branch(
        &self,
        store: &dyn ApplicationStore,
        repository: &Repository,
    ) -> Result<String, Fault> {
        let url = format!(
            "https://api.github.com/repos/{}/{}",
            repository.owner, repository.name
        );
        let value = self.request(store, Method::Get, url, None)?;
        if !value["full_name"]
            .as_str()
            .is_some_and(|name| name.eq_ignore_ascii_case(&repository.full_name()))
        {
            return Err(Fault::InvalidResponse);
        }
        value["default_branch"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .ok_or(Fault::InvalidResponse)
    }

    /// Look up all open PRs of this exact repository/head, independently of the old base so
    /// linking an issue to a feature can retarget its existing PR without duplicating it.
    pub fn find(
        &self,
        store: &dyn ApplicationStore,
        repository: &Repository,
        head: &str,
    ) -> Result<Option<PullRequest>, Fault> {
        let mut found = None;
        for page in 1..=100 {
            let mut url = url::Url::parse(&repository.api()).expect("fixed API origin");
            url.query_pairs_mut()
                .append_pair("state", "open")
                .append_pair("head", &format!("{}:{head}", repository.owner))
                .append_pair("per_page", "100")
                .append_pair("page", &page.to_string());
            let value = self.request(store, Method::Get, url.into(), None)?;
            let rows = value.as_array().ok_or(Fault::InvalidResponse)?;
            for row in rows {
                let pr = parse(repository, row)?;
                if pr.head != head {
                    return Err(Fault::InvalidResponse);
                }
                if found.replace(pr).is_some() {
                    return Err(Fault::Ambiguous);
                }
            }
            if rows.len() < 100 {
                return Ok(found);
            }
        }
        Err(Fault::Ambiguous)
    }
    pub fn create(
        &self,
        store: &dyn ApplicationStore,
        repository: &Repository,
        head: &str,
        base: &str,
        title: &str,
        description: &str,
    ) -> Result<PullRequest, Fault> {
        parse(
            repository,
            &self.request(
                store,
                Method::Post,
                repository.api(),
                Some(json!({"head":head,"base":base,"title":title,"body":description})),
            )?,
        )
    }
    pub fn update(
        &self,
        store: &dyn ApplicationStore,
        repository: &Repository,
        number: u64,
        base: &str,
        title: &str,
        description: &str,
    ) -> Result<PullRequest, Fault> {
        parse(
            repository,
            &self.request(
                store,
                Method::Patch,
                format!("{}/{number}", repository.api()),
                Some(json!({"base":base,"title":title,"body":description})),
            )?,
        )
    }
}

/// Encode a branch as one URI path segment, including Git-valid #, %, + and slash characters.
pub fn branch_url(repository_url: &str, branch: &str) -> Option<String> {
    let mut url = url::Url::parse(repository_url).ok()?;
    if url.scheme() != "https"
        || url.host_str() != Some("github.com")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    url.path_segments_mut()
        .ok()?
        .pop_if_empty()
        .extend(["tree", branch.strip_prefix("refs/heads/").unwrap_or(branch)]);
    Some(url.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn repository_identity_rejects_other_origins_and_credentials() {
        for remote in [
            "https://github.com/o/r.git",
            "git@github.com:o/r.git",
            "ssh://git@github.com/o/r.git",
        ] {
            assert_eq!(
                Repository::from_remote(remote),
                Some(Repository {
                    owner: "o".into(),
                    name: "r".into()
                })
            );
        }
        for remote in [
            "https://github.com.evil/o/r",
            "https://user:secret@github.com/o/r",
            "https://github.com/o/r/extra",
            "ext::bad",
            "https://github.com/o/%2fr",
        ] {
            assert!(Repository::from_remote(remote).is_none());
        }
    }
}
