use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

const BUILTIN_JOBS: &str = include_str!("../jobs.toml");

#[derive(Debug, Deserialize)]
struct JobsFile {
    repos: BTreeMap<String, Vec<String>>,
}

/// One Jenkins job to trigger, e.g. `/job/stage/job/web/job/dashboard` with `ENV=stage`.
#[derive(Debug, Clone, PartialEq)]
pub struct Job {
    /// Path as written in the mapping (used for display).
    pub raw: String,
    /// `/build` or `/buildWithParameters` endpoint path.
    pub endpoint: String,
    pub params: Vec<(String, String)>,
}

impl Job {
    /// Mirrors the Lambda: `path?k=v` becomes `.../buildWithParameters` with form params.
    pub fn parse(raw: &str) -> Job {
        let (path, query) = match raw.split_once('?') {
            Some((p, q)) => (p, Some(q)),
            None => (raw, None),
        };
        let params: Vec<(String, String)> = query
            .map(|q| {
                q.split('&')
                    .filter(|kv| !kv.is_empty())
                    .map(|kv| match kv.split_once('=') {
                        Some((k, v)) => (url_decode(k), url_decode(v)),
                        None => (url_decode(kv), String::new()),
                    })
                    .collect()
            })
            .unwrap_or_default();

        let endpoint = if params.is_empty() {
            path.to_string()
        } else {
            match path.strip_suffix("/build") {
                Some(base) => format!("{base}/buildWithParameters"),
                None => path.to_string(),
            }
        };

        Job {
            raw: raw.to_string(),
            endpoint,
            params,
        }
    }

    /// The job's own page, e.g. `/job/stage/job/web/job/dashboard/`.
    pub fn job_path(&self) -> String {
        let p = self.endpoint.trim_end_matches('/');
        let p = p
            .strip_suffix("/buildWithParameters")
            .or_else(|| p.strip_suffix("/build"))
            .unwrap_or(p);
        format!("{p}/")
    }

    /// The job's last path segment, e.g. `dashboard` — usually the Helm chart
    /// name, which maps to an ArgoCD app via `argocd::app_for_chart`.
    pub fn name(&self) -> String {
        self.job_path()
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or("")
            .to_string()
    }
}

pub struct JobMap {
    repos: BTreeMap<String, Vec<String>>,
    pub source: String,
}

impl JobMap {
    /// Uses `~/.config/sts/jobs.toml` when present, otherwise the built-in mapping.
    pub fn load() -> Result<JobMap> {
        let user_file: PathBuf = crate::config::config_dir().join("jobs.toml");
        let (text, source) = if user_file.exists() {
            let text = fs::read_to_string(&user_file)
                .with_context(|| format!("reading {}", user_file.display()))?;
            (text, user_file.display().to_string())
        } else {
            (BUILTIN_JOBS.to_string(), "built-in jobs.toml".to_string())
        };
        let file: JobsFile =
            toml::from_str(&text).with_context(|| format!("parsing job mapping from {source}"))?;
        Ok(JobMap {
            repos: file.repos,
            source,
        })
    }

    /// Exact match first, then case-insensitive (GitHub repo names are case-insensitive).
    pub fn lookup(&self, repo: &str) -> Option<Vec<Job>> {
        let paths = self.repos.get(repo).or_else(|| {
            self.repos
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(repo))
                .map(|(_, v)| v)
        })?;
        Some(paths.iter().map(|p| Job::parse(p)).collect())
    }

    pub fn all(&self) -> impl Iterator<Item = (&String, &Vec<String>)> {
        self.repos.iter()
    }
}

fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 2;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_job_uses_build() {
        let j = Job::parse("/job/stage/job/soochi/job/soochi-dash/job/soochi-dash-app/build");
        assert_eq!(j.endpoint, "/job/stage/job/soochi/job/soochi-dash/job/soochi-dash-app/build");
        assert!(j.params.is_empty());
        assert_eq!(j.job_path(), "/job/stage/job/soochi/job/soochi-dash/job/soochi-dash-app/");
    }

    #[test]
    fn query_switches_to_build_with_parameters() {
        let j = Job::parse("/job/stage/job/web/job/dashboard/build?ENV=stage&X=a%20b+c");
        assert_eq!(j.endpoint, "/job/stage/job/web/job/dashboard/buildWithParameters");
        assert_eq!(
            j.params,
            vec![("ENV".into(), "stage".into()), ("X".into(), "a b c".into())]
        );
        assert_eq!(j.job_path(), "/job/stage/job/web/job/dashboard/");
    }

    #[test]
    fn builtin_mapping_parses_and_matches_lambda() {
        let m = JobMap {
            repos: toml::from_str::<JobsFile>(BUILTIN_JOBS).unwrap().repos,
            source: "test".into(),
        };
        assert_eq!(m.repos.len(), 43);
        assert_eq!(m.lookup("Blitzscale-19/soochi_dash").unwrap().len(), 2);
        assert_eq!(m.lookup("blitzscale-19/ALFRED").unwrap()[0].params.len(), 1);
        assert!(m.lookup("Blitzscale-19/gcp-infra").is_none());
    }
}
