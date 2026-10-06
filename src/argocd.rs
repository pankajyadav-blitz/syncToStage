use crate::config::ArgoConfig;
use anyhow::{Context, Result, bail};
use std::time::Duration;
use ureq::Agent;

const CHART_DIR: &str = "stage-argocd-helm/";

pub struct Argo {
    cfg: ArgoConfig,
    agent: Agent,
    auth: String,
}

/// An ArgoCD application and the chart paths of its sources.
pub struct App {
    pub name: String,
    pub paths: Vec<String>,
}

pub enum SyncOutcome {
    Started,
    /// "another operation is already in progress": a sync is running already
    /// (often the one the Jenkinsfile itself started).
    AlreadyRunning,
}

/// Live sync/health of an ArgoCD app, as shown by `sts -S`.
pub struct AppStatus {
    /// `Synced` / `OutOfSync` / `Unknown` (`status.sync.status`).
    pub sync: String,
    /// `Healthy` / `Progressing` / `Degraded` / ... (`status.health.status`).
    pub health: String,
    /// Phase of the running/last operation, e.g. `Running`, `Succeeded`, `Error`
    /// (`status.operationState.phase`); empty when there is none.
    pub phase: String,
}

impl AppStatus {
    /// Done once ArgoCD reports the app Synced and Healthy with no operation
    /// still running. These are the entries `sts -S` prunes.
    pub fn is_settled(&self) -> bool {
        self.sync == "Synced"
            && self.health == "Healthy"
            && !self.phase.eq_ignore_ascii_case("Running")
    }
}

impl Argo {
    pub fn new(cfg: ArgoConfig) -> Argo {
        let agent: Agent = Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(30)))
            .build()
            .into();
        let auth = format!("Bearer {}", cfg.token);
        Argo { cfg, agent, auth }
    }

    pub fn url(&self) -> &str {
        &self.cfg.url
    }

    /// Verifies the URL and token; returns the ArgoCD username.
    pub fn whoami(&self) -> Result<String> {
        let v = self.get_json("/api/v1/session/userinfo")?;
        if v["loggedIn"].as_bool() != Some(true) {
            bail!("ArgoCD at {} did not accept the token", self.cfg.url);
        }
        Ok(v["username"].as_str().unwrap_or("?").to_string())
    }

    pub fn apps(&self) -> Result<Vec<App>> {
        let v = self.get_json("/api/v1/applications")?;
        let items = v["items"].as_array().cloned().unwrap_or_default();
        Ok(items
            .iter()
            .map(|a| {
                let spec = &a["spec"];
                let sources: Vec<&serde_json::Value> = match spec["sources"].as_array() {
                    Some(s) => s.iter().collect(),
                    None => vec![&spec["source"]],
                };
                App {
                    name: a["metadata"]["name"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                    paths: sources
                        .iter()
                        .filter_map(|s| s["path"].as_str())
                        .map(str::to_string)
                        .collect(),
                }
            })
            .collect())
    }

    /// Hard refresh (so ArgoCD sees the image tag Jenkins just pushed), then sync.
    pub fn refresh_and_sync(&self, app: &str) -> Result<SyncOutcome> {
        let api = format!("{}/api/v1/applications/{app}", self.cfg.url);

        let refresh = format!("{api}?refresh=hard");
        let mut resp = self
            .agent
            .get(&refresh)
            .header("Authorization", &self.auth)
            .call()
            .with_context(|| format!("GET {refresh}"))?;
        let status = resp.status().as_u16();
        if status != 200 {
            let body = resp.body_mut().read_to_string().unwrap_or_default();
            bail!("refresh of {app}: HTTP {status}: {}", snippet(&body));
        }

        let sync = format!("{api}/sync");
        let mut resp = self
            .agent
            .post(&sync)
            .header("Authorization", &self.auth)
            .header("Content-Type", "application/json")
            .send("{}")
            .with_context(|| format!("POST {sync}"))?;
        let status = resp.status().as_u16();
        let body = resp.body_mut().read_to_string().unwrap_or_default();
        match status {
            200 => Ok(SyncOutcome::Started),
            _ if body.contains("another operation is already in progress") => {
                Ok(SyncOutcome::AlreadyRunning)
            }
            401 | 403 => {
                bail!("sync of {app}: HTTP {status}: token rejected or lacks sync permission")
            }
            404 => bail!("sync of {app}: ArgoCD app not found"),
            _ => bail!("sync of {app}: HTTP {status}: {}", snippet(&body)),
        }
    }

    /// Live sync/health of a single app. `404` means the app is gone, which
    /// `sts -S` treats as settled (nothing left to track).
    pub fn status(&self, app: &str) -> Result<Option<AppStatus>> {
        let v = match self.get_json(&format!("/api/v1/applications/{app}")) {
            Ok(v) => v,
            Err(e) if e.to_string().contains("HTTP 404") => return Ok(None),
            Err(e) => return Err(e),
        };
        let status = &v["status"];
        Ok(Some(AppStatus {
            sync: status["sync"]["status"]
                .as_str()
                .unwrap_or("Unknown")
                .to_string(),
            health: status["health"]["status"]
                .as_str()
                .unwrap_or("Unknown")
                .to_string(),
            phase: status["operationState"]["phase"]
                .as_str()
                .unwrap_or("")
                .to_string(),
        }))
    }

    fn get_json(&self, path: &str) -> Result<serde_json::Value> {
        let url = format!("{}{path}", self.cfg.url);
        let mut resp = self
            .agent
            .get(&url)
            .header("Authorization", &self.auth)
            .call()
            .with_context(|| format!("GET {url}"))?;
        let status = resp.status().as_u16();
        let body = resp
            .body_mut()
            .with_config()
            .limit(64 * 1024 * 1024)
            .read_to_string()
            .unwrap_or_default();
        match status {
            200 => serde_json::from_str(&body).with_context(|| format!("parsing {url}")),
            401 | 403 => bail!("ArgoCD rejected the token (HTTP {status}) for {url}"),
            _ => bail!(
                "ArgoCD returned HTTP {status} for {url}: {}",
                snippet(&body)
            ),
        }
    }
}

/// Helm charts a Jenkins build deployed, read from its console log, e.g.
/// `sed -i s/tag: .*/tag: v-1/ ./cicd/helm/stage-argocd-helm/soochi-dash-app/values.yaml`.
pub fn charts_in_console(console: &str) -> Vec<String> {
    let mut charts: Vec<String> = Vec::new();
    for line in console.lines().filter(|l| l.contains("values.yaml")) {
        let mut rest = line;
        while let Some(i) = rest.find(CHART_DIR) {
            rest = &rest[i + CHART_DIR.len()..];
            if let Some((chart, after)) = rest.split_once('/')
                && after.starts_with("values.yaml")
                && !chart.is_empty()
                && !charts.iter().any(|c| c == chart)
            {
                charts.push(chart.to_string());
            }
        }
    }
    charts
}

/// The app deploying `chart`: one whose source path ends in `stage-argocd-helm/<chart>`,
/// else one named `chart`.
pub fn app_for_chart<'a>(apps: &'a [App], chart: &str) -> Option<&'a App> {
    let suffix = format!("{CHART_DIR}{chart}");
    apps.iter()
        .find(|a| {
            a.paths
                .iter()
                .any(|p| p.trim_end_matches('/').ends_with(&suffix))
        })
        .or_else(|| apps.iter().find(|a| a.name == chart))
}

fn snippet(body: &str) -> String {
    let text = body.split_whitespace().collect::<Vec<_>>().join(" ");
    text.chars().take(300).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_charts_in_console() {
        let log = "\
+ sed -i s/tag: .*/tag: v-20261005/ ./cicd/helm/stage-argocd-helm/soochi-dash-app/values.yaml\n\
+ git add ./cicd/helm/stage-argocd-helm/soochi-dash-app/values.yaml\n\
+ sed -i s/tag: .*/tag: v-1/ cicd/helm/stage-argocd-helm/dashboard/values.yaml\n\
cicd/helm/stage-argocd-helm/other/Chart.yaml\n";
        assert_eq!(charts_in_console(log), vec!["soochi-dash-app", "dashboard"]);
    }

    #[test]
    fn maps_chart_to_app_by_path_then_name() {
        let apps = vec![
            App {
                name: "aadesh-app-test".into(),
                paths: vec!["cicd/helm/stage-argocd-helm/aadesh-app".into()],
            },
            App {
                name: "root-app".into(),
                paths: vec!["cicd/helm/stage-argocd-helm".into()],
            },
            App {
                name: "keda".into(),
                paths: vec![],
            },
        ];
        assert_eq!(
            app_for_chart(&apps, "aadesh-app").unwrap().name,
            "aadesh-app-test"
        );
        assert_eq!(app_for_chart(&apps, "keda").unwrap().name, "keda");
        assert!(app_for_chart(&apps, "nope").is_none());
    }

    #[test]
    fn settled_only_when_synced_healthy_and_not_running() {
        let settled = AppStatus {
            sync: "Synced".into(),
            health: "Healthy".into(),
            phase: "Succeeded".into(),
        };
        assert!(settled.is_settled());

        let running = AppStatus {
            sync: "Synced".into(),
            health: "Healthy".into(),
            phase: "Running".into(),
        };
        assert!(!running.is_settled());

        let out_of_sync = AppStatus {
            sync: "OutOfSync".into(),
            health: "Healthy".into(),
            phase: String::new(),
        };
        assert!(!out_of_sync.is_settled());

        let progressing = AppStatus {
            sync: "Synced".into(),
            health: "Progressing".into(),
            phase: String::new(),
        };
        assert!(!progressing.is_settled());
    }
}
