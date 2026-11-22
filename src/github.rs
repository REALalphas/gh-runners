use std::collections::HashSet;
use std::io::Read;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail, ensure};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use reqwest::blocking::{Client, Response};
use reqwest::header::{HeaderMap, LINK, RETRY_AFTER};
use reqwest::{Method, StatusCode, Url, redirect};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

use crate::pool::POOL_LABEL;

const API: &str = "https://api.github.com";
const TOKEN_CACHE: Duration = Duration::from_secs(50 * 60);

pub struct Github {
    client: Client,
    app_id: String,
    installation_id: String,
    key: EncodingKey,
    token: Option<InstallationToken>,
    rate_limited_until: Option<Instant>,
}

struct InstallationToken {
    value: String,
    cached_at: Instant,
}

#[derive(Serialize)]
struct Claims<'a> {
    iss: &'a str,
    iat: u64,
    exp: u64,
}

#[derive(Deserialize)]
struct TokenResponse {
    token: String,
}

pub struct Job {
    pub id: u64,
    pub labels: Vec<String>,
}

#[derive(Deserialize)]
pub struct Runner {
    pub id: u64,
    pub name: String,
    pub busy: bool,
    pub status: String,
    #[serde(default)]
    labels: Vec<RunnerLabel>,
}

impl Runner {
    pub fn has_label(&self, label: &str) -> bool {
        self.labels
            .iter()
            .any(|registered| registered.name.eq_ignore_ascii_case(label))
    }
}

#[derive(Deserialize)]
struct RunnerLabel {
    name: String,
}

#[derive(Deserialize)]
struct WorkflowRun {
    id: u64,
    run_attempt: u64,
}

#[derive(Deserialize)]
struct WorkflowJob {
    id: u64,
    labels: Vec<String>,
    status: String,
}

#[derive(Deserialize)]
struct JitResponse {
    runner: JitRunner,
    encoded_jit_config: String,
}

#[derive(Deserialize)]
struct JitRunner {
    id: u64,
}

#[derive(Deserialize)]
struct InstallationRepository {
    full_name: String,
    archived: bool,
    disabled: bool,
}

#[derive(Deserialize)]
struct RepositoryVisibility {
    private: bool,
}

#[derive(Deserialize)]
struct PrivateForkSettings {
    run_workflows_from_fork_pull_requests: Option<bool>,
}

impl Github {
    pub fn new(app_id: &str, installation_id: &str, pem: &[u8]) -> Result<Self> {
        ensure!(!app_id.is_empty(), "GitHub App ID is empty");
        ensure!(
            !installation_id.is_empty() && installation_id.bytes().all(|b| b.is_ascii_digit()),
            "Invalid GitHub App installation ID"
        );

        let key = EncodingKey::from_rsa_pem(pem)
            .map_err(|_| anyhow!("Invalid GitHub App RSA private key"))?;
        let client = Client::builder()
            .use_rustls_tls()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(20))
            .redirect(redirect::Policy::none())
            .user_agent("gh-runners")
            .build()
            .map_err(|_| anyhow!("Failed to initialize GitHub HTTP client"))?;

        Ok(Self {
            client,
            app_id: app_id.to_owned(),
            installation_id: installation_id.to_owned(),
            key,
            token: None,
            rate_limited_until: None,
        })
    }

    pub fn repositories(&mut self) -> Result<Vec<String>> {
        let mut seen = HashSet::new();
        let mut repositories = Vec::new();
        for repo in
            self.pages::<InstallationRepository>("/installation/repositories", "repositories", &[])?
        {
            if !repo.archived && !repo.disabled {
                repository_path(&repo.full_name)?;
                if seen.insert(repo.full_name.to_ascii_lowercase()) {
                    repositories.push(repo.full_name);
                }
            }
        }
        Ok(repositories)
    }

    pub fn allows_private_cache(&mut self, repo: &str) -> Result<bool> {
        let path = repository_path(repo)?;
        let response = self.installation_request(Method::GET, &api_url(&path)?, None)?;
        let visibility: RepositoryVisibility =
            read_json(require_success(response, "read repository visibility")?)?;
        if !visibility.private {
            return Ok(false);
        }

        let url = api_url(&format!(
            "{path}/actions/permissions/fork-pr-workflows-private-repos"
        ))?;
        let response = self.installation_request(Method::GET, &url, None)?;
        let settings: PrivateForkSettings = read_json(require_success(
            response,
            "read private fork workflow settings",
        )?)?;
        Ok(settings.run_workflows_from_fork_pull_requests == Some(false))
    }

    pub fn queued_jobs(&mut self, repo: &str) -> Result<Vec<Job>> {
        let path = format!("{}/actions/runs", repository_path(repo)?);
        let mut seen_attempts = HashSet::new();
        let mut seen_jobs = HashSet::new();
        let mut jobs = Vec::new();

        for status in ["queued", "in_progress"] {
            for run in self.pages::<WorkflowRun>(&path, "workflow_runs", &[("status", status)])? {
                if !seen_attempts.insert((run.id, run.run_attempt)) {
                    continue;
                }
                let jobs_path = format!("{path}/{}/attempts/{}/jobs", run.id, run.run_attempt);
                for job in self.pages::<WorkflowJob>(&jobs_path, "jobs", &[])? {
                    if job.status == "queued" && seen_jobs.insert(job.id) {
                        jobs.push(Job {
                            id: job.id,
                            labels: job.labels,
                        });
                    }
                }
            }
        }
        Ok(jobs)
    }

    pub fn runners(&mut self, repo: &str) -> Result<Vec<Runner>> {
        let path = format!("{}/actions/runners", repository_path(repo)?);
        let mut runners = self.pages::<Runner>(&path, "runners", &[])?;
        let mut seen = HashSet::new();
        runners.retain(|runner| seen.insert(runner.id));
        Ok(runners)
    }

    pub fn jit(&mut self, repo: &str, name: &str, profile: &str) -> Result<(u64, String)> {
        let url = api_url(&format!(
            "{}/actions/runners/generate-jitconfig",
            repository_path(repo)?
        ))?;
        let payload = json!({
            "name": name,
            "runner_group_id": 1,
            "labels": ["self-hosted", "Linux", "X64", POOL_LABEL, profile],
        });
        let response = self.installation_request(Method::POST, &url, Some(&payload))?;
        let config: JitResponse = read_json(require_success(
            response,
            "generate runner JIT configuration",
        )?)?;
        Ok((config.runner.id, config.encoded_jit_config))
    }

    pub fn remove_runner(&mut self, repo: &str, id: u64) -> Result<()> {
        let url = api_url(&format!("{}/actions/runners/{id}", repository_path(repo)?))?;
        let response = self.installation_request(Method::DELETE, &url, None)?;
        if response.status() != StatusCode::NOT_FOUND {
            require_success(response, "remove runner")?;
        }
        Ok(())
    }

    fn refresh_token(&mut self) -> Result<()> {
        if self
            .token
            .as_ref()
            .is_some_and(|token| token.cached_at.elapsed() < TOKEN_CACHE)
        {
            return Ok(());
        }
        let cached_at = Instant::now();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("System clock precedes the Unix epoch")?
            .as_secs();
        let jwt = encode(
            &Header::new(Algorithm::RS256),
            &Claims {
                iss: &self.app_id,
                iat: now.saturating_sub(60),
                exp: now + 540,
            },
            &self.key,
        )
        .map_err(|_| anyhow!("Failed to sign GitHub App JWT"))?;
        let url = api_url(&format!(
            "/app/installations/{}/access_tokens",
            self.installation_id
        ))?;
        let response = send(&self.client, Method::POST, &url, &jwt, Some(&json!({})))?;
        let response = self.observe_rate_limit(response)?;
        let token: TokenResponse =
            read_json(require_success(response, "create installation token")?)?;
        ensure!(
            !token.token.is_empty(),
            "GitHub returned an empty installation token"
        );
        self.token = Some(InstallationToken {
            value: token.token,
            cached_at,
        });
        Ok(())
    }

    fn installation_request(
        &mut self,
        method: Method,
        url: &Url,
        payload: Option<&Value>,
    ) -> Result<Response> {
        self.check_rate_limit()?;
        let mut response = self.send_with_installation_token(method.clone(), url, payload)?;
        if response.status() == StatusCode::UNAUTHORIZED {
            self.token = None;
            response = self.send_with_installation_token(method, url, payload)?;
            if response.status() == StatusCode::UNAUTHORIZED {
                self.token = None;
            }
        }
        Ok(response)
    }

    fn send_with_installation_token(
        &mut self,
        method: Method,
        url: &Url,
        payload: Option<&Value>,
    ) -> Result<Response> {
        self.refresh_token()?;
        let token = self
            .token
            .as_ref()
            .context("Missing GitHub installation token")?;
        self.check_rate_limit()?;
        let response = send(&self.client, method, url, &token.value, payload)?;
        self.observe_rate_limit(response)
    }

    fn check_rate_limit(&self) -> Result<()> {
        if self
            .rate_limited_until
            .is_some_and(|until| Instant::now() < until)
        {
            bail!("GitHub API rate limit is active");
        }
        Ok(())
    }

    fn observe_rate_limit(&mut self, response: Response) -> Result<Response> {
        let status = response.status();
        let headers = response.headers();
        let primary = header_number(headers, "x-ratelimit-remaining") == Some(0);
        let retry = headers.contains_key(RETRY_AFTER);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let reset = if primary {
            header_number(headers, "x-ratelimit-reset")
                .filter(|&reset| reset >= now)
                .map(|reset| reset.saturating_sub(now).saturating_add(1))
        } else {
            None
        };
        let delay = reset.max(header_number(headers, RETRY_AFTER.as_str()).filter(|&s| s > 0));
        let secondary =
            status == StatusCode::TOO_MANY_REQUESTS || (status == StatusCode::FORBIDDEN && retry);
        if primary || retry || secondary {
            let now = Instant::now();
            self.rate_limited_until = now
                .checked_add(Duration::from_secs(delay.unwrap_or(60)))
                .or_else(|| now.checked_add(Duration::from_secs(60)));
        }
        if status == StatusCode::FORBIDDEN && !primary && !retry {
            let mut body = Vec::new();
            let _ = response.take(4096).read_to_end(&mut body);
            if String::from_utf8_lossy(&body)
                .to_ascii_lowercase()
                .contains("secondary rate limit")
            {
                self.rate_limited_until = Some(Instant::now() + Duration::from_secs(60));
                bail!("GitHub API rate limit reached (HTTP 403)");
            }
            bail!("GitHub API request forbidden (HTTP 403)");
        }
        if secondary || (primary && !status.is_success()) {
            bail!("GitHub API rate limit reached (HTTP {})", status.as_u16());
        }
        Ok(response)
    }

    fn pages<T: DeserializeOwned>(
        &mut self,
        path: &str,
        key: &str,
        query: &[(&str, &str)],
    ) -> Result<Vec<T>> {
        let mut url = api_url(path)?;
        url.query_pairs_mut()
            .append_pair("per_page", "100")
            .extend_pairs(query.iter().copied());
        let mut visited = HashSet::new();
        let mut items = Vec::new();
        loop {
            ensure!(visited.insert(url.clone()), "GitHub API pagination loop");
            let response = require_success(
                self.installation_request(Method::GET, &url, None)?,
                &format!("list {key}"),
            )?;
            let next = next_page(&response)?;
            let mut body: Value = read_json(response)?;
            let page = body
                .as_object_mut()
                .and_then(|object| object.remove(key))
                .with_context(|| format!("GitHub response is missing {key}"))?;
            let page: Vec<T> = serde_json::from_value(page)
                .map_err(|_| anyhow!("Invalid GitHub response data for {key}"))?;
            items.extend(page);
            match next {
                Some(next) => url = next,
                None => return Ok(items),
            }
        }
    }
}

fn repository_path(repo: &str) -> Result<String> {
    let (owner, repo) = repo
        .split_once('/')
        .context("GitHub repository must be in owner/repo format")?;
    ensure!(
        [owner, repo].into_iter().all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        }),
        "Invalid GitHub repository"
    );
    Ok(format!("/repos/{owner}/{repo}"))
}

fn header_number(headers: &HeaderMap, name: &str) -> Option<u64> {
    headers.get(name)?.to_str().ok()?.parse().ok()
}

fn api_url(path: &str) -> Result<Url> {
    Url::parse(&format!("{API}{path}")).map_err(|_| anyhow!("Invalid GitHub API URL"))
}

fn next_page(response: &Response) -> Result<Option<Url>> {
    for header in response.headers().get_all(LINK) {
        let header = header
            .to_str()
            .context("Invalid GitHub pagination header")?;
        for link in header.split(',') {
            let Some((target, attributes)) = link.trim().split_once('>') else {
                continue;
            };
            if !attributes
                .split(';')
                .any(|attr| attr.trim() == "rel=\"next\"")
            {
                continue;
            }
            let url = target
                .strip_prefix('<')
                .and_then(|target| Url::parse(target).ok())
                .context("Invalid GitHub pagination URL")?;
            return Ok(Some(url));
        }
    }
    Ok(None)
}

fn send(
    client: &Client,
    method: Method,
    url: &Url,
    token: &str,
    payload: Option<&Value>,
) -> Result<Response> {
    ensure!(
        url.scheme() == "https"
            && url.host_str() == Some("api.github.com")
            && url.port_or_known_default() == Some(443)
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none(),
        "Invalid GitHub API request URL"
    );
    let mut request = client
        .request(method, url.clone())
        .bearer_auth(token)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28");
    if let Some(payload) = payload {
        request = request.json(payload);
    }
    request.send().map_err(|err| {
        if err.is_timeout() {
            anyhow!("GitHub API request timed out")
        } else if err.is_connect() {
            anyhow!("Could not connect to GitHub API")
        } else {
            anyhow!("GitHub API request failed")
        }
    })
}

fn require_success(response: Response, operation: &str) -> Result<Response> {
    if !response.status().is_success() {
        bail!(
            "GitHub API could not {operation} (HTTP {})",
            response.status().as_u16()
        );
    }
    Ok(response)
}

fn read_json<T: DeserializeOwned>(response: Response) -> Result<T> {
    response.json().map_err(|err| {
        if err.is_timeout() {
            anyhow!("GitHub API response timed out")
        } else {
            anyhow!("Could not read GitHub API JSON response")
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_repository_paths() {
        assert_eq!(repository_path("owner/repo").unwrap(), "/repos/owner/repo");
        for repo in [
            "owner",
            "/repo",
            "owner/",
            "owner/../repo",
            "owner/..",
            "./repo",
            "owner/repo?foo",
            "owner/repo%2fextra",
            "owner/repo#fragment",
        ] {
            assert!(repository_path(repo).is_err(), "{repo}");
        }
    }

    #[test]
    fn private_fork_workflows_must_be_explicitly_disabled() {
        for (json, expected) in [
            (r#"{"run_workflows_from_fork_pull_requests":false}"#, true),
            (r#"{"run_workflows_from_fork_pull_requests":true}"#, false),
            (r#"{"run_workflows_from_fork_pull_requests":null}"#, false),
            ("{}", false),
        ] {
            let settings: PrivateForkSettings = serde_json::from_str(json).unwrap();
            assert_eq!(
                settings.run_workflows_from_fork_pull_requests == Some(false),
                expected
            );
        }
    }
}
