# sts

Ship your branch to `stage` and trigger the Jenkins stage build from your machine,
without going through the build-trigger Lambda.

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

| mode (pick one) | |
|---|---|
| `-s, --ship` | merge current branch into stage, push, trigger Jenkins |
| `-b, --build` | trigger this repo's Jenkins job(s) only, no git |
| `-j, --jobs` | show this repo's Jenkins jobs (`-ja` for every repo) |
| `-c, --check` | test the config and credentials |

| option | |
|---|---|
| `-y, --yes` | no confirmation prompt |
| `-d, --dry-run` | show the plan; merge/push/build nothing |
| `-n, --no-build` | with `-s`: push only |
| `-p, --push-branch` | with `-s`: also push your branch to origin first |
| `-f, --from <branch>` | with `-s`: merge a branch other than the current one |
| `-r, --repo <owner/name>` | override the repo used for the job lookup |
| `-w, --wait <secs>` | how long to wait for build URLs (default 15, 0 = don't) |

## Setup

```sh
cargo install --path .
mkdir -p ~/.config/sts
cat > ~/.config/sts/config.toml <<'EOF'
url   = "https://jenkins-stage-aws.sdloki.in"
user  = "your-jenkins-username"
token = "your-jenkins-api-token"
EOF
chmod 600 ~/.config/sts/config.toml
sts -c
```

`JENKINS_URL`, `JENKINS_USER`, `JENKINS_TOKEN` (and optionally `JENKINS_CRUMB`)
override the file.

Create the API token in Jenkins under your user, then **Security** (or **Configure**),
then **API Token**, then **Add new Token**. Requests made with an API token don't
need a CSRF crumb. Set `crumb` only if Jenkins answers 403 asking for one.

## Job mapping

`jobs.toml` is the Lambda's `REPO_TO_JENKINS_JOB`, compiled into the binary.
A `?k=v` suffix calls `/buildWithParameters` with those parameters, the same
as the Lambda. To change the mapping without rebuilding, copy it to
`~/.config/sts/jobs.toml`; that file then replaces the built-in one.
