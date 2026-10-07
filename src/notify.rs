//! Best-effort notifications for the background watcher: a desktop pop-up when
//! available, and an optional Slack-compatible webhook. Everything here is
//! best-effort — a failure to notify must never break a deploy.
//!
//! On WSL this uses `notify-send`, which works when WSLg is present (Windows 11
//! or WSL with the GUI stack). Without it, `notify-send` is usually missing, so
//! we fall back to a terminal bell and the watch log — the deploy still proceeds.

use crate::config;
use std::process::{Command, Stdio};
use std::time::Duration;

/// Severity of a notification; drives the desktop urgency and webhook colour.
#[derive(Clone, Copy, PartialEq)]
pub enum Level {
    Info,
    Failure,
}

/// Send a notification through every configured channel. `summary` is the short
/// title, `body` the detail line. Never panics or returns an error.
pub fn send(level: Level, summary: &str, body: &str) {
    desktop(level, summary, body);
    if level == Level::Failure {
        webhook(summary, body);
    }
}

/// Fire a desktop notification via `notify-send` if it exists; otherwise ring
/// the terminal bell. Both are best-effort.
fn desktop(level: Level, summary: &str, body: &str) {
    let urgency = match level {
        Level::Info => "normal",
        Level::Failure => "critical",
    };
    let ran = Command::new("notify-send")
        .args(["-u", urgency, "-a", "sts", summary, body])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ran {
        // No desktop notifier (common on WSL without WSLg): ring the bell so a
        // watching terminal still gets a cue.
        eprint!("\x07");
    }
}

/// POST a Slack-style message to the configured webhook, if any. Short timeout,
/// errors swallowed: the watcher must not stall on a flaky webhook.
fn webhook(summary: &str, body: &str) {
    let Some(url) = config::webhook_url() else {
        return;
    };
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(10)))
        .build()
        .into();
    let payload = serde_json::json!({ "text": format!("*{summary}*\n{body}") });
    let _ = agent
        .post(&url)
        .header("Content-Type", "application/json")
        .send_json(&payload);
}
