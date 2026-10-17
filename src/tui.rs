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
    Block, BorderType, Borders, Clear, List, ListItem, ListState, Padding, Paragraph, Wrap,
};
use std::time::{Duration, Instant};

/// How often the TUI redraws. Fast enough to animate the spinners and let the
/// clock and relative ages tick smoothly; the network data is only re-polled on
/// the slower per-view `poll` cadence, so this costs nothing but a repaint.
const REDRAW_TICK: Duration = Duration::from_millis(120);

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
            View::Dashboard => "dashboard",
            View::Pending => "pending builds",
            View::Sync => "argocd syncs",
            View::Follow => "follow",
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
    /// Public form of [`Snapshot::drained`], so callers can check whether a
    /// view is worth opening at all.
    pub fn is_drained(&self) -> bool {
        self.drained()
    }

    /// Both tracked lists this view cares about are empty — nothing left to
    /// watch, so the loop can exit on its own.
    fn drained(&self) -> bool {
        let b = self.builds.as_ref().is_none_or(|v| v.is_empty());
        let s = self.syncs.as_ref().is_none_or(|v| v.is_empty());
        b && s
    }

    /// Anything in flight: drives the header spinner and the footer's
    /// live/idle badge, so a still screen means a still pipeline.
    fn busy(&self) -> bool {
        !self.drained()
    }

    /// `2 builds · 1 syncing` for the footer's right gutter. Empty when there
    /// is nothing to count, so the footer stays quiet.
    fn counts(&self) -> String {
        let builds = self.builds.as_ref().map_or(0, |b| b.rows.len());
        let syncs = self
            .syncs
            .as_ref()
            .and_then(|s| s.rows.as_ref())
            .map_or(0, |r| r.len());
        let mut parts = Vec::new();
        if builds > 0 {
            parts.push(format!(
                "{builds} build{}",
                if builds == 1 { "" } else { "s" }
            ));
        }
        if syncs > 0 {
            parts.push(format!("{syncs} syncing"));
        }
        parts.join(" · ")
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
    crate::deploy::set_tui_active(true);
    let result = run_loop(&mut terminal, view, &mut refresh, &mut collect);
    crate::deploy::set_tui_active(false);
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
    // Redraw often so the spinners animate and the clock and relative ages
    // tick smoothly, but only re-poll the network on the slower `poll`
    // cadence. This keeps the UI live without hammering Jenkins/ArgoCD.
    let redraw = REDRAW_TICK.min(poll);

    // Prime the first frame immediately.
    refresh()?;
    let mut snapshot = collect()?;
    let mut collected_at = Instant::now();
    // Frame counter: drives the spinner phase.
    let mut tick = 0u64;

    loop {
        // Seconds elapsed since the data was collected, added to each row's age
        // at draw time so the "45s ago" counters tick up every redraw rather
        // than jumping once per poll.
        let age_offset = collected_at.elapsed().as_secs();
        terminal.draw(|f| draw(f, view, &snapshot, age_offset, tick))?;
        tick = tick.wrapping_add(1);

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

// ---- theme ------------------------------------------------------------------
//
// One palette, one set of glyphs, so every panel agrees and a re-skin is a
// single edit. The colours are 256-colour indices rather than the 8 basic ANSI
// ones: the basic set is remapped by every terminal theme (and "dark gray" is
// invisible on some), while the indexed palette renders the same soft blue /
// green / amber everywhere that supports 256 colours — which any terminal
// running this TUI does.

/// Live / in-progress, and every accent (titles, keys, bars).
const ACCENT: Color = Color::Indexed(39); // soft blue
/// Secondary text: ages, URLs, hints.
const MUTED: Color = Color::Indexed(245);
/// Panel borders — dimmer than the text they frame.
const BORDER: Color = Color::Indexed(238);
const OK: Color = Color::Indexed(78); // mint green
const WARN: Color = Color::Indexed(214); // amber
const BAD: Color = Color::Indexed(203); // soft red

/// Braille spinner frames for in-progress rows, advanced once per redraw so
/// the dashboard visibly breathes between the slower network polls.
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn spinner(tick: u64) -> &'static str {
    SPINNER[(tick % SPINNER.len() as u64) as usize]
}

/// Width of the state column ("building", "syncing", …) so the names that
/// follow it line up into a column.
const STATE_W: usize = 10;
/// Width of the inline progress bar.
const BAR_W: usize = 14;
/// Rows are two lines tall: identity, then detail.
const ROW_H: u16 = 2;

// ---- small span helpers -----------------------------------------------------

fn muted(text: impl Into<String>) -> Span<'static> {
    Span::styled(text.into(), Style::default().fg(MUTED))
}

fn bold(text: impl Into<String>) -> Span<'static> {
    Span::styled(text.into(), Style::default().add_modifier(Modifier::BOLD))
}

/// A keycap in the footer / hints.
fn key(k: &str) -> Span<'static> {
    Span::styled(
        k.to_string(),
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    )
}

/// A line of `left` spans with a dim right-hand gutter: the first of
/// `right_choices` that fits is used, so wide terminals get the full URL,
/// narrower ones the short form, and tiny ones none at all — detail degrades
/// instead of wrapping and breaking the two-line rows.
fn gutter_line(
    left: Vec<Span<'static>>,
    right_choices: &[String],
    width: u16,
) -> Line<'static> {
    let used: usize = left.iter().map(|s| s.width()).sum();
    let mut spans = left;
    let w = width as usize;
    for choice in right_choices {
        let len = choice.chars().count();
        // Keep a two-column gap from the left text and one from the edge.
        if used + len + 3 <= w {
            spans.push(Span::raw(" ".repeat(w - used - len - 1)));
            spans.push(muted(choice.clone()));
            break;
        }
    }
    Line::from(spans)
}

/// Pick the URL form for a whole panel: the full URL when every row can fit
/// one, otherwise the short path everywhere. Deciding per row left one line
/// showing `https://…` next to another showing `…/job/x/`, which read as a
/// ragged edge; the gutter should be one column.
fn use_full_urls(rows: &[(usize, &str)], width: u16) -> bool {
    let w = width as usize;
    rows.iter()
        .all(|(used, url)| used + url.chars().count() + 3 <= w)
}

/// `▕████████░░░░░░▏ 61%` — an inline bar, sized so it leaves room for the
/// timing text beside it (a full-width `Gauge` swallowed its own label).
fn bar_spans(ratio: f64) -> Vec<Span<'static>> {
    let ratio = ratio.clamp(0.0, 1.0);
    let filled = (ratio * BAR_W as f64).round() as usize;
    vec![
        Span::styled("▕", Style::default().fg(BORDER)),
        Span::styled("█".repeat(filled), Style::default().fg(ACCENT)),
        Span::styled("░".repeat(BAR_W - filled), Style::default().fg(BORDER)),
        Span::styled("▏", Style::default().fg(BORDER)),
        Span::styled(
            format!(" {:>3}%", (ratio * 100.0).round() as u64),
            Style::default().fg(ACCENT),
        ),
    ]
}

/// An indeterminate bar for "running, but we can't say how much is left": a
/// block sliding back and forth instead of a fill that would imply progress.
fn pulse_spans(tick: u64) -> Vec<Span<'static>> {
    const RUN: usize = 4;
    let span = BAR_W - RUN;
    // Bounce: 0..span then back, so the block never jumps.
    let phase = (tick / 2) as usize % (span * 2);
    let at = if phase <= span { phase } else { span * 2 - phase };
    vec![
        Span::styled("\u{2595}", Style::default().fg(BORDER)),
        Span::styled("\u{2591}".repeat(at), Style::default().fg(BORDER)),
        Span::styled("\u{2588}".repeat(RUN), Style::default().fg(WARN)),
        Span::styled("\u{2591}".repeat(span - at), Style::default().fg(BORDER)),
        Span::styled("\u{258f}", Style::default().fg(BORDER)),
        muted("  ··%"),
    ]
}

/// `jenkins::fmt_secs` under a local name, for the row details.
fn fmt_secs_pub(secs: u64) -> String {
    crate::jenkins::fmt_secs(secs)
}

// ---- rendering --------------------------------------------------------------

fn draw(f: &mut Frame, view: View, snap: &Snapshot, age_offset: u64, tick: u64) {
    f.render_widget(Clear, f.area());

    // header | body | footer. The header is a single line (a boxed title bar
    // cost three rows to say one thing), leaving the height to the panels.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(f.area());

    let busy = snap.busy();
    draw_header(
        f,
        chunks[0],
        view.title(),
        fmt_clock(&crate::deploy::utc_now()),
        busy.then(|| spinner(tick)),
    );
    draw_body(f, chunks[1], view, snap, age_offset, tick);
    draw_footer(f, chunks[2], snap);
}

/// The header bar: an `sts` badge, the view name, an optional spinner while
/// anything is in flight, and the clock in the right gutter.
fn draw_header(
    f: &mut Frame,
    area: Rect,
    title: &str,
    right: String,
    spin: Option<&'static str>,
) {
    let mut left = vec![
        Span::styled(
            " sts ",
            Style::default()
                .fg(Color::Black)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        bold(title.to_string()),
    ];
    if let Some(s) = spin {
        left.push(Span::raw(" "));
        left.push(Span::styled(s, Style::default().fg(ACCENT)));
    }
    f.render_widget(
        Paragraph::new(gutter_line(left, &[right], area.width)),
        area,
    );
}

fn draw_body(
    f: &mut Frame,
    area: Rect,
    view: View,
    snap: &Snapshot,
    age_offset: u64,
    tick: u64,
) {
    match (view.shows_builds(), view.shows_syncs()) {
        (true, true) => {
            // Size each panel to its contents when both fit, so two builds and
            // one sync don't sit in two half-empty boxes; fall back to halves
            // once there's more than the screen can hold.
            let bh = panel_height(snap.builds.as_ref().map_or(0, |b| b.rows.len()));
            let sh = panel_height(
                snap.syncs
                    .as_ref()
                    .and_then(|s| s.rows.as_ref())
                    .map_or(0, |r| r.len()),
            );
            let constraints = if bh + sh <= area.height {
                vec![
                    Constraint::Length(bh),
                    Constraint::Length(sh),
                    Constraint::Min(0),
                ]
            } else {
                vec![Constraint::Percentage(50), Constraint::Percentage(50)]
            };
            let parts = Layout::default()
                .direction(Direction::Vertical)
                .constraints(constraints)
                .split(area);
            draw_builds(f, parts[0], snap.builds.as_ref(), age_offset, tick);
            draw_syncs(f, parts[1], snap.syncs.as_ref(), age_offset, tick);
        }
        (true, false) => draw_builds(f, area, snap.builds.as_ref(), age_offset, tick),
        (false, true) => draw_syncs(f, area, snap.syncs.as_ref(), age_offset, tick),
        (false, false) => {}
    }
}

/// Rows the panel needs: borders + two lines per row, or one line for the
/// empty / unavailable message.
fn panel_height(rows: usize) -> u16 {
    2 + if rows == 0 {
        1
    } else {
        rows as u16 * ROW_H
    }
}

fn panel<'a>(title: &'a str, count: Option<usize>) -> Block<'a> {
    let mut spans = vec![
        Span::raw(" "),
        Span::styled(
            title,
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
    ];
    if let Some(n) = count {
        spans.push(muted(format!(" {n}")));
    }
    spans.push(Span::raw(" "));
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(BORDER))
        .title(Line::from(spans))
}

/// A panel whose body is a single centred message (empty state, or "can't
/// check"). Keeps those cases looking deliberate rather than like a blank box.
fn draw_message(f: &mut Frame, area: Rect, block: Block, line: Line) {
    f.render_widget(
        Paragraph::new(line)
            .block(block)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn settled(text: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled("✓ ", Style::default().fg(OK)),
        Span::styled(text.to_string(), Style::default().fg(OK)),
    ])
}

/// Lay out `n` two-line row slots inside `inner`, capped at what fits.
fn row_slots(inner: Rect, n: usize) -> std::rc::Rc<[Rect]> {
    let height = inner.height as usize;
    let fits = (height / ROW_H as usize).max(1);
    // When rows don't all fit, the "+N more" note needs a line of its own:
    // use the odd line left over by the two-line rows if there is one, and
    // only give up a row when there isn't.
    let spare = height % ROW_H as usize;
    let max_rows = if n > fits && spare == 0 {
        fits.saturating_sub(1).max(1)
    } else {
        fits
    };
    Layout::default()
        .direction(Direction::Vertical)
        .constraints(
            std::iter::repeat_n(Constraint::Length(ROW_H), n.min(max_rows)).collect::<Vec<_>>(),
        )
        .split(inner)
}

/// When the panel is too short for every row, say how many are hidden instead
/// of silently dropping them.
fn draw_clipped_note(f: &mut Frame, inner: Rect, total: usize, shown: usize) {
    if total <= shown || inner.height == 0 {
        return;
    }
    let area = Rect {
        x: inner.x,
        y: inner.y + inner.height - 1,
        width: inner.width,
        height: 1,
    };
    f.render_widget(
        Paragraph::new(
            Line::from(muted(format!("+{} more ", total - shown))).right_aligned(),
        ),
        area,
    );
}

/// Split a row slot into its identity line and its detail line.
fn two_lines(slot: Rect) -> std::rc::Rc<[Rect]> {
    Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .split(slot)
}

fn draw_builds(
    f: &mut Frame,
    area: Rect,
    builds: Option<&BuildsView>,
    age_offset: u64,
    tick: u64,
) {
    let Some(builds) = builds else {
        draw_message(
            f,
            area,
            panel("builds", None),
            Line::from(muted("no build queue")),
        );
        return;
    };

    let (cron_text, cron_colour) = if builds.cron_installed {
        ("on", OK)
    } else {
        ("off", WARN)
    };
    let cron = vec![
        muted(" cron "),
        Span::styled(cron_text, Style::default().fg(cron_colour)),
        Span::raw(" "),
    ];
    let block = panel("builds", Some(builds.rows.len()))
        .title_bottom(Line::from(cron).right_aligned());

    if builds.rows.is_empty() {
        draw_message(f, area, block, settled("nothing waiting to deploy"));
        return;
    }

    // Draw the frame first, then the rows inside it: each row is two lines with
    // its own spans, which a Table cell can't express.
    f.render_widget(&block, area);
    let inner = block.inner(area);
    if inner.height == 0 {
        return;
    }
    let slots = row_slots(inner, builds.rows.len());
    draw_clipped_note(f, inner, builds.rows.len(), slots.len());

    // First pass: build each row's two lines of spans. Second pass: now that
    // every row's width is known, pick one URL form for the panel and draw.
    let mut rows: Vec<(Vec<Span>, Vec<Span>, &str)> = Vec::new();
    for b in builds.rows.iter().take(slots.len()) {
        let (glyph, state, colour) = match b.state {
            BuildState::Building => (spinner(tick), "building", ACCENT),
            // Waiting rows get a still glyph: nothing is moving, so nothing
            // on the row should either.
            BuildState::Waiting => ("\u{25cc}", "waiting", WARN),
            BuildState::Queued => ("\u{25cc}", "queued", WARN),
        };

        // Line 1: what this is — job name and build number, with the repo and
        // the queue age in the right gutter (which keeps line 2 short enough
        // for the build URL to sit beside the progress bar).
        let mut head = vec![
            Span::styled(format!(" {glyph} "), Style::default().fg(colour)),
            Span::styled(
                format!("{state:<STATE_W$}"),
                Style::default().fg(colour).add_modifier(Modifier::BOLD),
            ),
            bold(b.job.clone()),
        ];
        if let Some(n) = b.build {
            head.push(muted(format!(" #{n}")));
        }
        let age = fmt_age(b.age_secs + age_offset);
        let head = gutter_line(
            head,
            &[format!("{} · queued {age} ago", b.repo), b.repo.clone()],
            inner.width,
        );

        // Line 2: how far along it is. `age_offset` advances both the bar and
        // the timings every redraw, so they tick smoothly between polls.
        let mut detail: Vec<Span> = vec![Span::raw("    ")];
        match b.state {
            // Executing. Progress comes from real stage completion when the
            // pipeline's length is known; otherwise from the clock against
            // Jenkins' estimate; and once past that estimate, from nothing at
            // all — an indeterminate bar, because what's left is unknown.
            BuildState::Building => {
                let live = b.elapsed.map(|e| e + age_offset);
                let stage = b.stage.as_ref();
                let stage_ratio = stage.filter(|st| st.total > 0).map(|st| st.ratio());
                let time_ratio = match (live, b.estimated) {
                    (Some(live), Some(est)) if est > live => Some(live as f64 / est as f64),
                    _ => None,
                };
                match (stage_ratio.or(time_ratio), live) {
                    (Some(ratio), _) => detail.extend(bar_spans(ratio)),
                    (None, Some(_)) => detail.extend(pulse_spans(tick)),
                    (None, None) => {}
                }

                // Text beside the bar: the stage it is on, then the timing.
                // With a stage bar the elapsed time is enough; without one the
                // full elapsed/estimate hint carries the progress.
                let mut text: Vec<String> = Vec::new();
                match (stage, live) {
                    (Some(st), live) if st.total > 0 => {
                        text.push(format!(
                            "stage {}/{} {}",
                            (st.done + 1).min(st.total),
                            st.total,
                            st.name
                        ));
                        if let Some(live) = live {
                            text.push(fmt_secs_pub(live));
                        }
                    }
                    // A stage name but no pipeline length: name it, don't
                    // invent a fraction.
                    (Some(st), live) => {
                        text.push(st.name.clone());
                        if let Some(live) = live {
                            text.push(build_progress_hint(live, b.estimated));
                        }
                    }
                    (None, Some(live)) => text.push(build_progress_hint(live, b.estimated)),
                    (None, None) => text.push("running".to_string()),
                }
                detail.push(muted(format!("  {}", text.join("  ·  "))));
            }
            // Has a number, but no executor has picked it up: show the wait,
            // not a progress bar that would creep towards "overdue".
            BuildState::Waiting => {
                let waited = b.elapsed.map(|e| e + age_offset);
                detail.push(Span::styled(
                    "waiting for a free executor",
                    Style::default().fg(WARN),
                ));
                if let Some(waited) = waited {
                    detail.push(muted(format!("  ·  {} in the queue", fmt_secs_pub(waited))));
                }
            }
            BuildState::Queued => detail.push(muted(
                b.why
                    .clone()
                    .unwrap_or_else(|| "waiting for Jenkins to start the build".to_string()),
            )),
        }
        rows.push((head.spans, detail, b.label.as_str()));
    }

    draw_rows(f, &slots, rows, inner.width);
}

/// Draw prepared rows into their slots: the identity line as-is, the detail
/// line with the panel's shared URL gutter.
fn draw_rows(
    f: &mut Frame,
    slots: &[Rect],
    rows: Vec<(Vec<Span<'static>>, Vec<Span<'static>>, &str)>,
    width: u16,
) {
    let widths: Vec<(usize, &str)> = rows
        .iter()
        .map(|(_, detail, url)| (detail.iter().map(|s| s.width()).sum(), *url))
        .collect();
    let full = use_full_urls(&widths, width);

    for ((head, detail, url), slot) in rows.into_iter().zip(slots.iter()) {
        let lines = two_lines(*slot);
        f.render_widget(Paragraph::new(Line::from(head)), lines[0]);
        let gutter = if full {
            url.to_string()
        } else {
            short_url(url)
        };
        f.render_widget(
            Paragraph::new(gutter_line(detail, &[gutter], lines[1].width)),
            lines[1],
        );
    }
}

fn draw_syncs(f: &mut Frame, area: Rect, syncs: Option<&SyncView>, age_offset: u64, tick: u64) {
    let Some(syncs) = syncs else {
        draw_message(
            f,
            area,
            panel("argocd", None),
            Line::from(muted("no sync tracking")),
        );
        return;
    };

    let Some(rows) = syncs.rows.as_ref() else {
        draw_message(
            f,
            area,
            panel("argocd", None),
            Line::from(Span::styled(
                "no argocd_token configured — can't check sync status",
                Style::default().fg(WARN),
            )),
        );
        return;
    };

    let block = panel("argocd", Some(rows.len()));
    if rows.is_empty() {
        draw_message(f, area, block, settled("all tracked syncs have settled"));
        return;
    }

    f.render_widget(&block, area);
    let inner = block.inner(area);
    if inner.height == 0 {
        return;
    }
    let slots = row_slots(inner, rows.len());
    draw_clipped_note(f, inner, rows.len(), slots.len());

    let mut prepared: Vec<(Vec<Span>, Vec<Span>, &str)> = Vec::new();
    for s in rows.iter().take(slots.len()) {
        let (glyph, label, colour) = sync_look(s.status);
        // A live row spins; settled ones keep their fixed glyph.
        let glyph = if s.status == SyncState::Syncing {
            spinner(tick)
        } else {
            glyph
        };

        // Line 1: state + app, repo in the right gutter.
        let head = vec![
            Span::styled(format!(" {glyph} "), Style::default().fg(colour)),
            Span::styled(
                format!("{label:<STATE_W$}"),
                Style::default().fg(colour).add_modifier(Modifier::BOLD),
            ),
            bold(s.app.clone()),
        ];
        let head = gutter_line(head, std::slice::from_ref(&s.repo), inner.width);

        // Line 2: the full ArgoCD status in the state's colour, with the app's
        // ArgoCD URL in the gutter — the counterpart to a build's Jenkins link.
        let detail = vec![
            Span::raw("    "),
            Span::styled(s.detail.clone(), Style::default().fg(colour)),
            muted(format!("  ·  {} ago", fmt_age(s.age_secs + age_offset))),
        ];
        prepared.push((head.spans, detail, s.url.as_str()));
    }

    draw_rows(f, &slots, prepared, inner.width);
}

/// (glyph, label, colour) for a sync status.
fn sync_look(state: SyncState) -> (&'static str, &'static str, Color) {
    match state {
        SyncState::Syncing => ("◍", "syncing", ACCENT),
        SyncState::Synced => ("✓", "synced", OK),
        SyncState::Degraded => ("✗", "degraded", BAD),
        SyncState::Gone => ("–", "gone", MUTED),
        SyncState::GaveUp => ("✗", "gave up", BAD),
        SyncState::Unknown => ("?", "unknown", WARN),
    }
}

fn draw_footer(f: &mut Frame, area: Rect, snap: &Snapshot) {
    let left = vec![
        Span::raw(" "),
        key("q"),
        muted(" quit"),
        muted("   "),
        Span::styled(
            if snap.busy() { "live" } else { "idle" },
            Style::default().fg(if snap.busy() { OK } else { MUTED }),
        ),
        muted(" · refreshes itself · exits when the pipeline is empty"),
    ];
    f.render_widget(
        Paragraph::new(gutter_line(left, &[snap.counts()], area.width)),
        area,
    );
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
            format!("{}/{} · ~{} left", fmt_secs(elapsed), fmt_secs(est), fmt_secs(est - elapsed))
        }
        // Past the estimate, how much is left is unknown — say that instead of
        // implying the build is late by a known amount.
        Some(est) => format!(
            "{} · over the usual {}",
            fmt_secs(elapsed),
            fmt_secs(est)
        ),
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

/// `2026-10-17T05:03:28Z` → `05:03:28 UTC`: the date is noise in a live view.
fn fmt_clock(iso: &str) -> String {
    match iso.split_once('T') {
        Some((_, time)) => format!("{} UTC", time.trim_end_matches('Z')),
        None => iso.to_string(),
    }
}

/// Drop scheme and host from a URL, keeping the path — the fallback for the
/// right gutter when the full URL doesn't fit.
fn short_url(url: &str) -> String {
    match url.split_once("://") {
        Some((_, rest)) => match rest.find('/') {
            Some(i) => format!("…{}", &rest[i..]),
            None => url.to_string(),
        },
        None => url.to_string(),
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
    /// Rows the steps panel is scrolled back from the newest line. 0 while the
    /// command runs (the newest output is what matters); the user can scroll
    /// once it has finished, which is what makes a long listing readable.
    scroll: usize,
    /// Whether this command's output is a sequence of timed steps (ship,
    /// build, check) or a static listing (`-j`, `-H`). Listings skip the
    /// elapsed timer and the per-line `mm:ss` gutter, which would just be a
    /// column of `00:00`.
    timed: bool,
    started: Instant,
    steps: Vec<Step>,
    /// Set by `request_handoff`: launch the live dashboard after this runner
    /// screen tears down (two alternate screens can't be nested).
    handoff_requested: bool,
}

impl TuiUi {
    /// Run `body` with a `TuiUi` for a command titled `title`. Sets up the
    /// alternate screen first and always restores it afterwards, even on error.
    /// After `body` returns, the final screen is held until the user presses a
    /// key so they can read the result. Returns whether the body requested the
    /// follow dashboard (so the caller can launch it on a fresh screen).
    pub fn run<F>(title: &str, timed: bool, body: F) -> Result<bool>
    where
        F: FnOnce(&mut TuiUi) -> Result<()>,
    {
        let terminal = ratatui::try_init()?;
        crate::deploy::set_tui_active(true);
        let mut ui = TuiUi {
            terminal,
            title: title.to_string(),
            scroll: 0,
            timed,
            started: Instant::now(),
            steps: Vec::new(),
            handoff_requested: false,
        };
        let result = body(&mut ui);
        let follow = result.is_ok() && ui.handoff_requested;
        let hold = ui.finish(&result);
        crate::deploy::set_tui_active(false);
        ratatui::try_restore()?;
        hold?;
        result?;
        Ok(follow)
    }

    /// Push a step and redraw. Draw errors are ignored so a transient redraw
    /// failure can't abort the command mid-flight; the next draw recovers.
    fn push(&mut self, kind: StepKind, text: String) {
        self.push_row(kind, None, text);
    }

    /// Push a line that may carry a label column, and redraw.
    fn push_row(&mut self, kind: StepKind, label: Option<String>, text: String) {
        let at = self.started.elapsed().as_secs();
        self.steps.push(Step { kind, label, text, at });
        let _ = self.redraw();
    }

    fn redraw(&mut self) -> Result<()> {
        let title = self.title.clone();
        let elapsed = self.timed.then(|| self.started.elapsed().as_secs());
        let steps = self.steps.clone();
        let scroll = self.scroll;
        self.terminal
            .draw(|f| draw_runner(f, &title, elapsed, &steps, scroll, None))?;
        Ok(())
    }

    /// After the body finishes, append a final outcome line and wait for a key
    /// — unless the command is about to flow into the follow dashboard, in
    /// which case we return immediately so the hand-off is seamless.
    fn finish(&mut self, result: &Result<()>) -> Result<()> {
        if result.is_ok() && self.handoff_requested {
            return Ok(());
        }
        let at = self.started.elapsed().as_secs();
        if result.is_ok() && !self.timed {
            // A listing is its own result; the footer already says how to exit.
            let _ = self.redraw();
            return self.wait_for_key();
        }
        match result {
            Ok(()) => self.steps.push(Step {
                kind: StepKind::Ok,
                label: None,
                text: "done".to_string(),
                at,
            }),
            Err(e) => self.steps.push(Step {
                kind: StepKind::Err,
                label: None,
                text: format!("{e:#}"),
                at,
            }),
        }
        let _ = self.redraw();
        self.wait_for_key()
    }

    /// Hold the final screen until the user quits, scrolling the output on
    /// request. Long listings (`sts -H`, `sts -ja`) and long runs don't fit on
    /// one screen, so this is the only way to read the start of them.
    fn wait_for_key(&mut self) -> Result<()> {
        loop {
            let page = self.page_rows() as isize;
            if event::poll(Duration::from_secs(3600))?
                && let Event::Key(k) = event::read()?
                && k.kind == KeyEventKind::Press
            {
                match k.code {
                    KeyCode::Char('q') | KeyCode::Esc | KeyCode::Enter => return Ok(()),
                    KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Ok(());
                    }
                    KeyCode::Up | KeyCode::Char('k') => self.scroll_by(1),
                    KeyCode::Down | KeyCode::Char('j') => self.scroll_by(-1),
                    KeyCode::PageUp => self.scroll_by(page),
                    KeyCode::PageDown => self.scroll_by(-page),
                    KeyCode::Home | KeyCode::Char('g') => self.scroll = usize::MAX,
                    KeyCode::End | KeyCode::Char('G') => self.scroll = 0,
                    _ => {}
                }
                // Clamp against what is on screen, then redraw.
                let max = self.steps.len().saturating_sub(self.page_rows());
                self.scroll = self.scroll.min(max);
                let _ = self.redraw();
            }
        }
    }

    /// Rows the steps panel can show: the terminal height minus the header,
    /// the panel's borders and the footer.
    fn page_rows(&self) -> usize {
        self.terminal
            .size()
            .map(|s| (s.height as usize).saturating_sub(4).max(1))
            .unwrap_or(10)
    }

    fn scroll_by(&mut self, rows: isize) {
        if rows >= 0 {
            self.scroll = self.scroll.saturating_add(rows as usize);
        } else {
            self.scroll = self.scroll.saturating_sub((-rows) as usize);
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
    fn row(&mut self, kind: StepKind, label: &str, text: &str) {
        self.push_row(kind, Some(label.to_string()), text.to_string());
    }

    fn confirm(&mut self, question: &str) -> Result<bool> {
        let title = self.title.clone();
        let steps = self.steps.clone();
        loop {
            let elapsed = self.timed.then(|| self.started.elapsed().as_secs());
            let modal = Modal::Confirm { question };
            self.terminal
                .draw(|f| draw_runner(f, &title, elapsed, &steps, 0, Some(&modal)))?;
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
            let elapsed = self.timed.then(|| self.started.elapsed().as_secs());
            let modal = Modal::Choose {
                prompt,
                items,
                multi,
                checked: &checked,
                selected,
            };
            self.terminal
                .draw(|f| draw_runner(f, &title, elapsed, &steps, 0, Some(&modal)))?;

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

    fn request_handoff(&mut self) {
        self.handoff_requested = true;
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

/// Draw the runner screen: a one-line header (command + elapsed), the
/// scrolling steps panel, a footer, and an optional modal on top.
fn draw_runner(
    f: &mut Frame,
    title: &str,
    elapsed: Option<u64>,
    steps: &[Step],
    scroll: usize,
    modal: Option<&Modal>,
) {
    f.render_widget(Clear, f.area());
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(f.area());

    // The runner only redraws when a step lands, so the spinner would freeze
    // mid-command — the elapsed clock carries the "still working" signal here.
    // Listings have no clock and no `mm:ss` gutter.
    let timed = elapsed.is_some();
    draw_header(
        f,
        chunks[0],
        title,
        elapsed.map(fmt_age).unwrap_or_default(),
        None,
    );

    // Steps panel: render the lines, scrolled so the newest are visible.
    let body = chunks[1];
    let inner_h = body.height.saturating_sub(2) as usize;
    let lines: Vec<Line> = steps
        .iter()
        .enumerate()
        .map(|(i, s)| step_line(s, i + 1 == steps.len(), timed))
        .collect();
    // `scroll` counts rows back from the newest line, so the view stays
    // pinned to the end while a command runs and can be wound back after.
    let max_scroll = lines.len().saturating_sub(inner_h);
    let scroll = scroll.min(max_scroll);
    let end = lines.len() - scroll;
    let start = end.saturating_sub(inner_h);
    let shown: Vec<Line> = lines[start..end].to_vec();
    let clipped = scroll > 0 || start > 0;
    f.render_widget(
        Paragraph::new(shown)
            // A timed command's lines are steps; a listing's are its rows, so
            // the panel takes the command's own name.
            .block(
                panel(if timed { "steps" } else { title }, Some(steps.len()))
                    .padding(Padding::horizontal(1)),
            )
            .wrap(Wrap { trim: false }),
        body,
    );

    // While a command runs, Ctrl-C is the only thing to say. Once it has
    // finished (or for a listing) the output may not fit, so advertise the
    // scroll keys and how to leave.
    let hint = if timed && !clipped {
        vec![Span::raw(" "), key("ctrl-c"), muted(" abort")]
    } else {
        vec![
            Span::raw(" "),
            key("q"),
            muted(" exit  "),
            key("↑↓/pgup/pgdn"),
            muted(" scroll  "),
            key("g/G"),
            muted(" top/end"),
        ]
    };
    let position = if clipped {
        format!("{}-{} of {}", start + 1, end, lines.len())
    } else {
        String::new()
    };
    f.render_widget(
        Paragraph::new(gutter_line(hint, &[position], chunks[2].width)),
        chunks[2],
    );

    if let Some(m) = modal {
        draw_modal(f, m);
    }
}

/// Render one step: a dim `mm:ss` gutter, a one-column glyph, then the text.
/// The fixed-width gutter and glyph keep every line's text in the same column,
/// so the panel reads as a list instead of a ragged edge. `last` marks the
/// newest line, which is the one still in flight.
fn step_line(s: &Step, last: bool, timed: bool) -> Line<'static> {
    let (glyph, colour) = match s.kind {
        StepKind::Step => ("▸", ACCENT),
        StepKind::Info => (" ", Color::Reset),
        StepKind::Detail => (" ", MUTED),
        StepKind::Ok => ("✓", OK),
        StepKind::Warn => ("!", WARN),
        StepKind::Err => ("✗", BAD),
    };
    let text_style = match s.kind {
        StepKind::Step if last => Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        StepKind::Step => Style::default().add_modifier(Modifier::BOLD),
        StepKind::Detail => Style::default().fg(MUTED),
        StepKind::Ok => Style::default().fg(OK),
        StepKind::Warn => Style::default().fg(WARN),
        StepKind::Err => Style::default().fg(BAD),
        StepKind::Info => Style::default(),
    };
    let mut spans = Vec::new();
    if timed {
        spans.push(muted(format!("{} ", fmt_mmss(s.at))));
    }
    spans.push(Span::styled(
        format!(" {glyph} "),
        Style::default().fg(colour),
    ));
    match &s.label {
        // A labelled row: the label sits in its own fixed-width column, in the
        // status colour, so a run of rows lines up as a table.
        Some(label) => {
            spans.push(Span::styled(
                format!("{label:<width$} ", width = crate::ui::LABEL_W),
                Style::default().fg(if s.kind == StepKind::Detail {
                    MUTED
                } else {
                    colour
                }),
            ));
            spans.push(Span::styled(s.text.clone(), text_style));
        }
        // Details are indented one extra level so they hang under their step.
        None => {
            let indent = if s.kind == StepKind::Detail { "  " } else { "" };
            spans.push(Span::styled(format!("{indent}{}", s.text), text_style));
        }
    }
    Line::from(spans)
}

/// `mm:ss` since the command started, for the steps gutter.
fn fmt_mmss(secs: u64) -> String {
    format!("{:02}:{:02}", secs / 60, secs % 60)
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

/// The modal frame: accent border, bold title, dim key hint along the bottom.
fn modal_block<'a>(title: String, hint: &'a str) -> Block<'a> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .padding(Padding::symmetric(1, 0))
        .title(Line::from(vec![
            Span::raw(" "),
            Span::styled(title, Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
            Span::raw(" "),
        ]))
        .title_bottom(Line::from(muted(format!(" {hint} "))).centered())
}

fn draw_modal(f: &mut Frame, modal: &Modal) {
    match modal {
        Modal::Confirm { question } => {
            let area = centred(f.area(), 60, 6);
            f.render_widget(Clear, area);
            let text = Text::from(vec![
                Line::from(bold(question.to_string())),
                Line::raw(""),
                Line::from(vec![
                    Span::styled(" y ", Style::default().fg(Color::Black).bg(OK).add_modifier(Modifier::BOLD)),
                    muted(" yes     "),
                    Span::styled(" n ", Style::default().fg(Color::Black).bg(BAD).add_modifier(Modifier::BOLD)),
                    muted(" no (default)"),
                ]),
            ]);
            f.render_widget(
                Paragraph::new(text)
                    .block(modal_block("confirm".to_string(), "y / n · esc cancels"))
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
                "↑↓ move · space toggle · a all · enter confirm"
            } else {
                "↑↓ move · enter select"
            };
            let block = modal_block(prompt.to_string(), hint);
            let rows: Vec<ListItem> = items
                .iter()
                .enumerate()
                .map(|(i, (short, long))| {
                    // Multi-select shows a tick box; single-select shows none
                    // (the highlighted row *is* the selection).
                    let mark = if *multi {
                        if checked[i] { "◉ " } else { "○ " }
                    } else {
                        ""
                    };
                    let mut spans = vec![
                        Span::styled(
                            mark,
                            Style::default().fg(if *multi && checked[i] { OK } else { MUTED }),
                        ),
                        Span::raw(format!("{short:<22}")),
                    ];
                    if !long.is_empty() {
                        spans.push(muted(long.clone()));
                    }
                    ListItem::new(Line::from(spans))
                })
                .collect();
            let list = List::new(rows)
                .block(block)
                .highlight_symbol("▌")
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
pub fn run_command<F>(title: &str, timed: bool, body: F) -> Result<bool>
where
    F: FnOnce(&mut dyn Ui) -> Result<()>,
{
    TuiUi::run(title, timed, |ui| body(ui))
}

// ---- the log viewer ---------------------------------------------------------
//
// A full-screen pager for `sts -l` (an ArgoCD app's container logs) and
// `sts -L` (the deploy watcher's own log). It follows new output like `tail -f`
// but, unlike the old plain-text streaming, you can scroll back through it,
// toggle wrapping, and see at a glance what is being tailed.
//
// The data source is a closure the caller polls: it returns any lines that have
// appeared since the last call. The viewer owns the buffer, the viewport and
// the keys; it knows nothing about ArgoCD or files.

/// How many lines of scrollback to keep. Older lines fall off the top, which
/// bounds memory on a chatty app without the user noticing.
const LOG_SCROLLBACK: usize = 5_000;

/// One line of log output. `text` is the printable text with ANSI sequences
/// removed; `parts` records the colours those sequences asked for, as character
/// ranges into `text`, so the viewer can wrap and slice the line and still
/// paint it the way the container meant it.
#[derive(Clone)]
pub struct LogLine {
    /// Which pod/stream it came from, shown as a dim prefix when several are
    /// being tailed at once. `None` for a single source.
    pub source: Option<String>,
    pub text: String,
    pub parts: Vec<(usize, usize, Style)>,
    pub kind: LogKind,
}

#[derive(Clone, Copy, PartialEq)]
pub enum LogKind {
    /// Ordinary output.
    Line,
    /// A note from sts itself (e.g. "pod restarted"), not from the log.
    Note,
    /// A failure sts wants to surface without aborting the tail.
    Error,
}

impl LogLine {
    pub fn plain(text: impl Into<String>) -> LogLine {
        LogLine::new(None, text.into(), LogKind::Line)
    }
    pub fn from(source: &str, text: impl Into<String>) -> LogLine {
        LogLine::new(Some(source.to_string()), text.into(), LogKind::Line)
    }
    pub fn note(text: impl Into<String>) -> LogLine {
        LogLine::new(None, text.into(), LogKind::Note)
    }
    pub fn error(text: impl Into<String>) -> LogLine {
        LogLine::new(None, text.into(), LogKind::Error)
    }

    /// Clean the raw text and keep whatever colour it carried.
    fn new(source: Option<String>, raw: String, kind: LogKind) -> LogLine {
        let (text, parts) = parse_ansi(&raw);
        LogLine { source, text, parts, kind }
    }

    /// The style for the character at `i`, if the line asked for one.
    fn style_at(&self, i: usize) -> Option<Style> {
        self.parts
            .iter()
            .find(|(start, end, _)| i >= *start && i < *end)
            .map(|(_, _, st)| *st)
    }
}

/// The viewer's state: the buffer plus where the viewport sits in it. Kept
/// free of ratatui and of the terminal so the scrolling rules can be tested.
pub struct LogView {
    lines: std::collections::VecDeque<LogLine>,
    /// Index of the first *display row* shown, when not following.
    offset: usize,
    /// Stick to the newest output. Scrolling up turns this off (like `less`),
    /// `End`/`G`/`f` turns it back on.
    follow: bool,
    /// Wrap long lines instead of truncating them.
    wrap: bool,
    /// Lines dropped off the top, so the footer can say the buffer is full.
    dropped: usize,
}

impl Default for LogView {
    fn default() -> Self {
        LogView {
            lines: std::collections::VecDeque::new(),
            offset: 0,
            follow: true,
            wrap: false,
            dropped: 0,
        }
    }
}

impl LogView {
    pub fn push(&mut self, line: LogLine) {
        self.lines.push_back(line);
        while self.lines.len() > LOG_SCROLLBACK {
            self.lines.pop_front();
            self.dropped += 1;
            // Keep the viewport pointing at the same text as the buffer slides.
            self.offset = self.offset.saturating_sub(1);
        }
    }

    pub fn extend(&mut self, lines: impl IntoIterator<Item = LogLine>) {
        for l in lines {
            self.push(line_or_empty(l));
        }
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// How many display rows the buffer occupies in a viewport `width` wide.
    /// Counted without building the rows, so a long scrollback costs nothing
    /// per redraw.
    fn row_count(&self, width: usize) -> usize {
        self.lines
            .iter()
            .map(|l| self.rows_of(l, width).len().max(1))
            .sum()
    }

    /// The display rows for one line, as character ranges into its text: one
    /// (possibly truncated) row, or several wrapped ones. Ranges rather than
    /// strings, so the colours recorded against those characters survive.
    fn rows_of(&self, line: &LogLine, width: usize) -> Vec<std::ops::Range<usize>> {
        let prefix = line.source.as_ref().map_or(0, |s| s.chars().count() + 2);
        let avail = width.saturating_sub(prefix).max(8);
        let len = line.text.chars().count();
        if self.wrap {
            wrap_ranges(&line.text, avail)
        } else {
            // One row covering the visible slice of the line.
            std::iter::once(0..len.min(avail)).collect()
        }
    }

    /// Materialise just the rows in `range`, so only what is on screen is
    /// built. Each row is (kind, source, spans) ready to render.
    fn visible_rows(
        &self,
        width: usize,
        range: std::ops::Range<usize>,
    ) -> Vec<(LogKind, Option<String>, Vec<Span<'static>>)> {
        let mut out = Vec::new();
        let mut i = 0usize;
        for line in &self.lines {
            let rows = self.rows_of(line, width);
            let truncated = !self.wrap && rows[0].end < line.text.chars().count();
            for r in rows {
                if range.contains(&i) {
                    out.push((
                        line.kind,
                        line.source.clone(),
                        row_spans(line, r.clone(), truncated),
                    ));
                }
                i += 1;
                if i >= range.end {
                    return out;
                }
            }
        }
        out
    }

    /// Clamp the viewport and resolve which rows are visible for a window
    /// `height` rows tall. Following pins it to the end.
    fn window(&mut self, rows: usize, height: usize) -> std::ops::Range<usize> {
        let max_offset = rows.saturating_sub(height);
        if self.follow {
            self.offset = max_offset;
        } else {
            self.offset = self.offset.min(max_offset);
        }
        let end = (self.offset + height).min(rows);
        self.offset..end
    }

    // ---- scrolling ----------------------------------------------------------

    pub fn scroll_up(&mut self, n: usize) {
        // Any upward movement means the user wants to read, not to follow.
        self.follow = false;
        self.offset = self.offset.saturating_sub(n);
    }

    pub fn scroll_down(&mut self, n: usize) {
        self.offset = self.offset.saturating_add(n);
    }

    pub fn scroll_to_top(&mut self) {
        self.follow = false;
        self.offset = 0;
    }

    pub fn scroll_to_bottom(&mut self) {
        self.follow = true;
    }

    pub fn toggle_follow(&mut self) {
        self.follow = !self.follow;
    }

    pub fn toggle_wrap(&mut self) {
        self.wrap = !self.wrap;
    }

    pub fn following(&self) -> bool {
        self.follow
    }
}

/// An empty log line still deserves a row, so blank separator lines in the
/// source survive into the viewer.
fn line_or_empty(mut l: LogLine) -> LogLine {
    if l.text.is_empty() {
        l.text = " ".to_string();
    }
    l
}

/// Build the spans for one display row: the characters in `range`, split where
/// the line's colours change, with a `…` when the row was cut short.
fn row_spans(line: &LogLine, range: std::ops::Range<usize>, truncated: bool) -> Vec<Span<'static>> {
    let base = match line.kind {
        LogKind::Line => Style::default(),
        LogKind::Note => Style::default().fg(MUTED),
        LogKind::Error => Style::default().fg(BAD),
    };
    let chars: Vec<char> = line.text.chars().collect();
    // A cut row keeps room for the ellipsis.
    let end = range.end.min(chars.len());
    let (end, ellipsis) = if truncated && end > range.start {
        (end - 1, true)
    } else {
        (end, false)
    };

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut text = String::new();
    let window = &chars[range.start.min(chars.len())..end.min(chars.len())];
    // Notes and errors own their colour; log output takes whatever ANSI said.
    let mut current = if line.kind == LogKind::Line {
        line.style_at(range.start).unwrap_or(base)
    } else {
        base
    };
    for (offset, c) in window.iter().enumerate() {
        let style = if line.kind == LogKind::Line {
            line.style_at(range.start + offset).unwrap_or(base)
        } else {
            base
        };
        if style != current && !text.is_empty() {
            spans.push(Span::styled(std::mem::take(&mut text), current));
        }
        current = style;
        text.push(*c);
    }
    if !text.is_empty() {
        spans.push(Span::styled(text, current));
    }
    if ellipsis {
        spans.push(muted("…"));
    }
    if spans.is_empty() {
        spans.push(Span::raw(""));
    }
    spans
}

/// Split `text` into character ranges of at most `width`, breaking at a space
/// when there is one near the end of the chunk so words stay readable.
fn wrap_ranges(text: &str, width: usize) -> Vec<std::ops::Range<usize>> {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= width {
        // Short enough to need no wrapping: a single row covering it all.
        return std::iter::once(0..chars.len()).collect();
    }
    let mut out = Vec::new();
    let mut start = 0;
    while start < chars.len() {
        let hard_end = (start + width).min(chars.len());
        let mut end = hard_end;
        if hard_end < chars.len() {
            // Look back a little for a space to break on; give up quickly so a
            // long unbroken token (a JSON blob, a URL) still fills the width.
            let lookback = width / 4;
            if let Some(pos) = (start + width - lookback..hard_end)
                .rev()
                .find(|&i| chars[i] == ' ')
            {
                end = pos + 1;
            }
        }
        out.push(start..end);
        start = end;
    }
    out
}

/// Split raw log text into printable text plus the colours its ANSI escape
/// sequences asked for, as character ranges. Containers emit SGR colour codes
/// freely; dropping them would lose information the author meant to convey,
/// and passing them through would corrupt the terminal grid, so they are
/// translated into real styles. Everything else a terminal can't show (bells,
/// carriage returns, other escapes) is removed, and tabs become spaces.
fn parse_ansi(raw: &str) -> (String, Vec<(usize, usize, Style)>) {
    let mut text = String::with_capacity(raw.len());
    let mut parts: Vec<(usize, usize, Style)> = Vec::new();
    let mut chars = raw.chars().peekable();
    let mut style: Option<Style> = None;
    let mut start = 0usize;
    let mut at = 0usize;

    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => {
                // Collect the escape sequence; only CSI ... m (SGR) means
                // anything to us.
                if chars.peek() == Some(&'[') {
                    chars.next();
                    let mut body = String::new();
                    let mut final_byte = None;
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            final_byte = Some(c);
                            break;
                        }
                        body.push(c);
                    }
                    if final_byte == Some('m') {
                        // Close the run that was open, then start the new one.
                        if let Some(st) = style.take()
                            && at > start
                        {
                            parts.push((start, at, st));
                        }
                        style = sgr_style(&body);
                        start = at;
                    }
                } else {
                    chars.next();
                }
            }
            '\t' => {
                text.push_str("    ");
                at += 4;
            }
            c if (c.is_control() && c != ' ') || c == '\u{7f}' => {}
            c => {
                text.push(c);
                at += 1;
            }
        }
    }
    if let Some(st) = style
        && at > start
    {
        parts.push((start, at, st));
    }
    (text, parts)
}

/// Turn an SGR parameter list (`0;32`, `1`, `38;5;208`) into a style, or `None`
/// for a reset. Unknown parameters are ignored rather than guessed at.
fn sgr_style(body: &str) -> Option<Style> {
    let codes: Vec<u8> = body
        .split(';')
        .filter(|p| !p.is_empty())
        .filter_map(|p| p.parse().ok())
        .collect();
    if codes.is_empty() || codes == [0] {
        return None;
    }
    let mut style = Style::default();
    let mut i = 0;
    let mut touched = false;
    while i < codes.len() {
        let code = codes[i];
        match code {
            0 => style = Style::default(),
            1 => style = style.add_modifier(Modifier::BOLD),
            2 => style = style.add_modifier(Modifier::DIM),
            3 => style = style.add_modifier(Modifier::ITALIC),
            4 => style = style.add_modifier(Modifier::UNDERLINED),
            30..=37 => style = style.fg(Color::Indexed(code - 30)),
            90..=97 => style = style.fg(Color::Indexed(code - 90 + 8)),
            40..=47 => style = style.bg(Color::Indexed(code - 40)),
            100..=107 => style = style.bg(Color::Indexed(code - 100 + 8)),
            // 256-colour and true-colour forms: 38/48 ; 5 ; n or 38/48 ; 2 ; r ; g ; b
            38 | 48 => {
                let fg = code == 38;
                match codes.get(i + 1) {
                    Some(5) => {
                        if let Some(&n) = codes.get(i + 2) {
                            style = if fg {
                                style.fg(Color::Indexed(n))
                            } else {
                                style.bg(Color::Indexed(n))
                            };
                        }
                        i += 2;
                    }
                    Some(2) => {
                        if let (Some(&r), Some(&g), Some(&b)) =
                            (codes.get(i + 2), codes.get(i + 3), codes.get(i + 4))
                        {
                            style = if fg {
                                style.fg(Color::Rgb(r, g, b))
                            } else {
                                style.bg(Color::Rgb(r, g, b))
                            };
                        }
                        i += 4;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        touched = true;
        i += 1;
    }
    touched.then_some(style)
}

/// Run the viewer. `meta` is the key/value summary shown under the title (what
/// is being tailed), `poll` how often `fetch` is called, and `fetch` returns the
/// lines that have appeared since its last call.
///
/// Returns when the user quits. The terminal is always restored.
pub fn run_logs<F>(
    title: &str,
    meta: Vec<(String, String)>,
    poll: Duration,
    mut fetch: F,
) -> Result<()>
where
    F: FnMut() -> Vec<LogLine>,
{
    let mut terminal = ratatui::try_init()?;
    crate::deploy::set_tui_active(true);
    let result = logs_loop(&mut terminal, title, &meta, poll, &mut fetch);
    crate::deploy::set_tui_active(false);
    ratatui::try_restore()?;
    result
}

fn logs_loop<F>(
    terminal: &mut ratatui::DefaultTerminal,
    title: &str,
    meta: &[(String, String)],
    poll: Duration,
    fetch: &mut F,
) -> Result<()>
where
    F: FnMut() -> Vec<LogLine>,
{
    let mut view = LogView::default();
    let mut tick = 0u64;
    // Fetch once up front so the first frame already has output.
    view.extend(fetch());
    let mut fetched_at = Instant::now();
    // Page size is set from the last drawn frame, so PgUp/PgDn move by a
    // screenful of whatever the terminal currently is.
    let mut page = 10usize;

    loop {
        terminal.draw(|f| page = draw_logs(f, title, meta, &mut view, tick))?;
        tick = tick.wrapping_add(1);

        if event::poll(REDRAW_TICK)?
            && let Event::Key(k) = event::read()?
            && k.kind == KeyEventKind::Press
        {
            match k.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Ok(());
                }
                KeyCode::Char('f') => view.toggle_follow(),
                KeyCode::Char('w') => view.toggle_wrap(),
                KeyCode::Up | KeyCode::Char('k') => view.scroll_up(1),
                KeyCode::Down | KeyCode::Char('j') => view.scroll_down(1),
                KeyCode::PageUp => view.scroll_up(page),
                KeyCode::PageDown => view.scroll_down(page),
                KeyCode::Home | KeyCode::Char('g') => view.scroll_to_top(),
                KeyCode::End | KeyCode::Char('G') => view.scroll_to_bottom(),
                _ => {}
            }
        }

        if fetched_at.elapsed() >= poll {
            view.extend(fetch());
            fetched_at = Instant::now();
        }
    }
}

/// Draw the viewer and return the viewport height (the caller uses it as the
/// page size for PgUp/PgDn).
fn draw_logs(
    f: &mut Frame,
    title: &str,
    meta: &[(String, String)],
    view: &mut LogView,
    tick: u64,
) -> usize {
    f.render_widget(Clear, f.area());
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(f.area());

    // Header: what we're tailing, plus a spinner while following.
    draw_header(
        f,
        chunks[0],
        title,
        fmt_clock(&crate::deploy::utc_now()),
        view.following().then(|| spinner(tick)),
    );

    // Meta line: the key/value summary (namespace, pods, container, file …).
    let mut spans = vec![Span::raw(" ")];
    for (i, (k, v)) in meta.iter().enumerate() {
        if i > 0 {
            spans.push(muted("  ·  "));
        }
        spans.push(muted(format!("{k} ")));
        spans.push(Span::styled(v.clone(), Style::default().fg(ACCENT)));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), chunks[1]);

    // Body: the log pane.
    let block = panel("output", Some(view.len())).padding(Padding::horizontal(1));
    let area = chunks[2];
    f.render_widget(&block, area);
    let inner = block.inner(area);
    let height = inner.height as usize;
    if height == 0 || inner.width == 0 {
        return 1;
    }

    if view.is_empty() {
        f.render_widget(
            Paragraph::new(Line::from(muted("waiting for output…"))),
            inner,
        );
        return height;
    }

    let width = inner.width as usize;
    let total = view.row_count(width);
    let visible = view.window(total, height);
    let lines: Vec<Line> = view
        .visible_rows(width, visible.clone())
        .into_iter()
        .map(|(_, source, spans)| log_row(source.as_deref(), spans))
        .collect();
    f.render_widget(Paragraph::new(lines), inner);

    draw_logs_footer(f, chunks[3], view, total, visible.end);
    height
}

/// One rendered log row: dim source prefix, then the already-styled text.
fn log_row(source: Option<&str>, text: Vec<Span<'static>>) -> Line<'static> {
    let mut spans = Vec::new();
    if let Some(src) = source {
        spans.push(muted(src.to_string()));
        spans.push(Span::styled(" \u{2502} ", Style::default().fg(BORDER)));
    }
    spans.extend(text);
    Line::from(spans)
}

fn draw_logs_footer(f: &mut Frame, area: Rect, view: &LogView, rows: usize, shown_to: usize) {
    let (state, colour) = if view.following() {
        ("following", OK)
    } else {
        ("paused", WARN)
    };
    let left = vec![
        Span::raw(" "),
        key("q"),
        muted(" quit  "),
        key("f"),
        muted(" follow  "),
        key("w"),
        muted(" wrap  "),
        key("↑↓/pgup/pgdn"),
        muted(" scroll  "),
        key("g/G"),
        muted(" top/end"),
    ];
    // Right: follow state and position, so a paused viewer says where it is.
    let pos = if view.following() {
        format!("{state} · {rows} rows")
    } else {
        format!("{state} · {shown_to}/{rows}")
    };
    let line = gutter_line(left, std::slice::from_ref(&pos), area.width);
    f.render_widget(Paragraph::new(line), area);
    // Colour the state word by painting it again over the gutter text when it
    // fits; cheap and keeps `gutter_line`'s single-style contract intact.
    let w = area.width as usize;
    if pos.chars().count() + 2 <= w {
        let x = area.x + (w - pos.chars().count() - 1) as u16;
        let rect = Rect { x, y: area.y, width: state.len() as u16, height: 1 };
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                state.to_string(),
                Style::default().fg(colour).add_modifier(Modifier::BOLD),
            ))),
            rect,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deploy::{BuildRow, BuildsView, SyncRow, SyncView};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn render<F: FnOnce(&mut Frame)>(w: u16, h: u16, f: F) -> String {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|frame| f(frame)).unwrap();
        t.backend().to_string()
    }

    #[test]
    fn age_formats_compactly() {
        assert_eq!(fmt_age(5), "5s");
        assert_eq!(fmt_age(90), "1m");
        assert_eq!(fmt_age(3_900), "1h5m");
    }

    #[test]
    fn clock_keeps_only_the_time() {
        assert_eq!(fmt_clock("2026-10-17T05:03:28Z"), "05:03:28 UTC");
        // Anything unexpected is passed through rather than mangled.
        assert_eq!(fmt_clock("whenever"), "whenever");
    }

    #[test]
    fn step_gutter_is_mm_ss() {
        assert_eq!(fmt_mmss(0), "00:00");
        assert_eq!(fmt_mmss(75), "01:15");
    }

    #[test]
    fn short_url_keeps_the_path() {
        assert_eq!(
            short_url("https://jenkins.example/job/demo-app/12/"),
            "…/job/demo-app/12/"
        );
        assert_eq!(short_url("not-a-url"), "not-a-url");
    }

    #[test]
    fn progress_hint_elapsed_estimate_and_overdue() {
        assert_eq!(build_progress_hint(110, Some(190)), "1m50s/3m10s · ~1m20s left");
        assert_eq!(
            build_progress_hint(200, Some(190)),
            "3m20s · over the usual 3m10s"
        );
        assert_eq!(build_progress_hint(45, None), "45s elapsed");
    }

    #[test]
    fn bar_fills_in_proportion() {
        let filled = |r: f64| {
            bar_spans(r)
                .iter()
                .map(|s| s.content.matches('█').count())
                .sum::<usize>()
        };
        assert_eq!(filled(0.0), 0);
        assert_eq!(filled(0.5), BAR_W / 2);
        assert_eq!(filled(1.0), BAR_W);
        // Out-of-range ratios (an overdue build) clamp instead of panicking on
        // a negative/oversized repeat.
        assert_eq!(filled(2.5), BAR_W);
        assert_eq!(filled(-1.0), 0);
        // The percentage is rendered alongside the bar.
        assert!(bar_spans(0.61).iter().any(|s| s.content.contains("61%")));
    }

    #[test]
    fn panels_are_sized_to_their_contents() {
        // borders + one line for the empty message, or two lines per row.
        assert_eq!(panel_height(0), 3);
        assert_eq!(panel_height(1), 4);
        assert_eq!(panel_height(3), 8);
    }

    #[test]
    fn right_gutter_falls_back_then_drops() {
        let left = || vec![Span::raw("left")];
        let full = "https://jenkins.example/job/demo-app/12/".to_string();
        let short = "…/job/demo-app/12/".to_string();
        let choices = [full.clone(), short.clone()];

        let wide: String = gutter_line(left(), &choices, 80)
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert!(wide.contains(&full), "wide terminal should show the full URL");

        let narrow: String = gutter_line(left(), &choices, 30)
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert!(
            narrow.contains(&short) && !narrow.contains(&full),
            "narrow terminal should fall back to the short form: {narrow:?}"
        );

        let tiny: String = gutter_line(left(), &choices, 10)
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert_eq!(tiny, "left", "tiny terminal should drop the gutter entirely");
    }

    fn dashboard_snapshot() -> Snapshot {
        Snapshot {
            builds: Some(BuildsView {
                cron_installed: true,
                rows: vec![BuildRow {
                    state: BuildState::Building,
                    stage: None,
                    why: None,
                    job: "demo-app".to_string(),
                    build: Some(1423),
                    label: "https://jenkins.example/job/demo-app/1423/".to_string(),
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
                    url: "https://argocd.example/applications/vartalap-dash".to_string(),
                    detail: "Synced/Progressing, Succeeded".to_string(),
                    repo: "Blitzscale-19/vartalap".to_string(),
                    age_secs: 60,
                }]),
            }),
        }
    }

    #[test]
    fn renders_panels_and_rows_to_a_buffer() {
        let snap = dashboard_snapshot();
        let out = render(120, 24, |f| draw(f, View::Dashboard, &snap, 0, 0));

        // Header badge + view name, and both panels with their counts.
        assert!(out.contains("sts") && out.contains("dashboard"), "header missing");
        assert!(out.contains("builds"), "builds panel missing");
        assert!(out.contains("argocd"), "argocd panel missing");

        // A build row leads with its identity (job + build number), not a URL.
        assert!(out.contains("building"), "build state missing");
        assert!(out.contains("demo-app"), "job name missing");
        assert!(out.contains("#1423"), "build number missing");
        // …and its second line carries the live bar + timings.
        assert!(out.contains('█'), "progress bar missing");
        assert!(out.contains("58%"), "progress percentage missing");
        assert!(out.contains("1m50s/3m10s"), "build progress hint missing");
        assert!(out.contains("queued 45s ago"), "build age missing");
        // The URL still shows, in the right-hand gutter.
        assert!(
            out.contains("https://jenkins.example/job/demo-app/1423/"),
            "build URL missing"
        );

        // Sync rows: app + full status, neither truncated.
        assert!(out.contains("syncing"), "sync state missing");
        assert!(out.contains("vartalap-dash"), "sync app missing");
        assert!(
            out.contains("Synced/Progressing, Succeeded"),
            "sync detail was truncated"
        );
        assert!(out.contains("Blitzscale-19/vartalap"), "repo was truncated");
        // Sync rows link to the app in ArgoCD, like build rows link to Jenkins.
        assert!(
            out.contains("https://argocd.example/applications/vartalap-dash"),
            "argocd URL missing"
        );

        assert!(out.contains("cron on"), "cron status missing");
        assert!(out.contains("quit"), "footer missing");
        assert!(out.contains("1 build") && out.contains("1 syncing"), "footer counts missing");
        // Rounded borders mean the box-drawing corner should appear.
        assert!(out.contains('╭'), "panel border missing");
    }

    fn build_row(state: BuildState, stage: Option<crate::jenkins::Stage>) -> BuildRow {
        BuildRow {
            state,
            stage,
            why: None,
            job: "demo-app".to_string(),
            build: Some(7),
            label: "https://jenkins.example/job/demo-app/7/".to_string(),
            repo: "demo/app".to_string(),
            age_secs: 600,
            // Ten minutes in, against a three-minute estimate: the old view
            // showed a full bar marked "overdue" for this.
            elapsed: Some(600),
            estimated: Some(190),
        }
    }

    fn builds_only(rows: Vec<BuildRow>) -> Snapshot {
        Snapshot {
            builds: Some(BuildsView { cron_installed: true, rows }),
            syncs: None,
        }
    }

    #[test]
    fn a_build_waiting_for_an_executor_shows_no_progress_bar() {
        // Jenkins gave it a number and calls it "building", but nothing is
        // executing: the row must say so instead of timing the queue wait
        // against the build estimate.
        let snap = builds_only(vec![build_row(BuildState::Waiting, None)]);
        let out = render(100, 10, |f| draw(f, View::Pending, &snap, 0, 0));
        assert!(out.contains("waiting"), "state should read as waiting");
        assert!(
            out.contains("waiting for a free executor"),
            "the reason should be spelled out: {out}"
        );
        assert!(out.contains("10m00s in the queue"), "queue wait missing");
        assert!(!out.contains('█'), "a waiting row must not show a filled bar");
        assert!(!out.contains("overdue"), "a waiting row is not overdue");
    }

    #[test]
    fn a_build_past_its_estimate_shows_an_indeterminate_bar() {
        // Running, but with no stage information and past the estimate: how
        // much is left is unknown, so the bar must not read as 100%.
        let snap = builds_only(vec![build_row(BuildState::Building, None)]);
        let out = render(100, 10, |f| draw(f, View::Pending, &snap, 0, 0));
        assert!(out.contains("building"), "state missing");
        assert!(out.contains("over the usual 3m10s"), "wording missing: {out}");
        assert!(!out.contains("100%"), "must not claim to be complete");
        assert!(out.contains("··%"), "indeterminate marker missing");

        // …and it animates, so the row still looks alive.
        let a = render(100, 10, |f| draw(f, View::Pending, &snap, 0, 0));
        let b = render(100, 10, |f| draw(f, View::Pending, &snap, 0, 5));
        assert_ne!(a, b, "the indeterminate bar should move");
    }

    #[test]
    fn a_running_build_prefers_stage_progress() {
        let stage = crate::jenkins::Stage {
            name: "Build & Push Docker Image".to_string(),
            done: 3,
            total: 6,
        };
        let snap = builds_only(vec![build_row(BuildState::Building, Some(stage))]);
        let out = render(100, 10, |f| draw(f, View::Pending, &snap, 0, 0));
        // Real progress: half the stages done, named stage, no time guesswork.
        assert!(out.contains("stage 4/6 Build & Push Docker Image"), "stage missing: {out}");
        assert!(out.contains("50%"), "stage ratio missing");
        assert!(!out.contains("over the usual"), "stage rows need no estimate talk");
    }

    #[test]
    fn a_queued_build_shows_jenkins_own_reason() {
        let mut row = build_row(BuildState::Queued, None);
        row.build = None;
        row.elapsed = None;
        row.estimated = None;
        row.why = Some("Waiting for next available executor".to_string());
        let snap = builds_only(vec![row]);
        let out = render(100, 10, |f| draw(f, View::Pending, &snap, 0, 0));
        assert!(out.contains("queued"), "state missing");
        assert!(
            out.contains("Waiting for next available executor"),
            "Jenkins' reason should be shown: {out}"
        );
    }

    #[test]
    fn spinner_animates_between_frames() {
        let snap = dashboard_snapshot();
        let a = render(120, 24, |f| draw(f, View::Dashboard, &snap, 0, 0));
        let b = render(120, 24, |f| draw(f, View::Dashboard, &snap, 0, 3));
        assert_ne!(a, b, "the in-progress rows should animate with the tick");
    }

    #[test]
    fn panels_shrink_to_fit_their_rows() {
        // One build and one sync on a tall terminal: neither panel should be
        // stretched to half the screen, so the bottom half stays empty.
        let snap = dashboard_snapshot();
        let out = render(120, 30, |f| draw(f, View::Dashboard, &snap, 0, 0));
        let last_border = out
            .lines()
            .enumerate()
            .filter(|(_, l)| l.contains('╰'))
            .map(|(i, _)| i)
            .last()
            .expect("a panel border");
        assert!(
            last_border < 12,
            "panels should be content-sized, last border at line {last_border}"
        );
    }

    #[test]
    fn empty_views_say_so() {
        let snap = Snapshot {
            builds: Some(BuildsView { cron_installed: false, rows: vec![] }),
            syncs: Some(SyncView { rows: Some(vec![]) }),
        };
        let out = render(100, 20, |f| draw(f, View::Dashboard, &snap, 0, 0));
        assert!(out.contains("nothing waiting to deploy"), "empty builds message");
        assert!(out.contains("all tracked syncs have settled"), "empty syncs message");
        assert!(out.contains("cron off"), "cron off state");
        assert!(out.contains("idle"), "idle badge");
    }

    fn step(kind: StepKind, text: &str, at: u64) -> Step {
        Step { kind, label: None, text: text.to_string(), at }
    }

    fn row(kind: StepKind, label: &str, text: &str, at: u64) -> Step {
        Step { kind, label: Some(label.to_string()), text: text.to_string(), at }
    }

    #[test]
    fn runner_shows_header_and_steps() {
        let steps = vec![
            step(StepKind::Step, "Fetching origin/stage", 0),
            step(StepKind::Detail, "repo    demo/app", 1),
            step(StepKind::Ok, "Pushed stage at abc1234", 12),
            step(StepKind::Err, "push failed", 13),
        ];
        let out = render(100, 20, |f| draw_runner(f, "ship", Some(42), &steps, 0, None));
        assert!(out.contains("sts") && out.contains("ship"), "runner title missing");
        assert!(out.contains("Fetching origin/stage"), "step line missing");
        assert!(out.contains("Pushed stage at abc1234"), "ok line missing");
        assert!(out.contains("push failed"), "err line missing");
        assert!(out.contains("steps"), "steps panel title missing");
        assert!(out.contains("42s"), "elapsed timer missing");
        // Each step carries a mm:ss gutter so slow stages are obvious.
        assert!(out.contains("00:00") && out.contains("00:12"), "step gutter missing");
        assert!(out.contains("ctrl-c"), "abort hint missing");
    }

    #[test]
    fn a_long_listing_can_be_scrolled_back() {
        // 40 rows in a 10-row window: the newest are shown by default, and
        // scrolling back reveals the start — without it, `sts -H` would only
        // ever show its tail.
        let steps: Vec<Step> = (1..=40)
            .map(|i| row(StepKind::Ok, "synced", &format!("entry {i}"), 0))
            .collect();

        let end = render(80, 10, |f| draw_runner(f, "history", None, &steps, 0, None));
        assert!(end.contains("entry 40"), "newest row should be visible");
        assert!(!end.contains("entry 1 "), "oldest row should be off screen");
        assert!(end.contains("of 40"), "position indicator missing: {end}");

        let top = render(80, 10, |f| {
            draw_runner(f, "history", None, &steps, usize::MAX, None)
        });
        assert!(top.contains("entry 1 "), "scrolled view should reach the start");
        assert!(!top.contains("entry 40"), "newest row should now be off screen");
    }

    #[test]
    fn a_dashboard_too_short_for_its_rows_says_how_many_are_hidden() {
        let rows: Vec<BuildRow> = (1..=8)
            .map(|i| {
                let mut r = build_row(BuildState::Queued, None);
                r.job = format!("job-{i}");
                r.build = None;
                r.elapsed = None;
                r.estimated = None;
                r
            })
            .collect();
        let snap = builds_only(rows);
        // Room for a couple of two-line rows only.
        let out = render(100, 9, |f| draw(f, View::Pending, &snap, 0, 0));
        assert!(out.contains("job-1"), "first row should still show");
        assert!(out.contains("more"), "hidden rows must be reported: {out}");
    }

    #[test]
    fn labelled_rows_line_up_into_columns() {
        let steps = vec![
            row(StepKind::Ok, "jenkins", "https://jenkins.example as virat", 1),
            row(StepKind::Detail, "repo", "demo/app", 2),
            row(StepKind::Detail, "merge", "feature/x (a1b2c3d)", 2),
            // An empty label continues the previous row's list.
            row(StepKind::Detail, "", "a1b2c3d fix the thing", 2),
        ];
        let out = render(100, 12, |f| draw_runner(f, "ship", Some(3), &steps, 0, None));
        let lines: Vec<&str> = out.lines().collect();

        // Every row's text must start in the same column, whatever its label.
        // Compared in characters, not bytes: the glyph column holds multi-byte
        // glyphs, so byte offsets would differ between an ✓ row and a plain one.
        let col = |needle: &str| {
            lines
                .iter()
                .find_map(|l| l.find(needle).map(|b| l[..b].chars().count()))
                .unwrap_or_else(|| panic!("{needle} missing from:\n{out}"))
        };
        let text_col = col("https://jenkins.example");
        assert_eq!(text_col, col("demo/app"), "value column drifted");
        assert_eq!(text_col, col("feature/x"), "value column drifted");
        assert_eq!(text_col, col("a1b2c3d fix"), "unlabelled row didn't align");
        // …and the labels share a column of their own.
        let label_col = col("jenkins");
        assert_eq!(label_col, col("repo"));
        assert_eq!(label_col, col("merge"));
    }

    #[test]
    fn listings_drop_the_clock_and_gutter() {
        let steps = vec![
            row(StepKind::Ok, "synced", "demo/app   17h ago   dashboard", 0),
            row(StepKind::Err, "aborted", "demo/web   1d ago    web", 0),
        ];
        // A listing (no elapsed): no timer, no `00:00` gutter, panel named
        // after the command, and the footer says how to leave.
        let listing = render(100, 10, |f| draw_runner(f, "history", None, &steps, 0, None));
        assert!(!listing.contains("00:00"), "listing should have no gutter");
        assert!(listing.contains("history"), "panel should be named after the command");
        assert!(listing.contains("exit") && listing.contains("scroll"), "exit/scroll hints missing");
        assert!(listing.contains("synced") && listing.contains("aborted"), "rows missing");

        // A timed command keeps both.
        let timed = render(100, 10, |f| draw_runner(f, "ship", Some(7), &steps, 0, None));
        assert!(timed.contains("00:00"), "timed command should show the gutter");
        assert!(timed.contains("7s"), "elapsed timer missing");
        assert!(timed.contains("steps"), "steps panel title missing");
        assert!(timed.contains("ctrl-c"), "abort hint missing");
    }

    // ---- the log viewer -----------------------------------------------------

    /// The plain text of a rendered row, for assertions.
    fn spans_text(spans: &[Span]) -> String {
        spans.iter().map(|s| s.content.to_string()).collect()
    }

    fn row_text(v: &LogView, width: usize, row: usize) -> String {
        let rows = v.visible_rows(width, row..row + 1);
        rows.first().map(|(_, _, s)| spans_text(s)).unwrap_or_default()
    }

    fn filled(n: usize) -> LogView {
        let mut v = LogView::default();
        for i in 1..=n {
            v.push(LogLine::plain(format!("line {i}")));
        }
        v
    }

    #[test]
    fn following_pins_the_viewport_to_the_newest_output() {
        let mut v = filled(100);
        // A 10-row window on 100 rows shows the last 10 while following.
        assert_eq!(v.window(100, 10), 90..100);
        v.push(LogLine::plain("line 101"));
        assert_eq!(v.window(101, 10), 91..101, "new output should scroll in");
    }

    #[test]
    fn scrolling_up_pauses_and_scrolling_back_resumes() {
        let mut v = filled(100);
        v.window(100, 10);
        v.scroll_up(5);
        assert!(!v.following(), "scrolling up must pause following");
        assert_eq!(v.window(100, 10), 85..95);

        // New output while paused must not move the viewport.
        v.push(LogLine::plain("new"));
        assert_eq!(v.window(101, 10), 85..95, "paused view should stay put");

        v.scroll_to_bottom();
        assert!(v.following());
        assert_eq!(v.window(101, 10), 91..101);
    }

    #[test]
    fn scroll_is_clamped_at_both_ends() {
        let mut v = filled(20);
        v.scroll_up(1000);
        assert_eq!(v.window(20, 5), 0..5, "can't scroll above the first line");
        v.scroll_down(1000);
        assert_eq!(v.window(20, 5), 15..20, "can't scroll past the last line");
        v.scroll_to_top();
        assert_eq!(v.window(20, 5), 0..5);
    }

    #[test]
    fn a_window_taller_than_the_buffer_shows_everything() {
        let mut v = filled(3);
        assert_eq!(v.window(3, 10), 0..3);
    }

    #[test]
    fn scrollback_is_capped_and_keeps_the_newest() {
        let v = filled(LOG_SCROLLBACK + 50);
        assert_eq!(v.len(), LOG_SCROLLBACK, "buffer must be bounded");
        assert_eq!(
            row_text(&v, 80, 0), "line 51",
            "the oldest lines should be the ones dropped"
        );
    }

    #[test]
    fn long_lines_truncate_by_default_and_wrap_on_request() {
        let mut v = LogView::default();
        v.push(LogLine::plain("x".repeat(100)));

        assert_eq!(v.row_count(20), 1, "truncated: one row per line");
        let row = row_text(&v, 20, 0);
        assert_eq!(row.chars().count(), 20);
        assert!(row.ends_with('…'), "a cut line should say so: {row}");

        v.toggle_wrap();
        assert_eq!(v.row_count(20), 5, "wrapped: 100 chars over 20 columns");
        let rows: Vec<String> = (0..5).map(|i| row_text(&v, 20, i)).collect();
        assert!(rows.iter().all(|t| t.chars().count() <= 20), "{rows:?}");
        assert_eq!(
            rows.iter().map(|t| t.chars().count()).sum::<usize>(),
            100,
            "wrapping must not lose or duplicate characters"
        );
    }

    #[test]
    fn wrapping_breaks_on_spaces_when_it_can() {
        let text = "the quick brown fox jumps over it";
        let chars: Vec<char> = text.chars().collect();
        let ranges = wrap_ranges(text, 12);
        let chunks: Vec<String> = ranges
            .iter()
            .map(|r| chars[r.clone()].iter().collect())
            .collect();
        assert!(
            chunks.iter().all(|c| c.chars().count() <= 12),
            "chunks must fit: {chunks:?}"
        );
        assert_eq!(chunks.concat(), text, "wrapping must preserve the text");
        assert!(
            chunks[0].ends_with(' ') || chunks[1].starts_with("quick"),
            "should prefer a space break: {chunks:?}"
        );
        // A single unbroken token still fills the width rather than hanging.
        assert_eq!(wrap_ranges(&"a".repeat(30), 10).len(), 3);
    }

    #[test]
    fn a_pod_prefix_leaves_room_for_the_text() {
        let mut v = LogView::default();
        v.push(LogLine::from("api-7d9f-abcde", "hello"));
        // The prefix is accounted for, so the text column never overflows.
        let row = v.visible_rows(30, 0..1).pop().unwrap();
        assert_eq!(row.1.as_deref(), Some("api-7d9f-abcde"));
        assert_eq!(spans_text(&row.2), "hello");
    }

    #[test]
    fn control_characters_and_colour_codes_are_stripped() {
        // A bell (the notification fallback writes these into the watch log),
        // an ANSI colour sequence (containers love them), a tab, and a CR.
        let messy = "\u{7}\u{1b}[32mok\u{1b}[0m\tdone\r";
        let (text, parts) = parse_ansi(messy);
        assert_eq!(text, "ok    done", "control bytes must not reach the grid");
        // The colour the container asked for is kept, as a range over "ok".
        assert_eq!(parts.len(), 1);
        assert_eq!((parts[0].0, parts[0].1), (0, 2));
        assert_eq!(parts[0].2.fg, Some(Color::Indexed(2)));

        let mut v = LogView::default();
        v.push(LogLine::plain(messy));
        assert_eq!(row_text(&v, 80, 0), "ok    done");
    }

    #[test]
    fn ansi_colour_is_translated_not_thrown_away() {
        // A real Mongoose log line from the stage cluster: cyan label, then
        // green and yellow values.
        let raw = "\u{1b}[0;36mMongoose:\u{1b}[0m sellers.find({ \u{1b}[32m'$in'\u{1b}[39m: [ \u{1b}[33m1\u{1b}[39m ] })";
        let (text, parts) = parse_ansi(raw);
        assert_eq!(text, "Mongoose: sellers.find({ '$in': [ 1 ] })");
        assert!(!parts.is_empty(), "colours should be recorded");
        // The label keeps its cyan…
        assert_eq!(parts[0].2.fg, Some(Color::Indexed(6)));
        assert_eq!((parts[0].0, parts[0].1), (0, 9));
        // …and the quoted value its green.
        let green = parts
            .iter()
            .find(|(_, _, st)| st.fg == Some(Color::Indexed(2)))
            .expect("green run");
        assert_eq!(&text[green.0..green.1], "'$in'");
    }

    #[test]
    fn ansi_modifiers_and_256_colours_are_understood() {
        assert_eq!(sgr_style("1").unwrap().add_modifier, Modifier::BOLD);
        assert_eq!(sgr_style("38;5;208").unwrap().fg, Some(Color::Indexed(208)));
        assert_eq!(
            sgr_style("38;2;10;20;30").unwrap().fg,
            Some(Color::Rgb(10, 20, 30))
        );
        assert_eq!(sgr_style("41").unwrap().bg, Some(Color::Indexed(1)));
        assert_eq!(sgr_style("90").unwrap().fg, Some(Color::Indexed(8)));
        // A reset is "no style", not a style of its own.
        assert!(sgr_style("0").is_none());
        assert!(sgr_style("").is_none());
        // Unknown codes are ignored rather than guessed at.
        assert!(sgr_style("53").is_some());
    }

    #[test]
    fn colour_survives_wrapping_and_scrolling() {
        let esc = '\u{1b}';
        let raw = format!("{esc}[32m{}{esc}[0m{}", "a".repeat(30), "b".repeat(30));
        let mut v = LogView::default();
        v.push(LogLine::plain(raw));
        v.toggle_wrap();
        assert_eq!(v.row_count(20), 3, "60 chars over 20 columns");

        // First row is entirely inside the green run…
        let first = v.visible_rows(20, 0..1).pop().unwrap().2;
        assert!(
            first.iter().all(|s| s.style.fg == Some(Color::Indexed(2))),
            "the first wrapped row should stay green"
        );
        // …and the last is past it, so it takes the default style.
        let last = v.visible_rows(20, 2..3).pop().unwrap().2;
        assert!(
            last.iter().all(|s| s.style.fg.is_none()),
            "text after the reset must not inherit the colour"
        );
    }

    #[test]
    fn a_coloured_line_renders_through_the_viewer() {
        let mut v = LogView::default();
        v.push(LogLine::from(
            "api-1",
            "\u{1b}[31mERROR\u{1b}[0m something broke",
        ));
        let out = render(80, 8, |f| {
            draw_logs(f, "logs · x", &[], &mut v, 0);
        });
        // The text is there and the escape bytes are not.
        assert!(out.contains("ERROR something broke"), "text missing: {out}");
        assert!(!out.contains('\u{1b}'), "escape sequences must not be rendered");
    }

    #[test]
    fn the_viewer_renders_header_meta_output_and_keys() {
        let mut v = LogView::default();
        v.push(LogLine::from("api-1", "GET /healthz 200"));
        v.push(LogLine::error("logs for api-2 failed: 503"));
        let meta = vec![
            ("app".to_string(), "soochi-dash-app-test".to_string()),
            ("namespace".to_string(), "stage".to_string()),
        ];
        let out = render(100, 14, |f| {
            draw_logs(f, "logs · soochi-dash-app-test", &meta, &mut v, 0);
        });

        assert!(out.contains("sts") && out.contains("logs ·"), "header missing");
        assert!(out.contains("soochi-dash-app-test"), "app missing from meta");
        assert!(out.contains("stage"), "namespace missing from meta");
        assert!(out.contains("output"), "log panel missing");
        assert!(out.contains("GET /healthz 200"), "log line missing");
        assert!(out.contains("api-1"), "pod prefix missing");
        assert!(out.contains("503"), "error line missing");
        assert!(out.contains("following"), "follow state missing");
        assert!(out.contains("quit") && out.contains("wrap") && out.contains("scroll"), "keys missing");
    }

    #[test]
    fn a_paused_viewer_says_where_it_is() {
        let mut v = filled(100);
        v.scroll_up(20);
        let out = render(100, 14, |f| {
            draw_logs(f, "watch log", &[], &mut v, 0);
        });
        assert!(out.contains("paused"), "paused state missing: {out}");
        assert!(out.contains("/100"), "position missing");
    }

    #[test]
    fn an_empty_viewer_waits_visibly() {
        let mut v = LogView::default();
        let out = render(80, 10, |f| {
            draw_logs(f, "logs · x", &[], &mut v, 0);
        });
        assert!(out.contains("waiting for output"), "empty state missing");
    }

    #[test]
    fn confirm_modal_renders_question() {
        let steps = vec![step(StepKind::Step, "ready", 0)];
        let modal = Modal::Confirm {
            question: "Merge `feature` into stage and push?",
        };
        let out = render(100, 20, |f| draw_runner(f, "ship", Some(1), &steps, 0, Some(&modal)));
        assert!(out.contains("confirm"), "confirm title missing");
        assert!(out.contains("Merge `feature` into stage"), "question missing");
        assert!(out.contains("yes") && out.contains("no"), "y/n hint missing");
    }

    #[test]
    fn choose_modal_lists_items_with_checkboxes() {
        let steps = vec![step(StepKind::Step, "ready", 0)];
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
        let out = render(100, 20, |f| draw_runner(f, "ship", Some(1), &steps, 0, Some(&modal)));
        assert!(out.contains("Trigger which"), "choose prompt missing");
        assert!(out.contains("x-app"), "item 1 missing");
        assert!(out.contains("x-dash"), "item 2 missing");
        assert!(out.contains('◉'), "checked box missing");
        assert!(out.contains('○'), "unchecked box missing");
        assert!(out.contains("space toggle"), "multi hint missing");
    }
}
