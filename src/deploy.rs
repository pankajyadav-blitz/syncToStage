//! Deploy-after-build: `sts -s` / `sts -b` record each triggered Jenkins build in a
//! pending queue and install a crontab entry that runs `sts --watch` every minute.
//! Each `--watch` run polls every 10 seconds for that minute (cron can't fire more
//! often than once a minute). Each tick checks the builds; when one succeeds it
//! hard-refreshes and syncs the ArgoCD app(s) for the chart(s) the build updated.
//! The crontab entry removes itself once the queue is empty.

use crate::argocd::{self, App, Argo, SyncOutcome};
use crate::config;
use crate::history;
use crate::jenkins::{Jenkins, QueueState};
use crate::notify;
use crate::style;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::IsTerminal;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Set while a full-screen TUI owns the terminal (e.g. `sts --follow`). While it
/// is active, `log()` must not `println!` — that would scribble over the ratatui
/// frame. Instead log lines are appended to `watch.log`, which is where cron's
/// output goes anyway, so nothing is lost.
static TUI_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Mark the TUI as active or not. Called by the TUI runner around its event loop
/// so inline watcher ticks (which call `log()`) don't corrupt the screen.
pub fn set_tui_active(active: bool) {
    TUI_ACTIVE.store(active, Ordering::SeqCst);
}

const CRON_MARK: &str = "# sts-watch";
/// Give up on a build that hasn't finished (or a sync that keeps failing) after this long.
/// This also bounds how long the cron watcher stays alive: once every build is older
/// than this it is dropped, the queue empties, and the crontab entry removes itself.
const MAX_AGE_SECS: u64 = 30 * 60;
const MAX_SYNC_ATTEMPTS: u32 = 3;
/// How often the watcher polls. cron's floor is 1 minute, so a single `--watch`
/// run loops internally, firing a tick every `WATCH_INTERVAL_SECS` for one minute.
const WATCH_INTERVAL_SECS: u64 = 10;
/// How often `sts --follow` advances and redraws the pipeline (piped path; the
/// TUI uses its own cadence). The live `-P`/`-S`/dashboard views also refresh
/// on the TUI's internal timer.
const FOLLOW_POLL_SECS: u64 = 5;
/// How often `sts -l` polls ArgoCD for new container log lines.
const LOGS_POLL_SECS: u64 = 2;
/// How many trailing lines `sts -l` asks for on its first poll (and each poll;
/// we dedupe by timestamp so a generous tail just guards against missing lines
/// between polls).
const LOGS_TAIL: u32 = 100;
/// Trim `watch.log` once it grows past this, keeping the most recent lines.
const LOG_MAX_BYTES: u64 = 1 << 20; // 1 MiB
const LOG_KEEP_LINES: usize = 2000;

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
        crate::jobs::Job::parse(&self.job).name()
    }

    fn label(&self) -> String {
        self.build_url
            .clone()
            .unwrap_or_else(|| self.job_url.clone())
    }
}

/// Jenkins build number out of a build URL (`…/job/x/1423/` → `1423`). None for
/// anything that doesn't end in a numeric path segment.
pub fn build_number(build_url: &str) -> Option<u64> {
    build_url
        .trim_end_matches('/')
        .rsplit('/')
        .next()?
        .parse()
        .ok()
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
        save_items(&queue_file(), &self.items)
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
        save_items(&syncing_file(), &self.items)
    }

    /// Lock-free check: is the syncing list empty? Missing/empty file counts as yes.
    fn is_empty_now() -> bool {
        match fs::read_to_string(syncing_file()) {
            Ok(text) => match serde_json::from_str::<Vec<Syncing>>(&text) {
                Ok(items) => items.is_empty(),
                Err(_) => text.trim().is_empty(),
            },
            Err(_) => true,
        }
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

/// A build row for the dashboard: one entry in the pending queue, resolved to
/// a display state. Produced by `collect_builds` and rendered either as a plain
/// snapshot (piped) or in the ratatui TUI (on a terminal).
pub struct BuildRow {
    /// What the build is doing: queued, waiting for an executor, or building.
    pub state: BuildState,
    /// Job name, e.g. `soochi-dash-app` — the row's identity.
    pub job: String,
    /// Jenkins build number, once the build has left the queue.
    pub build: Option<u64>,
    /// Build/job URL (the clickable thing).
    pub label: String,
    pub repo: String,
    pub age_secs: u64,
    /// Seconds the build has been running (from Jenkins), if building; the TUI
    /// adds the since-poll offset so it ticks every second between polls.
    pub elapsed: Option<u64>,
    /// Jenkins' estimate of total build time in seconds, if known.
    pub estimated: Option<u64>,
    /// Which pipeline stage it is on, when Jenkins' stage view knows.
    pub stage: Option<crate::jenkins::Stage>,
    /// Why it hasn't started, in Jenkins' words, for a waiting row.
    pub why: Option<String>,
}

#[derive(Clone, Copy, PartialEq)]
pub enum BuildState {
    /// Accepted by Jenkins but not yet given a build number.
    Queued,
    /// Has a build number, but no executor is running it yet — so its elapsed
    /// time is queue time, not build time.
    Waiting,
    /// Actually executing.
    Building,
}

/// What the whole builds panel shows at a glance.
pub struct BuildsView {
    pub cron_installed: bool,
    pub rows: Vec<BuildRow>,
}

impl BuildsView {
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

/// A sync row for the dashboard: one ArgoCD app we are tracking, resolved to a
/// display status. Settled/failed apps appear once (as `done`) then drop out.
pub struct SyncRow {
    pub status: SyncState,
    pub app: String,
    /// The app's ArgoCD URL, shown in the dashboard's right-hand gutter so the
    /// row is clickable the way a build row's Jenkins URL is.
    pub url: String,
    /// e.g. "Synced/Healthy" or "OutOfSync/Progressing, Running".
    pub detail: String,
    pub repo: String,
    pub age_secs: u64,
}

#[derive(Clone, Copy, PartialEq)]
pub enum SyncState {
    /// Still reconciling.
    Syncing,
    /// Reached Synced + Healthy.
    Synced,
    /// Degraded after sync.
    Degraded,
    /// No such ArgoCD app anymore.
    Gone,
    /// Gave up after the timeout.
    GaveUp,
    /// Transient status-check error; kept and retried.
    Unknown,
}

/// What the whole ArgoCD panel shows at a glance.
pub struct SyncView {
    /// None when no ArgoCD token is configured (can't check).
    pub rows: Option<Vec<SyncRow>>,
}

impl SyncView {
    pub fn is_empty(&self) -> bool {
        self.rows.as_ref().is_none_or(|r| r.is_empty())
    }
}

/// Gather the pending-build queue into display rows without printing. Shares
/// the exact queue/Jenkins-polling logic the snapshot path uses; the only
/// difference is it returns data instead of writing lines. Does not mutate the
/// queue (the cron watcher owns that).
fn collect_builds() -> Result<BuildsView> {
    let q = Queue::open()?;
    let cron = cron_installed().unwrap_or(false);
    let t = now();
    let jenkins = if q.items.iter().any(|p| p.build_url.is_some()) {
        config::load().ok().map(Jenkins::new)
    } else {
        None
    };
    let rows = q
        .items
        .iter()
        .map(|p| {
            let progress = match (&p.build_url, &jenkins) {
                (Some(url), Some(j)) => j.build_progress(url),
                _ => None,
            };
            // A build with no number is queued; one that has a number but
            // whose pipeline hasn't started is waiting for an executor. Only a
            // build that is really executing gets timed against the estimate.
            let state = match (&p.build_url, &progress) {
                (None, _) => BuildState::Queued,
                (Some(_), Some(pr)) if pr.waiting => BuildState::Waiting,
                (Some(_), _) => BuildState::Building,
            };
            // Jenkins' own wording for a not-yet-started item ("Waiting for
            // next available executor"), looked up only while it is queued.
            let why = match (&p.queue_url, &jenkins, state) {
                (Some(q), Some(j), BuildState::Queued) => j.queue_why(q),
                _ => None,
            };
            BuildRow {
                state,
                label: p.label(),
                job: p.job_name(),
                build: p.build_url.as_deref().and_then(build_number),
                repo: p.repo.clone(),
                age_secs: t.saturating_sub(p.queued_at),
                elapsed: progress.as_ref().map(|pr| pr.elapsed),
                estimated: progress.as_ref().and_then(|pr| pr.estimated),
                stage: progress.as_ref().and_then(|pr| pr.stage.clone()),
                why,
            }
        })
        .collect();
    Ok(BuildsView {
        cron_installed: cron,
        rows,
    })
}

/// `sts --pending`: show the builds waiting to be synced to ArgoCD. On a
/// terminal it refreshes in place every few seconds (dropping each build as the
/// cron watcher deploys it) and exits once the queue is empty; piped or
/// redirected it prints a single snapshot and exits.
pub fn show() -> Result<()> {
    if std::io::stdout().is_terminal() {
        return run_tui(crate::tui::View::Pending);
    }
    // Piped: a single plain snapshot.
    println!("{} {}", style::dim("sts -P"), style::dim(&utc_now()));
    show_once()?;
    Ok(())
}

/// Build the TUI `Snapshot` for `view`, collecting only the panels it shows.
fn tui_snapshot(view: crate::tui::View) -> Result<crate::tui::Snapshot> {
    let builds = if view.shows_builds() {
        Some(collect_builds()?)
    } else {
        None
    };
    let syncs = if view.shows_syncs() {
        Some(collect_syncing()?)
    } else {
        None
    };
    Ok(crate::tui::Snapshot {
        builds,
        syncs,
    })
}

/// Launch the ratatui dashboard for a non-`Follow` view. The refresh callback
/// is a no-op (the cron watcher advances the pipeline); `--follow` advances it
/// inline instead.
///
/// With nothing to watch, the TUI would draw one frame, see an empty pipeline,
/// exit and leave the alternate screen — a flash and no output. So the empty
/// case prints the plain snapshot and stays out of the TUI entirely.
fn run_tui(view: crate::tui::View) -> Result<()> {
    if tui_snapshot(view)?.is_drained() {
        return print_empty(view);
    }
    crate::tui::run(view, || Ok(()), move || tui_snapshot(view))?;
    Ok(())
}

/// The plain "nothing to show" summary for a live view, used when there is
/// nothing to open the TUI for.
fn print_empty(view: crate::tui::View) -> Result<()> {
    if view.shows_builds() {
        show_once()?;
    }
    if view.shows_syncs() {
        sync_status_once()?;
    }
    Ok(())
}

/// One pass of the pending view. Returns true when the queue is empty.
fn show_once() -> Result<bool> {
    let q = Queue::open()?;
    let cron = cron_installed().unwrap_or(false);
    println!("{} {}", style::dim("queue:"), queue_file().display());
    println!("{} {}", style::dim("log:  "), log_file().display());
    println!(
        "{} {}",
        style::dim("cron: "),
        if cron {
            style::green("installed")
        } else {
            style::yellow("not installed")
        }
    );
    if q.items.is_empty() {
        println!("{} No builds waiting to be deployed.", style::ok());
        return Ok(true);
    }
    let t = now();
    // Only build a Jenkins client (and poll progress) when something is
    // actually building, so a queue of not-yet-started items stays cheap.
    let jenkins = if q.items.iter().any(|p| p.build_url.is_some()) {
        config::load().ok().map(Jenkins::new)
    } else {
        None
    };
    for p in &q.items {
        let progress = match (&p.build_url, &jenkins) {
            (Some(url), Some(j)) => j.build_progress(url),
            _ => None,
        };
        // Same three states the dashboard shows: a build that has a number but
        // no executor is waiting, not building.
        let state = match (&p.build_url, &progress) {
            (None, _) => style::yellow("queued  "),
            (Some(_), Some(pr)) if pr.waiting => style::yellow("waiting "),
            (Some(_), _) => style::bold_cyan("building"),
        };
        let progress = progress
            .map(|pr| format!(", {}", pr.hint()))
            .unwrap_or_default();
        println!(
            "  {} {state}  {}  {}",
            style::busy(),
            p.label(),
            style::dim(&format!(
                "({}s ago, {}{progress})",
                t.saturating_sub(p.queued_at),
                p.repo
            )),
        );
    }
    Ok(false)
}

/// `sts` with no mode flag, or `sts --dashboard`: a combined live view of the
/// whole pipeline — builds waiting/running (from `-P`) and ArgoCD apps still
/// syncing (from `-S`) — in one auto-refreshing screen. On a terminal it
/// refreshes until both are empty; piped it prints a single snapshot.
pub fn dashboard() -> Result<()> {
    if std::io::stdout().is_terminal() {
        return run_tui(crate::tui::View::Dashboard);
    }
    // Piped: a single plain snapshot of both sections.
    println!("{} {}", style::dim("sts dashboard"), style::dim(&utc_now()));
    println!("── builds ──────────────────────────────────");
    show_once()?;
    println!("── argocd ──────────────────────────────────");
    sync_status_once()?;
    Ok(())
}

/// Block until the given repo's builds (and the syncs they trigger) finish,
/// printing the combined dashboard as it goes. Used by `sts -s --follow` after
/// a build is triggered, and by `sts --follow` on its own. Runs watcher ticks
/// inline so it works even if cron hasn't fired yet. Returns when both queues
/// are empty.
pub fn follow() -> Result<()> {
    // The pipeline is advanced inline (rather than waiting on cron) by running
    // a watcher tick + sync check each cycle. Shared by both the TUI and the
    // piped path.
    let advance = || -> Result<()> {
        tick()?;
        if let Err(e) = check_syncing(SyncMode::Notify) {
            log(&format!("sync check failed: {e:#}"));
        }
        Ok(())
    };

    if std::io::stdout().is_terminal() {
        // Nothing in flight: say so rather than flashing an empty screen.
        advance()?;
        if tui_snapshot(crate::tui::View::Follow)?.is_drained() {
            return print_empty(crate::tui::View::Follow);
        }
        let drained = crate::tui::run(
            crate::tui::View::Follow,
            advance,
            || tui_snapshot(crate::tui::View::Follow),
        )?;
        // Only announce completion if the pipeline actually drained (not if the
        // user quit early). Printed after the terminal is restored.
        if drained {
            println!(
                "{} {}",
                style::ok(),
                style::bold_green("All builds deployed and synced.")
            );
        }
        return Ok(());
    }

    // Piped: advance and print a plain snapshot each cycle until drained.
    loop {
        advance()?;
        println!("{} {}", style::dim("sts --follow"), style::dim(&utc_now()));
        println!("── builds ──────────────────────────────────");
        let builds_empty = show_once()?;
        println!("── argocd ──────────────────────────────────");
        let syncs_empty = sync_status_once()?;
        if builds_empty && syncs_empty {
            println!(
                "\n{} {}",
                style::ok(),
                style::bold_green("All builds deployed and synced.")
            );
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs(FOLLOW_POLL_SECS));
    }
}

/// `sts -L`: the deploy watcher's log. On a terminal this opens the TUI log
/// viewer and follows the file (scrollback, wrap, follow/pause); piped or
/// redirected it prints the last `n` lines and exits, which is what scripts and
/// `grep` want.
pub fn tail_log(n: usize) -> Result<()> {
    let path = log_file();
    if !std::io::stdout().is_terminal() {
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                println!("No watch log yet ({}).", path.display());
                return Ok(());
            }
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let lines: Vec<&str> = text.lines().collect();
        for line in &lines[lines.len().saturating_sub(n)..] {
            println!("{line}");
        }
        return Ok(());
    }

    // Terminal: follow the file in the viewer. `cursor` is how far through the
    // file we have read; a file that shrinks (rotated by the watcher) is read
    // from the start again.
    let mut cursor: u64 = 0;
    let mut primed = false;
    let follow_path = path.clone();
    crate::tui::run_logs(
        "watch log",
        vec![("file".to_string(), path.display().to_string())],
        Duration::from_secs(1),
        move || {
            let text = match fs::read_to_string(&follow_path) {
                Ok(t) => t,
                Err(_) if primed => return Vec::new(),
                Err(_) => {
                    primed = true;
                    return vec![crate::tui::LogLine::note(format!(
                        "No watch log yet ({})",
                        follow_path.display()
                    ))];
                }
            };
            let size = text.len() as u64;
            if size < cursor {
                // Rotated/truncated: start over rather than skipping content.
                cursor = 0;
            }
            // Slice on a character boundary: the file grew since the last
            // read, and a byte offset from the previous read can land inside a
            // multi-byte character of the new content. `get` returns None then,
            // and re-showing the whole tail is better than panicking.
            let fresh = text.get(cursor as usize..).unwrap_or(&text).to_string();
            cursor = size;
            let mut lines: Vec<crate::tui::LogLine> = Vec::new();
            if !primed {
                primed = true;
                // First pass: show the tail, not the whole history.
                let all: Vec<&str> = text.lines().collect();
                lines.extend(
                    all[all.len().saturating_sub(n)..]
                        .iter()
                        .map(|l| crate::tui::LogLine::plain(*l)),
                );
                return lines;
            }
            lines.extend(
                fresh
                    .lines()
                    .filter(|l| !l.is_empty())
                    .map(crate::tui::LogLine::plain),
            );
            lines
        },
    )
}

/// What `sts -l` resolved before it starts tailing: which app, namespace, pods
/// and container. Collected through the [`Ui`](crate::ui::Ui) so the picking
/// happens in the TUI (or as plain prompts when piped).
pub struct LogTarget {
    pub app: String,
    pub namespace: Option<String>,
    pub container: Option<String>,
    pub pods: Vec<argocd::Pod>,
}

impl LogTarget {
    /// The key/value summary the viewer shows under its title.
    fn meta(&self) -> Vec<(String, String)> {
        let mut meta = vec![("app".to_string(), self.app.clone())];
        if let Some(ns) = &self.namespace {
            meta.push(("namespace".to_string(), ns.clone()));
        }
        meta.push((
            "pods".to_string(),
            if self.pods.len() == 1 {
                self.pods[0].name.clone()
            } else {
                format!("{}", self.pods.len())
            },
        ));
        if let Some(c) = &self.container {
            meta.push(("container".to_string(), c.clone()));
        }
        meta
    }
}

/// Resolve what to tail: the app (explicit or auto-detected from the repo), the
/// namespace (explicit, the only one, or picked) and the pods to read. Reports
/// each step through `ui`, so the TUI shows the resolution and can ask.
pub fn resolve_log_target(
    ui: &mut dyn crate::ui::Ui,
    app: Option<String>,
    repo: Option<String>,
    namespace: Option<String>,
    container: Option<String>,
) -> Result<LogTarget> {
    let Some(cfg) = config::load_argo()? else {
        bail!(
            "no ArgoCD token configured (argocd_token in {}); can't read logs",
            config::config_file().display()
        );
    };
    let argo = Argo::new(cfg);

    let app = match app {
        Some(a) => a,
        None => {
            ui.step("Detecting the ArgoCD app for this repo");
            resolve_app_for_repo(ui, &argo, repo.as_deref())?
        }
    };

    ui.step(&format!("Finding the pods of `{app}`"));
    let pods = argo
        .pods(&app)
        .with_context(|| format!("listing pods of ArgoCD app `{app}`"))?;
    if pods.is_empty() {
        bail!("ArgoCD app `{app}` has no running pods (nothing to log)");
    }

    let mut namespaces: Vec<String> = pods
        .iter()
        .map(|p| p.namespace.clone())
        .filter(|n| !n.is_empty())
        .collect();
    namespaces.sort();
    namespaces.dedup();
    let ns = choose_namespace(ui, &app, namespace, &namespaces)?;

    let selected: Vec<argocd::Pod> = pods
        .into_iter()
        .filter(|p| ns.as_deref().is_none_or(|n| p.namespace == n))
        .collect();
    if selected.is_empty() {
        bail!(
            "no pods of `{app}` in namespace `{}` (have: {})",
            ns.unwrap_or_default(),
            namespaces.join(", ")
        );
    }
    for p in &selected {
        let health = if p.health.is_empty() {
            String::new()
        } else {
            format!(" ({})", p.health)
        };
        ui.kv("pod", &format!("{}{health}", p.name));
    }

    Ok(LogTarget {
        app,
        namespace: ns,
        container,
        pods: selected,
    })
}

/// Tail a resolved target. On a terminal this runs the TUI viewer; piped it
/// appends new lines to stdout like `kubectl logs -f`, as before.
pub fn tail_logs(target: LogTarget) -> Result<()> {
    let Some(cfg) = config::load_argo()? else {
        bail!("no ArgoCD token configured; can't read logs");
    };
    let argo = Argo::new(cfg);
    let mut poller = LogPoller::new(target.pods.clone(), target.container.clone());
    let app = target.app.clone();

    if std::io::stdout().is_terminal() {
        let meta = target.meta();
        return crate::tui::run_logs(
            &format!("logs · {app}"),
            meta,
            Duration::from_secs(LOGS_POLL_SECS),
            move || poller.poll(&argo, &app),
        );
    }

    // Piped: stream plainly and never return.
    println!(
        "{}  {}",
        style::bold_cyan(&format!("sts -l {app}")),
        style::dim(&format!(
            "{} pod(s){}, polling every {LOGS_POLL_SECS}s",
            target.pods.len(),
            target
                .namespace
                .as_deref()
                .map(|n| format!(" in {n}"))
                .unwrap_or_default()
        ))
    );
    let out = std::io::stdout();
    loop {
        for line in poller.poll(&argo, &app) {
            let mut handle = out.lock();
            match line.source {
                Some(src) => {
                    let _ = writeln!(handle, "[{src}] {}", line.text);
                }
                None => {
                    let _ = writeln!(handle, "{}", line.text);
                }
            }
            let _ = handle.flush();
        }
        std::thread::sleep(Duration::from_secs(LOGS_POLL_SECS));
    }
}

/// Polls each pod's log and yields only the lines it hasn't yielded before, so
/// neither the viewer nor a pipe sees duplicates — and, just as importantly,
/// nothing is skipped.
///
/// Each poll asks for the last `LOGS_TAIL` lines, so consecutive responses
/// overlap. The overlap is resolved by *position*: remember the last line
/// emitted for a pod and, in the next response, emit everything after it.
/// Comparing timestamps instead (the obvious approach) silently loses lines,
/// because several lines routinely carry the same timestamp.
struct LogPoller {
    pods: Vec<argocd::Pod>,
    container: Option<String>,
    /// The last line emitted per pod, matched against the next response.
    last: std::collections::HashMap<String, argocd::LogLine>,
    /// Prefix lines with the pod name (only worth it with several pods).
    multi: bool,
    /// Pod name → short label for that prefix.
    labels: std::collections::HashMap<String, String>,
}

/// Shorten pod names for the log prefix. Pods of one app share a long prefix
/// (`soochi-dash-app-7d9f-abcde`, `soochi-dash-app-5c4e-fghij`), which would
/// eat the width of every line; strip what they all have in common and keep
/// what tells them apart.
fn short_pod_labels(pods: &[argocd::Pod]) -> std::collections::HashMap<String, String> {
    let names: Vec<&str> = pods.iter().map(|p| p.name.as_str()).collect();
    let mut labels = std::collections::HashMap::new();
    // Longest common prefix, cut back to a `-` so a label never starts
    // mid-token.
    let mut common = names.first().copied().unwrap_or("").to_string();
    for n in &names[names.len().min(1)..] {
        let keep = common
            .chars()
            .zip(n.chars())
            .take_while(|(a, b)| a == b)
            .count();
        common.truncate(common.char_indices().nth(keep).map_or(common.len(), |(i, _)| i));
    }
    let cut = match common.rfind('-') {
        Some(i) if names.len() > 1 => i + 1,
        _ => 0,
    };
    for name in names {
        // Too short to be recognisable? Keep the full name.
        let short = &name[cut.min(name.len())..];
        labels.insert(
            name.to_string(),
            if short.len() >= 4 { short.to_string() } else { name.to_string() },
        );
    }
    labels
}

/// Which lines of `lines` are new, given the last one already emitted.
///
/// The responses overlap, so find that line in the new batch (searching from
/// the end, since the same text can repeat) and take everything after it. If
/// it isn't there the window has slid past it — fall back to the timestamp so
/// at least nothing is shown twice.
fn new_lines<'a>(lines: &'a [argocd::LogLine], last: Option<&argocd::LogLine>) -> &'a [argocd::LogLine] {
    let Some(last) = last else {
        // First poll: everything the tail gave us.
        return lines;
    };
    if let Some(i) = lines.iter().rposition(|l| l == last) {
        return &lines[i + 1..];
    }
    let from = lines
        .iter()
        .position(|l| l.ts.as_str() > last.ts.as_str())
        .unwrap_or(lines.len());
    &lines[from..]
}

impl LogPoller {
    fn new(pods: Vec<argocd::Pod>, container: Option<String>) -> LogPoller {
        let multi = pods.len() > 1;
        let labels = short_pod_labels(&pods);
        LogPoller {
            pods,
            container,
            last: std::collections::HashMap::new(),
            multi,
            labels,
        }
    }

    fn poll(&mut self, argo: &Argo, app: &str) -> Vec<crate::tui::LogLine> {
        // (timestamp, line): collected per pod, then merged in time order so
        // several pods read as one chronological stream instead of one pod's
        // block after another's.
        let mut batch: Vec<(String, crate::tui::LogLine)> = Vec::new();
        let mut out = Vec::new();
        for pod in &self.pods {
            let lines = match argo.logs(
                app,
                &pod.namespace,
                &pod.name,
                self.container.as_deref(),
                LOGS_TAIL,
            ) {
                Ok(l) => l,
                Err(e) => {
                    // Transient (pod restart, brief 5xx): surface it and keep
                    // tailing the others.
                    out.push(crate::tui::LogLine::error(format!(
                        "logs for {} failed: {e:#}",
                        pod.name
                    )));
                    continue;
                }
            };
            let fresh = new_lines(&lines, self.last.get(&pod.name));
            for line in fresh {
                let rendered = if self.multi {
                    let label = self
                        .labels
                        .get(&pod.name)
                        .map(String::as_str)
                        .unwrap_or(&pod.name);
                    crate::tui::LogLine::from(label, line.content.clone())
                } else {
                    crate::tui::LogLine::plain(line.content.clone())
                };
                batch.push((line.ts.clone(), rendered));
            }
            // Remember where we got to, even when this poll added nothing.
            if let Some(newest) = lines.last() {
                self.last.insert(pod.name.clone(), newest.clone());
            }
        }
        // RFC3339 sorts chronologically as text. A stable sort keeps each pod's
        // own order intact where timestamps tie.
        batch.sort_by(|a, b| a.0.cmp(&b.0));
        out.extend(batch.into_iter().map(|(_, line)| line));
        out
    }
}

/// Auto-detect the ArgoCD app to tail from a repo: look up the repo's Jenkins
/// job(s), take each job's chart name (its last path segment) and map it to an
/// ArgoCD app the same way the deploy watcher does (`app_for_chart`). With one
/// app we use it; with several we ask through the UI (which bails with the list
/// when there is nobody to ask). Errors clearly when the repo has no job or
/// none of its charts match an app, pointing at `-l <app>` as the fallback.
fn resolve_app_for_repo(
    ui: &mut dyn crate::ui::Ui,
    argo: &Argo,
    repo: Option<&str>,
) -> Result<String> {
    let Some(slug) = repo else {
        bail!("no repo to auto-detect the app from; pass the app name: `sts -l <app>`");
    };
    let Some(jobs) = crate::jobs::JobMap::load()?.lookup(slug) else {
        bail!(
            "no Jenkins job mapped for `{slug}`, so the ArgoCD app can't be auto-detected; \
             pass it explicitly: `sts -l <app>` (see `sts -ja` for mapped repos)"
        );
    };

    let apps = argo.apps().context("listing ArgoCD apps")?;
    let mut matched: Vec<String> = Vec::new();
    let mut charts: Vec<String> = Vec::new();
    for job in &jobs {
        let chart = job.name();
        if chart.is_empty() {
            continue;
        }
        charts.push(chart.clone());
        if let Some(a) = argocd::app_for_chart(&apps, &chart)
            && !matched.contains(&a.name)
        {
            matched.push(a.name.clone());
        }
    }

    match matched.len() {
        0 => bail!(
            "none of `{slug}`'s chart(s) ({}) map to an ArgoCD app; \
             pass the app name: `sts -l <app>`",
            charts.join(", ")
        ),
        1 => {
            ui.kv("app", &format!("{} (from {slug})", matched[0]));
            Ok(matched.remove(0))
        }
        _ => {
            let items: Vec<(String, String)> = matched
                .iter()
                .map(|a| (a.clone(), String::new()))
                .collect();
            let picked = ui
                .choose(&format!("`{slug}` maps to several apps — tail which?"), &items, false)
                .with_context(|| {
                    format!(
                        "`{slug}` maps to several ArgoCD apps ({}); pass one: `sts -l <app>`",
                        matched.join(", ")
                    )
                })?;
            Ok(matched.remove(picked[0]))
        }
    }
}

/// Pick the namespace to view. With one namespace (or none reported) use it.
/// With several and no explicit choice, ask through the UI (which bails with
/// the list when there is nobody to ask, so `-N` can be passed instead).
/// An explicit `wanted` must match one the app actually runs in.
fn choose_namespace(
    ui: &mut dyn crate::ui::Ui,
    app: &str,
    wanted: Option<String>,
    namespaces: &[String],
) -> Result<Option<String>> {
    if let Some(w) = wanted {
        if namespaces.is_empty() || namespaces.iter().any(|n| n == &w) {
            return Ok(Some(w));
        }
        bail!(
            "`{app}` has no pods in namespace `{w}`; it runs in: {}",
            namespaces.join(", ")
        );
    }
    match namespaces.len() {
        0 => Ok(None),
        1 => Ok(Some(namespaces[0].clone())),
        _ => {
            let items: Vec<(String, String)> = namespaces
                .iter()
                .map(|n| (n.clone(), String::new()))
                .collect();
            let picked = ui
                .choose(
                    &format!("`{app}` runs in several namespaces — view which?"),
                    &items,
                    false,
                )
                .with_context(|| {
                    format!(
                        "`{app}` runs in {} namespaces ({}); pass -N <namespace> to pick one",
                        namespaces.len(),
                        namespaces.join(", ")
                    )
                })?;
            Ok(Some(namespaces[picked[0]].clone()))
        }
    }
}


/// dropping each once it reaches Synced + Healthy (or vanishes). On a terminal it
/// refreshes in place every few seconds until the list is empty; piped or
/// redirected it prints a single snapshot and exits.
pub fn sync_status() -> Result<()> {
    if std::io::stdout().is_terminal() {
        return run_tui(crate::tui::View::Sync);
    }
    // Piped: a single plain snapshot.
    println!("{} {}", style::dim("sts -S"), style::dim(&utc_now()));
    sync_status_once()?;
    Ok(())
}

/// One pass: poll ArgoCD for each tracked app, print the status, prune settled
/// ones, and save. Returns true when nothing is left to track. The queue lock is
/// released as soon as this returns, so the cron watcher can enqueue new syncs
/// between polls.
fn sync_status_once() -> Result<bool> {
    let (empty, rows) = check_syncing(SyncMode::Print)?;
    drop(rows);
    Ok(empty)
}

/// Collect the syncing list into display rows (for the TUI) while running the
/// exact same prune/save pass `sts -S` does. Returns the apps settled/failed
/// this pass plus the ones still in progress, newest state first.
fn collect_syncing() -> Result<SyncView> {
    let (_empty, rows) = check_syncing(SyncMode::Collect)?;
    Ok(SyncView { rows })
}

/// What a syncing-list pass should do with each app's status: print it
/// (`sts -S`), collect it into rows (the TUI), or stay silent but fire
/// notifications + record history (the cron watcher).
#[derive(Clone, Copy, PartialEq)]
enum SyncMode {
    /// `sts -S`: print each app's live status; no notifications.
    Print,
    /// The TUI dashboard: return rows to render; no notifications.
    Collect,
    /// cron `--watch`: silent, but notify + record history as apps settle or fail.
    Notify,
}

/// Shared core for the interactive `sts -S` view, the TUI dashboard and the
/// cron watcher's sync-completion check. Polls each tracked app, prunes the
/// ones that have settled (or vanished, or timed out), and in `Notify` mode
/// fires a notification and records history for each terminal outcome. Returns
/// `(empty, rows)`: `empty` is true when nothing is left to track; `rows` is
/// `Some` only in `Collect` mode (settled/failed-this-pass rows first, then
/// the ones still syncing).
fn check_syncing(mode: SyncMode) -> Result<(bool, Option<Vec<SyncRow>>)> {
    let print = mode == SyncMode::Print;
    let notify = mode == SyncMode::Notify;
    let collect = mode == SyncMode::Collect;
    // Rows gathered in Collect mode: terminal outcomes this pass, then in-progress.
    let mut done_rows: Vec<SyncRow> = Vec::new();
    let mut live_rows: Vec<SyncRow> = Vec::new();
    let mut q = SyncQueue::open()?;
    if print {
        println!("{} {}", style::dim("tracking:"), syncing_file().display());
    }
    if q.items.is_empty() {
        if print {
            println!("{} No ArgoCD syncs in progress.", style::ok());
        }
        return Ok((true, collect.then(Vec::new)));
    }

    let Some(argo_cfg) = config::load_argo()? else {
        if print {
            println!(
                "no ArgoCD token configured (argocd_token in config.toml); can't check sync status"
            );
        }
        // No token: the TUI shows "none configured" (rows = None), the others
        // treat it as done so they don't spin.
        return Ok((true, None));
    };
    // Captured before the client takes the config: used to build each row's
    // clickable app URL.
    let argo_base = argo_cfg.url.trim_end_matches('/').to_string();
    let app_url = |app: &str| format!("{argo_base}/applications/{app}");
    let argo = Argo::new(argo_cfg);

    let t = now();
    let mut keep = Vec::new();
    let mut done = Vec::new();
    for s in std::mem::take(&mut q.items) {
        let age = t.saturating_sub(s.started_at);
        // Stop tracking anything that never settles so the list can't grow forever.
        if age > MAX_AGE_SECS {
            done.push(format!(
                "  {} {}  {}",
                style::err(),
                style::bold_red("gave up"),
                style::dim(&format!(
                    "{}  (still not settled after {}m)",
                    s.app,
                    MAX_AGE_SECS / 60
                ))
            ));
            done_rows.push(SyncRow {
                status: SyncState::GaveUp,
                app: s.app.clone(),
                detail: format!("not settled after {}m", MAX_AGE_SECS / 60),
                repo: s.repo.clone(),
                url: app_url(&s.app),
                age_secs: age,
            });
            if notify {
                record_history(&s.repo, "SYNC_GAVE_UP", &s.app);
                notify::send(
                    notify::Level::Failure,
                    &format!("sts: sync stuck ({})", s.repo),
                    &format!("{} not settled after {}m", s.app, MAX_AGE_SECS / 60),
                );
            }
            continue;
        }
        match argo.status(&s.app) {
            Ok(None) => {
                done.push(format!(
                    "  {} {}  {}",
                    style::err(),
                    style::dim("gone"),
                    style::dim(&format!("{}  (no such ArgoCD app)", s.app))
                ));
                done_rows.push(SyncRow {
                    status: SyncState::Gone,
                    app: s.app.clone(),
                    detail: "no such ArgoCD app".to_string(),
                    repo: s.repo.clone(),
                    url: app_url(&s.app),
                    age_secs: age,
                });
                if notify {
                    record_history(&s.repo, "APP_GONE", &s.app);
                }
            }
            Ok(Some(st)) if st.is_settled() => {
                done.push(format!(
                    "  {} {}    {}  {}",
                    style::ok(),
                    style::bold_green("synced"),
                    s.app,
                    style::dim(&format!("({})", st.health))
                ));
                done_rows.push(SyncRow {
                    status: SyncState::Synced,
                    app: s.app.clone(),
                    detail: st.health.clone(),
                    repo: s.repo.clone(),
                    url: app_url(&s.app),
                    age_secs: age,
                });
                if notify {
                    record_history(&s.repo, "SYNCED", &s.app);
                    notify::send(
                        notify::Level::Info,
                        &format!("sts: deployed ({})", s.repo),
                        &format!("{} is now Synced + {}", s.app, st.health),
                    );
                }
            }
            Ok(Some(st)) if st.health == "Degraded" => {
                // A degraded app won't settle on its own; surface it and drop it.
                done.push(format!(
                    "  {} {}  {}  {}",
                    style::err(),
                    style::bold_red("degraded"),
                    s.app,
                    style::dim(&format!("({}/{})", st.sync, st.health))
                ));
                done_rows.push(SyncRow {
                    status: SyncState::Degraded,
                    app: s.app.clone(),
                    detail: format!("{}/{}", st.sync, st.health),
                    repo: s.repo.clone(),
                    url: app_url(&s.app),
                    age_secs: age,
                });
                if notify {
                    record_history(&s.repo, "DEGRADED", &s.app);
                    notify::send(
                        notify::Level::Failure,
                        &format!("sts: app degraded ({})", s.repo),
                        &format!("{} is {}/{} after sync", s.app, st.sync, st.health),
                    );
                }
            }
            Ok(Some(st)) => {
                let phase = if st.phase.is_empty() {
                    String::new()
                } else {
                    format!(", {}", st.phase)
                };
                if print {
                    let status = format!("{}/{}{phase}", st.sync, st.health);
                    let age = style::dim(&format!("({age}s ago, {})", s.repo));
                    println!(
                        "  {} {}  {:<28} {status}  {age}",
                        style::busy(),
                        style::bold_cyan("syncing"),
                        s.app,
                    );
                }
                if collect {
                    live_rows.push(SyncRow {
                        status: SyncState::Syncing,
                        app: s.app.clone(),
                        detail: format!("{}/{}{phase}", st.sync, st.health),
                        repo: s.repo.clone(),
                        url: app_url(&s.app),
                        age_secs: age,
                    });
                }
                keep.push(s);
            }
            Err(e) => {
                // Keep it; a transient error shouldn't lose the entry.
                if print {
                    println!(
                        "  {} {}  {}",
                        style::yellow("?"),
                        s.app,
                        style::dim(&format!("(status check failed: {e:#})"))
                    );
                }
                if collect {
                    live_rows.push(SyncRow {
                        status: SyncState::Unknown,
                        app: s.app.clone(),
                        detail: "status check failed".to_string(),
                        repo: s.repo.clone(),
                        url: app_url(&s.app),
                        age_secs: age,
                    });
                }
                keep.push(s);
            }
        }
    }

    q.items = keep;
    q.save()?;

    if print && !done.is_empty() {
        for line in &done {
            println!("{line}");
        }
    }

    // In Collect mode, hand back the terminal outcomes seen this pass followed
    // by the apps still syncing, so the TUI can show both briefly.
    let rows = if collect {
        done_rows.extend(live_rows);
        Some(done_rows)
    } else {
        None
    };

    let empty = q.items.is_empty();
    if empty {
        if print {
            println!("{} {}", style::ok(), style::bold_green("All tracked ArgoCD syncs have settled."));
        }
        return Ok((true, rows));
    }
    Ok((false, rows))
}

/// What cron runs: polls every `WATCH_INTERVAL_SECS` for one minute by looping
/// `tick()` internally, since a crontab can't fire more than once a minute.
/// Stops early (and lets the entry remove itself) once both the pending build
/// queue and the syncing list are empty.
pub fn watch_loop() -> Result<()> {
    trim_log();
    let runs = (60 / WATCH_INTERVAL_SECS).max(1);
    for i in 0..runs {
        tick()?;
        // Also advance tracked ArgoCD syncs so a "deployed" (or failed) notice
        // fires even when nobody is running `sts -S`.
        if let Err(e) = check_syncing(SyncMode::Notify) {
            log(&format!("sync check failed: {e:#}"));
        }
        if Queue::is_empty_now() && SyncQueue::is_empty_now() {
            remove_cron()?;
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
        // No builds to watch, but keep the watcher alive while apps are still
        // syncing so their completion is tracked; watch_loop removes the cron
        // entry once both queues are empty.
        return Ok(());
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
            record_history(&p.repo, "GAVE_UP", &p.label());
            notify::send(
                notify::Level::Failure,
                &format!("sts: gave up on {}", p.repo),
                &format!("build not deployed after {}m: {}", MAX_AGE_SECS / 60, p.label()),
            );
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
                    record_history(&p.repo, "CANCELLED", &p.job_url);
                    notify::send(
                        notify::Level::Failure,
                        &format!("sts: build cancelled ({})", p.repo),
                        &format!("cancelled in the Jenkins queue: {}", p.job_url),
                    );
                    continue;
                }
                QueueState::Gone => {
                    log(&format!(
                        "{}: queue item {queue_url} expired before its build was seen; not deploying",
                        p.job_url
                    ));
                    record_history(&p.repo, "LOST", &p.job_url);
                    notify::send(
                        notify::Level::Failure,
                        &format!("sts: build lost ({})", p.repo),
                        &format!("queue item expired before the build was seen: {}", p.job_url),
                    );
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
                // Build finished OK; moving on to the ArgoCD sync.
                log(&format!("{build_url}: build SUCCESS, syncing ArgoCD"));
                notify::send(
                    notify::Level::Info,
                    &format!("sts: build succeeded ({})", p.repo),
                    &format!("syncing ArgoCD now: {build_url}"),
                );
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
                            record_history(&p.repo, "SYNC_FAILED", &build_url);
                            notify::send(
                                notify::Level::Failure,
                                &format!("sts: ArgoCD sync failed ({})", p.repo),
                                &format!("gave up after {} attempts: {e:#}", p.attempts),
                            );
                        }
                    }
                }
            }
            Ok(Some(r)) => {
                log(&format!("{build_url}: build finished {r}, not deploying"));
                record_history(&p.repo, &r, &build_url);
                notify::send(
                    notify::Level::Failure,
                    &format!("sts: build {r} ({})", p.repo),
                    &format!("not deploying: {build_url}"),
                );
            }
            Err(e) => {
                log(&format!("{build_url}: status check failed: {e:#}"));
                keep.push(p);
            }
        }
    }

    q.items = keep;
    q.save()?;
    // Drop the lock before touching cron. Only stop the watcher when there are
    // neither builds left to watch nor apps left to track.
    drop(q);
    if Queue::is_empty_now() && SyncQueue::is_empty_now() {
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
    // While a TUI owns the screen (e.g. `sts --follow` running inline watcher
    // ticks), printing to stdout would corrupt the ratatui frame. Append to the
    // watch log instead — the same place cron's output lands — so the message
    // is kept without scribbling over the UI.
    if TUI_ACTIVE.load(Ordering::SeqCst) {
        let line = format!("{} {msg}\n", utc_now());
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(log_file()) {
            let _ = f.write_all(line.as_bytes());
        }
        return;
    }
    println!("{} {msg}", utc_now());
}

/// Append a deploy outcome to the history log, logging (not failing) on error.
fn record_history(repo: &str, result: &str, detail: &str) {
    if let Err(e) = history::record(repo, result, detail) {
        log(&format!("could not record history for {repo}: {e:#}"));
    }
}

/// Keep `watch.log` from growing without bound. cron appends to it via shell
/// redirection, so once it passes `LOG_MAX_BYTES` we rewrite it with just the
/// most recent lines. Best-effort: any error here is ignored.
fn trim_log() {
    let path = log_file();
    let Ok(meta) = fs::metadata(&path) else {
        return;
    };
    if meta.len() <= LOG_MAX_BYTES {
        return;
    }
    let Ok(text) = fs::read_to_string(&path) else {
        return;
    };
    let lines: Vec<&str> = text.lines().collect();
    let keep = lines.len().min(LOG_KEEP_LINES);
    let tail = lines[lines.len() - keep..].join("\n");
    let tmp = path.with_extension("log.tmp");
    if fs::write(&tmp, format!("{tail}\n")).is_ok() {
        let _ = fs::rename(&tmp, &path);
    }
}

/// Writes a queue atomically, or removes the file when the list is empty so the
/// state dir doesn't keep a stale `[]` around once a trace completes. A missing
/// file is already treated as an empty queue everywhere it is read.
fn save_items<T: Serialize>(path: &std::path::Path, items: &[T]) -> Result<()> {
    if items.is_empty() {
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
        }
    } else {
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_string_pretty(items)?)
            .with_context(|| format!("writing {}", tmp.display()))?;
        fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }
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
pub fn utc_now() -> String {
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

    fn pod(name: &str) -> argocd::Pod {
        argocd::Pod {
            name: name.to_string(),
            namespace: "stage".to_string(),
            health: "Healthy".to_string(),
        }
    }

    #[test]
    fn pod_labels_drop_the_prefix_every_pod_shares() {
        let pods = [
            pod("soochi-dash-app-7d9f8-abcde"),
            pod("soochi-dash-app-7d9f8-fghij"),
        ];
        let labels = short_pod_labels(&pods);
        assert_eq!(labels[&pods[0].name], "abcde");
        assert_eq!(labels[&pods[1].name], "fghij");
    }

    #[test]
    fn a_single_pod_keeps_its_full_name() {
        let pods = [pod("soochi-dash-app-7d9f8-abcde")];
        let labels = short_pod_labels(&pods);
        assert_eq!(labels[&pods[0].name], "soochi-dash-app-7d9f8-abcde");
    }

    #[test]
    fn unrelated_pod_names_are_left_alone() {
        let pods = [pod("api-1"), pod("worker-2")];
        let labels = short_pod_labels(&pods);
        assert_eq!(labels[&pods[0].name], "api-1");
        assert_eq!(labels[&pods[1].name], "worker-2");
    }

    #[test]
    fn labels_that_would_be_too_short_keep_the_full_name() {
        // Stripping the shared prefix would leave "a"/"b" — not recognisable.
        let pods = [pod("deploy-abc-a"), pod("deploy-abc-b")];
        let labels = short_pod_labels(&pods);
        assert_eq!(labels[&pods[0].name], "deploy-abc-a");
    }

    fn log(ts: &str, content: &str) -> argocd::LogLine {
        argocd::LogLine { ts: ts.to_string(), content: content.to_string() }
    }

    #[test]
    fn the_first_poll_shows_the_whole_tail() {
        let batch = [log("t1", "a"), log("t2", "b")];
        assert_eq!(new_lines(&batch, None).len(), 2);
    }

    #[test]
    fn overlapping_polls_emit_only_what_is_new() {
        let first = [log("t1", "a"), log("t2", "b"), log("t3", "c")];
        let last = first.last().unwrap().clone();
        // Next poll: the window slid by one, so two lines overlap.
        let second = [log("t2", "b"), log("t3", "c"), log("t4", "d")];
        let fresh = new_lines(&second, Some(&last));
        assert_eq!(fresh.len(), 1);
        assert_eq!(fresh[0].content, "d");
    }

    #[test]
    fn lines_sharing_a_timestamp_are_not_lost() {
        // This is what the stage cluster actually produces: a request line and
        // its JSON tail, logged in the same instant. A cursor that compared
        // timestamps dropped the second one.
        let first = [log("06:21:47Z", "info: GET /health 200")];
        let last = first.last().unwrap().clone();
        let second = [
            log("06:21:47Z", "info: GET /health 200"),
            log("06:21:47Z", " {\"service_name\":\"dukandar-soochi\"}"),
            log("06:21:48Z", "info: GET /health 200"),
        ];
        let fresh = new_lines(&second, Some(&last));
        assert_eq!(fresh.len(), 2, "same-second lines must still come through");
        assert!(fresh[0].content.contains("service_name"));
    }

    #[test]
    fn nothing_new_emits_nothing() {
        let batch = [log("t1", "a"), log("t2", "b")];
        let last = batch.last().unwrap().clone();
        assert!(new_lines(&batch, Some(&last)).is_empty());
    }

    #[test]
    fn repeated_identical_lines_are_resolved_from_the_end() {
        // A health check logs the same text over and over; matching the *last*
        // occurrence is what keeps the cursor moving forward.
        let last = log("t2", "same");
        let batch = [
            log("t1", "same"),
            log("t2", "same"),
            log("t3", "new"),
        ];
        let fresh = new_lines(&batch, Some(&last));
        assert_eq!(fresh.len(), 1);
        assert_eq!(fresh[0].content, "new");
    }

    #[test]
    fn a_window_that_slid_past_falls_back_to_the_timestamp() {
        // The pod logged so much that the previous line is no longer in the
        // tail: emit what is newer than it rather than repeating the batch.
        let last = log("t1", "gone");
        let batch = [log("t5", "e"), log("t6", "f")];
        let fresh = new_lines(&batch, Some(&last));
        assert_eq!(fresh.len(), 2);

        // …and if everything in the batch is older, show nothing rather than
        // re-showing history.
        let last = log("t9", "newest");
        assert!(new_lines(&batch, Some(&last)).is_empty());
    }

    #[test]
    fn multi_pod_output_is_merged_in_time_order() {
        // Two pods polled in turn produce two blocks; sorting by timestamp is
        // what makes them read as one stream. (Same sort the poller applies.)
        let mut batch = [
            ("06:00:03Z".to_string(), "pod-a third"),
            ("06:00:01Z".to_string(), "pod-a first"),
            ("06:00:02Z".to_string(), "pod-b second"),
        ];
        batch.sort_by(|a, b| a.0.cmp(&b.0));
        let order: Vec<&str> = batch.iter().map(|(_, t)| *t).collect();
        assert_eq!(order, ["pod-a first", "pod-b second", "pod-a third"]);
    }

    #[test]
    fn build_numbers_come_from_the_url() {
        assert_eq!(build_number("https://j/job/x/1423/"), Some(1423));
        assert_eq!(build_number("https://j/job/x/1423"), Some(1423));
        assert_eq!(build_number("https://j/job/x/"), None);
    }
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

    #[test]
    fn save_items_writes_then_removes_when_empty() {
        let dir = std::env::temp_dir().join(format!("sts-test-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("q.json");

        let items = vec![Pending::new("r", "/job/x/build", "", None, None)];
        save_items(&path, &items).unwrap();
        assert!(path.exists(), "non-empty queue should be written");

        save_items::<Pending>(&path, &[]).unwrap();
        assert!(!path.exists(), "empty queue should remove the file");

        // Removing an already-missing file is a no-op, not an error.
        save_items::<Pending>(&path, &[]).unwrap();

        let _ = fs::remove_dir_all(&dir);
    }
}
