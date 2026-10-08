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

/// A running pod of an ArgoCD app, from its resource tree.
pub struct Pod {
    pub name: String,
    pub namespace: String,
    /// `Healthy` / `Progressing` / `Degraded` / ... (empty if unknown).
    pub health: String,
}

/// One line of container output from the logs API. `ts` is the raw RFC3339
/// timestamp ArgoCD attaches, used to drop lines we have already printed.
pub struct LogLine {
    pub ts: String,
    pub content: String,
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

    /// The pods of an app, read from its resource tree. Each carries the
    /// namespace it runs in (apps can span namespaces) and its health.
    pub fn pods(&self, app: &str) -> Result<Vec<Pod>> {
        let v = self.get_json(&format!("/api/v1/applications/{app}/resource-tree"))?;
        let nodes = v["nodes"].as_array().cloned().unwrap_or_default();
        Ok(nodes
            .iter()
            .filter(|n| n["kind"].as_str() == Some("Pod"))
            .map(|n| Pod {
                name: n["name"].as_str().unwrap_or_default().to_string(),
                namespace: n["namespace"].as_str().unwrap_or_default().to_string(),
                health: n["health"]["status"].as_str().unwrap_or("").to_string(),
            })
            .filter(|p| !p.name.is_empty())
            .collect())
    }

    /// The last `tail` lines of a pod's logs (optionally one container), each
    /// with the RFC3339 timestamp ArgoCD attaches. ArgoCD answers with
    /// newline-delimited JSON: one `{"result":{"content","timeStamp",...}}` per
    /// line. We poll this on an interval rather than holding a `follow=true`
    /// stream open, so the caller dedupes by timestamp between polls.
    pub fn logs(
        &self,
        app: &str,
        namespace: &str,
        pod: &str,
        container: Option<&str>,
        tail: u32,
    ) -> Result<Vec<LogLine>> {
        let mut path = format!(
            "/api/v1/applications/{app}/logs?namespace={}&podName={}&tailLines={tail}",
            enc(namespace),
            enc(pod),
        );
        if let Some(c) = container {
            path.push_str(&format!("&container={}", enc(c)));
        }
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
            200 => Ok(parse_log_stream(&body)),
            401 | 403 => bail!(
                "ArgoCD rejected the token (HTTP {status}) for logs of {app} (needs the `logs, get` permission)"
            ),
            404 => bail!("ArgoCD app {app} or pod {pod} not found (HTTP 404)"),
            _ => bail!(
                "ArgoCD returned HTTP {status} for logs of {app}: {}",
                snippet(&body)
            ),
        }
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

/// Parse ArgoCD's newline-delimited log stream. Each non-empty line is a JSON
/// object `{"result":{"content":"...","timeStamp":"...","podName":"..."}}`.
/// Lines that don't parse (or carry no content) are skipped. The `last` marker
/// ArgoCD sends to close a stream carries no content and is ignored.
fn parse_log_stream(body: &str) -> Vec<LogLine> {
    let mut out = Vec::new();
    for line in body.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let r = &v["result"];
        let Some(content) = r["content"].as_str() else {
            continue;
        };
        out.push(LogLine {
            // ArgoCD spells it `timeStamp`; fall back to `timeStampStr`.
            ts: r["timeStamp"]
                .as_str()
                .or_else(|| r["timeStampStr"].as_str())
                .unwrap_or("")
                .to_string(),
            content: content.trim_end_matches('\n').to_string(),
        });
    }
    out
}

/// Minimal percent-encoding for a query-string value. Pod and namespace names
/// are DNS labels (safe already), but container names and anything the user
/// types could contain reserved characters, so encode defensively.
fn enc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
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

    #[test]
    fn parses_argocd_log_stream() {
        let body = "\
{\"result\":{\"content\":\"line one\",\"timeStamp\":\"2026-10-08T08:00:00Z\",\"podName\":\"web-1\"}}\n\
\n\
{\"result\":{\"content\":\"line two\\n\",\"timeStamp\":\"2026-10-08T08:00:01Z\",\"podName\":\"web-1\"}}\n\
not json\n\
{\"result\":{\"last\":true}}\n";
        let lines = parse_log_stream(body);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].content, "line one");
        assert_eq!(lines[0].ts, "2026-10-08T08:00:00Z");
        assert_eq!(lines[1].content, "line two"); // trailing newline trimmed
    }

    #[test]
    fn enc_leaves_safe_chars_and_escapes_others() {
        assert_eq!(enc("web-app_1.0~x"), "web-app_1.0~x");
        assert_eq!(enc("a b/c"), "a%20b%2Fc");
    }
}
