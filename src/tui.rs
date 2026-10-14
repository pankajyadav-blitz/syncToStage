//! The full-screen terminal UI for the live views (`sts`/`-D`, `-P`, `-S`,
//! `--follow`). Built on `ratatui` + `crossterm`.
//!
//! This is only ever entered when stdout is a real terminal — the piped /
//! redirected path still prints a single plain snapshot from `deploy.rs`, which
//! a TUI can't do (it needs raw mode and the alternate screen). One-shot
//! commands (`sts -s`, `-b`, `-c`, `-l`) stay as ordinary scrolling output so
//! their prompts and log tails keep working.
//!
//! The event loop polls `crossterm` for key presses (q / Esc / Ctrl-C quit) and
//! otherwise refreshes the data on the view's normal cadence. A `refresh`
//! callback lets `--follow` advance the deploy pipeline (run a watcher tick)
//! between redraws; the plain dashboards pass a no-op.

use crate::deploy::{BuildState, BuildsView, SyncState, SyncView};
use crate::ui::{Step, StepKind, Ui};
use anyhow::Result;
use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Clear, Gauge, List, ListItem, ListState, Paragraph, Row,
    Table, Wrap,
};
use std::time::{Duration, Instant};

/// How often the TUI redraws so the clock and relative ages tick smoothly. The
/// network data is only re-polled on the slower per-view `poll` cadence.
const REDRAW_TICK: Duration = Duration::from_millis(1000);

/// Which live view the TUI is showing. Picks the title, which panels appear,
/// and the refresh cadence.
#[derive(Clone, Copy, PartialEq)]
pub enum View {
    /// `sts` / `-D`: both panels.
    Dashboard,
    /// `-P`: builds only.
    Pending,
    /// `-S`: ArgoCD syncs only.
    Sync,
    /// `--follow`: both panels, advances the pipeline each tick.
    Follow,
}

impl View {
    fn title(self) -> &'static str {
        match self {
            View::Dashboard => "sts · dashboard",
            View::Pending => "sts · pending builds",
            View::Sync => "sts · argocd syncs",
            View::Follow => "sts · follow",
        }
    }

    pub fn shows_builds(self) -> bool {
        !matches!(self, View::Sync)
    }

    pub fn shows_syncs(self) -> bool {
        !matches!(self, View::Pending)
    }

    fn poll(self) -> Duration {
        Duration::from_secs(5)
    }
}

/// A snapshot of what the TUI is currently displaying. Collected fresh each
/// refresh by the caller-supplied `collect` closure. A `None` panel means the
/// view doesn't show it at all; a `Some(view)` that is empty means "settled".
pub struct Snapshot {
    pub builds: Option<BuildsView>,
    pub syncs: Option<SyncView>,
}

impl Snapshot {
    /// Both tracked lists this view cares about are empty — nothing left to
    /// watch, so the loop can exit on its own.
    fn drained(&self) -> bool {
        let b = self.builds.as_ref().is_none_or(|v| v.is_empty());
        let s = self.syncs.as_ref().is_none_or(|v| v.is_empty());
        b && s
    }
}

/// Run the TUI for `view`. `refresh` advances any side effects (a watcher tick
/// for `--follow`; a no-op otherwise) and is called once per cycle before data
/// is re-collected. `collect` gathers the current `Snapshot` to render.
///
/// Returns `true` if the view drained itself (both lists empty) so the loop
/// exited on its own, or `false` if the user quit with a key. The terminal is
/// always restored before returning, even on error.
pub fn run<R, C>(view: View, mut refresh: R, mut collect: C) -> Result<bool>
where
    R: FnMut() -> Result<()>,
    C: FnMut() -> Result<Snapshot>,
{
    let mut terminal = ratatui::try_init()?;
    let result = run_loop(&mut terminal, view, &mut refresh, &mut collect);
    ratatui::try_restore()?;
    result
}

fn run_loop<R, C>(
    terminal: &mut ratatui::DefaultTerminal,
    view: View,
    refresh: &mut R,
    collect: &mut C,
) -> Result<bool>
where
    R: FnMut() -> Result<()>,
    C: FnMut() -> Result<Snapshot>,
{
    let poll = view.poll();
    // Redraw often so the clock and relative ages animate smoothly, but only
    // re-poll the network on the slower `poll` cadence. This keeps the UI live
    // without hammering Jenkins/ArgoCD.
    let redraw = REDRAW_TICK.min(poll);

    // Prime the first frame immediately.
    refresh()?;
    let mut snapshot = collect()?;
    let mut collected_at = Instant::now();

    loop {
        // Seconds elapsed since the data was collected, added to each row's age
        // at draw time so the "45s ago" counters tick up every redraw rather
        // than jumping once per poll.
        let age_offset = collected_at.elapsed().as_secs();
        terminal.draw(|f| draw(f, view, &snapshot, age_offset))?;

        // The dashboards exit by themselves once both lists are empty, matching
        // the old scrolling behaviour. Only trusted after a refresh, which has
        // already happened at least once above.
        if snapshot.drained() {
            return Ok(true);
        }

        // Wake for the next redraw, or sooner on a key press.
        if event::poll(redraw)?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
            && should_quit(key.code, key.modifiers)
        {
            return Ok(false);
        }

        // Re-poll the data only once the slower interval has elapsed.
        if collected_at.elapsed() >= poll {
            refresh()?;
            snapshot = collect()?;
            collected_at = Instant::now();
        }
    }
}

/// q / Esc / Ctrl-C all quit the TUI.
fn should_quit(code: KeyCode, mods: KeyModifiers) -> bool {
    matches!(code, KeyCode::Char('q') | KeyCode::Esc)
        || (code == KeyCode::Char('c') && mods.contains(KeyModifiers::CONTROL))
}

// ---- rendering --------------------------------------------------------------

const ACCENT: Color = Color::Cyan;

fn draw(f: &mut Frame, view: View, snap: &Snapshot, age_offset: u64) {
    f.render_widget(Clear, f.area());

    // title bar | body | footer
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(f.area());

    draw_title(f, chunks[0], view);
    draw_body(f, chunks[1], view, snap, age_offset);
    draw_footer(f, chunks[2]);
}

fn draw_title(f: &mut Frame, area: Rect, view: View) {
    let left = Span::styled(
        format!(" {} ", view.title()),
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    );
    // Recompute the clock every redraw so it ticks once a second, independent
    // of the slower data poll.
    let right = Span::styled(
        format!("{}  ", crate::deploy::utc_now()),
        Style::default().fg(Color::DarkGray),
    );
    let spacer_len = (area.width as usize)
        .saturating_sub(left.width() + right.width() + 2);
    let line = Line::from(vec![left, Span::raw(" ".repeat(spacer_len)), right]);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT));
    f.render_widget(Paragraph::new(line).block(block), area);
}

fn draw_body(f: &mut Frame, area: Rect, view: View, snap: &Snapshot, age_offset: u64) {
    match (view.shows_builds(), view.shows_syncs()) {
        (true, true) => {
            let halves = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                .split(area);
            draw_builds(f, halves[0], snap.builds.as_ref(), age_offset);
            draw_syncs(f, halves[1], snap.syncs.as_ref(), age_offset);
        }
        (true, false) => draw_builds(f, area, snap.builds.as_ref(), age_offset),
        (false, true) => draw_syncs(f, area, snap.syncs.as_ref(), age_offset),
        (false, false) => {}
    }
}

fn panel<'a>(title: &'a str, count: Option<usize>) -> Block<'a> {
    let title = match count {
        Some(n) => format!(" {title} ({n}) "),
        None => format!(" {title} "),
    };
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Span::styled(
            title,
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ))
}

fn draw_builds(f: &mut Frame, area: Rect, builds: Option<&BuildsView>, age_offset: u64) {
    let Some(builds) = builds else {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "no build queue",
                Style::default().fg(Color::DarkGray),
            )))
            .block(panel("Builds", None)),
            area,
        );
        return;
    };

    let cron = if builds.cron_installed {
        Span::styled("cron: on ", Style::default().fg(Color::Green))
    } else {
        Span::styled("cron: off ", Style::default().fg(Color::Yellow))
    };
    let block = panel("Builds", Some(builds.rows.len()))
        .title_bottom(Line::from(vec![Span::raw(" "), cron]).right_aligned());

    if builds.rows.is_empty() {
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("✓ ", Style::default().fg(Color::Green)),
                Span::styled(
                    "No builds waiting to be deployed.",
                    Style::default().fg(Color::Green),
                ),
            ]))
            .block(block),
            area,
        );
        return;
    }

    let rows = &builds.rows;
    // Render the panel border/title first, then lay out rows inside it so each
    // building row can carry a real progress bar (a Gauge, which a Table cell
    // can't hold).
    f.render_widget(&block, area);
    let inner = block.inner(area);
    if inner.height == 0 {
        return;
    }

    // Two rows of height per build; cap at what fits.
    let max_rows = (inner.height as usize / 2).max(1);
    let slots = Layout::default()
        .direction(Direction::Vertical)
        .constraints(
            std::iter::repeat_n(Constraint::Length(2), rows.len().min(max_rows))
                .collect::<Vec<_>>(),
        )
        .split(inner);

    for (b, slot) in rows.iter().zip(slots.iter()) {
        let (glyph, state_span) = match b.state {
            BuildState::Building => (
                Span::styled("● ", Style::default().fg(ACCENT)),
                Span::styled("building", Style::default().fg(ACCENT).bold()),
            ),
            BuildState::Queued => (
                Span::styled("○ ", Style::default().fg(Color::Yellow)),
                Span::styled("queued", Style::default().fg(Color::Yellow)),
            ),
        };
        // Split this row's slot into its two lines.
        let two = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Length(1)])
            .split(*slot);

        // Line 1: glyph + state + the build/job URL.
        let line1 = Line::from(vec![glyph, state_span, Span::raw("  "), Span::raw(b.label.clone())]);
        f.render_widget(Paragraph::new(line1), two[0]);

        // Line 2: a progress bar for a building job with an estimate; otherwise
        // the plain right-aligned detail. Both advance by `age_offset` so they
        // tick every second between network polls.
        let age = fmt_age(b.age_secs + age_offset);
        match (b.state, b.elapsed, b.estimated) {
            (BuildState::Building, Some(elapsed), Some(est)) if est > 0 => {
                let live = elapsed + age_offset;
                let ratio = (live as f64 / est as f64).clamp(0.0, 1.0);
                let label = format!(
                    "{}  ·  {} · {}",
                    build_progress_hint(live, Some(est)),
                    age,
                    b.repo
                );
                let gauge = Gauge::default()
                    .gauge_style(Style::default().fg(ACCENT))
                    .ratio(ratio)
                    .label(Span::styled(label, Style::default().fg(Color::Gray)));
                f.render_widget(gauge, two[1]);
            }
            _ => {
                let mut detail = String::new();
                if let Some(elapsed) = b.elapsed {
                    detail.push_str(&build_progress_hint(elapsed + age_offset, b.estimated));
                    detail.push_str(" · ");
                }
                detail.push_str(&format!("{age} · {}", b.repo));
                f.render_widget(
                    Paragraph::new(
                        Line::from(Span::styled(detail, Style::default().fg(Color::DarkGray)))
                            .right_aligned(),
                    ),
                    two[1],
                );
            }
        }
    }
}

fn draw_syncs(f: &mut Frame, area: Rect, syncs: Option<&SyncView>, age_offset: u64) {
    let Some(syncs) = syncs else {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "no sync tracking",
                Style::default().fg(Color::DarkGray),
            )))
            .block(panel("ArgoCD", None)),
            area,
        );
        return;
    };

    let Some(rows) = syncs.rows.as_ref() else {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "no argocd_token configured — can't check sync status",
                Style::default().fg(Color::Yellow),
            )))
            .block(panel("ArgoCD", None))
            .wrap(Wrap { trim: true }),
            area,
        );
        return;
    };

    let block = panel("ArgoCD", Some(rows.len()));
    if rows.is_empty() {
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("✓ ", Style::default().fg(Color::Green)),
                Span::styled(
                    "All tracked ArgoCD syncs have settled.",
                    Style::default().fg(Color::Green),
                ),
            ]))
            .block(block),
            area,
        );
        return;
    }

    let table_rows = rows.iter().map(|s| {
        let (glyph, label, colour) = sync_look(s.status);
        // Line 1: glyph + state + app name (full width, left).
        let line1 = Line::from(vec![
            Span::styled(format!("{glyph} "), Style::default().fg(colour)),
            Span::styled(label, Style::default().fg(colour).bold()),
            Span::raw("  "),
            Span::raw(s.app.clone()),
        ]);
        // Line 2: the full detail — status · age · repo — right-aligned on its
        // own line so long status text ("Synced/Progressing, Succeeded") and
        // the repo are shown in full instead of being clipped.
        let detail = format!(
            "{} · {} · {}",
            s.detail,
            fmt_age(s.age_secs + age_offset),
            s.repo
        );
        let line2 = Line::from(Span::styled(detail, Style::default().fg(Color::DarkGray)))
            .right_aligned();
        Row::new(vec![Cell::from(Text::from(vec![line1, line2]))]).height(2)
    });

    let table = Table::new(table_rows, [Constraint::Percentage(100)])
        .column_spacing(0)
        .block(block);
    f.render_widget(table, area);
}

/// (glyph, label, colour) for a sync status.
fn sync_look(state: SyncState) -> (&'static str, &'static str, Color) {
    match state {
        SyncState::Syncing => ("●", "syncing", ACCENT),
        SyncState::Synced => ("✓", "synced", Color::Green),
        SyncState::Degraded => ("✗", "degraded", Color::Red),
        SyncState::Gone => ("✗", "gone", Color::DarkGray),
        SyncState::GaveUp => ("✗", "gave up", Color::Red),
        SyncState::Unknown => ("?", "unknown", Color::Yellow),
    }
}

fn draw_footer(f: &mut Frame, area: Rect) {
    let line = Line::from(vec![
        Span::styled(" q", Style::default().fg(ACCENT).bold()),
        Span::styled(" quit", Style::default().fg(Color::DarkGray)),
        Span::raw("   "),
        Span::styled("live", Style::default().fg(Color::Green)),
        Span::styled(
            " · refreshes automatically, exits when the pipeline is empty",
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

// ---- small helpers ----------------------------------------------------------

/// Build-progress hint like `1m50s/3m10s (~1m20s left)`, computed from a live
/// `elapsed` (which the caller advances every second) and Jenkins' estimate.
/// Mirrors `jenkins::BuildProgress::hint` but takes raw seconds so the TUI can
/// recompute it between polls.
fn build_progress_hint(elapsed: u64, estimated: Option<u64>) -> String {
    use crate::jenkins::fmt_secs;
    match estimated {
        Some(est) if est > elapsed => {
            format!("{}/{} (~{} left)", fmt_secs(elapsed), fmt_secs(est), fmt_secs(est - elapsed))
        }
        Some(est) => format!("{}/{} (overdue)", fmt_secs(elapsed), fmt_secs(est)),
        None => format!("{} elapsed", fmt_secs(elapsed)),
    }
}

/// Compact relative age, e.g. `12s`, `3m`, `1h4m`.
fn fmt_age(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    }
}

// ---- the command runner screen ---------------------------------------------
//
// `TuiUi` is the full-screen counterpart to `ui::TermUi`. While a command runs
// (ship, build, check, …) it owns the alternate screen and renders a header +
// a scrolling "steps" panel. `step`/`info`/… push a line and redraw; `confirm`
// and `choose` open a blocking modal over the steps and return the answer.
//
// The control flow stays synchronous: the command body calls these methods in
// order, exactly as it called `println!`/`read_line` before.

/// A command running inside the TUI. Created with [`TuiUi::run`], which sets up
/// and tears down the terminal around a closure that does the work.
pub struct TuiUi {
    terminal: ratatui::DefaultTerminal,
    title: String,
    started: Instant,
    steps: Vec<Step>,
    /// Set by `request_follow`: launch the live dashboard after this runner
    /// screen tears down (two alternate screens can't be nested).
    follow_requested: bool,
}

impl TuiUi {
    /// Run `body` with a `TuiUi` for a command titled `title`. Sets up the
    /// alternate screen first and always restores it afterwards, even on error.
    /// After `body` returns, the final screen is held until the user presses a
    /// key so they can read the result. Returns whether the body requested the
    /// follow dashboard (so the caller can launch it on a fresh screen).
    pub fn run<F>(title: &str, body: F) -> Result<bool>
    where
        F: FnOnce(&mut TuiUi) -> Result<()>,
    {
        let terminal = ratatui::try_init()?;
        let mut ui = TuiUi {
            terminal,
            title: title.to_string(),
            started: Instant::now(),
            steps: Vec::new(),
            follow_requested: false,
        };
        let result = body(&mut ui);
        let follow = result.is_ok() && ui.follow_requested;
        let hold = ui.finish(&result);
        ratatui::try_restore()?;
        hold?;
        result?;
        Ok(follow)
    }

    /// Push a step and redraw. Draw errors are ignored so a transient redraw
    /// failure can't abort the command mid-flight; the next draw recovers.
    fn push(&mut self, kind: StepKind, text: String) {
        self.steps.push(Step { kind, text });
        let _ = self.redraw();
    }

    fn redraw(&mut self) -> Result<()> {
        let title = self.title.clone();
        let elapsed = self.started.elapsed().as_secs();
        let steps = self.steps.clone();
        self.terminal
            .draw(|f| draw_runner(f, &title, elapsed, &steps, None))?;
        Ok(())
    }

    /// After the body finishes, append a final outcome line and wait for a key
    /// — unless the command is about to flow into the follow dashboard, in
    /// which case we return immediately so the hand-off is seamless.
    fn finish(&mut self, result: &Result<()>) -> Result<()> {
        if result.is_ok() && self.follow_requested {
            return Ok(());
        }
        match result {
            Ok(()) => self.steps.push(Step {
                kind: StepKind::Ok,
                text: "done — press any key to exit".to_string(),
            }),
            Err(e) => self.steps.push(Step {
                kind: StepKind::Err,
                text: format!("{e:#} — press any key to exit"),
            }),
        }
        let _ = self.redraw();
        loop {
            if event::poll(Duration::from_secs(3600))?
                && let Event::Key(k) = event::read()?
                && k.kind == KeyEventKind::Press
            {
                return Ok(());
            }
        }
    }
}

impl Ui for TuiUi {
    fn step(&mut self, msg: &str) {
        self.push(StepKind::Step, msg.to_string());
    }
    fn info(&mut self, msg: &str) {
        self.push(StepKind::Info, msg.to_string());
    }
    fn detail(&mut self, msg: &str) {
        self.push(StepKind::Detail, msg.to_string());
    }
    fn ok(&mut self, msg: &str) {
        self.push(StepKind::Ok, msg.to_string());
    }
    fn warn(&mut self, msg: &str) {
        self.push(StepKind::Warn, msg.to_string());
    }
    fn err(&mut self, msg: &str) {
        self.push(StepKind::Err, msg.to_string());
    }

    fn confirm(&mut self, question: &str) -> Result<bool> {
        let title = self.title.clone();
        let steps = self.steps.clone();
        loop {
            let elapsed = self.started.elapsed().as_secs();
            let modal = Modal::Confirm { question };
            self.terminal
                .draw(|f| draw_runner(f, &title, elapsed, &steps, Some(&modal)))?;
            if let Event::Key(k) = event::read()?
                && k.kind == KeyEventKind::Press
            {
                match k.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') => return Ok(true),
                    KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc | KeyCode::Enter => {
                        return Ok(false);
                    }
                    KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Ok(false);
                    }
                    _ => {}
                }
            }
        }
    }

    fn choose(
        &mut self,
        prompt: &str,
        items: &[(String, String)],
        multi: bool,
    ) -> Result<Vec<usize>> {
        if items.is_empty() {
            anyhow::bail!("nothing to choose from");
        }
        let title = self.title.clone();
        let steps = self.steps.clone();
        let mut selected = 0usize;
        let mut checked = vec![false; items.len()];

        loop {
            let elapsed = self.started.elapsed().as_secs();
            let modal = Modal::Choose {
                prompt,
                items,
                multi,
                checked: &checked,
                selected,
            };
            self.terminal
                .draw(|f| draw_runner(f, &title, elapsed, &steps, Some(&modal)))?;

            if let Event::Key(k) = event::read()?
                && k.kind == KeyEventKind::Press
            {
                match k.code {
                    KeyCode::Up | KeyCode::Char('k') => {
                        selected = selected.saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        selected = (selected + 1).min(items.len() - 1);
                    }
                    KeyCode::Char(' ') if multi => checked[selected] = !checked[selected],
                    KeyCode::Char('a') | KeyCode::Char('A') if multi => {
                        let all = !checked.iter().all(|&c| c);
                        checked.iter_mut().for_each(|c| *c = all);
                    }
                    KeyCode::Enter => {
                        if multi {
                            let picked: Vec<usize> = checked
                                .iter()
                                .enumerate()
                                .filter_map(|(i, &c)| c.then_some(i))
                                .collect();
                            // Enter with nothing ticked selects the highlighted row.
                            return Ok(if picked.is_empty() {
                                vec![selected]
                            } else {
                                picked
                            });
                        }
                        return Ok(vec![selected]);
                    }
                    KeyCode::Esc => anyhow::bail!("aborted: nothing chosen"),
                    KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                        anyhow::bail!("aborted");
                    }
                    _ => {}
                }
            }
        }
    }

    fn is_tui(&self) -> bool {
        true
    }

    fn request_follow(&mut self) {
        self.follow_requested = true;
    }
}

/// A modal overlay drawn on top of the steps panel.
enum Modal<'a> {
    Confirm {
        question: &'a str,
    },
    Choose {
        prompt: &'a str,
        items: &'a [(String, String)],
        multi: bool,
        checked: &'a [bool],
        selected: usize,
    },
}

/// Draw the runner screen: header (command + elapsed), the scrolling steps
/// panel, a footer, and an optional modal on top.
fn draw_runner(f: &mut Frame, title: &str, elapsed: u64, steps: &[Step], modal: Option<&Modal>) {
    f.render_widget(Clear, f.area());
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(f.area());

    // Header: command title + live elapsed timer.
    let left = Span::styled(
        format!(" sts · {title} "),
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    );
    let right = Span::styled(
        format!("{}  ", fmt_age(elapsed)),
        Style::default().fg(Color::DarkGray),
    );
    let spacer = (chunks[0].width as usize).saturating_sub(left.width() + right.width() + 2);
    let header = Line::from(vec![left, Span::raw(" ".repeat(spacer)), right]);
    f.render_widget(
        Paragraph::new(header).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(ACCENT)),
        ),
        chunks[0],
    );

    // Steps panel: render the lines, scrolled so the newest are visible.
    let body = chunks[1];
    let inner_h = body.height.saturating_sub(2) as usize;
    let lines: Vec<Line> = steps.iter().map(step_line).collect();
    let total = lines.len();
    let start = total.saturating_sub(inner_h);
    let shown: Vec<Line> = lines[start..].to_vec();
    f.render_widget(
        Paragraph::new(shown)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(Color::DarkGray))
                    .title(Span::styled(
                        " steps ",
                        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                    )),
            )
            .wrap(Wrap { trim: false }),
        body,
    );

    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " Ctrl-C to abort",
            Style::default().fg(Color::DarkGray),
        ))),
        chunks[2],
    );

    if let Some(m) = modal {
        draw_modal(f, m);
    }
}

/// Render one step as a styled line with its glyph.
fn step_line(s: &Step) -> Line<'static> {
    let (glyph, colour) = match s.kind {
        StepKind::Step => ("==>", ACCENT),
        StepKind::Info => (" ", Color::Reset),
        StepKind::Detail => ("   ", Color::DarkGray),
        StepKind::Ok => ("✓", Color::Green),
        StepKind::Warn => ("!", Color::Yellow),
        StepKind::Err => ("✗", Color::Red),
    };
    let text_style = match s.kind {
        StepKind::Step => Style::default().add_modifier(Modifier::BOLD),
        StepKind::Detail => Style::default().fg(Color::DarkGray),
        StepKind::Ok => Style::default().fg(Color::Green),
        StepKind::Warn => Style::default().fg(Color::Yellow),
        StepKind::Err => Style::default().fg(Color::Red),
        StepKind::Info => Style::default(),
    };
    Line::from(vec![
        Span::styled(format!("{glyph} "), Style::default().fg(colour)),
        Span::styled(s.text.clone(), text_style),
    ])
}

/// A centred rectangle `pct_w`% wide and `height` rows tall within `area`.
fn centred(area: Rect, pct_w: u16, height: u16) -> Rect {
    let w = area.width * pct_w / 100;
    let x = area.x + area.width.saturating_sub(w) / 2;
    let h = height.min(area.height);
    let y = area.y + area.height.saturating_sub(h) / 2;
    Rect {
        x,
        y,
        width: w,
        height: h,
    }
}

fn draw_modal(f: &mut Frame, modal: &Modal) {
    match modal {
        Modal::Confirm { question } => {
            let area = centred(f.area(), 60, 5);
            f.render_widget(Clear, area);
            let block = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(ACCENT))
                .title(Span::styled(
                    " confirm ",
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ));
            let text = Text::from(vec![
                Line::from(Span::raw(question.to_string())),
                Line::from(vec![
                    Span::styled(
                        "y",
                        Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" yes    "),
                    Span::styled(
                        "n",
                        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" no (default)"),
                ]),
            ]);
            f.render_widget(
                Paragraph::new(text)
                    .block(block)
                    .alignment(Alignment::Center)
                    .wrap(Wrap { trim: true }),
                area,
            );
        }
        Modal::Choose {
            prompt,
            items,
            multi,
            checked,
            selected,
        } => {
            let h = (items.len() as u16 + 4).min(f.area().height.saturating_sub(2));
            let area = centred(f.area(), 70, h);
            f.render_widget(Clear, area);
            let hint = if *multi {
                " ↑↓ move · space toggle · a all · enter confirm "
            } else {
                " ↑↓ move · enter select "
            };
            let block = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(ACCENT))
                .title(Span::styled(
                    format!(" {prompt} "),
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ))
                .title_bottom(Line::from(Span::styled(
                    hint,
                    Style::default().fg(Color::DarkGray),
                )));
            let rows: Vec<ListItem> = items
                .iter()
                .enumerate()
                .map(|(i, (short, long))| {
                    let mark = if *multi {
                        if checked[i] { "[x] " } else { "[ ] " }
                    } else {
                        ""
                    };
                    let label = if long.is_empty() {
                        short.clone()
                    } else {
                        format!("{short:<22} {long}")
                    };
                    ListItem::new(Line::from(vec![
                        Span::styled(mark, Style::default().fg(Color::Green)),
                        Span::raw(label),
                    ]))
                })
                .collect();
            let list = List::new(rows)
                .block(block)
                .highlight_symbol("➤ ")
                .highlight_style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD));
            let mut st = ListState::default();
            st.select(Some(*selected));
            f.render_stateful_widget(list, area, &mut st);
        }
    }
}

/// Run a command body titled `title` inside the TUI runner, passing a
/// `&mut dyn Ui` so the body is identical to the plain-text path. Returns
/// whether the command asked to follow (so the caller can open the live
/// dashboard on a fresh screen after this runner exits).
pub fn run_command<F>(title: &str, body: F) -> Result<bool>
where
    F: FnOnce(&mut dyn Ui) -> Result<()>,
{
    TuiUi::run(title, |ui| body(ui))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn age_formats_compactly() {
        assert_eq!(fmt_age(5), "5s");
        assert_eq!(fmt_age(90), "1m");
        assert_eq!(fmt_age(3_900), "1h5m");
    }

    #[test]
    fn progress_hint_elapsed_estimate_and_overdue() {
        assert_eq!(build_progress_hint(110, Some(190)), "1m50s/3m10s (~1m20s left)");
        assert_eq!(build_progress_hint(200, Some(190)), "3m20s/3m10s (overdue)");
        assert_eq!(build_progress_hint(45, None), "45s elapsed");
    }

    #[test]
    fn renders_panels_and_rows_to_a_buffer() {
        use crate::deploy::{BuildRow, BuildsView, SyncRow, SyncView};

        let snap = Snapshot {
            builds: Some(BuildsView {
                cron_installed: true,
                rows: vec![BuildRow {
                    state: BuildState::Building,
                    label: "https://jenkins.example/job/demo-app/".to_string(),
                    repo: "demo/app".to_string(),
                    age_secs: 45,
                    elapsed: Some(110),
                    estimated: Some(190),
                }],
            }),
            syncs: Some(SyncView {
                rows: Some(vec![SyncRow {
                    status: SyncState::Syncing,
                    app: "vartalap-dash".to_string(),
                    detail: "Synced/Progressing, Succeeded".to_string(),
                    repo: "Blitzscale-19/vartalap".to_string(),
                    age_secs: 60,
                }]),
            }),
        };

        let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
        terminal
            .draw(|f| draw(f, View::Dashboard, &snap, 0))
            .unwrap();
        let rendered = terminal.backend().to_string();

        // Title bar, both panels, and the seeded rows must all be present.
        assert!(rendered.contains("sts · dashboard"), "title missing");
        assert!(rendered.contains("Builds"), "builds panel missing");
        assert!(rendered.contains("ArgoCD"), "argocd panel missing");
        assert!(rendered.contains("building"), "build state missing");
        // The building row shows a progress bar whose label carries the live
        // elapsed/estimated hint (1m50s/3m10s) advanced each second.
        assert!(
            rendered.contains("1m50s/3m10s"),
            "build progress hint missing"
        );
        assert!(rendered.contains("vartalap-dash"), "sync app missing");
        assert!(rendered.contains("syncing"), "sync state missing");
        // The full status detail and repo must show in full (two-line row), not
        // clipped like the old single-line columns did.
        assert!(
            rendered.contains("Synced/Progressing, Succeeded"),
            "sync detail was truncated"
        );
        assert!(
            rendered.contains("Blitzscale-19/vartalap"),
            "repo was truncated"
        );
        assert!(rendered.contains("cron: on"), "cron status missing");
        assert!(rendered.contains('q') && rendered.contains("quit"), "footer missing");
        // Rounded borders mean the box-drawing corner should appear.
        assert!(rendered.contains('╭'), "panel border missing");
    }

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn render<F: FnOnce(&mut Frame)>(w: u16, h: u16, f: F) -> String {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|frame| f(frame)).unwrap();
        t.backend().to_string()
    }

    #[test]
    fn runner_shows_header_and_steps() {
        let steps = vec![
            Step { kind: StepKind::Step, text: "Fetching origin/stage".into() },
            Step { kind: StepKind::Ok, text: "Pushed stage at abc1234".into() },
            Step { kind: StepKind::Err, text: "push failed".into() },
        ];
        let out = render(100, 20, |f| draw_runner(f, "ship", 42, &steps, None));
        assert!(out.contains("sts · ship"), "runner title missing");
        assert!(out.contains("Fetching origin/stage"), "step line missing");
        assert!(out.contains("Pushed stage at abc1234"), "ok line missing");
        assert!(out.contains("push failed"), "err line missing");
        assert!(out.contains("steps"), "steps panel title missing");
        assert!(out.contains("42s"), "elapsed timer missing");
    }

    #[test]
    fn confirm_modal_renders_question() {
        let steps = vec![Step { kind: StepKind::Step, text: "ready".into() }];
        let modal = Modal::Confirm {
            question: "Merge `feature` into stage and push?",
        };
        let out = render(100, 20, |f| draw_runner(f, "ship", 1, &steps, Some(&modal)));
        assert!(out.contains("confirm"), "confirm title missing");
        assert!(out.contains("Merge `feature` into stage"), "question missing");
        assert!(out.contains("yes") && out.contains("no"), "y/n hint missing");
    }

    #[test]
    fn choose_modal_lists_items_with_checkboxes() {
        let steps = vec![Step { kind: StepKind::Step, text: "ready".into() }];
        let items = vec![
            ("x-app".to_string(), "/job/x-app/build".to_string()),
            ("x-dash".to_string(), "/job/x-dash/build".to_string()),
        ];
        let checked = [true, false];
        let modal = Modal::Choose {
            prompt: "Trigger which Jenkins job(s)?",
            items: &items,
            multi: true,
            checked: &checked,
            selected: 0,
        };
        let out = render(100, 20, |f| draw_runner(f, "ship", 1, &steps, Some(&modal)));
        assert!(out.contains("Trigger which"), "choose prompt missing");
        assert!(out.contains("x-app"), "item 1 missing");
        assert!(out.contains("x-dash"), "item 2 missing");
        assert!(out.contains("[x]"), "checked box missing");
        assert!(out.contains("[ ]"), "unchecked box missing");
        assert!(out.contains("space toggle"), "multi hint missing");
    }
}
