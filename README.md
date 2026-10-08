# sts

Ship your branch to `stage`, trigger the Jenkins stage build from your machine
(without going through the build-trigger Lambda), and sync ArgoCD once the build succeeds.

```sh
cd ~/home/soochi_dash   # any repo, on your feature branch
sts -s                  # merge into stage, push, trigger Jenkins (asks first)
sts -sy                 # same, no confirmation
```

## What `sts -s` does

1. Checks your Jenkins credentials (skipped with `-n` or when the repo has no job mapped).
2. Fetches `origin/stage`. Local `stage` is ignored: whatever is on GitHub wins.
3. Prints the commits that will land on stage and asks for confirmation.
4. Merges your branch (`--no-ff`) onto `origin/stage` in a temporary worktree,
   so your checkout and any uncommitted changes are left alone.
5. Pushes `stage`. It never force-pushes. If someone else pushed in the meantime,
   it re-fetches and re-merges (up to 3 tries).
6. Resets your local `stage` branch to the pushed commit.
7. Triggers the Jenkins job(s) mapped to the repo in `jobs.toml` and prints the build URLs.
   If the repo isn't mapped, it says so: pushed to stage, build not triggered.
8. Schedules the ArgoCD sync (unless `-A`, see below).

If `origin/stage` already contains your branch (nothing to merge), it skips the
merge and push and goes straight to triggering the Jenkins build, so you can
rebuild stage without a no-op commit. Use `-n` to skip the build in that case.

## ArgoCD sync after the build

Each triggered build is added to `~/.local/state/sts/pending.json`, and a crontab
entry (tagged `# sts-watch`) runs `sts --watch` every minute. Each run:

- waits for the build to leave the Jenkins queue and finish;
- on `SUCCESS`, reads the build's console for the chart it updated
  (`stage-argocd-helm/<chart>/values.yaml`), finds the ArgoCD app whose source path
  is that chart (e.g. chart `aadesh-app` is app `aadesh-app-test`), hard-refreshes it
  and syncs it. "Sync already in progress" (e.g. started by the Jenkinsfile) counts as done;
- on `FAILURE`/`ABORTED`/cancelled, drops it without deploying;
- retries a failed sync up to 3 times, and gives up on builds older than 4 hours.

When nothing is pending, the crontab entry removes itself. `sts -P` shows the queue;
on a terminal it refreshes in place every few seconds, dropping each build as the
watcher deploys it, and exits once the queue is empty. Piped or redirected it prints
a single snapshot instead. The log is `~/.local/state/sts/watch.log`.

Firing the sync only starts it; ArgoCD then rolls out in the background. Each app the
watcher syncs is recorded in `~/.local/state/sts/syncing.json`, and `sts -S` shows the
ones still in progress with their sync/health (e.g. `OutOfSync/Progressing`). On a
terminal it stays up and refreshes in place every few seconds, dropping each app as it
reaches `Synced` + `Healthy` (or stops existing), and exits by itself once the list is
empty — no need to re-run it. Piped or redirected it prints a single snapshot instead.
Only syncs this tool triggered are tracked, and an app that never settles is dropped
after 30 minutes.

## Tailing container logs

`sts -l <app>` streams an ArgoCD app's container logs straight from the ArgoCD
API — no `kubectl`, cluster context or pod names needed. It finds the app's pods
from its resource tree, then polls their logs every 2 seconds and **appends only
the new lines** to the terminal (it never clears or redraws the screen), so it
scrolls like `kubectl logs -f`. Ctrl-C to stop.

The app name is optional. Run `sts -l` with no argument inside a repo and it
auto-detects the app the same way the deploy watcher does: the repo's Jenkins
job → its Helm chart → the ArgoCD app that deploys that chart. If the repo maps
to several apps it asks which to tail (or, piped, lists them so you can pass one);
if it maps to none it tells you to name the app. `-r <owner/name>` picks the repo
to detect from.

Apps can run across several namespaces. If the app's pods span more than one and
you didn't pass `-N`, `sts` lists the namespaces and asks which to view; with a
single namespace it uses it. Pass `-N <namespace>` to pick one up front (required
when the output is piped, since there's nobody to ask), and `-C <container>` to
show just one container. It needs `argocd_token` configured with the `logs, get`
permission.

```sh
sts -l                                   # auto-detect the app from the current repo
sts -l soochi-dash-app-test              # name the app; ask for the namespace if there's more than one
sts -l soochi-dash-app-test -N stage     # view the stage namespace
sts -l soochi-dash-app-test -N stage -C app   # just the `app` container
```

If you run `sts -s` while on `stage` itself, it pulls `origin/stage` into your local
`stage`, pushes it back to `origin/stage` and triggers Jenkins. If the pull
conflicts, it is aborted and nothing is pushed.

The merge always goes one way: your branch into `stage`. `stage` is never merged
into your branch, because it carries code from other branches that doesn't belong there.

On a merge conflict, the merge is aborted: nothing is merged or pushed and your
branch is left unchanged. `sts` lists the conflicting files. If you want to resolve
the conflict, do it on a throwaway branch cut from `origin/stage`, not on your branch.

## Flags

Short flags combine: `-sy`, `-sd`, `-sny`, `-ja`.

| mode (pick one, or none for the dashboard) | |
|---|---|
| *(none)* | live dashboard: builds + ArgoCD syncs in progress (same as `-D`) |
| `-s, --ship` | merge current branch into stage, push, trigger Jenkins |
| `-b, --build` | trigger this repo's Jenkins job(s) only, no git |
| `-j, --jobs` | show this repo's Jenkins jobs (`-ja` for every repo) |
| `-c, --check` | test the Jenkins and ArgoCD config and credentials |
| `-P, --pending` | show builds waiting to be synced to ArgoCD |
| `-S, --sync-status` | show ArgoCD apps still syncing (drops each once Synced + Healthy) |
| `-l, --logs [app]` | tail an ArgoCD app's logs (auto-detects the app from the repo if omitted; polls every 2s) |
| `-D, --dashboard` | combined live view of pending builds + syncing apps |
| `-L, --log` | tail the watch log |
| `-H, --history` | show recent deploy history (filter with `-r`) |
| `-W, --watch` | run one watcher tick (what cron runs) |
| `--retry <owner/name>` | re-trigger a dropped build for a repo |
| `--completions <shell>` | print shell completions (bash, zsh, fish, elvish, powershell) |
| `-h, --help` | print help |
| `-V, --version` | print version |

| option | |
|---|---|
| `-y, --yes` | no confirmation prompt |
| `-d, --dry-run` | show the plan (with a diff summary); merge/push/build nothing |
| `-F, --follow` | with `-s`/`-b`: block and watch the build + sync to completion |
| `-n, --no-build` | with `-s`: push only |
| `-A, --no-argo` | with `-s`/`-b`: don't sync ArgoCD after the build |
| `-p, --push-branch` | with `-s`: also push your branch to origin first |
| `-f, --from <branch>` | with `-s`: merge a branch other than the current one |
| `-r, --repo <owner/name>` | override the repo used for the job lookup |
| `-N, --namespace <ns>` | with `-l`: which namespace to view (apps can span several) |
| `-C, --container <name>` | with `-l`: show just one container's logs |
| `-w, --wait <secs>` | how long to wait for build URLs (default 15, 0 = don't) |
| `-a, --all` | with `-j`: list every mapped repo |

Common combinations:

- `sts -sy` — ship, no confirmation
- `sts -sd` — dry run of a ship (shows commits + diff summary)
- `sts -sn` — ship without triggering Jenkins
- `sts -sF` — ship, then follow the build + sync to completion
- `sts -sny` — ship, push only, no prompt
- `sts -ja` — list jobs for every mapped repo

On a terminal, `sts`, `-P`, `-S`, `-D` and `--follow` stay up and refresh in place
until their queue empties (Ctrl-C to exit); piped or redirected they print a single
snapshot.

## Setup

```sh
cargo install --path .
mkdir -p ~/.config/sts
cat > ~/.config/sts/config.toml <<'EOF'
url   = "https://jenkins-stage-aws.sdloki.in"
user  = "your-jenkins-username"
token = "your-jenkins-api-token"
argocd_url   = "https://argocd-stage-aws.sdloki.in"   # default
argocd_token = "your-argocd-api-token"

# optional
default_repo = "owner/name"                 # used when the cwd isn't the repo
webhook_url  = "https://hooks.slack.com/…"  # POST failures here (Slack-compatible)
EOF
chmod 600 ~/.config/sts/config.toml
sts -c
```

`JENKINS_URL`, `JENKINS_USER`, `JENKINS_TOKEN` (and optionally `JENKINS_CRUMB`),
`ARGOCD_URL`, `ARGOCD_TOKEN`, `STS_DEFAULT_REPO` and `STS_WEBHOOK_URL` override the
file. Cron doesn't see your shell's environment, so keep the tokens in the file for
the ArgoCD sync to work. Without `argocd_token`, builds are triggered but ArgoCD
isn't synced.

Create the API token in Jenkins under your user, then **Security** (or **Configure**),
then **API Token**, then **Add new Token**. Requests made with an API token don't
need a CSRF crumb. Set `crumb` only if Jenkins answers 403 asking for one.

## Notifications

The background watcher notifies you as a deploy moves through its stages — build
succeeded (now syncing ArgoCD), app synced + healthy — and on every failure
(build failed/aborted, cancelled or lost in the queue, sync failed, gave up, or the
app ended up degraded). Each outcome is also written to the deploy history (`sts -H`).

Notifications are best-effort and never block a deploy:

- **Desktop:** uses `notify-send`. On WSL this needs WSLg (Windows 11, or WSL with the
  GUI stack) and `libnotify-bin` (`sudo apt install libnotify-bin`). Without it, sts
  falls back to a terminal bell and the watch log — the deploy still runs.
- **Webhook:** if `webhook_url` is set, failures are POSTed as a Slack-style
  `{"text": …}` message. Works for Slack and any compatible incoming webhook.

Shell completions: `sts --completions bash > ~/.local/share/bash-completion/completions/sts`
(or `zsh`/`fish`/…).

## Job mapping

`jobs.toml` is the Lambda's `REPO_TO_JENKINS_JOB`, compiled into the binary.
A `?k=v` suffix calls `/buildWithParameters` with those parameters, the same
as the Lambda. To change the mapping without rebuilding, copy it to
`~/.config/sts/jobs.toml`; that file then replaces the built-in one.
