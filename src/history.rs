//! A rolling log of finished deploys, so `sts --history` can answer
//! "when did I last ship this repo, and did it succeed?". Kept as a JSON array
//! in the state dir, capped at the most recent `MAX_ENTRIES`.

use crate::config;
use crate::ui::StepKind;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// How many deploy records to keep. Old ones are dropped as new ones land.
const MAX_ENTRIES: usize = 200;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub repo: String,
    /// `SUCCESS`, `FAILURE`, `ABORTED`, `CANCELLED`, `GAVE_UP`, `SYNCED`, ...
    pub result: String,
    /// What this record is about: the Jenkins build or the ArgoCD app.
    #[serde(default)]
    pub detail: String,
    pub at: u64,
}

fn history_file() -> PathBuf {
    config::state_dir().join("history.json")
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn read() -> Vec<Record> {
    match fs::read_to_string(history_file()) {
        Ok(text) if !text.trim().is_empty() => serde_json::from_str(&text).unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// Append one record. Best-effort: an error here must not derail a deploy, so
/// the caller logs but ignores failures. Trims to `MAX_ENTRIES`.
pub fn record(repo: &str, result: &str, detail: &str) -> Result<()> {
    let mut items = read();
    items.push(Record {
        repo: repo.to_string(),
        result: result.to_string(),
        detail: detail.to_string(),
        at: now(),
    });
    let len = items.len();
    if len > MAX_ENTRIES {
        items.drain(0..len - MAX_ENTRIES);
    }
    let dir = config::state_dir();
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = history_file();
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_string_pretty(&items)?)
        .with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// `sts --history [repo]`: print the most recent deploys, newest first,
/// optionally filtered to one repo (substring match on the slug).
pub fn show(ui: &mut dyn crate::ui::Ui, filter: Option<&str>) -> Result<()> {
    let mut items = read();
    if items.is_empty() {
        ui.info(&format!("No deploy history yet ({}).", history_file().display()));
        return Ok(());
    }
    if let Some(f) = filter {
        items.retain(|r| r.repo.contains(f));
        if items.is_empty() {
            ui.info(&format!("No deploy history for `{f}`."));
            return Ok(());
        }
    }
    ui.step(&format!(
        "{} recent deploy(s){}",
        items.len(),
        filter.map(|f| format!(" for `{f}`")).unwrap_or_default()
    ));
    for r in items.iter().rev() {
        // The outcome is the row's label (lowercased so it reads as a status,
        // not a shout), coloured green/red by what it means; the rest is an
        // aligned repo column, then what it was about and when.
        let kind = match r.result.as_str() {
            "SUCCESS" | "SYNCED" => StepKind::Ok,
            "FAILURE" | "ABORTED" | "CANCELLED" | "GAVE_UP" | "SYNC_FAILED" | "SYNC_GAVE_UP"
            | "DEGRADED" | "LOST" | "APP_GONE" => StepKind::Err,
            _ => StepKind::Info,
        };
        let label = r.result.to_lowercase().replace('_', " ");
        ui.row(
            kind,
            &label,
            &format!("{:<28} {:<10} {}", r.repo, ago(r.at), r.detail),
        );
    }
    ui.kv("history", &history_file().display().to_string());
    Ok(())
}

/// `2h ago`, `5m ago`, `just now` — a compact relative time for the history list.
fn ago(then: u64) -> String {
    let secs = now().saturating_sub(then);
    if secs < 60 {
        "just now".to_string()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}
