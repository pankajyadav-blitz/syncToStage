use crate::config::JenkinsConfig;
use crate::jobs::Job;
use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use std::thread::sleep;
use std::time::{Duration, Instant};
use ureq::http::Response;
use ureq::{Agent, Body};

pub struct Jenkins {
    cfg: JenkinsConfig,
    agent: Agent,
    auth: String,
}

/// A build request Jenkins accepted.
pub struct Queued {
    pub job_url: String,
    /// `.../queue/item/<n>/`, from the Location header.
    pub queue_url: Option<String>,
}

impl Jenkins {
    pub fn new(cfg: JenkinsConfig) -> Jenkins {
        let agent: Agent = Agent::config_builder()
            .http_status_as_error(false)
            // A redirect (e.g. http -> https) would turn the POST into a GET that
            // "succeeds" without building anything, so treat it as an error instead.
            .max_redirects(0)
            .timeout_global(Some(Duration::from_secs(30)))
            .build()
            .into();
        let auth = format!(
            "Basic {}",
            STANDARD.encode(format!("{}:{}", cfg.user, cfg.token))
        );
        Jenkins { cfg, agent, auth }
    }

    pub fn url(&self) -> &str {
        &self.cfg.url
    }

    /// Verifies the URL and credentials; returns the Jenkins user id.
    pub fn whoami(&self) -> Result<String> {
        let url = format!("{}/me/api/json", self.cfg.url);
        let mut resp = self.get(&url)?;
        let status = resp.status().as_u16();
        let body = resp.body_mut().read_to_string().unwrap_or_default();
        match status {
            200 => {
                let v: serde_json::Value =
                    serde_json::from_str(&body).context("unexpected /me/api/json response")?;
                Ok(v["id"].as_str().unwrap_or("?").to_string())
            }
            401 => bail!("Jenkins rejected the credentials (401). Check user and API token."),
            403 => bail!(
                "Jenkins returned 403 for {url}: the user/token is wrong or lacks Overall/Read."
            ),
            300..=399 => bail!(
                "Jenkins redirected {url} (HTTP {status}). Use the final URL (https?) as `url`."
            ),
            _ => bail!("Jenkins returned HTTP {status} for {url}: {}", snippet(&body)),
        }
    }

    pub fn trigger(&self, job: &Job) -> Result<Queued> {
        let url = format!("{}{}", self.cfg.url, job.endpoint);
        let mut req = self.agent.post(&url).header("Authorization", &self.auth);
        if let Some(crumb) = &self.cfg.crumb {
            req = req.header("Jenkins-Crumb", crumb);
        }
        let mut resp = if job.params.is_empty() {
            req.send_empty()
        } else {
            req.send_form(job.params.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        }
        .with_context(|| format!("POST {url}"))?;

        let status = resp.status().as_u16();
        let location = header(&resp, "location");
        let body = resp.body_mut().read_to_string().unwrap_or_default();
        let job_url = format!("{}{}", self.cfg.url, job.job_path());

        match status {
            200 | 201 => Ok(Queued {
                job_url,
                queue_url: location.filter(|l| l.contains("/queue/item/")),
            }),
            300..=399 => bail!(
                "HTTP {status} redirect to {} - check `url` (http vs https, trailing path)",
                location.unwrap_or_default()
            ),
            400 if body.contains("is parameterized") || body.contains("Nothing is submitted") => {
                bail!(
                    "HTTP 400: job is parameterized; add `?PARAM=value` to its path in jobs.toml"
                )
            }
            401 => bail!("HTTP 401: Jenkins rejected the credentials"),
            403 if body.contains("crumb") => bail!(
                "HTTP 403: Jenkins wants a CSRF crumb. Set `crumb` in the config (as the Lambda's JENKINS_CRUMB)"
            ),
            403 => bail!("HTTP 403: user lacks Job/Build permission on {job_url}"),
            404 => bail!("HTTP 404: job not found at {job_url}"),
            _ => bail!("HTTP {status}: {}", snippet(&body)),
        }
    }

    /// Polls the queue item until Jenkins assigns a build, up to `deadline`.
    /// Returns the build URL, or None if it is still queued (or the queue is unreadable).
    pub fn wait_for_build(&self, queue_url: &str, deadline: Instant) -> Option<String> {
        let api = format!("{}/api/json", queue_url.trim_end_matches('/'));
        loop {
            if let Ok(mut resp) = self.get(&api)
                && resp.status().as_u16() == 200
                && let Ok(body) = resp.body_mut().read_to_string()
                && let Ok(v) = serde_json::from_str::<serde_json::Value>(&body)
            {
                if let Some(url) = v["executable"]["url"].as_str() {
                    return Some(url.to_string());
                }
                if v["cancelled"].as_bool() == Some(true) {
                    return None;
                }
            }
            if Instant::now() >= deadline {
                return None;
            }
            sleep(Duration::from_secs(2));
        }
    }

    fn get(&self, url: &str) -> Result<Response<Body>> {
        self.agent
            .get(url)
            .header("Authorization", &self.auth)
            .call()
            .with_context(|| format!("GET {url}"))
    }
}

fn header(resp: &Response<Body>, name: &str) -> Option<String> {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

fn snippet(body: &str) -> String {
    let text = body.split_whitespace().collect::<Vec<_>>().join(" ");
    text.chars().take(300).collect()
}
