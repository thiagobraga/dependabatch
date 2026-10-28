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
            out.push(PullRequest::new(number, &title, DEPENDABOT_LOGIN, files));
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
                    .unwrap_or_default()
                    .to_string();
                out.push(PrFile {
                    filename,
                    hunks: parse_hunks_from_patch(&patch),
                });
            }
            page += 1;
        }
        Ok(out)
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
