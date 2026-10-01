use anyhow::{Result, bail};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Output of a git command, with stdout and stderr captured.
pub struct GitOut {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

fn exec(dir: &Path, args: &[&str]) -> Result<GitOut> {
    let out: Output = Command::new("git").arg("-C").arg(dir).args(args).output()?;
    Ok(GitOut {
        ok: out.status.success(),
        stdout: String::from_utf8_lossy(&out.stdout).trim_end().to_string(),
        stderr: String::from_utf8_lossy(&out.stderr).trim_end().to_string(),
    })
}

/// Runs git and returns trimmed stdout, failing with git's stderr.
pub fn run(dir: &Path, args: &[&str]) -> Result<String> {
    let out = exec(dir, args)?;
    if !out.ok {
        bail!("`git {}` failed:\n{}", args.join(" "), out.stderr);
    }
    Ok(out.stdout)
}

/// Runs git, returning the outcome without failing on a non-zero exit.
pub fn try_run(dir: &Path, args: &[&str]) -> Result<GitOut> {
    exec(dir, args)
}

pub fn toplevel(cwd: &Path) -> Result<PathBuf> {
    match exec(cwd, &["rev-parse", "--show-toplevel"]) {
        Ok(out) if out.ok => Ok(PathBuf::from(out.stdout)),
        _ => bail!("not inside a git repository: {}", cwd.display()),
    }
}

pub fn current_branch(repo: &Path) -> Result<String> {
    let out = exec(repo, &["symbolic-ref", "--quiet", "--short", "HEAD"])?;
    if !out.ok || out.stdout.is_empty() {
        bail!("HEAD is detached; check out the branch you want to merge (or pass -f <branch>)");
    }
    Ok(out.stdout)
}

pub fn rev(repo: &Path, refname: &str) -> Option<String> {
    exec(repo, &["rev-parse", "--verify", "--quiet", &format!("{refname}^{{commit}}")])
        .ok()
        .filter(|o| o.ok)
        .map(|o| o.stdout)
}

/// `Blitzscale-19/soochi_dash` from `git@host-alias:Blitzscale-19/soochi_dash.git`
/// or `https://github.com/Blitzscale-19/soochi_dash.git`.
pub fn repo_slug_from_url(url: &str) -> Option<String> {
    let url = url.trim().trim_end_matches('/');
    let url = url.strip_suffix(".git").unwrap_or(url);
    let mut parts = url.rsplit(['/', ':']);
    let name = parts.next().filter(|s| !s.is_empty())?;
    let owner = parts.next().filter(|s| !s.is_empty())?;
    Some(format!("{owner}/{name}"))
}

pub fn repo_slug(repo: &Path, remote: &str) -> Result<String> {
    let url = run(repo, &["remote", "get-url", remote])?;
    match repo_slug_from_url(&url) {
        Some(slug) => Ok(slug),
        None => bail!("cannot work out owner/repo from remote URL {url:?}; pass --repo"),
    }
}

/// A detached worktree in a temp dir, removed on drop. Merging there leaves the
/// user's checkout (and any uncommitted work in it) untouched.
pub struct TempWorktree {
    repo: PathBuf,
    pub path: PathBuf,
}

impl TempWorktree {
    pub fn create(repo: &Path, at: &str) -> Result<TempWorktree> {
        let path = std::env::temp_dir().join(format!("sts-{}", std::process::id()));
        if path.exists() {
            let _ = exec(repo, &["worktree", "remove", "--force", &path.to_string_lossy()]);
            let _ = std::fs::remove_dir_all(&path);
        }
        run(
            repo,
            &["worktree", "add", "--quiet", "--detach", &path.to_string_lossy(), at],
        )?;
        Ok(TempWorktree {
            repo: repo.to_path_buf(),
            path,
        })
    }

    /// Moves the worktree to `at`, discarding any in-progress merge.
    pub fn reset_to(&self, at: &str) -> Result<()> {
        let _ = exec(&self.path, &["merge", "--abort"]);
        run(&self.path, &["reset", "--quiet", "--hard", at])?;
        Ok(())
    }
}

impl Drop for TempWorktree {
    fn drop(&mut self) {
        let p = self.path.to_string_lossy().to_string();
        let _ = exec(&self.repo, &["worktree", "remove", "--force", &p]);
        let _ = std::fs::remove_dir_all(&self.path);
        let _ = exec(&self.repo, &["worktree", "prune"]);
    }
}

#[cfg(test)]
mod tests {
    use super::repo_slug_from_url;

    #[test]
    fn slugs() {
        for (url, want) in [
            ("git@github.com:Blitzscale-19/soochi_dash.git", "Blitzscale-19/soochi_dash"),
            ("git@soochi_dash_github:Blitzscale-19/soochi_dash.git", "Blitzscale-19/soochi_dash"),
            ("https://github.com/Blitzscale-19/alfred.git", "Blitzscale-19/alfred"),
            ("https://github.com/Blitzscale-19/alfred/", "Blitzscale-19/alfred"),
            ("ssh://git@github.com/Blitzscale-19/naarad", "Blitzscale-19/naarad"),
        ] {
            assert_eq!(repo_slug_from_url(url).as_deref(), Some(want), "{url}");
        }
    }
}
