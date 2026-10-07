use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// `$XDG_CONFIG_HOME/sts`, falling back to `~/.config/sts`.
pub fn config_dir() -> PathBuf {
    if let Ok(dir) = env::var("XDG_CONFIG_HOME")
        && !dir.is_empty()
    {
        return PathBuf::from(dir).join("sts");
    }
    PathBuf::from(env::var("HOME").unwrap_or_default())
        .join(".config")
        .join("sts")
}

pub fn config_file() -> PathBuf {
    config_dir().join("config.toml")
}

/// `$XDG_STATE_HOME/sts`, falling back to `~/.local/state/sts`. Holds the
/// pending-deploy queue and the watcher log.
pub fn state_dir() -> PathBuf {
    if let Ok(dir) = env::var("XDG_STATE_HOME")
        && !dir.is_empty()
    {
        return PathBuf::from(dir).join("sts");
    }
    PathBuf::from(env::var("HOME").unwrap_or_default())
        .join(".local")
        .join("state")
        .join("sts")
}

#[derive(Debug, Default, Deserialize)]
struct FileConfig {
    url: Option<String>,
    user: Option<String>,
    token: Option<String>,
    crumb: Option<String>,
    argocd_url: Option<String>,
    argocd_token: Option<String>,
    /// Repo slug (`owner/name`) used when `sts` can't find one from the cwd.
    default_repo: Option<String>,
    /// Slack-compatible incoming webhook; failures are POSTed here when set.
    webhook_url: Option<String>,
}

pub const DEFAULT_ARGOCD_URL: &str = "https://argocd-stage-aws.sdloki.in";

/// `default_repo` from the config file (or `STS_DEFAULT_REPO`), used when the
/// current directory isn't a git repo with a recognizable remote.
pub fn default_repo() -> Option<String> {
    let file = read_file().unwrap_or_default();
    pick("STS_DEFAULT_REPO", file.default_repo)
}

/// Slack-compatible incoming webhook for failure notifications, if configured
/// (file `webhook_url` or `STS_WEBHOOK_URL`).
pub fn webhook_url() -> Option<String> {
    let file = read_file().unwrap_or_default();
    pick("STS_WEBHOOK_URL", file.webhook_url)
}

#[derive(Debug, Clone)]
pub struct ArgoConfig {
    pub url: String,
    pub token: String,
}

fn read_file() -> Result<FileConfig> {
    let path = config_file();
    if !path.exists() {
        return Ok(FileConfig::default());
    }
    warn_if_readable_by_others(&path);
    let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    toml::from_str::<FileConfig>(&text).with_context(|| format!("parsing {}", path.display()))
}

/// Env var if set and non-blank, else the file value if non-blank.
fn pick(var: &str, from_file: Option<String>) -> Option<String> {
    env::var(var)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or(from_file.filter(|v| !v.trim().is_empty()))
        .map(|v| v.trim().to_string())
}

/// ArgoCD settings; `None` when no token is configured (sync is then skipped).
/// ARGOCD_URL / ARGOCD_TOKEN win over `argocd_url` / `argocd_token` in the file.
pub fn load_argo() -> Result<Option<ArgoConfig>> {
    let file = read_file()?;
    let Some(token) = pick("ARGOCD_TOKEN", file.argocd_token) else {
        return Ok(None);
    };
    let url = pick("ARGOCD_URL", file.argocd_url).unwrap_or_else(|| DEFAULT_ARGOCD_URL.into());
    Ok(Some(ArgoConfig {
        url: url.trim_end_matches('/').to_string(),
        token,
    }))
}

#[derive(Debug, Clone)]
pub struct JenkinsConfig {
    pub url: String,
    pub user: String,
    pub token: String,
    /// Only needed if Jenkins rejects API-token requests without a crumb.
    pub crumb: Option<String>,
}

/// Env vars (JENKINS_URL / JENKINS_USER / JENKINS_TOKEN / JENKINS_CRUMB) win over the file.
pub fn load() -> Result<JenkinsConfig> {
    let path = config_file();
    let file = read_file()?;

    let url = pick("JENKINS_URL", file.url);
    let user = pick("JENKINS_USER", file.user);
    let token = pick("JENKINS_TOKEN", file.token);
    let crumb = pick("JENKINS_CRUMB", file.crumb);

    let missing: Vec<&str> = [("url", &url), ("user", &user), ("token", &token)]
        .iter()
        .filter(|(_, v)| v.is_none())
        .map(|(k, _)| *k)
        .collect();
    if !missing.is_empty() {
        bail!(
            "Jenkins config missing: {}.\n\
             Create {} (chmod 600) with:\n\n  \
             url   = \"https://jenkins-stage-aws.sdloki.in\"\n  \
             user  = \"<your jenkins username>\"\n  \
             token = \"<your jenkins API token>\"\n\n\
             or set JENKINS_URL / JENKINS_USER / JENKINS_TOKEN.",
            missing.join(", "),
            path.display()
        );
    }

    Ok(JenkinsConfig {
        url: url.unwrap().trim_end_matches('/').to_string(),
        user: user.unwrap(),
        token: token.unwrap(),
        crumb,
    })
}

#[cfg(unix)]
fn warn_if_readable_by_others(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = fs::metadata(path)
        && meta.permissions().mode() & 0o077 != 0
    {
        eprintln!(
            "warning: {} is readable by other users; run `chmod 600 {}`",
            path.display(),
            path.display()
        );
    }
}

#[cfg(not(unix))]
fn warn_if_readable_by_others(_path: &Path) {}
