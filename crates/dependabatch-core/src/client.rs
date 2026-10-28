use anyhow::{Context, Result, anyhow};
use reqwest::header::ACCEPT;
use serde_json::Value;

use crate::pr::{DEPENDABOT_LOGIN, PrFile, PullRequest, parse_hunks_from_patch};

/// `owner/name` repository identifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoSlug(String);

impl RepoSlug {
    pub fn parse(input: &str) -> Result<Self> {
        let parts: Vec<&str> = input.split('/').filter(|p| !p.is_empty()).collect();
        if parts.len() == 2 && parts.iter().all(|p| !p.is_empty()) {
            Ok(Self(input.to_string()))
        } else {
            Err(anyhow!(
                "expected repository in `owner/name` form, got `{input}`"
            ))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Minimal typed wrapper over the GitHub REST API used by dependabatch.
pub struct GithubClient {
    http: reqwest::Client,
    token: String,
}

impl GithubClient {
    pub fn new(token: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent("dependabatch")
            .build()?;
        Ok(Self {
            http,
            token: token.to_string(),
        })
    }

    /// Every open PR authored by dependabot, with parsed files and hunks.
    pub async fn open_dependabot_prs(&self, repo: &RepoSlug) -> Result<Vec<PullRequest>> {
        let mut raw = Vec::new();
        let mut page = 1u32;
        loop {
            let query = vec![
                ("state", "open".to_string()),
                ("per_page", "100".to_string()),
                ("page", page.to_string()),
            ];
            let batch: Vec<Value> = self
                .get(
                    &format!("https://api.github.com/repos/{}/pulls", repo.as_str()),
                    &query,
                )
                .await?;
            if batch.is_empty() {
                break;
            }
            for pr in batch {
                let login = pr
                    .pointer("/user/login")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if login == DEPENDABOT_LOGIN {
                    raw.push(pr);
                }
            }
            page += 1;
        }

        let mut out = Vec::new();
        for pr in raw {
            let number = pr
                .pointer("/number")
                .and_then(Value::as_u64)
                .context("pull request without number")?;
            let title = pr
                .pointer("/title")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let files = self.pr_files(repo, number).await?;
            let base_sha = pr
                .pointer("/base/sha")
                .and_then(Value::as_str)
                .map(str::to_string);
            let mut pull = PullRequest::new(number, &title, DEPENDABOT_LOGIN, files);
            pull.base_sha = base_sha;
            out.push(pull);
        }
        Ok(out)
    }

    /// Files changed by one PR, including hunk positions from each patch.
    pub async fn pr_files(&self, repo: &RepoSlug, number: u64) -> Result<Vec<PrFile>> {
        let mut out = Vec::new();
        let mut page = 1u32;
        loop {
            let query = vec![("per_page", "100".to_string()), ("page", page.to_string())];
            let batch: Vec<Value> = self
                .get::<Vec<Value>>(
                    &format!(
                        "https://api.github.com/repos/{}/pulls/{}/files",
                        repo.as_str(),
                        number
                    ),
                    &query,
                )
                .await?;
            if batch.is_empty() {
                break;
            }
            for file in batch {
                let filename = file
                    .pointer("/filename")
                    .and_then(Value::as_str)
                    .context("pull request file without filename")?
                    .to_string();
                let patch = file
                    .pointer("/patch")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                out.push(PrFile {
                    filename,
                    hunks: parse_hunks_from_patch(patch.as_deref().unwrap_or_default()),
                    patch,
                });
            }
            page += 1;
        }
        Ok(out)
    }

    /// Default branch of the repo (`main`, `master`, ...).
    pub async fn default_branch(&self, repo: &RepoSlug) -> Result<String> {
        let json: Value = self
            .get(
                &format!("https://api.github.com/repos/{}", repo.as_str()),
                &[],
            )
            .await?;
        json.pointer("/default_branch")
            .and_then(Value::as_str)
            .map(str::to_string)
            .context("repository without default_branch")
    }

    /// `package-lock.json` presence check via the contents API.
    pub async fn file_exists(&self, repo: &RepoSlug, path: &str, git_ref: &str) -> Result<bool> {
        let url = format!(
            "https://api.github.com/repos/{}/contents/{path}",
            repo.as_str()
        );
        let resp = self
            .http
            .get(&url)
            .query(&[("ref", git_ref.to_string())])
            .bearer_auth(&self.token)
            .header(ACCEPT, "application/vnd.github+json")
            .send()
            .await?;
        Ok(resp.status() == 200)
    }

    /// Opens the batch PR; reuses the existing one when GitHub rejects a duplicate.
    pub async fn create_pull(
        &self,
        repo: &RepoSlug,
        title: &str,
        head: &str,
        base: &str,
        body: &str,
    ) -> Result<String> {
        let url = format!("https://api.github.com/repos/{}/pulls", repo.as_str());
        let payload = serde_json::json!({
            "title": title,
            "head": head,
            "base": base,
            "body": body,
        });
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.token)
            .header(ACCEPT, "application/vnd.github+json")
            .json(&payload)
            .send()
            .await?;
        if resp.status().as_u16() == 422 {
            if let Some(existing) = self.find_open_pull(repo, head).await? {
                return Ok(existing);
            }
        }
        let resp = resp
            .error_for_status()
            .context("creating pull request failed")?;
        let json: Value = resp.json().await?;
        json.pointer("/html_url")
            .and_then(Value::as_str)
            .map(str::to_string)
            .context("pull request response without html_url")
    }

    /// Open PR on `owner:branch`, if any.
    pub async fn find_open_pull(&self, repo: &RepoSlug, head: &str) -> Result<Option<String>> {
        let owner = repo.as_str().split('/').next().unwrap_or_default();
        let query = vec![
            ("state", "open".to_string()),
            ("head", format!("{owner}:{head}")),
        ];
        let batch: Vec<Value> = self
            .get(
                &format!("https://api.github.com/repos/{}/pulls", repo.as_str()),
                &query,
            )
            .await?;
        Ok(batch
            .first()
            .and_then(|pr| pr.pointer("/html_url"))
            .and_then(Value::as_str)
            .map(str::to_string))
    }

    async fn get<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        query: &[(&str, String)],
    ) -> Result<T> {
        let resp = self
            .http
            .get(url)
            .query(query)
            .bearer_auth(&self.token)
            .header(ACCEPT, "application/vnd.github+json")
            .send()
            .await?
            .error_for_status()?;
        Ok(resp.json().await?)
    }
}
