use crate::config::JenkinsConfig;
use crate::jobs::Job;
use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use std::collections::HashMap;
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

pub enum QueueState {
    Waiting,
    Started(String),
    Cancelled,
    /// 404: the item expired from the queue.
    Gone,
    /// Network error or unexpected response; try again later.
    Unknown,
}

/// How far a running build has got, for display in the pending view.
pub struct BuildProgress {
    /// Seconds since the build started.
    pub elapsed: u64,
    /// Jenkins' estimate of total build time in seconds, if known.
    pub estimated: Option<u64>,
    /// The build has a number and Jenkins calls it "building", but nothing is
    /// executing yet: its `node` block is waiting for a free executor. Timing
    /// it against the estimate would be misleading (the clock runs while no
    /// work happens), so callers show it as waiting instead.
    pub waiting: bool,
    /// Which pipeline stage it is on, when the stage-view API is available.
    pub stage: Option<Stage>,
}

/// Where a running pipeline is in its stages: `3/6 Build & Push Docker Image`.
/// Real progress, as opposed to elapsed-versus-estimate guesswork.
#[derive(Clone)]
pub struct Stage {
    pub name: String,
    /// Stages finished so far.
    pub done: usize,
    /// How many stages the pipeline has in total, or 0 when that isn't known.
    /// Jenkins' stage view only lists the stages a *running* build has reached
    /// so far, so this comes from the job's last finished build; without one
    /// (a brand-new job) the count is unknown and callers show the stage name
    /// without a fraction or a stage-based bar.
    pub total: usize,
}

impl Stage {
    /// Fraction of stages completed, for a progress bar.
    pub fn ratio(&self) -> f64 {
        if self.total == 0 {
            return 0.0;
        }
        (self.done as f64 / self.total as f64).clamp(0.0, 1.0)
    }
}

impl BuildProgress {
    /// A short human hint like `1m20s/3m00s (~1m40s left)` or `45s elapsed`.
    pub fn hint(&self) -> String {
        if self.waiting {
            return format!("waiting for an executor, {} in the queue", fmt_secs(self.elapsed));
        }
        let timing = match self.estimated {
            Some(est) if est > self.elapsed => format!(
                "{}/{} (~{} left)",
                fmt_secs(self.elapsed),
                fmt_secs(est),
                fmt_secs(est - self.elapsed)
            ),
            // Past the estimate the remaining time is unknown, so say that
            // rather than implying the build is late by a known amount.
            Some(est) => format!("{} (over the usual {})", fmt_secs(self.elapsed), fmt_secs(est)),
            None => format!("{} elapsed", fmt_secs(self.elapsed)),
        };
        match &self.stage {
            Some(st) => format!("stage {}/{} {} · {timing}", st.done + 1, st.total, st.name),
            None => timing,
        }
    }
}

/// `90` -> `1m30s`, `45` -> `45s`.
pub fn fmt_secs(s: u64) -> String {
    if s >= 60 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    }
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
            _ => bail!(
                "Jenkins returned HTTP {status} for {url}: {}",
                snippet(&body)
            ),
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
                bail!("HTTP 400: job is parameterized; add `?PARAM=value` to its path in jobs.toml")
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
        loop {
            match self.queue_state(queue_url) {
                QueueState::Started(url) => return Some(url),
                QueueState::Cancelled | QueueState::Gone => return None,
                QueueState::Waiting | QueueState::Unknown => {}
            }
            if Instant::now() >= deadline {
                return None;
            }
            sleep(Duration::from_secs(2));
        }
    }

    /// One look at a queue item.
    pub fn queue_state(&self, queue_url: &str) -> QueueState {
        let api = format!("{}/api/json", queue_url.trim_end_matches('/'));
        let Ok(mut resp) = self.get(&api) else {
            return QueueState::Unknown;
        };
        match resp.status().as_u16() {
            200 => {}
            // Jenkins drops queue items a few minutes after the build starts.
            404 => return QueueState::Gone,
            _ => return QueueState::Unknown,
        }
        let Some(v) = resp
            .body_mut()
            .read_to_string()
            .ok()
            .and_then(|b| serde_json::from_str::<serde_json::Value>(&b).ok())
        else {
            return QueueState::Unknown;
        };
        if let Some(url) = v["executable"]["url"].as_str() {
            return QueueState::Started(url.to_string());
        }
        if v["cancelled"].as_bool() == Some(true) {
            return QueueState::Cancelled;
        }
        QueueState::Waiting
    }

    /// Why a queued item hasn't started yet, as Jenkins words it ("Waiting for
    /// next available executor"). Display-only, so anything unexpected is
    /// `None`. An item that has already started has no reason.
    pub fn queue_why(&self, queue_url: &str) -> Option<String> {
        let api = format!("{}/api/json?tree=why", queue_url.trim_end_matches('/'));
        let mut resp = self.get(&api).ok()?;
        if resp.status().as_u16() != 200 {
            return None;
        }
        let body = resp.body_mut().read_to_string().ok()?;
        let v: serde_json::Value = serde_json::from_str(&body).ok()?;
        v["why"].as_str().map(str::to_string)
    }

    /// `None` while the build is running, else its result (SUCCESS, FAILURE, ABORTED, ...).
    pub fn build_result(&self, build_url: &str) -> Result<Option<String>> {
        let api = format!(
            "{}/api/json?tree=building,result",
            build_url.trim_end_matches('/')
        );
        let mut resp = self.get(&api)?;
        let status = resp.status().as_u16();
        let body = resp.body_mut().read_to_string().unwrap_or_default();
        if status != 200 {
            bail!("HTTP {status} for {api}: {}", snippet(&body));
        }
        let v: serde_json::Value = serde_json::from_str(&body).context("parsing build status")?;
        if v["building"].as_bool() == Some(true) {
            return Ok(None);
        }
        Ok(Some(v["result"].as_str().unwrap_or("UNKNOWN").to_string()))
    }

    /// Progress of a running build: how long it has run, Jenkins' estimate of
    /// how long it should take, whether it is actually executing, and which
    /// stage it is on. Used only for display, so any error degrades to `None`
    /// (or, for the stage view, to no stage information).
    pub fn build_progress(&self, build_url: &str) -> Option<BuildProgress> {
        let api = format!(
            "{}/api/json?tree=timestamp,estimatedDuration,building",
            build_url.trim_end_matches('/')
        );
        let mut resp = self.get(&api).ok()?;
        if resp.status().as_u16() != 200 {
            return None;
        }
        let body = resp.body_mut().read_to_string().ok()?;
        let v: serde_json::Value = serde_json::from_str(&body).ok()?;
        let start_ms = v["timestamp"].as_i64()?;
        let est_ms = v["estimatedDuration"].as_i64().unwrap_or(-1);
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_millis() as i64;
        let elapsed = ((now_ms - start_ms).max(0) / 1000) as u64;
        let estimated = if est_ms > 0 {
            Some((est_ms / 1000) as u64)
        } else {
            None
        };
        let building = v["building"].as_bool() == Some(true);
        // Ask the stage view where the pipeline actually is. A build can be
        // "building" while parked waiting for an executor, in which case no
        // stage has started yet.
        let (waiting, stage) = match self.stage_view(build_url) {
            Some(view) => {
                // The running build's own stage list only covers the stages it
                // has reached, so the pipeline's length comes from the last
                // finished build. Falling back to what this build has seen
                // would read as "stage 2/2" the moment the second one starts.
                let stage = view.current.map(|name| {
                    let total = self
                        .pipeline_stages(build_url)
                        .unwrap_or(0)
                        // A pipeline that grew since the reference build: never
                        // report fewer stages than this build has already run.
                        .max(view.seen);
                    Stage {
                        name,
                        done: view.done.min(total.saturating_sub(1)),
                        total,
                    }
                });
                (building && view.waiting, stage)
            }
            None => (false, None),
        };
        Some(BuildProgress {
            elapsed,
            estimated,
            waiting,
            stage,
        })
    }

    /// The pipeline stage view for a build (`/wfapi/describe`, from the
    /// Pipeline Stage View plugin). `None` when the plugin isn't installed or
    /// the response is unreadable — callers then fall back to time-only
    /// progress, exactly as before.
    fn stage_view(&self, build_url: &str) -> Option<StageView> {
        let url = format!("{}/wfapi/describe", build_url.trim_end_matches('/'));
        let mut resp = self.get(&url).ok()?;
        if resp.status().as_u16() != 200 {
            return None;
        }
        let body = resp.body_mut().read_to_string().ok()?;
        parse_stage_view(&body)
    }

    /// How many stages this job's pipeline has, from its last finished build
    /// (preferring a successful one, since a failed run can stop early).
    /// `None` when the job has no finished build or no stage view.
    ///
    /// Cached per job for the life of the process: a pipeline's shape barely
    /// changes, and the dashboard would otherwise re-ask every few seconds.
    fn pipeline_stages(&self, build_url: &str) -> Option<usize> {
        let job_url = job_url_of(build_url);
        if let Some(hit) = stage_count_cache().lock().ok()?.get(&job_url) {
            return *hit;
        }
        let mut found = None;
        for reference in ["lastSuccessfulBuild", "lastCompletedBuild"] {
            if let Some(view) = self.stage_view(&format!("{job_url}/{reference}"))
                && view.seen > 0
            {
                found = Some(view.seen);
                break;
            }
        }
        // A miss is cached too, so a job without a stage view isn't re-asked
        // on every poll.
        if let Ok(mut cache) = stage_count_cache().lock() {
            cache.insert(job_url, found);
        }
        found
    }

    pub fn console_text(&self, build_url: &str) -> Result<String> {
        let url = format!("{}/consoleText", build_url.trim_end_matches('/'));
        let mut resp = self.get(&url)?;
        let status = resp.status().as_u16();
        if status != 200 {
            bail!("HTTP {status} for {url}");
        }
        resp.body_mut()
            .with_config()
            .limit(64 * 1024 * 1024)
            .read_to_string()
            .with_context(|| format!("reading {url}"))
    }

    fn get(&self, url: &str) -> Result<Response<Body>> {
        self.agent
            .get(url)
            .header("Authorization", &self.auth)
            .call()
            .with_context(|| format!("GET {url}"))
    }
}

/// What the stage view says about a build.
struct StageView {
    /// Nothing has started executing yet: no stage has begun, or the current
    /// stage is itself queued waiting for an executor.
    waiting: bool,
    /// Stages finished.
    done: usize,
    /// Stages listed at all — for a running build, only those reached so far.
    seen: usize,
    /// Name of the stage currently in flight, if any.
    current: Option<String>,
}

/// Job URL → pipeline stage count (or `None` for "asked, not available").
fn stage_count_cache() -> &'static std::sync::Mutex<HashMap<String, Option<usize>>> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<HashMap<String, Option<usize>>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// `…/job/x/1423/` → `…/job/x`: the job a build belongs to.
fn job_url_of(build_url: &str) -> String {
    let trimmed = build_url.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(i) => trimmed[..i].to_string(),
        None => trimmed.to_string(),
    }
}

/// Stage statuses that mean the stage has finished, one way or another.
const FINISHED: [&str; 5] = ["SUCCESS", "FAILED", "UNSTABLE", "ABORTED", "NOT_BUILT"];

/// Parse `/wfapi/describe` into [`StageView`]. Kept separate from the HTTP call
/// so it can be tested against real payloads.
fn parse_stage_view(body: &str) -> Option<StageView> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let stages = v["stages"].as_array()?;
    let total = stages.len();
    let status = |s: &serde_json::Value| s["status"].as_str().unwrap_or("").to_string();
    let done = stages
        .iter()
        .filter(|s| FINISHED.contains(&status(s).as_str()))
        .count();
    let current = stages.iter().find(|s| !FINISHED.contains(&status(s).as_str()));

    // A run whose stages haven't started at all is parked: Jenkins gave it a
    // number (and the clock is running) but no executor has picked it up. The
    // stage view also reports a stage as QUEUED while it waits for a node.
    let running = v["status"].as_str() == Some("IN_PROGRESS");
    let nothing_started = total == 0 || done == 0 && current.is_none_or(|s| s["startTimeMillis"].as_i64().unwrap_or(0) == 0);
    let stage_queued = current.is_some_and(|s| status(s) == "QUEUED");
    let waiting = running && (nothing_started || stage_queued);

    Some(StageView {
        waiting,
        done,
        seen: total,
        current: current.and_then(|s| s["name"].as_str().map(str::to_string)),
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Field names and status values here match a real `/wfapi/describe`
    /// response from the stage Jenkins (WorkflowRun, Stage View plugin).
    fn describe(status: &str, stages: &str) -> String {
        format!(r##"{{"id":"1803","name":"#1803","status":"{status}","startTimeMillis":1792214419184,"durationMillis":71039,"queueDurationMillis":73,"stages":[{stages}]}}"##)
    }

    fn stage(name: &str, status: &str, start: i64) -> String {
        format!(
            r#"{{"id":"6","name":"{name}","execNode":"","status":"{status}","startTimeMillis":{start},"durationMillis":22834,"pauseDurationMillis":0}}"#
        )
    }

    #[test]
    fn a_run_whose_stages_have_not_started_is_waiting() {
        // Jenkins has given the run a number and calls it IN_PROGRESS, but no
        // stage has begun: it is parked waiting for an executor.
        let body = describe("IN_PROGRESS", "");
        let view = parse_stage_view(&body).expect("parsed");
        assert!(view.waiting, "a run with no stages yet must count as waiting");
        assert_eq!(view.seen, 0);
        assert!(view.current.is_none());
    }

    #[test]
    fn a_stage_queued_for_a_node_is_waiting() {
        let body = describe("IN_PROGRESS", &stage("Build & Analysis", "QUEUED", 0));
        let view = parse_stage_view(&body).expect("parsed");
        assert!(view.waiting, "a QUEUED stage means no executor yet");
    }

    #[test]
    fn a_running_stage_reports_progress_not_waiting() {
        let stages = [
            stage("Declarative: Checkout SCM", "SUCCESS", 1792214421958),
            stage("Cloning App Git Repo", "SUCCESS", 1792214444792),
            stage("Build & Analysis", "IN_PROGRESS", 1792214460000),
            stage("Build & Push Docker Image", "NOT_EXECUTED", 0),
        ]
        .join(",");
        let view = parse_stage_view(&describe("IN_PROGRESS", &stages)).expect("parsed");
        assert!(!view.waiting, "a started stage means it is really building");
        assert_eq!(view.current.as_deref(), Some("Build & Analysis"));
        assert_eq!((view.done, view.seen), (2, 4));
    }

    #[test]
    fn a_finished_run_is_not_waiting() {
        // The real failed build from the stage Jenkins: every stage finished.
        let stages = [
            stage("Declarative: Checkout SCM", "SUCCESS", 1792214421958),
            stage("Build & Analysis", "FAILED", 1792214444792),
        ]
        .join(",");
        let view = parse_stage_view(&describe("FAILED", &stages)).expect("parsed");
        assert!(!view.waiting);
        assert_eq!(view.seen, 2, "a finished run lists every stage");
        assert!(view.current.is_none(), "nothing is current once all stages ended");
    }

    #[test]
    fn a_running_build_lists_only_the_stages_it_has_reached() {
        // This is what the stage Jenkins actually returns for a run in flight:
        // three of the pipeline's six stages. Taking the total from this list
        // would read as "2 of 3 done" (67%) when it is really 2 of 6 (33%), so
        // the total has to come from a finished build instead.
        let running = [
            stage("Declarative: Checkout SCM", "SUCCESS", 1792214421958),
            stage("Cloning App Git Repo", "SUCCESS", 1792214444792),
            stage("Build & Analysis", "IN_PROGRESS", 1792214460000),
        ]
        .join(",");
        let view = parse_stage_view(&describe("IN_PROGRESS", &running)).expect("parsed");
        assert_eq!((view.done, view.seen), (2, 3));

        // The reference (finished) build of the same job lists all six.
        let finished = (1..=6)
            .map(|i| stage(&format!("Stage {i}"), "SUCCESS", 1792214421958))
            .collect::<Vec<_>>()
            .join(",");
        let reference = parse_stage_view(&describe("SUCCESS", &finished)).expect("parsed");
        assert_eq!(reference.seen, 6);

        // Combined the way `build_progress` does it: 2 of 6, not 2 of 3.
        let st = Stage {
            name: view.current.unwrap(),
            done: view.done,
            total: reference.seen,
        };
        assert_eq!((st.done, st.total), (2, 6));
        assert!(
            (st.ratio() - 1.0 / 3.0).abs() < 1e-9,
            "ratio should be 33%, was {}",
            st.ratio()
        );
    }

    #[test]
    fn a_grown_pipeline_never_reports_fewer_stages_than_it_has_run() {
        // The reference (finished) build had 4 stages; a stage has since been
        // added, so this run has already reached 5. The total must follow the
        // running build rather than showing "5/4".
        let reference_total = 4;
        let running = (1..=5)
            .map(|i| {
                let status = if i < 5 { "SUCCESS" } else { "IN_PROGRESS" };
                stage(&format!("Stage {i}"), status, 1792214421958)
            })
            .collect::<Vec<_>>()
            .join(",");
        let view = parse_stage_view(&describe("IN_PROGRESS", &running)).expect("parsed");
        let total = reference_total.max(view.seen);
        let st = Stage {
            name: view.current.unwrap(),
            done: view.done.min(total - 1),
            total,
        };
        assert_eq!((st.done, st.total), (4, 5));
        assert!(st.ratio() <= 1.0, "ratio must stay within the bar");
    }

    #[test]
    fn job_url_drops_the_build_number() {
        assert_eq!(
            job_url_of("https://jenkins.example/job/stage/job/x/1423/"),
            "https://jenkins.example/job/stage/job/x"
        );
        assert_eq!(
            job_url_of("https://jenkins.example/job/x/7"),
            "https://jenkins.example/job/x"
        );
    }

    #[test]
    fn hints_describe_each_state() {
        let waiting = BuildProgress {
            elapsed: 600,
            estimated: Some(190),
            waiting: true,
            stage: None,
        };
        assert_eq!(
            waiting.hint(),
            "waiting for an executor, 10m00s in the queue",
            "a parked build must not be timed against the estimate"
        );

        let running = BuildProgress {
            elapsed: 110,
            estimated: Some(190),
            waiting: false,
            stage: Some(Stage {
                name: "Build & Analysis".to_string(),
                done: 2,
                total: 4,
            }),
        };
        assert_eq!(
            running.hint(),
            "stage 3/4 Build & Analysis · 1m50s/3m10s (~1m20s left)"
        );

        let over = BuildProgress {
            elapsed: 600,
            estimated: Some(190),
            waiting: false,
            stage: None,
        };
        assert_eq!(over.hint(), "10m00s (over the usual 3m10s)");
    }

    #[test]
    fn stage_ratio_handles_an_empty_pipeline() {
        let st = Stage { name: "x".into(), done: 0, total: 0 };
        assert_eq!(st.ratio(), 0.0);
    }
}
