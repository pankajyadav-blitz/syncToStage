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
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::IsTerminal;
use std::io::{BufRead, Write};
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
/// How often `sts -P` refreshes the live pending view on a terminal.
const PENDING_POLL_SECS: u64 = 5;
/// How often `sts --follow` advances and redraws the pipeline.
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

/// `sts --pending`: show the builds waiting to be synced to ArgoCD. On a
/// terminal it refreshes in place every few seconds (dropping each build as the
/// cron watcher deploys it) and exits once the queue is empty; piped or
/// redirected it prints a single snapshot and exits.
pub fn show() -> Result<()> {
    let live = std::io::stdout().is_terminal();
    loop {
        if live {
            clear_screen();
            println!("sts -P  —  {}  (Ctrl-C to stop)", utc_now());
        }
        let empty = show_once()?;
        if empty || !live {
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs(PENDING_POLL_SECS));
    }
}

/// One pass of the pending view. Returns true when the queue is empty.
fn show_once() -> Result<bool> {
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
        let state = if p.build_url.is_some() {
            "building"
        } else {
            "queued"
        };
        let progress = match (&p.build_url, &jenkins) {
            (Some(url), Some(j)) => j
                .build_progress(url)
                .map(|pr| format!(", {}", pr.hint()))
                .unwrap_or_default(),
            _ => String::new(),
        };
        println!(
            "  {state:<9} {}  ({}s ago, {}{progress})",
            p.label(),
            t.saturating_sub(p.queued_at),
            p.repo
        );
    }
    Ok(false)
}

/// `sts` with no mode flag, or `sts --dashboard`: a combined live view of the
/// whole pipeline — builds waiting/running (from `-P`) and ArgoCD apps still
/// syncing (from `-S`) — in one auto-refreshing screen. On a terminal it
/// refreshes until both are empty; piped it prints a single snapshot.
pub fn dashboard() -> Result<()> {
    let live = std::io::stdout().is_terminal();
    loop {
        if live {
            clear_screen();
            println!("sts dashboard  —  {}  (Ctrl-C to stop)", utc_now());
        }
        println!("── builds ──────────────────────────────────");
        let builds_empty = show_once()?;
        println!("── argocd ──────────────────────────────────");
        let syncs_empty = sync_status_once()?;
        if (builds_empty && syncs_empty) || !live {
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs(PENDING_POLL_SECS));
    }
}

/// Block until the given repo's builds (and the syncs they trigger) finish,
/// printing the combined dashboard as it goes. Used by `sts -s --follow` after
/// a build is triggered, and by `sts --follow` on its own. Runs watcher ticks
/// inline so it works even if cron hasn't fired yet. Returns when both queues
/// are empty.
pub fn follow() -> Result<()> {
    let live = std::io::stdout().is_terminal();
    loop {
        // Advance the pipeline ourselves rather than waiting on cron.
        tick()?;
        if let Err(e) = check_syncing(SyncMode::Notify) {
            log(&format!("sync check failed: {e:#}"));
        }
        if live {
            clear_screen();
            println!("sts --follow  —  {}  (Ctrl-C to stop)", utc_now());
        }
        println!("── builds ──────────────────────────────────");
        let builds_empty = show_once()?;
        println!("── argocd ──────────────────────────────────");
        let syncs_empty = sync_status_once()?;
        if builds_empty && syncs_empty {
            println!("\nAll builds deployed and synced.");
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs(FOLLOW_POLL_SECS));
    }
}

/// `sts --log`: print the last `n` lines of the watch log.
pub fn tail_log(n: usize) -> Result<()> {
    let path = log_file();
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("No watch log yet ({}).", path.display());
            return Ok(());
        }
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    for line in &lines[start..] {
        println!("{line}");
    }
    Ok(())
}

/// `sts -l [app]`: tail an ArgoCD app's container logs. Polls every
/// `LOGS_POLL_SECS` and appends only lines newer than the last one seen per pod,
/// so the terminal scrolls like `kubectl logs -f` instead of being redrawn.
///
/// `app` is optional: when omitted it is auto-detected from `repo` (the current
/// repo's Jenkins job -> Helm chart -> ArgoCD app, the same mapping the deploy
/// watcher uses). Apps can run across several namespaces, so when the app's pods
/// span more than one and no `namespace` was given, we ask which to view
/// (listing them). With a single namespace we use it; `namespace` can also be
/// passed to skip the prompt.
pub fn logs(
    app: Option<String>,
    repo: Option<String>,
    namespace: Option<String>,
    container: Option<String>,
) -> Result<()> {
    let Some(cfg) = config::load_argo()? else {
        bail!(
            "no ArgoCD token configured (argocd_token in {}); can't read logs",
            config::config_file().display()
        );
    };
    let argo = Argo::new(cfg);

    // Resolve the ArgoCD app: explicit name, or auto-detected from the repo.
    let app = match app {
        Some(a) => a,
        None => resolve_app_for_repo(&argo, repo.as_deref())?,
    };

    // Discover the app's pods (and the namespaces they run in) up front.
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

    let ns = choose_namespace(&app, namespace, &namespaces)?;

    let selected: Vec<&argocd::Pod> = pods
        .iter()
        .filter(|p| ns.as_deref().map(|n| p.namespace == n).unwrap_or(true))
        .collect();
    if selected.is_empty() {
        bail!(
            "no pods of `{app}` in namespace `{}` (have: {})",
            ns.unwrap_or_default(),
            namespaces.join(", ")
        );
    }

    println!(
        "sts -l {app}  —  {} pod(s){}{}  (polling every {LOGS_POLL_SECS}s, Ctrl-C to stop)",
        selected.len(),
        ns.as_deref()
            .map(|n| format!(" in {n}"))
            .unwrap_or_default(),
        container
            .as_deref()
            .map(|c| format!(", container {c}"))
            .unwrap_or_default(),
    );
    for p in &selected {
        let health = if p.health.is_empty() {
            String::new()
        } else {
            format!(" ({})", p.health)
        };
        println!("  {}{health}", p.name);
    }

    // Last timestamp printed per pod, so each poll only appends newer lines.
    let mut last_ts: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let multi = selected.len() > 1;
    let out = std::io::stdout();

    loop {
        for pod in &selected {
            let lines = match argo.logs(
                &app,
                &pod.namespace,
                &pod.name,
                container.as_deref(),
                LOGS_TAIL,
            ) {
                Ok(l) => l,
                Err(e) => {
                    // Transient error (pod restart, brief 5xx): note it and keep going.
                    eprintln!("  (logs for {} failed: {e:#})", pod.name);
                    continue;
                }
            };

            let seen = last_ts.get(&pod.name).cloned().unwrap_or_default();
            let mut newest = seen.clone();
            let mut handle = out.lock();
            for line in &lines {
                // Only append lines strictly after the last timestamp we printed
                // for this pod. Timestamps are RFC3339, so lexicographic order
                // matches chronological order. Lines with no timestamp are only
                // printed on the first poll (when `seen` is empty).
                let newer = if line.ts.is_empty() {
                    seen.is_empty()
                } else {
                    line.ts.as_str() > seen.as_str()
                };
                if !newer {
                    continue;
                }
                if multi {
                    let _ = writeln!(handle, "[{}] {}", pod.name, line.content);
                } else {
                    let _ = writeln!(handle, "{}", line.content);
                }
                if line.ts > newest {
                    newest = line.ts.clone();
                }
            }
            let _ = handle.flush();
            drop(handle);
            if !newest.is_empty() {
                last_ts.insert(pod.name.clone(), newest);
            } else if seen.is_empty() {
                // First poll of a pod with no timestamps: mark it seen so we don't
                // reprint the same untimestamped tail every interval.
                last_ts.insert(pod.name.clone(), String::new());
            }
        }
        std::thread::sleep(Duration::from_secs(LOGS_POLL_SECS));
    }
}

/// Auto-detect the ArgoCD app to tail from a repo: look up the repo's Jenkins
/// job(s), take each job's chart name (its last path segment) and map it to an
/// ArgoCD app the same way the deploy watcher does (`app_for_chart`). With one
/// app we use it; with several we ask on a terminal (or bail with the list when
/// piped). Errors clearly when the repo has no job or none of its charts match
/// an app, pointing at `-l <app>` as the fallback.
fn resolve_app_for_repo(argo: &Argo, repo: Option<&str>) -> Result<String> {
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
            println!("auto-detected app `{}` for {slug}", matched[0]);
            Ok(matched.remove(0))
        }
        _ => {
            if !std::io::stdin().is_terminal() {
                bail!(
                    "`{slug}` maps to several ArgoCD apps ({}); pass one: `sts -l <app>`",
                    matched.join(", ")
                );
            }
            println!("`{slug}` maps to several ArgoCD apps:");
            for (i, a) in matched.iter().enumerate() {
                println!("  {}. {a}", i + 1);
            }
            print!("Tail which app? [1-{}] ", matched.len());
            std::io::stdout().flush()?;
            let mut answer = String::new();
            std::io::stdin().lock().read_line(&mut answer)?;
            let answer = answer.trim();
            if let Ok(i) = answer.parse::<usize>()
                && (1..=matched.len()).contains(&i)
            {
                return Ok(matched.remove(i - 1));
            }
            if let Some(a) = matched.iter().find(|a| a.as_str() == answer) {
                return Ok(a.clone());
            }
            bail!("`{answer}` is not one of the listed apps");
        }
    }
}

/// Pick the namespace to view. With one namespace (or none reported) use it.
/// With several and no explicit choice, ask on a terminal (listing them) or, if
/// stdin isn't a terminal, bail with the list so the user can pass `-N`.
/// An explicit `wanted` must match one the app actually runs in.
fn choose_namespace(
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
            if !std::io::stdin().is_terminal() {
                bail!(
                    "`{app}` runs in {} namespaces ({}); pass -N <namespace> to pick one",
                    namespaces.len(),
                    namespaces.join(", ")
                );
            }
            println!("`{app}` runs in several namespaces:");
            for (i, n) in namespaces.iter().enumerate() {
                println!("  {}. {n}", i + 1);
            }
            print!("View which namespace? [1-{}] ", namespaces.len());
            std::io::stdout().flush()?;
            let mut answer = String::new();
            std::io::stdin().lock().read_line(&mut answer)?;
            let answer = answer.trim();
            // Accept either the number or the namespace name typed out.
            if let Ok(i) = answer.parse::<usize>()
                && (1..=namespaces.len()).contains(&i)
            {
                return Ok(Some(namespaces[i - 1].clone()));
            }
            if let Some(n) = namespaces.iter().find(|n| n.as_str() == answer) {
                return Ok(Some(n.clone()));
            }
            bail!("`{answer}` is not one of the listed namespaces");
        }
    }
}


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
    check_syncing(SyncMode::Print)
}

/// Whether a syncing-list pass should print to stdout (interactive `sts -S`) or
/// fire desktop/webhook notifications (the background cron watcher).
#[derive(Clone, Copy, PartialEq)]
enum SyncMode {
    /// `sts -S`: print each app's live status; no notifications.
    Print,
    /// cron `--watch`: silent, but notify + record history as apps settle or fail.
    Notify,
}

/// Shared core for both the interactive `sts -S` view and the cron watcher's
/// sync-completion check. Polls each tracked app, prunes the ones that have
/// settled (or vanished, or timed out), and in `Notify` mode fires a
/// notification and records history for each terminal outcome. Returns true
/// when nothing is left to track.
fn check_syncing(mode: SyncMode) -> Result<bool> {
    let print = mode == SyncMode::Print;
    let notify = mode == SyncMode::Notify;
    let mut q = SyncQueue::open()?;
    if print {
        println!("tracking: {}", syncing_file().display());
    }
    if q.items.is_empty() {
        if print {
            println!("No ArgoCD syncs in progress.");
        }
        return Ok(true);
    }

    let Some(argo_cfg) = config::load_argo()? else {
        if print {
            println!(
                "no ArgoCD token configured (argocd_token in config.toml); can't check sync status"
            );
        }
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
                done.push(format!("  gone      {}  (no such ArgoCD app)", s.app));
                if notify {
                    record_history(&s.repo, "APP_GONE", &s.app);
                }
            }
            Ok(Some(st)) if st.is_settled() => {
                done.push(format!("  synced    {}  ({})", s.app, st.health));
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
                done.push(format!("  degraded  {}  ({}/{})", s.app, st.sync, st.health));
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
                    println!(
                        "  syncing   {:<28} {}/{}{phase}  ({}s ago, {})",
                        s.app,
                        st.sync,
                        st.health,
                        t.saturating_sub(s.started_at),
                        s.repo,
                    );
                }
                keep.push(s);
            }
            Err(e) => {
                // Keep it; a transient error shouldn't lose the entry.
                if print {
                    println!("  ?         {}  (status check failed: {e:#})", s.app);
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
    if q.items.is_empty() {
        if print {
            println!("All tracked ArgoCD syncs have settled.");
        }
        return Ok(true);
    }
    Ok(false)
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
