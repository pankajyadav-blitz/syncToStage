//! Terminal styling built on `owo-colors`: ANSI colours and status glyphs,
//! applied only when stdout actually supports colour. `owo-colors`'
//! `if_supports_color` honours `NO_COLOR`/`CLICOLOR`, CI environments and
//! whether stdout is a real terminal (via the `supports-color` crate), so
//! piped or redirected output stays plain — logs, pipelines and `grep` see no
//! escape codes.
//!
//! The rest of the program calls these small helpers (`green`, `ok`, `arrow`,
//! …) instead of touching `owo-colors` directly, so the colour policy lives in
//! one place and callers never branch on "is this a tty".

// A deliberately complete colour palette: not every helper is used today, but
// keeping the full set makes the module a stable styling vocabulary for the
// rest of the program to draw on.
#![allow(dead_code)]

use owo_colors::{OwoColorize, Stream::Stdout};
use std::sync::OnceLock;

/// Whether stdout supports colour. owo-colors doesn't expose a public boolean,
/// so probe it once: colour a non-empty marker through `if_supports_color` and
/// see whether the result changed. Cached, since the answer is fixed per run.
fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let probe = "x".if_supports_color(Stdout, |t| t.red()).to_string();
        probe != "x"
    })
}

pub fn bold(text: &str) -> String {
    if on() { text.bold().to_string() } else { text.to_string() }
}
pub fn dim(text: &str) -> String {
    if on() { text.dimmed().to_string() } else { text.to_string() }
}
pub fn red(text: &str) -> String {
    if on() { text.red().to_string() } else { text.to_string() }
}
pub fn green(text: &str) -> String {
    if on() { text.green().to_string() } else { text.to_string() }
}
pub fn yellow(text: &str) -> String {
    if on() { text.yellow().to_string() } else { text.to_string() }
}
pub fn blue(text: &str) -> String {
    if on() { text.blue().to_string() } else { text.to_string() }
}
pub fn cyan(text: &str) -> String {
    if on() { text.cyan().to_string() } else { text.to_string() }
}

/// Bold + coloured, for headers and emphasised status words.
pub fn bold_green(text: &str) -> String {
    if on() { text.green().bold().to_string() } else { text.to_string() }
}
pub fn bold_red(text: &str) -> String {
    if on() { text.red().bold().to_string() } else { text.to_string() }
}
pub fn bold_cyan(text: &str) -> String {
    if on() { text.cyan().bold().to_string() } else { text.to_string() }
}

// ---- status glyphs ----------------------------------------------------------
//
// A leading symbol lets the eye jump straight to outcomes in a live view. When
// colour is unsupported they degrade to plain ASCII so piped output and
// non-UTF terminals stay clean and columns still line up.

/// Green check: a successful / settled outcome.
pub fn ok() -> String {
    if on() { "✓".green().to_string() } else { "+".to_string() }
}

/// Red cross: a failure / dropped outcome.
pub fn err() -> String {
    if on() { "✗".red().to_string() } else { "x".to_string() }
}

/// Yellow dot: in-progress / waiting.
pub fn busy() -> String {
    if on() { "•".yellow().to_string() } else { "~".to_string() }
}

/// Cyan arrow: a step / section marker.
pub fn arrow() -> String {
    if on() { "==>".cyan().to_string() } else { "==>".to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_when_colour_unsupported() {
        // Tests capture stdout, so colour support is off and every helper must
        // return its input/ASCII form verbatim (no escape codes).
        if !on() {
            assert_eq!(red("hi"), "hi");
            assert_eq!(bold_green("hi"), "hi");
            assert_eq!(ok(), "+");
            assert_eq!(err(), "x");
            assert_eq!(busy(), "~");
            assert_eq!(arrow(), "==>");
        }
    }
}
