mod argocd;
mod config;
mod deploy;
mod git;
mod history;
mod jenkins;
mod jobs;
mod notify;
mod style;
mod tui;
mod ui;

use anyhow::{Context, Result, bail};
use clap::{ArgGroup, CommandFactory, Parser};
use clap_complete::Shell;
use jenkins::Jenkins;
use jobs::{Job, JobMap};
use std::io::IsTerminal;
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
        sts          live dashboard: builds + ArgoCD syncs in progress\n  \
        sts -s      ship: merge current branch into stage, push, build (asks first)\n  \
        sts -sy     same, no confirmation\n  \
        sts -sa     ship and trigger every job of a multi-job repo without asking\n  \
        sts -sd     dry run: show what -s would do (with a diff summary)\n  \
        sts -sn     ship without triggering Jenkins\n  \
        sts -sF     ship, then follow the build + sync to completion\n  \
        sts -b      trigger this repo's Jenkins build only (asks which job if several; -ba for all)\n  \
        sts -j      show this repo's Jenkins jobs (-ja for all)\n  \
        sts -c      check Jenkins/ArgoCD config and credentials\n  \
        sts -P      show builds waiting to be synced to ArgoCD\n  \
        sts -S      show ArgoCD apps still syncing\n  \
        sts -l <app> tail an ArgoCD app's container logs (-N <namespace> to pick one)\n  \
        sts -L      tail the watch log\n  \
        sts -H      show recent deploy history\n  \
        sts --retry <repo>       re-trigger a dropped build\n  \
        sts --completions bash   print shell completions\n\n\
After a build is triggered, a cron job (`sts --watch`, every minute) waits for it\n\
to finish and, if it succeeded, syncs its ArgoCD app. Skip that with -A.",
    group(ArgGroup::new("mode").required(false).args([
        "ship", "build", "jobs", "check", "pending", "watch", "sync_status",
        "dashboard", "log", "logs", "history", "retry", "completions",
    ]))
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
    /// Check the Jenkins and ArgoCD config and credentials
    #[arg(short = 'c', long)]
    check: bool,
    /// Show builds waiting to be synced to ArgoCD
    #[arg(short = 'P', long)]
    pending: bool,
    /// Run one deploy-watcher tick (what the cron job runs)
    #[arg(short = 'W', long)]
    watch: bool,
    /// Watch ArgoCD apps still syncing, refreshing until each is Synced + Healthy
    #[arg(short = 'S', long = "sync-status")]
    sync_status: bool,
    /// Combined live dashboard of builds + ArgoCD syncs (the default with no mode)
    #[arg(short = 'D', long)]
    dashboard: bool,
    /// Tail the deploy-watcher log
    #[arg(short = 'L', long)]
    log: bool,
    /// Show recent deploy history (optionally filtered by the -r repo)
    #[arg(short = 'H', long)]
    history: bool,
    /// Tail an ArgoCD app's container logs; the app is optional (auto-detected
    /// from the current repo's Jenkins job when omitted). E.g. -l soochi-dash-app-test
    #[arg(short = 'l', long, value_name = "APP", num_args = 0..=1, default_missing_value = "")]
    logs: Option<Option<String>>,
    /// With -l: which namespace to view (apps can span several)
    #[arg(short = 'N', long, value_name = "NAMESPACE")]
    namespace: Option<String>,
    /// With -l: a single container to show (default: the pod's default container)
    #[arg(short = 'C', long, value_name = "CONTAINER")]
    container: Option<String>,
    /// Re-trigger a dropped build for a repo (owner/name)
    #[arg(long, value_name = "OWNER/NAME")]
    retry: Option<String>,
    /// Print shell completions for the given shell and exit
    #[arg(long, value_name = "SHELL")]
    completions: Option<Shell>,

    /// Don't ask for confirmation
    #[arg(short = 'y', long)]
    yes: bool,
    /// Show what would happen; don't merge, push or build
    #[arg(short = 'd', long)]
    dry_run: bool,
    /// With -s/-b: block and follow the build + ArgoCD sync to completion
    #[arg(short = 'F', long)]
    follow: bool,
    /// With -s: push to stage but don't trigger Jenkins
    #[arg(short = 'n', long, conflicts_with_all = ["build", "jobs", "check", "pending", "watch", "sync_status"])]
    no_build: bool,
    /// With -s/-b: don't sync ArgoCD after the build finishes
    #[arg(short = 'A', long, conflicts_with_all = ["jobs", "check", "pending", "watch", "sync_status"])]
    no_argo: bool,
    /// With -s: also push your branch to origin before merging
    #[arg(short = 'p', long, conflicts_with_all = ["build", "jobs", "check", "pending", "watch", "sync_status"])]
    push_branch: bool,
    /// With -s: branch to merge into stage (default: the current branch)
    #[arg(short = 'f', long, value_name = "BRANCH", conflicts_with_all = ["build", "jobs", "check", "pending", "watch", "sync_status"])]
    from: Option<String>,
    /// GitHub repo as owner/name for the job lookup (default: from the origin remote)
    #[arg(short = 'r', long, value_name = "OWNER/NAME")]
    repo: Option<String>,
    /// Seconds to wait for Jenkins to start each build and print its URL (0 = don't wait)
    #[arg(short = 'w', long, value_name = "SECS", default_value_t = 15)]
    wait: u64,
    /// With -j: list every mapped repo. With -s/-b/--retry: trigger all of the
    /// repo's Jenkins jobs instead of asking which
    #[arg(short = 'a', long, conflicts_with_all = ["check", "pending", "watch", "sync_status"])]
    all: bool,
}

struct ShipArgs {
    from: Option<String>,
    repo: Option<String>,
    no_build: bool,
    no_argo: bool,
    push_branch: bool,
    dry_run: bool,
    yes: bool,
    wait: u64,
    follow: bool,
    all: bool,
}

struct BuildArgs {
    repo: Option<String>,
    no_argo: bool,
    dry_run: bool,
    wait: u64,
    follow: bool,
    all: bool,
}

fn main() {
    let cli = Cli::parse();
    let result = if let Some(shell) = cli.completions {
        print_completions(shell);
        Ok(())
    } else if let Some(repo) = cli.retry {
        with_ui("retry", |ui| {
            build(ui, BuildArgs {
                repo: Some(repo),
                no_argo: cli.no_argo,
                dry_run: cli.dry_run,
                wait: cli.wait,
                follow: cli.follow,
                all: cli.all,
            })
        })
    } else if let Some(app_opt) = cli.logs {
        logs(app_opt, cli.repo, cli.namespace, cli.container)
    } else if cli.ship {
        with_ui("ship", |ui| {
            ship(ui, ShipArgs {
                from: cli.from,
                repo: cli.repo,
                no_build: cli.no_build,
                no_argo: cli.no_argo,
                push_branch: cli.push_branch,
                dry_run: cli.dry_run,
                yes: cli.yes,
                wait: cli.wait,
                follow: cli.follow,
                all: cli.all,
            })
        })
    } else if cli.build {
        with_ui("build", |ui| {
            build(ui, BuildArgs {
                repo: cli.repo,
                no_argo: cli.no_argo,
                dry_run: cli.dry_run,
                wait: cli.wait,
                follow: cli.follow,
                all: cli.all,
            })
        })
    } else if cli.jobs {
        with_ui("jobs", |ui| list_jobs(ui, cli.repo, cli.all))
    } else if cli.pending {
        deploy::show()
    } else if cli.sync_status {
        deploy::sync_status()
    } else if cli.watch {
        deploy::watch_loop()
    } else if cli.log {
        deploy::tail_log(200)
    } else if cli.history {
        with_ui("history", |ui| history::show(ui, cli.repo.as_deref()))
    } else if cli.dashboard {
        deploy::dashboard()
    } else if cli.check {
        with_ui("check", check)
    } else if cli.follow {
        deploy::follow()
    } else {
        // Bare `sts`: the combined live dashboard.
        deploy::dashboard()
    };
    if let Err(e) = result {
        eprintln!("{} {e:#}", style::bold_red("error:"));
        std::process::exit(1);
    }
}

fn print_completions(shell: Shell) {
    let mut cmd = Cli::command();
    let name = cmd.get_name().to_string();
    clap_complete::generate(shell, &mut cmd, name, &mut std::io::stdout());
}

/// Run a command `body` through a [`Ui`](ui::Ui), choosing the full-screen TUI
/// runner when stdout is a real terminal and the plain-text backend when it is
/// piped / redirected (so scripting, CI and `--completions` keep working).
/// `title` labels the TUI runner screen.
fn with_ui<F>(title: &str, body: F) -> Result<()>
where
    F: FnOnce(&mut dyn ui::Ui) -> Result<()>,
{
    if std::io::stdout().is_terminal() {
        // The runner reports whether the command asked to follow; if so, open
        // the live dashboard on a fresh screen now that the runner has torn
        // down (two alternate screens can't be nested).
        let follow = tui::run_command(title, body)?;
        if follow {
            deploy::follow()?;
        }
        Ok(())
    } else {
        let mut term = ui::TermUi::new();
        body(&mut term)
    }
}

fn ship(ui: &mut dyn ui::Ui, a: ShipArgs) -> Result<()> {
    let repo_dir = repo_root()?;
    let source = match &a.from {
        Some(b) => b.clone(),
        None => git::current_branch(&repo_dir)?,
    };
    if source == STAGE {
        if git::current_branch(&repo_dir).ok().as_deref() != Some(STAGE) {
            bail!("check out `{STAGE}` to ship it directly (or pass -f <branch>)");
        }
        return ship_stage(ui, a, &repo_dir);
    }
    let source_sha = git::rev(&repo_dir, &format!("refs/heads/{source}"))
        .with_context(|| format!("no local branch named `{source}`"))?;
    let (slug, jobs, jenkins, argo) =
        prepare_jenkins(ui, &repo_dir, a.repo.clone(), a.no_build, a.no_argo)?;

    ui.step(&format!("Fetching {REMOTE}/{STAGE}"));
    fetch_stage(&repo_dir)?;
    let remote_stage = format!("{REMOTE}/{STAGE}");
    let remote_stage_sha = git::rev(&repo_dir, &remote_stage)
        .with_context(|| format!("{remote_stage} does not exist"))?;

    let incoming = git::run(
        &repo_dir,
        &[
            "log",
            "--oneline",
            "--no-decorate",
            &format!("{remote_stage}..refs/heads/{source}"),
        ],
    )?;
    if incoming.is_empty() {
        ui.info(&format!(
            "Nothing to merge: {remote_stage} already contains `{source}`."
        ));
        if a.no_build {
            ui.info("Nothing to build (-n); stage is already up to date.");
            return Ok(());
        }
        if a.dry_run {
            ui.info("Dry run: stage is up to date; would trigger the Jenkins build.");
            print_jenkins_plan(ui, &slug, &jobs, a.no_build, a.all);
            print_argo_plan(ui, &jobs, a.no_build, a.no_argo, argo);
            return Ok(());
        }
        let jobs = choose_jobs(ui, jobs, a.no_build, a.all)?;
        ui.info("Triggering the Jenkins build to rebuild stage anyway.");
        return finish_build(ui, &slug, jobs, jenkins, a.no_build, argo, a.wait, a.follow);
    }

    // Summary of what is about to happen.
    ui.detail(&format!("repo    {slug}"));
    ui.detail(&format!("merge   {source} ({})", short(&source_sha)));
    ui.detail(&format!("into    {remote_stage} ({})", short(&remote_stage_sha)));
    print_commits(ui, &incoming);
    print_diff_summary(ui, &repo_dir, &remote_stage, &format!("refs/heads/{source}"));
    warn_local_stage(ui, &repo_dir, &remote_stage, &remote_stage_sha)?;
    let dirty = git::run(
        &repo_dir,
        &["status", "--porcelain", "--untracked-files=no"],
    )?;
    if !dirty.is_empty() {
        ui.detail("note    uncommitted changes in your working tree are NOT included");
    }
    print_jenkins_plan(ui, &slug, &jobs, a.no_build, a.all);
    print_argo_plan(ui, &jobs, a.no_build, a.no_argo, argo);

    if a.dry_run {
        ui.info("Dry run: nothing merged, pushed or built.");
        return Ok(());
    }
    let jobs = choose_jobs(ui, jobs, a.no_build, a.all)?;
    if !a.yes && !ui.confirm(&format!("Merge `{source}` into {STAGE} and push?"))? {
        bail!("aborted");
    }

    if a.push_branch {
        ui.step(&format!("Pushing `{source}` to {REMOTE}"));
        let out = git::try_run(
            &repo_dir,
            &["push", REMOTE, &format!("refs/heads/{source}")],
        )?;
        if !out.ok {
            bail!("pushing `{source}` failed:\n{}", out.stderr);
        }
    }

    let pushed = merge_and_push(ui, &repo_dir, &source, &remote_stage)?;
    ui.ok(&format!("Pushed {STAGE} at {}", short(&pushed)));
    update_local_stage(ui, &repo_dir, &pushed);

    finish_build(ui, &slug, jobs, jenkins, a.no_build, argo, a.wait, a.follow)
}

/// Shipping from `stage` itself: pull origin/stage into the checked-out local
/// stage, push it back, and build. On a conflict the pull is aborted and
/// nothing is pushed.
fn ship_stage(ui: &mut dyn ui::Ui, a: ShipArgs, repo_dir: &Path) -> Result<()> {
    let (slug, jobs, jenkins, argo) =
        prepare_jenkins(ui, repo_dir, a.repo.clone(), a.no_build, a.no_argo)?;

    ui.step(&format!("Fetching {REMOTE}/{STAGE}"));
    fetch_stage(repo_dir)?;
    let remote_stage = format!("{REMOTE}/{STAGE}");
    let remote_stage_sha = git::rev(repo_dir, &remote_stage)
        .with_context(|| format!("{remote_stage} does not exist"))?;
    let local_sha =
        git::rev(repo_dir, &format!("refs/heads/{STAGE}")).context("reading local stage")?;

    let range_log =
        |range: String| git::run(repo_dir, &["log", "--oneline", "--no-decorate", &range]);
    let to_pull = range_log(format!("refs/heads/{STAGE}..{remote_stage}"))?;
    let to_push = range_log(format!("{remote_stage}..refs/heads/{STAGE}"))?;

    ui.detail(&format!("repo    {slug}"));
    ui.detail(&format!("local   {STAGE} ({})", short(&local_sha)));
    ui.detail(&format!("remote  {remote_stage} ({})", short(&remote_stage_sha)));
    ui.detail(&format!(
        "pull    {} commit(s) from {remote_stage}",
        to_pull.lines().count()
    ));
    if to_push.is_empty() {
        ui.detail(&format!(
            "push    nothing new (local {STAGE} has no commits {remote_stage} lacks)"
        ));
    } else {
        ui.detail(&format!("push    {} local commit(s):", to_push.lines().count()));
        for l in to_push.lines().take(15) {
            ui.detail(&format!("          {l}"));
        }
    }
    print_jenkins_plan(ui, &slug, &jobs, a.no_build, a.all);
    print_argo_plan(ui, &jobs, a.no_build, a.no_argo, argo);

    if a.dry_run {
        ui.info("Dry run: nothing pulled, pushed or built.");
        return Ok(());
    }
    let jobs = choose_jobs(ui, jobs, a.no_build, a.all)?;
    if !a.yes && !ui.confirm(&format!("Pull {remote_stage} into {STAGE}, push and build?"))? {
        bail!("aborted");
    }

    pull_and_push_stage(ui, repo_dir, &remote_stage)?;
    finish_build(ui, &slug, jobs, jenkins, a.no_build, argo, a.wait, a.follow)
}

/// Merges origin/stage into the checked-out local stage and pushes it.
/// Retries if someone else pushed in the meantime. Never force-pushes.
fn pull_and_push_stage(ui: &mut dyn ui::Ui, repo_dir: &Path, remote_stage: &str) -> Result<()> {
    const ATTEMPTS: u32 = 3;
    for attempt in 1..=ATTEMPTS {
        if attempt > 1 {
            fetch_stage(repo_dir)?;
        }

        ui.step(&format!("Pulling {remote_stage} into {STAGE}"));
        let merge = git::try_run(repo_dir, &["merge", "--no-edit", remote_stage])?;
        if !merge.ok {
            let conflicts =
                git::run(repo_dir, &["diff", "--name-only", "--diff-filter=U"]).unwrap_or_default();
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

        ui.step(&format!("Pushing to {REMOTE}/{STAGE}"));
        let push = git::try_run(
            repo_dir,
            &[
                "push",
                REMOTE,
                &format!("refs/heads/{STAGE}:refs/heads/{STAGE}"),
            ],
        )?;
        if push.ok {
            let sha = git::rev(repo_dir, &format!("refs/heads/{STAGE}")).unwrap_or_default();
            ui.ok(&format!("Pushed {STAGE} at {}", short(&sha)));
            return Ok(());
        }
        if push_raced(&push.stderr) && attempt < ATTEMPTS {
            ui.warn(&format!(
                "{STAGE} changed on {REMOTE} while pulling; retrying ({attempt}/{ATTEMPTS})"
            ));
            continue;
        }
        bail!("push to {STAGE} failed:\n{}", push.stderr);
    }
    unreachable!()
}

/// (repo slug, mapped jobs, Jenkins client if building, sync ArgoCD after the build)
type Prepared = (String, Option<Vec<Job>>, Option<Jenkins>, bool);

/// Looks up the repo's Jenkins jobs and, unless `no_build`, checks the login
/// before anything is pushed, so a bad token doesn't leave stage pushed but unbuilt.
/// The returned bool says whether to sync ArgoCD after the build.
fn prepare_jenkins(
    ui: &mut dyn ui::Ui,
    repo_dir: &Path,
    repo: Option<String>,
    no_build: bool,
    no_argo: bool,
) -> Result<Prepared> {
    let slug = resolve_slug(repo_dir, repo)?;
    let jobs = JobMap::load()?.lookup(&slug);
    let jenkins = match (&jobs, no_build) {
        (Some(_), false) => Some(connect(ui).context("use -n to push without building")?),
        _ => None,
    };
    let argo = jenkins.is_some() && !no_argo && argo_configured(ui)?;
    Ok((slug, jobs, jenkins, argo))
}

/// Whether an ArgoCD token is configured; warns (once, up front) when it isn't.
fn argo_configured(ui: &mut dyn ui::Ui) -> Result<bool> {
    if config::load_argo()?.is_some() {
        return Ok(true);
    }
    ui.warn(&format!(
        "no `argocd_token` in {}; ArgoCD will not be synced after the build",
        config::config_file().display()
    ));
    Ok(false)
}

fn print_argo_plan(ui: &mut dyn ui::Ui, jobs: &Option<Vec<Job>>, no_build: bool, no_argo: bool, argo: bool) {
    if jobs.is_none() || no_build {
        return;
    }
    if no_argo {
        ui.detail("argocd  skipped (-A)");
    } else if argo {
        ui.detail("argocd  sync each app when its build succeeds (cron, see `sts -P`)");
    } else {
        ui.detail("argocd  skipped (no argocd_token configured)");
    }
}

fn print_jenkins_plan(ui: &mut dyn ui::Ui, slug: &str, jobs: &Option<Vec<Job>>, no_build: bool, all: bool) {
    match (jobs, no_build) {
        (_, true) => ui.detail("jenkins skipped (-n)"),
        (None, _) => ui.detail(&format!("jenkins no job mapped for {slug} (see `sts -ja`)")),
        (Some(jobs), _) if jobs.len() > 1 && !all => {
            ui.detail(&format!(
                "jenkins {} job(s), will ask which to trigger (-a for all):",
                jobs.len()
            ));
            for j in jobs {
                ui.detail(&format!("          {}", j.raw));
            }
        }
        (Some(jobs), _) => {
            ui.detail(&format!("jenkins {} job(s):", jobs.len()));
            for j in jobs {
                ui.detail(&format!("          {}", j.raw));
            }
        }
    }
}

/// When a repo maps to several Jenkins jobs, ask which to trigger via the UI's
/// chooser; `-a` (or a single/zero job, or `-n`) skips the question. Asked
/// before anything is pushed, so a cancelled choice leaves stage untouched.
fn choose_jobs(
    ui: &mut dyn ui::Ui,
    jobs: Option<Vec<Job>>,
    no_build: bool,
    all: bool,
) -> Result<Option<Vec<Job>>> {
    let Some(jobs) = jobs else {
        return Ok(None);
    };
    if no_build || all || jobs.len() <= 1 {
        return Ok(Some(jobs));
    }
    let items = ui::job_items(&jobs);
    let picked = ui.choose("Trigger which Jenkins job(s)?", &items, true)?;
    Ok(Some(
        picked.into_iter().map(|i| jobs[i].clone()).collect(),
    ))
}

#[allow(clippy::too_many_arguments)]
fn finish_build(
    ui: &mut dyn ui::Ui,
    slug: &str,
    jobs: Option<Vec<Job>>,
    jenkins: Option<Jenkins>,
    no_build: bool,
    argo: bool,
    wait: u64,
    follow: bool,
) -> Result<()> {
    if no_build {
        ui.info(&format!("Pushed to {STAGE}; build not triggered (-n)."));
        return Ok(());
    }
    match (jobs, jenkins) {
        (Some(jobs), Some(jenkins)) => trigger_all(ui, &jenkins, slug, &jobs, argo, wait, follow),
        _ => {
            ui.info(&format!(
                "Pushed to {STAGE}, but no Jenkins job is mapped for {slug}; build not triggered."
            ));
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
fn merge_and_push(
    ui: &mut dyn ui::Ui,
    repo_dir: &Path,
    source: &str,
    remote_stage: &str,
) -> Result<String> {
    const ATTEMPTS: u32 = 3;
    let wt = git::TempWorktree::create(repo_dir, remote_stage)?;
    let message = format!("Merge branch '{source}' into {STAGE}");
    let source_ref = format!("refs/heads/{source}");

    for attempt in 1..=ATTEMPTS {
        if attempt > 1 {
            fetch_stage(repo_dir)?;
            wt.reset_to(remote_stage)?;
        }

        ui.step(&format!("Merging `{source}` into {remote_stage}"));
        let merge = git::try_run(
            &wt.path,
            &["merge", "--no-ff", "--no-edit", "-m", &message, &source_ref],
        )?;
        if !merge.ok {
            let conflicts =
                git::run(&wt.path, &["diff", "--name-only", "--diff-filter=U"]).unwrap_or_default();
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

        ui.step(&format!("Pushing to {REMOTE}/{STAGE}"));
        let push = git::try_run(
            &wt.path,
            &["push", REMOTE, &format!("HEAD:refs/heads/{STAGE}")],
        )?;
        if push.ok {
            return git::rev(&wt.path, "HEAD").context("reading merged commit");
        }
        if push_raced(&push.stderr) && attempt < ATTEMPTS {
            ui.warn(&format!(
                "{STAGE} changed on {REMOTE} while merging; retrying ({attempt}/{ATTEMPTS})"
            ));
            continue;
        }
        bail!("push to {STAGE} failed:\n{}", push.stderr);
    }
    unreachable!()
}

fn build(ui: &mut dyn ui::Ui, a: BuildArgs) -> Result<()> {
    let slug = match a.repo {
        Some(r) => r,
        None => match repo_root() {
            Ok(dir) => resolve_slug(&dir, None)?,
            Err(e) => config::default_repo().ok_or(e)?,
        },
    };
    let map = JobMap::load()?;
    let Some(jobs) = map.lookup(&slug) else {
        ui.info(&format!("No Jenkins job is mapped for {slug}; nothing to trigger."));
        return Ok(());
    };
    if a.dry_run {
        if jobs.len() > 1 && !a.all {
            ui.info(&format!(
                "Would ask which of {} job(s) to trigger for {slug} (-a for all):",
                jobs.len()
            ));
        } else {
            ui.info(&format!("Would trigger {} job(s) for {slug}:", jobs.len()));
        }
        for j in &jobs {
            ui.detail(&j.raw);
        }
        print_argo_plan(
            ui,
            &Some(jobs),
            false,
            a.no_argo,
            !a.no_argo && config::load_argo()?.is_some(),
        );
        return Ok(());
    }
    let jobs = choose_jobs(ui, Some(jobs), false, a.all)?.unwrap_or_default();
    let jenkins = connect(ui)?;
    let argo = !a.no_argo && argo_configured(ui)?;
    trigger_all(ui, &jenkins, &slug, &jobs, argo, a.wait, a.follow)
}

fn trigger_all(
    ui: &mut dyn ui::Ui,
    jenkins: &Jenkins,
    slug: &str,
    jobs: &[Job],
    argo: bool,
    wait: u64,
    follow: bool,
) -> Result<()> {
    ui.step(&format!("Triggering {} Jenkins job(s)", jobs.len()));
    let mut queued = Vec::new();
    let mut failed = 0;
    for job in jobs {
        match jenkins.trigger(job) {
            Ok(q) => {
                ui.ok(&format!("queued  {}", job.raw));
                queued.push((job, q));
            }
            Err(e) => {
                failed += 1;
                ui.err(&format!("FAILED  {}\n           {e:#}", job.raw));
            }
        }
    }

    let mut pending = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(wait);
    for (job, q) in &queued {
        let build = match (&q.queue_url, wait) {
            (Some(u), 1..) => jenkins.wait_for_build(u, deadline),
            _ => None,
        };
        match &build {
            Some(url) => ui.ok(&format!("started  {url}console")),
            None if wait > 0 => ui.detail(&format!("waiting  {} (still queued)", q.job_url)),
            None => ui.detail(&format!("job      {}", q.job_url)),
        }
        if q.queue_url.is_some() || build.is_some() {
            pending.push(deploy::Pending::new(
                slug,
                &job.raw,
                &q.job_url,
                q.queue_url.clone(),
                build,
            ));
        } else if argo {
            ui.detail(&format!(
                "note     Jenkins gave no queue URL for {}; ArgoCD sync not scheduled",
                job.raw
            ));
        }
    }

    let scheduled = argo && !pending.is_empty();
    if scheduled {
        match deploy::enqueue(pending) {
            Ok(()) => ui.ok(&format!(
                "ArgoCD sync scheduled: cron checks every minute and syncs when the build \
                 succeeds (`sts -P` to see it, log: {})",
                deploy::log_file().display()
            )),
            Err(e) => ui.warn(&format!("could not schedule the ArgoCD sync: {e:#}")),
        }
    }

    if failed > 0 {
        bail!(
            "{failed} of {} Jenkins trigger(s) failed (anything already pushed stays pushed); \
             retry with `sts --retry {slug}`",
            jobs.len()
        );
    }

    // -F/--follow: watch the build + sync through to completion. The follow
    // view is its own full-screen dashboard, so inside the TUI runner we can't
    // nest it — instead we ask the runner to open it after it tears down. On
    // the plain path we block inline as before.
    if follow && scheduled {
        if ui.is_tui() {
            ui.info("Following the build + ArgoCD sync — opening the live dashboard…");
            ui.request_follow();
        } else {
            ui.step("Following the build + ArgoCD sync (Ctrl-C to detach; cron keeps watching)");
            deploy::follow()?;
        }
    }
    Ok(())
}
/// `sts -l [app]`: tail an ArgoCD app's container logs. The app is optional:
/// when omitted it is auto-detected from the current repo (via its Jenkins job),
/// so you can just run `sts -l` inside a repo. `-r` overrides the repo.
fn logs(
    app_opt: Option<String>,
    repo: Option<String>,
    namespace: Option<String>,
    container: Option<String>,
) -> Result<()> {
    let app = app_opt.filter(|a| !a.trim().is_empty());
    // With no explicit app, resolve the repo (from -r or the cwd) so
    // deploy::logs can map its Jenkins job to the ArgoCD app.
    let repo = if app.is_none() {
        match repo {
            Some(r) => Some(r),
            None => match repo_root() {
                Ok(dir) => Some(resolve_slug(&dir, None)?),
                Err(e) => Some(config::default_repo().ok_or(e)?),
            },
        }
    } else {
        repo
    };
    deploy::logs(app, repo, namespace, container)
}


fn list_jobs(ui: &mut dyn ui::Ui, repo: Option<String>, all: bool) -> Result<()> {
    let map = JobMap::load()?;
    ui.detail(&format!("mapping: {}", map.source));
    if all {
        for (repo, paths) in map.all() {
            ui.info(repo);
            for p in paths {
                ui.detail(p);
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
            ui.info(&slug);
            for j in jobs {
                ui.detail(&j.raw);
            }
        }
        None => ui.info(&format!("No Jenkins job is mapped for {slug}.")),
    }
    Ok(())
}

fn check(ui: &mut dyn ui::Ui) -> Result<()> {
    ui.detail(&format!("config: {}", config::config_file().display()));
    connect(ui)?;
    match config::load_argo()? {
        Some(cfg) => {
            let argo = argocd::Argo::new(cfg);
            let who = argo.whoami()?;
            ui.ok(&format!("ArgoCD {} as {who}", argo.url()));
        }
        None => ui.warn("ArgoCD: no argocd_token configured; builds won't be synced"),
    }
    let map = JobMap::load()?;
    ui.detail(&format!("mapping: {}", map.source));
    if let Ok(dir) = repo_root()
        && let Ok(slug) = git::repo_slug(&dir, REMOTE)
    {
        match map.lookup(&slug) {
            Some(jobs) => ui.info(&format!("this repo: {slug} -> {} job(s)", jobs.len())),
            None => ui.info(&format!("this repo: {slug} -> no job mapped")),
        }
    }
    Ok(())
}

fn connect(ui: &mut dyn ui::Ui) -> Result<Jenkins> {
    let jenkins = Jenkins::new(config::load()?);
    let who = jenkins.whoami()?;
    ui.ok(&format!("Jenkins {} as {who}", jenkins.url()));
    Ok(jenkins)
}

fn repo_root() -> Result<PathBuf> {
    git::toplevel(&std::env::current_dir()?)
}

fn resolve_slug(repo_dir: &Path, explicit: Option<String>) -> Result<String> {
    match explicit {
        Some(r) => Ok(r),
        None => match git::repo_slug(repo_dir, REMOTE) {
            Ok(slug) => Ok(slug),
            Err(e) => config::default_repo().ok_or(e),
        },
    }
}

/// Local `stage` is reset to whatever we push, so warn about commits only it has.
fn warn_local_stage(
    ui: &mut dyn ui::Ui,
    repo_dir: &Path,
    remote_stage: &str,
    remote_sha: &str,
) -> Result<()> {
    let Some(local) = git::rev(repo_dir, &format!("refs/heads/{STAGE}")) else {
        return Ok(());
    };
    if local == remote_sha {
        return Ok(());
    }
    let only_local = git::run(
        repo_dir,
        &[
            "log",
            "--oneline",
            "--no-decorate",
            &format!("{remote_stage}..refs/heads/{STAGE}"),
        ],
    )?;
    if !only_local.is_empty() {
        let n = only_local.lines().count();
        ui.warn(&format!(
            "local `{STAGE}` has {n} commit(s) not on {remote_stage}; they will be dropped"
        ));
        ui.detail(&format!(
            "(local {STAGE} is at {}; recover with `git branch {STAGE}-backup {local}`)",
            short(&local)
        ));
    }
    Ok(())
}

fn update_local_stage(ui: &mut dyn ui::Ui, repo_dir: &Path, sha: &str) {
    match git::try_run(repo_dir, &["branch", "--force", STAGE, sha]) {
        Ok(o) if o.ok => {
            let _ = git::try_run(
                repo_dir,
                &[
                    "branch",
                    "--quiet",
                    &format!("--set-upstream-to={REMOTE}/{STAGE}"),
                    STAGE,
                ],
            );
        }
        Ok(o) => ui.warn(&format!(
            "local `{STAGE}` not updated ({}); where it is checked out, run \
             `git reset --hard {REMOTE}/{STAGE}`",
            o.stderr.lines().last().unwrap_or("").trim()
        )),
        Err(_) => {}
    }
}

fn print_commits(ui: &mut dyn ui::Ui, log: &str) {
    const SHOW: usize = 15;
    let lines: Vec<&str> = log.lines().collect();
    ui.detail(&format!("commits {} new on stage:", lines.len()));
    for l in lines.iter().take(SHOW) {
        ui.detail(&format!("          {l}"));
    }
    if lines.len() > SHOW {
        ui.detail(&format!("          ... and {} more", lines.len() - SHOW));
    }
}

/// A one-line diff summary (files changed, insertions, deletions) for what the
/// merge would bring onto stage, from `git diff --shortstat base...tip`. The
/// three-dot range diffs tip against the merge base, matching what will land.
/// Best-effort: silent on error.
fn print_diff_summary(ui: &mut dyn ui::Ui, repo_dir: &Path, base: &str, tip: &str) {
    let range = format!("{base}...{tip}");
    if let Ok(stat) = git::run(repo_dir, &["diff", "--shortstat", &range]) {
        let stat = stat.trim();
        if !stat.is_empty() {
            // e.g. "8 files changed, 312 insertions(+), 45 deletions(-)"
            ui.detail(&format!("diff    {stat}"));
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The job chooser maps selected indices back to the right jobs; the choice
    /// parsing itself is tested in `ui::tests`.
    #[test]
    fn job_items_pair_name_and_raw() {
        let jobs: Vec<Job> = ["app", "dash"]
            .iter()
            .map(|n| Job::parse(&format!("/job/stage/job/x/job/x-{n}/build")))
            .collect();
        let items = ui::job_items(&jobs);
        assert_eq!(items[0].0, "x-app");
        assert_eq!(items[0].1, "/job/stage/job/x/job/x-app/build");
        assert_eq!(items[1].0, "x-dash");
    }
}
