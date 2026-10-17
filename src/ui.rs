//! A UI abstraction that lets every command render either as a full-screen
//! ratatui "runner" screen (on a real terminal) or as plain scrolling text
//! (when piped / redirected, which a TUI cannot do and which scripting, CI and
//! `--completions` rely on).
//!
//! Command bodies call these methods instead of `println!` / `read_line`:
//!   * [`Ui::step`]   — a headline action ("Fetching origin/stage")
//!   * [`Ui::info`] / [`Ui::detail`] — ordinary / secondary lines
//!   * [`Ui::ok`] / [`Ui::warn`] / [`Ui::err`] — outcome lines with a glyph
//!   * [`Ui::confirm`] — a yes/no question
//!   * [`Ui::choose`]  — pick one or more from a list
//!
//! The control flow is synchronous: the command drives the work and calls back
//! into the UI to draw or to block on a prompt. There are no worker threads, so
//! `?`/`bail!` error propagation is unchanged.

use crate::jobs::Job;
use crate::style;
use anyhow::{Context, Result, bail};
use std::io::{BufRead, IsTerminal, Write};

/// A line in the runner's steps panel: text plus the status it should render
/// with (glyph + colour), and how far into the command it was emitted (so the
/// TUI can show a `mm:ss` gutter).
#[derive(Clone)]
pub struct Step {
    pub kind: StepKind,
    /// Optional short label rendered in a fixed-width column before the text,
    /// so a run of rows lines up as a table (`repo  …`, `jenkins  …`).
    pub label: Option<String>,
    pub text: String,
    /// Seconds since the command started.
    pub at: u64,
}

/// Width of the label column in [`Ui::row`] / [`Ui::kv`] output.
pub const LABEL_W: usize = 9;

#[derive(Clone, Copy, PartialEq)]
pub enum StepKind {
    /// A headline action, shown with an arrow.
    Step,
    /// Plain informational line.
    Info,
    /// Secondary / dimmed detail (indented under a step).
    Detail,
    /// Succeeded.
    Ok,
    /// Warning, non-fatal.
    Warn,
    /// Failed.
    Err,
}

/// What a command needs from its UI. Implemented by [`TermUi`] (plain text) and
/// `TuiUi` (the ratatui runner, in `tui.rs`).
pub trait Ui {
    /// A headline action is starting.
    fn step(&mut self, msg: &str);
    /// An ordinary line.
    fn info(&mut self, msg: &str);
    /// A secondary / indented detail line.
    fn detail(&mut self, msg: &str);
    /// A success / warning / failure outcome line.
    fn ok(&mut self, msg: &str);
    fn warn(&mut self, msg: &str);
    fn err(&mut self, msg: &str);

    /// A row of a two-column list: a short `label` in a fixed-width column
    /// (coloured by `kind`), then the rest. Several rows in a row read as a
    /// small table — used for the ship plan, `-j`, `-c` and `-H`.
    fn row(&mut self, kind: StepKind, label: &str, text: &str);

    /// A plain `key   value` row: [`Ui::row`] with no status colour.
    fn kv(&mut self, key: &str, value: &str) {
        self.row(StepKind::Detail, key, value);
    }

    /// Ask a yes/no question. Returns the user's answer. In non-interactive
    /// backends this must not be called (the caller passes `-y`), and the
    /// default errors to make that contract explicit.
    fn confirm(&mut self, question: &str) -> Result<bool>;

    /// Offer `items` (each a `(short, long)` pair) and return the selected
    /// indices. `multi` allows more than one; otherwise exactly one is chosen.
    /// `prompt` labels the list.
    fn choose(&mut self, prompt: &str, items: &[(String, String)], multi: bool)
    -> Result<Vec<usize>>;

    /// True if this UI is the full-screen TUI (so callers can, e.g., defer the
    /// live dashboard hand-off to the same screen). Plain backends return false.
    fn is_tui(&self) -> bool {
        false
    }

    /// Tell the UI that the caller will open another full-screen view once this
    /// command finishes (the follow dashboard after `-s -F`, the log viewer
    /// after `-l`). The TUI then tears its runner screen down without waiting
    /// for a key, so the hand-off is seamless — two alternate screens can't be
    /// nested. The plain backend ignores it.
    fn request_handoff(&mut self) {}
}

/// Plain-text UI: exactly the behaviour sts had before the TUI — `println!`
/// lines with colour/glyphs from [`style`], and `read_line` prompts. Used when
/// stdout is not a terminal, and as the universal fallback.
pub struct TermUi;

impl TermUi {
    pub fn new() -> TermUi {
        TermUi
    }
}

impl Default for TermUi {
    fn default() -> Self {
        TermUi::new()
    }
}

impl Ui for TermUi {
    fn step(&mut self, msg: &str) {
        println!("{} {}", style::arrow(), style::bold(msg));
    }
    fn info(&mut self, msg: &str) {
        println!("{msg}");
    }
    fn detail(&mut self, msg: &str) {
        println!("  {}", style::dim(msg));
    }
    fn ok(&mut self, msg: &str) {
        println!("{} {msg}", style::ok());
    }
    fn warn(&mut self, msg: &str) {
        println!("{} {msg}", style::yellow("warning:"));
    }
    fn err(&mut self, msg: &str) {
        println!("{} {msg}", style::err());
    }

    fn row(&mut self, kind: StepKind, label: &str, text: &str) {
        // Pad before colouring: escape codes would break `{:<width$}`.
        let padded = format!("{label:<LABEL_W$}");
        let (glyph, label) = match kind {
            StepKind::Ok => (style::ok(), style::green(&padded)),
            StepKind::Err => (style::err(), style::red(&padded)),
            StepKind::Warn => (style::yellow("!"), style::yellow(&padded)),
            StepKind::Step => (style::arrow(), style::bold(&padded)),
            StepKind::Info | StepKind::Detail => (" ".to_string(), style::dim(&padded)),
        };
        println!("{glyph} {label} {text}");
    }

    fn confirm(&mut self, question: &str) -> Result<bool> {
        if !std::io::stdin().is_terminal() {
            bail!("stdin is not a terminal; pass -y to skip the confirmation");
        }
        print!("{question} [y/N] ");
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().lock().read_line(&mut answer)?;
        Ok(matches!(answer.trim().to_lowercase().as_str(), "y" | "yes"))
    }

    fn choose(
        &mut self,
        prompt: &str,
        items: &[(String, String)],
        multi: bool,
    ) -> Result<Vec<usize>> {
        if !std::io::stdin().is_terminal() {
            bail!("stdin is not a terminal; cannot prompt for a choice");
        }
        println!("{prompt}");
        for (i, (short, long)) in items.iter().enumerate() {
            if long.is_empty() {
                println!("  {}. {short}", i + 1);
            } else {
                println!("  {}. {:<24} {long}", i + 1, short);
            }
        }
        if multi {
            print!(
                "Choose [1-{}, comma-separated, a = all] ",
                items.len()
            );
        } else {
            print!("Choose [1-{}] ", items.len());
        }
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().lock().read_line(&mut answer)?;
        let answer = answer.trim();
        if multi {
            parse_multi_choice(answer, items)
        } else {
            parse_single_choice(answer, items)
        }
    }
}

/// Parse a multi-select answer: `a`/`all`, or 1-based numbers and item short
/// names separated by commas or spaces. Shared by the plain and TUI backends'
/// text-entry paths and by the job chooser.
pub fn parse_multi_choice(answer: &str, items: &[(String, String)]) -> Result<Vec<usize>> {
    if answer.is_empty() {
        bail!("aborted: nothing chosen");
    }
    if matches!(answer.to_lowercase().as_str(), "a" | "all") {
        return Ok((0..items.len()).collect());
    }
    let mut picked = Vec::new();
    for tok in answer.split([',', ' ']).filter(|t| !t.is_empty()) {
        let i = match tok.parse::<usize>() {
            Ok(i) if (1..=items.len()).contains(&i) => i - 1,
            _ => items
                .iter()
                .position(|(short, _)| short == tok)
                .with_context(|| format!("`{tok}` is not one of the listed choices"))?,
        };
        if !picked.contains(&i) {
            picked.push(i);
        }
    }
    Ok(picked)
}

/// Parse a single-select answer: a 1-based number or an item short name.
pub fn parse_single_choice(answer: &str, items: &[(String, String)]) -> Result<Vec<usize>> {
    if answer.is_empty() {
        bail!("aborted: nothing chosen");
    }
    if let Ok(i) = answer.parse::<usize>()
        && (1..=items.len()).contains(&i)
    {
        return Ok(vec![i - 1]);
    }
    if let Some(i) = items.iter().position(|(short, _)| short == answer) {
        return Ok(vec![i]);
    }
    bail!("`{answer}` is not one of the listed choices");
}

/// Build the `(short, long)` list a job chooser offers: short = `Job::name()`
/// (so it can be typed), long = the raw job path.
pub fn job_items(jobs: &[Job]) -> Vec<(String, String)> {
    jobs.iter().map(|j| (j.name(), j.raw.clone())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items() -> Vec<(String, String)> {
        ["app", "dash", "web"]
            .iter()
            .map(|n| (n.to_string(), format!("/job/x-{n}/build")))
            .collect()
    }

    #[test]
    fn multi_choice_numbers_names_and_all() {
        let it = items();
        assert_eq!(parse_multi_choice("2", &it).unwrap(), [1]);
        assert_eq!(parse_multi_choice("3, 1", &it).unwrap(), [2, 0]);
        assert_eq!(parse_multi_choice("dash web", &it).unwrap(), [1, 2]);
        assert_eq!(parse_multi_choice("A", &it).unwrap(), [0, 1, 2]);
        assert!(parse_multi_choice("", &it).is_err());
        assert!(parse_multi_choice("4", &it).is_err());
        assert!(parse_multi_choice("nope", &it).is_err());
    }

    #[test]
    fn single_choice_number_or_name() {
        let it = items();
        assert_eq!(parse_single_choice("2", &it).unwrap(), [1]);
        assert_eq!(parse_single_choice("web", &it).unwrap(), [2]);
        assert!(parse_single_choice("", &it).is_err());
        assert!(parse_single_choice("9", &it).is_err());
    }
}
