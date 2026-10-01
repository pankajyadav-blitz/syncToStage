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

#[derive(Debug, Default, Deserialize)]
struct FileConfig {
    url: Option<String>,
    user: Option<String>,
    token: Option<String>,
    crumb: Option<String>,
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
    let file = if path.exists() {
        warn_if_readable_by_others(&path);
        let text =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        toml::from_str::<FileConfig>(&text).with_context(|| format!("parsing {}", path.display()))?
    } else {
        FileConfig::default()
    };

    let pick = |var: &str, from_file: Option<String>| {
        env::var(var)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .or(from_file.filter(|v| !v.trim().is_empty()))
            .map(|v| v.trim().to_string())
    };

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
