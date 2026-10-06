//! Deploy-after-build: `sts -s` / `sts -b` record each triggered Jenkins build in a
//! pending queue and install a crontab entry that runs `sts --watch` every minute.
//! Each `--watch` run polls every 10 seconds for that minute (cron can't fire more
//! often than once a minute). Each tick checks the builds; when one succeeds it
//! hard-refreshes and syncs the ArgoCD app(s) for the chart(s) the build updated.
//! The crontab entry removes itself once the queue is empty.

use crate::argocd::{self, App, Argo, SyncOutcome};
use crate::config;
use crate::jenkins::{Jenkins, QueueState};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const CRON_MARK: &str = "# sts-watch";
/// Give up on a build that hasn't finished (or a sync that keeps failing) after this long.
/// This also bounds how long the cron watcher stays alive: once every build is older
/// than this it is dropped, the queue empties, and the crontab entry removes itself.
const MAX_AGE_SECS: u64 = 30 * 60;
const MAX_SYNC_ATTEMPTS: u32 = 3;
/// How often the watcher polls. cron's floor is 1 minute, so a single `--watch`
/// run loops internally, firing a tick every `WATCH_INTERVAL_SECS` for one minute.
const WATCH_INTERVAL_SECS: u64 = 10;
/// How often `sts -S` refreshes the live sync view on a terminal.
const SYNC_POLL_SECS: u64 = 5;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pending {
    pub repo: String,
    /// Job path as written in jobs.toml.
    pub job: String,
    pub job_url: String,
    pub queue_url: Option<String>,
    pub build_url: Option<String>,
    pub queued_at: u64,
    #[serde(default)]
    pub attempts: u32,
}

impl Pending {
    pub fn new(
        repo: &str,
        job: &str,
        job_url: &str,
        queue_url: Option<String>,
        build_url: Option<String>,
    ) -> Pending {
        Pending {
            repo: repo.to_string(),
            job: job.to_string(),
            job_url: job_url.to_string(),
            queue_url,
            build_url,
            queued_at: now(),
            attempts: 0,
        }
    }

    /// Last path segment of the job, e.g. `soochi-dash-app`; the usual chart name.
    fn job_name(&self) -> String {
        let p = self.job.split('?').next().unwrap_or("");
        let p = p.trim_end_matches('/');
        let p = p
            .strip_suffix("/buildWithParameters")
            .or_else(|| p.strip_suffix("/build"))
            .unwrap_or(p);
        p.rsplit('/').next().unwrap_or("").to_string()
    }

    fn label(&self) -> String {
        self.build_url
            .clone()
            .unwrap_or_else(|| self.job_url.clone())
    }
}

/// The pending queue, held under an exclusive file lock for as long as it lives.
struct Queue {
    _lock: File,
    items: Vec<Pending>,
}

fn queue_file() -> PathBuf {
    config::state_dir().join("pending.json")
}

pub fn log_file() -> PathBuf {
    config::state_dir().join("watch.log")
}

impl Queue {
    /// Blocks until the lock is free. `None` from `try_open` means another tick holds it.
    fn open() -> Result<Queue> {
        let lock = Self::lock_file()?;
        lock.lock().context("locking the deploy queue")?;
        Self::read(lock)
    }

    fn try_open() -> Result<Option<Queue>> {
        let lock = Self::lock_file()?;
        match lock.try_lock() {
            Ok(()) => Ok(Some(Self::read(lock)?)),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(e)) => Err(e).context("locking the deploy queue"),
        }
    }

    fn lock_file() -> Result<File> {
        let dir = config::state_dir();
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = dir.join("pending.lock");
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))
    }

    fn read(lock: File) -> Result<Queue> {
        let path = queue_file();
        let items = match fs::read_to_string(&path) {
            Ok(text) if !text.trim().is_empty() => serde_json::from_str(&text)
                .with_context(|| format!("parsing {}", path.display()))?,
            _ => Vec::new(),
        };
        Ok(Queue { _lock: lock, items })
    }

    fn save(&self) -> Result<()> {
        let path = queue_file();
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_string_pretty(&self.items)?)
            .with_context(|| format!("writing {}", tmp.display()))?;
        fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    /// Cheap, lock-free check used by the watch loop to stop early once the
    /// queue has drained. A missing or empty file counts as empty.
    fn is_empty_now() -> bool {
        match fs::read_to_string(queue_file()) {
            Ok(text) => match serde_json::from_str::<Vec<Pending>>(&text) {
                Ok(items) => items.is_empty(),
                Err(_) => text.trim().is_empty(),
            },
            Err(_) => true,
        }
    }
}

/// An ArgoCD app we told to sync, tracked by `sts -S` until it settles
/// (Synced + Healthy). Only syncs this tool triggered are recorded here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Syncing {
    pub app: String,
    /// The build whose success triggered this sync, for display.
    pub build_url: Option<String>,
    pub repo: String,
    pub started_at: u64,
}

/// The syncing list: ArgoCD apps `sts --watch` has synced, held under an
/// exclusive file lock while open. Pruned by `sts -S` once each app settles.
struct SyncQueue {
    _lock: File,
    items: Vec<Syncing>,
}

fn syncing_file() -> PathBuf {
    config::state_dir().join("syncing.json")
}

impl SyncQueue {
    fn open() -> Result<SyncQueue> {
        let lock = Self::lock_file()?;
        lock.lock().context("locking the syncing list")?;
        let items = match fs::read_to_string(syncing_file()) {
            Ok(text) if !text.trim().is_empty() => serde_json::from_str(&text)
                .with_context(|| format!("parsing {}", syncing_file().display()))?,
            _ => Vec::new(),
        };
        Ok(SyncQueue { _lock: lock, items })
    }

    fn lock_file() -> Result<File> {
        let dir = config::state_dir();
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = dir.join("syncing.lock");
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))
    }

    fn save(&self) -> Result<()> {
        let path = syncing_file();
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_string_pretty(&self.items)?)
            .with_context(|| format!("writing {}", tmp.display()))?;
        fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }
}

/// Records apps the watcher just synced so `sts -S` can track them. A repeat
/// sync of an app already listed just refreshes its entry.
fn record_syncing(entries: Vec<Syncing>) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    let mut q = SyncQueue::open()?;
    for e in entries {
        q.items.retain(|s| s.app != e.app);
        q.items.push(e);
    }
    q.save()
}

/// Adds builds to the queue and makes sure the cron watcher is installed.
pub fn enqueue(entries: Vec<Pending>) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    let mut q = Queue::open()?;
    for e in entries {
        // A re-trigger of the same build replaces the old entry.
        q.items
            .retain(|p| !(p.job == e.job && p.build_url.is_some() && p.build_url == e.build_url));
        q.items.push(e);
    }
    q.save()?;
    install_cron()
}

/// `sts --pending`
pub fn show() -> Result<()> {
    let q = Queue::open()?;
    let cron = cron_installed().unwrap_or(false);
    println!("queue:   {}", queue_file().display());
    println!("log:     {}", log_file().display());
    println!(
        "cron:    {}",
        if cron { "installed" } else { "not installed" }
    );
    if q.items.is_empty() {
        println!("No builds waiting to be deployed.");
        return Ok(());
    }
    let t = now();
    for p in &q.items {
        let state = if p.build_url.is_some() {
            "building"
        } else {
            "queued"
        };
        println!(
            "  {state:<9} {}  ({}m ago, {})",
            p.label(),
            t.saturating_sub(p.queued_at) / 60,
            p.repo
        );
    }
    Ok(())
}

/// `sts --sync-status`: show the ArgoCD apps we synced and their live sync/health,
/// dropping each once it reaches Synced + Healthy (or vanishes). On a terminal it
/// refreshes in place every few seconds until the list is empty; piped or
/// redirected it prints a single snapshot and exits.
pub fn sync_status() -> Result<()> {
    let live = std::io::stdout().is_terminal();
    loop {
        if live {
            clear_screen();
            println!("sts -S  —  {}  (Ctrl-C to stop)", utc_now());
        }
        let empty = sync_status_once()?;
        if empty || !live {
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs(SYNC_POLL_SECS));
    }
}

/// One pass: poll ArgoCD for each tracked app, print the status, prune settled
/// ones, and save. Returns true when nothing is left to track. The queue lock is
/// released as soon as this returns, so the cron watcher can enqueue new syncs
/// between polls.
fn sync_status_once() -> Result<bool> {
    let mut q = SyncQueue::open()?;
    println!("tracking: {}", syncing_file().display());
    if q.items.is_empty() {
        println!("No ArgoCD syncs in progress.");
        return Ok(true);
    }

    let Some(argo_cfg) = config::load_argo()? else {
        println!(
            "no ArgoCD token configured (argocd_token in config.toml); can't check sync status"
        );
        // Can't make progress without a token; treat as done so we don't spin.
        return Ok(true);
    };
    let argo = Argo::new(argo_cfg);

    let t = now();
    let mut keep = Vec::new();
    let mut done = Vec::new();
    for s in std::mem::take(&mut q.items) {
        // Stop tracking anything that never settles so the list can't grow forever.
        if t.saturating_sub(s.started_at) > MAX_AGE_SECS {
            done.push(format!(
                "  gave up   {}  (still not settled after {}m)",
                s.app,
                MAX_AGE_SECS / 60
            ));
            continue;
        }
        match argo.status(&s.app) {
            Ok(None) => done.push(format!("  gone      {}  (no such ArgoCD app)", s.app)),
            Ok(Some(st)) if st.is_settled() => {
                done.push(format!("  synced    {}  ({})", s.app, st.health))
            }
            Ok(Some(st)) => {
                let phase = if st.phase.is_empty() {
                    String::new()
                } else {
                    format!(", {}", st.phase)
                };
                println!(
                    "  syncing   {:<28} {}/{}{phase}  ({}m ago, {})",
                    s.app,
                    st.sync,
                    st.health,
                    t.saturating_sub(s.started_at) / 60,
                    s.repo,
                );
                keep.push(s);
            }
            Err(e) => {
                // Keep it; a transient error shouldn't lose the entry.
                println!("  ?         {}  (status check failed: {e:#})", s.app);
                keep.push(s);
            }
        }
    }

    q.items = keep;
    q.save()?;

    if !done.is_empty() {
        for line in &done {
            println!("{line}");
        }
    }
    if q.items.is_empty() {
        println!("All tracked ArgoCD syncs have settled.");
        return Ok(true);
    }
    Ok(false)
}

/// What cron runs: polls every `WATCH_INTERVAL_SECS` for one minute by looping
/// `tick()` internally, since a crontab can't fire more than once a minute.
/// Stops early (and lets the entry remove itself) once the queue is empty.
pub fn watch_loop() -> Result<()> {
    let runs = (60 / WATCH_INTERVAL_SECS).max(1);
    for i in 0..runs {
        tick()?;
        if Queue::is_empty_now() {
            break;
        }
        if i + 1 < runs {
            std::thread::sleep(Duration::from_secs(WATCH_INTERVAL_SECS));
        }
    }
    Ok(())
}

/// One watcher tick.
pub fn tick() -> Result<()> {
    let Some(mut q) = Queue::try_open()? else {
        return Ok(()); // previous tick still running
    };
    if q.items.is_empty() {
        return remove_cron();
    }

    let jenkins = Jenkins::new(config::load()?);
    let Some(argo_cfg) = config::load_argo()? else {
        log("no ArgoCD token configured (argocd_token in config.toml); dropping the deploy queue");
        q.items.clear();
        q.save()?;
        return remove_cron();
    };
    let argo = Argo::new(argo_cfg);
    let mut apps: Option<Vec<App>> = None;

    let mut keep = Vec::new();
    for mut p in std::mem::take(&mut q.items) {
        if now().saturating_sub(p.queued_at) > MAX_AGE_SECS {
            log(&format!(
                "{}: gave up after {}m, not deploying",
                p.label(),
                MAX_AGE_SECS / 60
            ));
            continue;
        }

        if p.build_url.is_none() {
            let Some(queue_url) = p.queue_url.clone() else {
                continue;
            };
            match jenkins.queue_state(&queue_url) {
                QueueState::Started(url) => {
                    log(&format!("{}: build started {url}", p.job_url));
                    p.build_url = Some(url);
                }
                QueueState::Cancelled => {
                    log(&format!(
                        "{}: cancelled in the Jenkins queue, not deploying",
                        p.job_url
                    ));
                    continue;
                }
                QueueState::Gone => {
                    log(&format!(
                        "{}: queue item {queue_url} expired before its build was seen; not deploying",
                        p.job_url
                    ));
                    continue;
                }
                QueueState::Waiting | QueueState::Unknown => {
                    keep.push(p);
                    continue;
                }
            }
        }

        let build_url = p.build_url.clone().unwrap_or_default();
        match jenkins.build_result(&build_url) {
            Ok(None) => keep.push(p), // still running
            Ok(Some(r)) if r == "SUCCESS" => {
                if apps.is_none() {
                    match argo.apps() {
                        Ok(a) => apps = Some(a),
                        Err(e) => {
                            log(&format!("listing ArgoCD apps failed: {e:#}"));
                            keep.push(p);
                            continue;
                        }
                    }
                }
                match deploy(&jenkins, &argo, apps.as_deref().unwrap_or_default(), &p) {
                    Ok(()) => {}
                    Err(e) => {
                        p.attempts += 1;
                        if p.attempts < MAX_SYNC_ATTEMPTS {
                            log(&format!(
                                "{build_url}: deploy failed (attempt {}), retrying next tick: {e:#}",
                                p.attempts
                            ));
                            keep.push(p);
                        } else {
                            log(&format!(
                                "{build_url}: deploy failed {} times, giving up: {e:#}",
                                p.attempts
                            ));
                        }
                    }
                }
            }
            Ok(Some(r)) => log(&format!("{build_url}: build finished {r}, not deploying")),
            Err(e) => {
                log(&format!("{build_url}: status check failed: {e:#}"));
                keep.push(p);
            }
        }
    }

    q.items = keep;
    q.save()?;
    if q.items.is_empty() {
        remove_cron()?;
    }
    Ok(())
}

fn deploy(jenkins: &Jenkins, argo: &Argo, apps: &[App], p: &Pending) -> Result<()> {
    let build_url = p.build_url.as_deref().unwrap_or_default();
    let mut charts = match jenkins.console_text(build_url) {
        Ok(text) => argocd::charts_in_console(&text),
        Err(e) => {
            log(&format!(
                "{build_url}: reading console failed ({e:#}); guessing chart from job name"
            ));
            Vec::new()
        }
    };
    if charts.is_empty() {
        charts.push(p.job_name());
    }

    let mut targets: Vec<&str> = Vec::new();
    for chart in &charts {
        match argocd::app_for_chart(apps, chart) {
            Some(app) if !targets.contains(&app.name.as_str()) => targets.push(&app.name),
            Some(_) => {}
            None => log(&format!(
                "{build_url}: no ArgoCD app deploys chart `{chart}`; skipped"
            )),
        }
    }
    if targets.is_empty() {
        return Ok(()); // nothing to sync; not worth retrying
    }

    let mut failed = Vec::new();
    let mut synced = Vec::new();
    for app in targets {
        match argo.refresh_and_sync(app) {
            Ok(SyncOutcome::Started) => {
                log(&format!(
                    "{build_url}: SUCCESS -> ArgoCD sync started for {app} ({}/applications/{app})",
                    argo.url()
                ));
                synced.push(app.to_string());
            }
            Ok(SyncOutcome::AlreadyRunning) => {
                log(&format!(
                    "{build_url}: SUCCESS -> ArgoCD sync already running for {app}"
                ));
                synced.push(app.to_string());
            }
            Err(e) => failed.push(format!("{e:#}")),
        }
    }

    // Track every app we synced (or that was already syncing) so `sts -S` can
    // show it until ArgoCD reports it Synced + Healthy.
    if !synced.is_empty() {
        let now = now();
        let entries = synced
            .into_iter()
            .map(|app| Syncing {
                app,
                build_url: p.build_url.clone(),
                repo: p.repo.clone(),
                started_at: now,
            })
            .collect();
        if let Err(e) = record_syncing(entries) {
            log(&format!("{build_url}: could not record sync status: {e:#}"));
        }
    }

    if !failed.is_empty() {
        bail!("{}", failed.join("; "));
    }
    Ok(())
}

fn log(msg: &str) {
    println!("{} {msg}", utc_now());
}

/// Clear the terminal and move the cursor home, so the live `sts -S` view
/// redraws in place instead of scrolling.
fn clear_screen() {
    print!("\x1b[2J\x1b[H");
    let _ = std::io::stdout().flush();
}

// ---- crontab ----------------------------------------------------------------

fn cron_line() -> Result<String> {
    let exe = std::env::current_exe().context("locating the sts binary")?;
    let mut env = String::new();
    for var in ["XDG_CONFIG_HOME", "XDG_STATE_HOME"] {
        if let Ok(v) = std::env::var(var)
            && !v.is_empty()
        {
            env.push_str(&format!("{var}={} ", sh_quote(&v)));
        }
    }
    Ok(format!(
        "* * * * * {env}{} --watch >> {} 2>&1 {CRON_MARK}",
        sh_quote(&exe.to_string_lossy()),
        sh_quote(&log_file().to_string_lossy())
    ))
}

fn read_crontab() -> Result<String> {
    let out = Command::new("crontab")
        .arg("-l")
        .output()
        .context("running `crontab -l` (is cron installed?)")?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    let err = String::from_utf8_lossy(&out.stderr);
    if err.contains("no crontab") {
        return Ok(String::new());
    }
    bail!("`crontab -l` failed: {}", err.trim());
}

fn write_crontab(text: &str) -> Result<()> {
    let mut child = Command::new("crontab")
        .arg("-")
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("running `crontab -`")?;
    child.stdin.take().unwrap().write_all(text.as_bytes())?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!(
            "`crontab -` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn cron_installed() -> Result<bool> {
    Ok(read_crontab()?.lines().any(|l| l.contains(CRON_MARK)))
}

fn install_cron() -> Result<()> {
    let want = cron_line()?;
    let current = read_crontab()?;
    if current.lines().any(|l| l == want) {
        return Ok(());
    }
    let mut lines: Vec<&str> = current.lines().filter(|l| !l.contains(CRON_MARK)).collect();
    lines.push(&want);
    write_crontab(&(lines.join("\n") + "\n"))
}

fn remove_cron() -> Result<()> {
    let current = read_crontab()?;
    if !current.lines().any(|l| l.contains(CRON_MARK)) {
        return Ok(());
    }
    let rest: Vec<&str> = current.lines().filter(|l| !l.contains(CRON_MARK)).collect();
    let text = if rest.is_empty() {
        String::new()
    } else {
        rest.join("\n") + "\n"
    };
    write_crontab(&text)
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

// ---- time -------------------------------------------------------------------

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `2026-10-05T17:21:00Z`, without pulling in a date crate.
fn utc_now() -> String {
    let t = now();
    let (days, secs) = ((t / 86400) as i64, t % 86400);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_name_strips_build_suffix_and_params() {
        let p = Pending::new(
            "r",
            "/job/stage/job/web/job/dashboard/build?ENV=stage",
            "",
            None,
            None,
        );
        assert_eq!(p.job_name(), "dashboard");
        let p = Pending::new(
            "r",
            "/job/stage/job/orders/job/batua/job/batua-app/build",
            "",
            None,
            None,
        );
        assert_eq!(p.job_name(), "batua-app");
    }

    #[test]
    fn quotes_for_shell() {
        assert_eq!(sh_quote("/a b/it's"), r"'/a b/it'\''s'");
    }
}
