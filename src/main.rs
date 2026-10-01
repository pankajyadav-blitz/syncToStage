mod config;
mod git;
mod jenkins;
mod jobs;

use anyhow::{Context, Result, bail};
use clap::{ArgGroup, Parser};
use jenkins::Jenkins;
use jobs::{Job, JobMap};
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const REMOTE: &str = "origin";
const STAGE: &str = "stage";

#[derive(Parser)]
#[command(
    name = "sts",
    version,
    about = "Merge your branch into stage, push it, and trigger the Jenkins stage build",
    after_help = "Examples:\n  \
        sts -s      ship: merge current branch into stage, push, build (asks first)\n  \
        sts -sy     same, no confirmation\n  \
        sts -sd     dry run: show what -s would do\n  \
        sts -sn     ship without triggering Jenkins\n  \
        sts -b      trigger this repo's Jenkins build only\n  \
        sts -j      show this repo's Jenkins jobs (-ja for all)\n  \
        sts -c      check Jenkins config and credentials",
    group(ArgGroup::new("mode").required(true).args(["ship", "build", "jobs", "check"]))
)]
struct Cli {
    /// Ship: reset stage to origin/stage, merge your branch, push, trigger Jenkins
    #[arg(short = 's', long)]
    ship: bool,
    /// Build: trigger this repo's Jenkins job(s) only, no git
    #[arg(short = 'b', long)]
    build: bool,
    /// Show the Jenkins jobs mapped to this repo
    #[arg(short = 'j', long)]
    jobs: bool,
    /// Check the Jenkins config and credentials
    #[arg(short = 'c', long)]
    check: bool,

    /// Don't ask for confirmation
    #[arg(short = 'y', long)]
    yes: bool,
    /// Show what would happen; don't merge, push or build
    #[arg(short = 'd', long)]
    dry_run: bool,
    /// With -s: push to stage but don't trigger Jenkins
    #[arg(short = 'n', long, conflicts_with_all = ["build", "jobs", "check"])]
    no_build: bool,
    /// With -s: also push your branch to origin before merging
    #[arg(short = 'p', long, conflicts_with_all = ["build", "jobs", "check"])]
    push_branch: bool,
    /// With -s: branch to merge into stage (default: the current branch)
    #[arg(short = 'f', long, value_name = "BRANCH", conflicts_with_all = ["build", "jobs", "check"])]
    from: Option<String>,
    /// GitHub repo as owner/name for the job lookup (default: from the origin remote)
    #[arg(short = 'r', long, value_name = "OWNER/NAME")]
    repo: Option<String>,
    /// Seconds to wait for Jenkins to start each build and print its URL (0 = don't wait)
    #[arg(short = 'w', long, value_name = "SECS", default_value_t = 15)]
    wait: u64,
    /// With -j: list every mapped repo
    #[arg(short = 'a', long, conflicts_with_all = ["ship", "build", "check"])]
    all: bool,
}

struct ShipArgs {
    from: Option<String>,
    repo: Option<String>,
    no_build: bool,
    push_branch: bool,
    dry_run: bool,
    yes: bool,
    wait: u64,
}

struct BuildArgs {
    repo: Option<String>,
    dry_run: bool,
    wait: u64,
}

fn main() {
    let cli = Cli::parse();
    let result = if cli.ship {
        ship(ShipArgs {
            from: cli.from,
            repo: cli.repo,
            no_build: cli.no_build,
            push_branch: cli.push_branch,
            dry_run: cli.dry_run,
            yes: cli.yes,
            wait: cli.wait,
        })
    } else if cli.build {
        build(BuildArgs {
            repo: cli.repo,
            dry_run: cli.dry_run,
            wait: cli.wait,
        })
    } else if cli.jobs {
        list_jobs(cli.repo, cli.all)
    } else {
        check()
    };
    if let Err(e) = result {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn ship(a: ShipArgs) -> Result<()> {
    let repo_dir = repo_root()?;
    let source = match &a.from {
        Some(b) => b.clone(),
        None => git::current_branch(&repo_dir)?,
    };
    if source == STAGE {
        if git::current_branch(&repo_dir).ok().as_deref() != Some(STAGE) {
            bail!("check out `{STAGE}` to ship it directly (or pass -f <branch>)");
        }
        return ship_stage(a, &repo_dir);
    }
    let source_sha = git::rev(&repo_dir, &format!("refs/heads/{source}"))
        .with_context(|| format!("no local branch named `{source}`"))?;
    let (slug, jobs, jenkins) = prepare_jenkins(&repo_dir, a.repo.clone(), a.no_build)?;

    step(&format!("Fetching {REMOTE}/{STAGE}"));
    fetch_stage(&repo_dir)?;
    let remote_stage = format!("{REMOTE}/{STAGE}");
    let remote_stage_sha = git::rev(&repo_dir, &remote_stage)
        .with_context(|| format!("{remote_stage} does not exist"))?;

    let incoming = git::run(
        &repo_dir,
        &["log", "--oneline", "--no-decorate", &format!("{remote_stage}..refs/heads/{source}")],
    )?;
    if incoming.is_empty() {
        println!("Nothing to merge: {remote_stage} already contains `{source}`.");
        println!("To rebuild stage anyway, run `sts -b`.");
        return Ok(());
    }

    // Summary of what is about to happen.
    println!();
    println!("  repo      {slug}");
    println!("  merge     {source} ({})", short(&source_sha));
    println!("  into      {remote_stage} ({})", short(&remote_stage_sha));
    print_commits(&incoming);
    warn_local_stage(&repo_dir, &remote_stage, &remote_stage_sha)?;
    let dirty = git::run(&repo_dir, &["status", "--porcelain", "--untracked-files=no"])?;
    if !dirty.is_empty() {
        println!("  note      uncommitted changes in your working tree are NOT included");
    }
    print_jenkins_plan(&slug, &jobs, a.no_build);
    println!();

    if a.dry_run {
        println!("Dry run: nothing merged, pushed or built.");
        return Ok(());
    }
    if !a.yes && !confirm(&format!("Merge `{source}` into {STAGE} and push?"))? {
        bail!("aborted");
    }

    if a.push_branch {
        step(&format!("Pushing `{source}` to {REMOTE}"));
        let out = git::try_run(&repo_dir, &["push", REMOTE, &format!("refs/heads/{source}")])?;
        if !out.ok {
            bail!("pushing `{source}` failed:\n{}", out.stderr);
        }
    }

    let pushed = merge_and_push(&repo_dir, &source, &remote_stage)?;
    println!("Pushed {STAGE} at {}", short(&pushed));
    update_local_stage(&repo_dir, &pushed);

    finish_build(&slug, jobs, jenkins, a.no_build, a.wait)
}

/// Shipping from `stage` itself: pull origin/stage into the checked-out local
/// stage, push it back, and build. On a conflict the pull is aborted and
/// nothing is pushed.
fn ship_stage(a: ShipArgs, repo_dir: &Path) -> Result<()> {
    let (slug, jobs, jenkins) = prepare_jenkins(repo_dir, a.repo.clone(), a.no_build)?;

    step(&format!("Fetching {REMOTE}/{STAGE}"));
    fetch_stage(repo_dir)?;
    let remote_stage = format!("{REMOTE}/{STAGE}");
    let remote_stage_sha = git::rev(repo_dir, &remote_stage)
        .with_context(|| format!("{remote_stage} does not exist"))?;
    let local_sha = git::rev(repo_dir, &format!("refs/heads/{STAGE}"))
        .context("reading local stage")?;

    let range_log = |range: String| {
        git::run(repo_dir, &["log", "--oneline", "--no-decorate", &range])
    };
    let to_pull = range_log(format!("refs/heads/{STAGE}..{remote_stage}"))?;
    let to_push = range_log(format!("{remote_stage}..refs/heads/{STAGE}"))?;

    println!();
    println!("  repo      {slug}");
    println!("  local     {STAGE} ({})", short(&local_sha));
    println!("  remote    {remote_stage} ({})", short(&remote_stage_sha));
    println!("  pull      {} commit(s) from {remote_stage}", to_pull.lines().count());
    if to_push.is_empty() {
        println!("  push      nothing new (local {STAGE} has no commits {remote_stage} lacks)");
    } else {
        println!("  push      {} local commit(s):", to_push.lines().count());
        for l in to_push.lines().take(15) {
            println!("              {l}");
        }
    }
    print_jenkins_plan(&slug, &jobs, a.no_build);
    println!();

    if a.dry_run {
        println!("Dry run: nothing pulled, pushed or built.");
        return Ok(());
    }
    if !a.yes && !confirm(&format!("Pull {remote_stage} into {STAGE}, push and build?"))? {
        bail!("aborted");
    }

    pull_and_push_stage(repo_dir, &remote_stage)?;
    finish_build(&slug, jobs, jenkins, a.no_build, a.wait)
}

/// Merges origin/stage into the checked-out local stage and pushes it.
/// Retries if someone else pushed in the meantime. Never force-pushes.
fn pull_and_push_stage(repo_dir: &Path, remote_stage: &str) -> Result<()> {
    const ATTEMPTS: u32 = 3;
    for attempt in 1..=ATTEMPTS {
        if attempt > 1 {
            fetch_stage(repo_dir)?;
        }

        step(&format!("Pulling {remote_stage} into {STAGE}"));
        let merge = git::try_run(repo_dir, &["merge", "--no-edit", remote_stage])?;
        if !merge.ok {
            let conflicts = git::run(repo_dir, &["diff", "--name-only", "--diff-filter=U"])
                .unwrap_or_default();
            if conflicts.is_empty() {
                bail!("pull failed:\n{}\n{}", merge.stdout, merge.stderr);
            }
            let _ = git::try_run(repo_dir, &["merge", "--abort"]);
            bail!(
                "merge conflict between local {STAGE} and {remote_stage} in:\n{}\n\n\
                 Pull aborted: nothing was merged or pushed, and local {STAGE} was not changed.",
                indent(&conflicts)
            );
        }

        step(&format!("Pushing to {REMOTE}/{STAGE}"));
        let push = git::try_run(
            repo_dir,
            &["push", REMOTE, &format!("refs/heads/{STAGE}:refs/heads/{STAGE}")],
        )?;
        if push.ok {
            let sha = git::rev(repo_dir, &format!("refs/heads/{STAGE}")).unwrap_or_default();
            println!("Pushed {STAGE} at {}", short(&sha));
            return Ok(());
        }
        if push_raced(&push.stderr) && attempt < ATTEMPTS {
            println!("{STAGE} changed on {REMOTE} while pulling; retrying ({attempt}/{ATTEMPTS})");
            continue;
        }
        bail!("push to {STAGE} failed:\n{}", push.stderr);
    }
    unreachable!()
}

/// Looks up the repo's Jenkins jobs and, unless `no_build`, checks the login
/// before anything is pushed, so a bad token doesn't leave stage pushed but unbuilt.
fn prepare_jenkins(
    repo_dir: &Path,
    repo: Option<String>,
    no_build: bool,
) -> Result<(String, Option<Vec<Job>>, Option<Jenkins>)> {
    let slug = resolve_slug(repo_dir, repo)?;
    let jobs = JobMap::load()?.lookup(&slug);
    let jenkins = match (&jobs, no_build) {
        (Some(_), false) => Some(connect().context("use -n to push without building")?),
        _ => None,
    };
    Ok((slug, jobs, jenkins))
}

fn print_jenkins_plan(slug: &str, jobs: &Option<Vec<Job>>, no_build: bool) {
    match (jobs, no_build) {
        (_, true) => println!("  jenkins   skipped (-n)"),
        (None, _) => println!("  jenkins   no job mapped for {slug} (see `sts -ja`)"),
        (Some(jobs), _) => {
            println!("  jenkins   {} job(s):", jobs.len());
            for j in jobs {
                println!("              {}", j.raw);
            }
        }
    }
}

fn finish_build(
    slug: &str,
    jobs: Option<Vec<Job>>,
    jenkins: Option<Jenkins>,
    no_build: bool,
    wait: u64,
) -> Result<()> {
    if no_build {
        println!("Pushed to {STAGE}; build not triggered (-n).");
        return Ok(());
    }
    match (jobs, jenkins) {
        (Some(jobs), Some(jenkins)) => trigger_all(&jenkins, &jobs, wait),
        _ => {
            println!(
                "Pushed to {STAGE}, but no Jenkins job is mapped for {slug}; build not triggered."
            );
            Ok(())
        }
    }
}

/// "[rejected] (fetch first)" when stage moved before the push started;
/// "cannot lock ref" when it moved while the push was in flight.
fn push_raced(stderr: &str) -> bool {
    [
        "[rejected]",
        "non-fast-forward",
        "fetch first",
        "cannot lock ref",
        "(failed to update ref)",
    ]
    .iter()
    .any(|s| stderr.contains(s))
}

fn fetch_stage(repo_dir: &Path) -> Result<()> {
    git::run(
        repo_dir,
        &[
            "fetch",
            "--quiet",
            REMOTE,
            &format!("+refs/heads/{STAGE}:refs/remotes/{REMOTE}/{STAGE}"),
        ],
    )?;
    Ok(())
}

/// Merges `source` onto a fresh copy of origin/stage in a temp worktree and pushes it.
/// Retries if someone else pushed to stage in the meantime. Never force-pushes.
fn merge_and_push(repo_dir: &Path, source: &str, remote_stage: &str) -> Result<String> {
    const ATTEMPTS: u32 = 3;
    let wt = git::TempWorktree::create(repo_dir, remote_stage)?;
    let message = format!("Merge branch '{source}' into {STAGE}");
    let source_ref = format!("refs/heads/{source}");

    for attempt in 1..=ATTEMPTS {
        if attempt > 1 {
            fetch_stage(repo_dir)?;
            wt.reset_to(remote_stage)?;
        }

        step(&format!("Merging `{source}` into {remote_stage}"));
        let merge = git::try_run(
            &wt.path,
            &["merge", "--no-ff", "--no-edit", "-m", &message, &source_ref],
        )?;
        if !merge.ok {
            let conflicts = git::run(&wt.path, &["diff", "--name-only", "--diff-filter=U"])
                .unwrap_or_default();
            let _ = git::try_run(&wt.path, &["merge", "--abort"]);
            if conflicts.is_empty() {
                bail!("merge failed:\n{}\n{}", merge.stdout, merge.stderr);
            }
            // Never suggest merging stage into the source branch: stage carries
            // code from other branches that must not leak into it.
            bail!(
                "merge conflict between `{source}` and {remote_stage} in:\n{}\n\n\
                 Merge aborted: nothing was merged or pushed, and `{source}` was not changed.\n\
                 Do NOT merge {STAGE} into `{source}`. To resolve it on {STAGE} instead:\n  \
                 git switch -c {STAGE}-merge-{source} {remote_stage}\n  \
                 git merge {source}\n  \
                 # resolve, commit\n  \
                 git push {REMOTE} HEAD:{STAGE}",
                indent(&conflicts)
            );
        }

        step(&format!("Pushing to {REMOTE}/{STAGE}"));
        let push = git::try_run(&wt.path, &["push", REMOTE, &format!("HEAD:refs/heads/{STAGE}")])?;
        if push.ok {
            return git::rev(&wt.path, "HEAD").context("reading merged commit");
        }
        if push_raced(&push.stderr) && attempt < ATTEMPTS {
            println!("{STAGE} changed on {REMOTE} while merging; retrying ({attempt}/{ATTEMPTS})");
            continue;
        }
        bail!("push to {STAGE} failed:\n{}", push.stderr);
    }
    unreachable!()
}

fn build(a: BuildArgs) -> Result<()> {
    let slug = match a.repo {
        Some(r) => r,
        None => resolve_slug(&repo_root()?, None)?,
    };
    let map = JobMap::load()?;
    let Some(jobs) = map.lookup(&slug) else {
        println!("No Jenkins job is mapped for {slug}; nothing to trigger.");
        return Ok(());
    };
    if a.dry_run {
        println!("Would trigger {} job(s) for {slug}:", jobs.len());
        for j in &jobs {
            println!("  {}", j.raw);
        }
        return Ok(());
    }
    let jenkins = connect()?;
    trigger_all(&jenkins, &jobs, a.wait)
}

fn trigger_all(jenkins: &Jenkins, jobs: &[Job], wait: u64) -> Result<()> {
    step(&format!("Triggering {} Jenkins job(s)", jobs.len()));
    let mut queued = Vec::new();
    let mut failed = 0;
    for job in jobs {
        match jenkins.trigger(job) {
            Ok(q) => {
                println!("  queued   {}", job.raw);
                queued.push(q);
            }
            Err(e) => {
                failed += 1;
                println!("  FAILED   {}\n           {e:#}", job.raw);
            }
        }
    }

    if wait > 0 && !queued.is_empty() {
        let deadline = Instant::now() + Duration::from_secs(wait);
        for q in &queued {
            let build = q
                .queue_url
                .as_deref()
                .and_then(|u| jenkins.wait_for_build(u, deadline));
            match build {
                Some(url) => println!("  started  {url}console"),
                None => println!("  waiting  {} (still queued)", q.job_url),
            }
        }
    } else {
        for q in &queued {
            println!("  job      {}", q.job_url);
        }
    }

    if failed > 0 {
        bail!(
            "{failed} of {} Jenkins trigger(s) failed (anything already pushed stays pushed); \
             retry with `sts -b`",
            jobs.len()
        );
    }
    Ok(())
}

fn list_jobs(repo: Option<String>, all: bool) -> Result<()> {
    let map = JobMap::load()?;
    println!("mapping: {}", map.source);
    if all {
        for (repo, paths) in map.all() {
            println!("{repo}");
            for p in paths {
                println!("  {p}");
            }
        }
        return Ok(());
    }
    let slug = match repo {
        Some(r) => r,
        None => resolve_slug(&repo_root()?, None)?,
    };
    match map.lookup(&slug) {
        Some(jobs) => {
            println!("{slug}");
            for j in jobs {
                println!("  {}", j.raw);
            }
        }
        None => println!("No Jenkins job is mapped for {slug}."),
    }
    Ok(())
}

fn check() -> Result<()> {
    println!("config: {}", config::config_file().display());
    connect()?;
    let map = JobMap::load()?;
    println!("mapping: {}", map.source);
    if let Ok(dir) = repo_root()
        && let Ok(slug) = git::repo_slug(&dir, REMOTE)
    {
        match map.lookup(&slug) {
            Some(jobs) => println!("this repo: {slug} -> {} job(s)", jobs.len()),
            None => println!("this repo: {slug} -> no job mapped"),
        }
    }
    Ok(())
}

fn connect() -> Result<Jenkins> {
    let jenkins = Jenkins::new(config::load()?);
    let who = jenkins.whoami()?;
    println!("Jenkins {} as {who}", jenkins.url());
    Ok(jenkins)
}

fn repo_root() -> Result<PathBuf> {
    git::toplevel(&std::env::current_dir()?)
}

fn resolve_slug(repo_dir: &Path, explicit: Option<String>) -> Result<String> {
    match explicit {
        Some(r) => Ok(r),
        None => git::repo_slug(repo_dir, REMOTE),
    }
}

/// Local `stage` is reset to whatever we push, so warn about commits only it has.
fn warn_local_stage(repo_dir: &Path, remote_stage: &str, remote_sha: &str) -> Result<()> {
    let Some(local) = git::rev(repo_dir, &format!("refs/heads/{STAGE}")) else {
        return Ok(());
    };
    if local == remote_sha {
        return Ok(());
    }
    let only_local = git::run(
        repo_dir,
        &["log", "--oneline", "--no-decorate", &format!("{remote_stage}..refs/heads/{STAGE}")],
    )?;
    if !only_local.is_empty() {
        let n = only_local.lines().count();
        println!(
            "  warning   local `{STAGE}` has {n} commit(s) not on {remote_stage}; they will be dropped"
        );
        println!(
            "            (local {STAGE} is at {}; recover with `git branch {STAGE}-backup {local}`)",
            short(&local)
        );
    }
    Ok(())
}

fn update_local_stage(repo_dir: &Path, sha: &str) {
    match git::try_run(repo_dir, &["branch", "--force", STAGE, sha]) {
        Ok(o) if o.ok => {
            let _ = git::try_run(
                repo_dir,
                &["branch", "--quiet", &format!("--set-upstream-to={REMOTE}/{STAGE}"), STAGE],
            );
        }
        Ok(o) => println!(
            "note: local `{STAGE}` not updated ({}); where it is checked out, run \
             `git reset --hard {REMOTE}/{STAGE}`",
            o.stderr.lines().last().unwrap_or("").trim()
        ),
        Err(_) => {}
    }
}

fn print_commits(log: &str) {
    const SHOW: usize = 15;
    let lines: Vec<&str> = log.lines().collect();
    println!("  commits   {} new on stage:", lines.len());
    for l in lines.iter().take(SHOW) {
        println!("              {l}");
    }
    if lines.len() > SHOW {
        println!("              ... and {} more", lines.len() - SHOW);
    }
}

fn confirm(question: &str) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        bail!("stdin is not a terminal; pass -y to skip the confirmation");
    }
    print!("{question} [y/N] ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    Ok(matches!(answer.trim().to_lowercase().as_str(), "y" | "yes"))
}

fn step(msg: &str) {
    println!("==> {msg}");
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(10)]
}

fn indent(text: &str) -> String {
    text.lines()
        .map(|l| format!("  {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}
