//! The live terminal pane: one PTY-backed session rendered as text runs.
//!
//! This view owns the transport handle, the character grid, and the keyboard for
//! a single connection. It knows nothing about the host inventory; the shell
//! above it supplies a [`SessionConfig`] and reads [`PaneStatus`] back for the
//! tab strip and status bar.

use gpui_kit::component::notification::Notification;
use gpui_kit::component::ActiveTheme as _;
// `push_notification` is a `WindowExt` method; without the trait in scope the
// window has no such method (see AGENTS.md errata on missing trait imports).
use gpui_kit::component::WindowExt as _;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    div, font, px, Bounds, Context, Div, FocusHandle, FontWeight, KeyDownEvent, ParentElement as _,
    Render, Rgba, SharedString, Styled as _, Window,
};
use gpui_kit::{InteractiveElement as _, IntoElement};
use sshdeck_core::session::{Session, SessionConfig, SessionEvent};
use sshdeck_core::SessionState;
use sshdeck_terminal::{encode_key, Cell, Color, Key, Modifiers, Terminal};

/// Scrollback cap. Must stay bounded so a long build log cannot grow the parser
/// without limit (docs/BUDGET.md: "scrollback is the memory cliff").
const SCROLLBACK_LINES: usize = 10_000;

/// Terminal font size in pixels, alongside the shell's `text_sm`.
const FONT_SIZE: f32 = 13.0;

/// The largest 256-colour index with a defined value.
const MAX_IDX: u8 = 255;

/// Levels of the 6x6x6 colour cube used by xterm-256 palette entries 16-231.
const CUBE_LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];

/// The conventional xterm/ANSI palette for indices 0-15.
const BASIC_COLORS: [[u8; 3]; 16] = [
    [0, 0, 0],
    [205, 0, 0],
    [0, 205, 0],
    [205, 205, 0],
    [0, 0, 238],
    [205, 0, 205],
    [0, 205, 205],
    [229, 229, 229],
    [127, 127, 127],
    [255, 0, 0],
    [0, 255, 0],
    [255, 255, 0],
    [92, 92, 255],
    [255, 0, 255],
    [0, 255, 255],
    [255, 255, 255],
];

/// Renders a `vt100::Color` as RGB.
///
/// `Default` is reported as `None` so the caller can substitute the theme
/// foreground or background; a hardcoded black would be invisible on a dark
/// theme.
///
/// Indexed colours follow the standard xterm-256 rule: 0-15 are the basic ANSI
/// palette, 16-231 index the 6x6x6 cube above, and 232-255 are the 24-step grey
/// ramp from 8 to 238.
#[must_use]
pub fn color_to_rgb(color: Color) -> Option<Rgba> {
    let rgba = |[r, g, b]: [u8; 3]| Rgba {
        r: f32::from(r) / 255.0,
        g: f32::from(g) / 255.0,
        b: f32::from(b) / 255.0,
        a: 1.0,
    };
    match color {
        Color::Default => None,
        Color::Rgb(r, g, b) => Some(rgba([r, g, b])),
        Color::Idx(index) => match index {
            0..=15 => Some(rgba(BASIC_COLORS[usize::from(index)])),
            16..=231 => {
                let n = usize::from(index) - 16;
                Some(rgba([
                    CUBE_LEVELS[(n / 36) % 6],
                    CUBE_LEVELS[(n / 6) % 6],
                    CUBE_LEVELS[n % 6],
                ]))
            }
            // `index` is a u8, so the 0-231 arms above leave only 232-255 here.
            _ => {
                debug_assert!(index <= MAX_IDX, "vt100 colour indices are u8");
                let grey = 8 + 10 * (index - 232);
                Some(rgba([grey, grey, grey]))
            }
        },
    }
}

/// The attribute-and-colour identity of a cell, used to decide run boundaries.
#[derive(PartialEq)]
struct CellStyle {
    bold: bool,
    italic: bool,
    underline: bool,
    inverse: bool,
    fg: Option<Rgba>,
    bg: Option<Rgba>,
}

/// A maximal horizontal span of cells sharing colour and attributes.
struct Run {
    start: u16,
    len: usize,
    text: String,
    style: CellStyle,
}

/// Coalesces one row of cells into as few text runs as possible.
///
/// One element per character would mean `cols * rows` stack elements per frame:
/// a 200x50 pane would push ~20k of them through layout on every byte of output.
/// Coalescing makes that proportional to the number of colour changes instead.
///
/// A cell with no contents still joins a run, so runs keep their width in
/// columns; dropping blanks would slide the characters after them leftwards.
fn coalesce_row<'a>(cells: impl Iterator<Item = Cell<'a>>) -> Vec<Run> {
    let mut runs: Vec<Run> = Vec::new();
    for (col, cell) in cells.enumerate() {
        let style = CellStyle {
            bold: cell.bold(),
            italic: cell.italic(),
            underline: cell.underline(),
            inverse: cell.inverse(),
            fg: color_to_rgb(cell.fg()),
            bg: color_to_rgb(cell.bg()),
        };
        if let Some(run) = runs.last_mut() {
            // `col` is a running index into one row, so it cannot overflow u16
            // before the terminal's own column count does.
            let adjacent = run.start as usize + run.len == col;
            if adjacent && run.style == style {
                run.text.push_str(cell.text());
                run.len += 1;
                continue;
            }
        }
        runs.push(Run {
            start: col as u16,
            len: 1,
            text: cell.text().to_string(),
            style,
        });
    }
    runs
}

/// The visible grid of a session, rendered as text runs.
pub struct TerminalPane {
    /// `None` when the transport rejected the configuration outright; the pane
    /// then renders the failure instead of a live grid.
    session: Option<Session>,
    terminal: Terminal,
    focus_handle: FocusHandle,
    /// Cell metrics in pixels, measured from the real font rather than guessed.
    cell: (f32, f32),
    /// Grid size last sent to the transport, so a resize only crosses threads
    /// when the size actually changed.
    sent: (u16, u16),
    status: SessionState,
    title: Option<String>,
    /// Why the session ended, when it did not end cleanly.
    ended: Option<String>,
}

impl TerminalPane {
    /// Connects and returns the pane. A configuration the transport rejects
    /// synchronously (a missing secret, an unsupported auth method) is kept as a
    /// failed state rather than a returned error, so the pane can show why.
    ///
    /// Construct it with `cx.new(|cx| TerminalPane::new(config, window, cx))`
    /// from the owning view, so construction runs in this pane's own context and
    /// the event-watch task is spawned against this pane's handle.
    pub fn new(config: SessionConfig, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let cell = measure_cell(window);
        let focus_handle = cx.focus_handle();

        // A rejected config is a terminal state: no connection thread exists, so
        // no `SessionEvent` will ever arrive to explain it. Recording the failure
        // at construction is what keeps that from being a silent no-op.
        let connected = Session::connect(config);
        let (status, ended) = match &connected {
            Ok(_) => (SessionState::Connecting, None),
            Err(error) => (
                SessionState::Failed {
                    message: error.to_string(),
                },
                Some(error.to_string()),
            ),
        };
        // The event receiver is taken once, here: `Session::events` is
        // competing-consumer, so a second receiver would split the stream.
        let events = connected.as_ref().ok().map(Session::events);

        // A transport that was rejected up front has already been folded into
        // `status`/`ended` above, and the shell reports that on the tab, so the
        // pane keeps no handle. `.ok()` after the failure has been recorded is
        // not a silent discard: the error is preserved in the pane's state.
        let mut pane = Self {
            session: connected.ok(),
            terminal: Terminal::new(80, 24, SCROLLBACK_LINES),
            focus_handle,
            cell,
            sent: (80, 24),
            status,
            title: None,
            ended,
        };

        if let Some(events) = events {
            // One task per pane awaits the session's events and notifies the view
            // on arrival. This is the invalidation-driven path docs/BUDGET.md
            // asks for: when the remote is quiet nothing arrives, no frame is
            // scheduled, and the process is genuinely idle. A polling loop would
            // draw 60 times a second to show text that has not changed, which is
            // the exact failure mode this port exists to avoid.
            //
            // The shared borrow is taken up front so the closure captures `&Window`
            // rather than the caller's `&mut Window`.
            let window: &Window = window;
            pane.watch(events, window, cx);
        }
        pane
    }

    /// Forwards session events to the view for as long as the session lives.
    ///
    /// Runs on GPUI's foreground executor and yields at every `await`, so it
    /// never blocks the UI thread; the connection's own thread does the I/O.
    fn watch(
        &mut self,
        events: async_channel::Receiver<SessionEvent>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        cx.spawn_in(window, async move |pane, cx| {
            // `Err` means the connection thread dropped the sender, which is
            // itself the end of the session.
            while let Ok(event) = events.recv().await {
                let stop = pane
                    .update_in(cx, |pane, window, cx| pane.apply(event, window, cx))
                    .unwrap_or(true);
                if stop {
                    break;
                }
            }
            // The channel closed without a `Closed` event: say so rather than
            // leave the pane looking connected forever.
            pane.update_in(cx, |pane, _, cx| {
                if pane.ended.is_none() {
                    pane.ended = Some("the connection ended unexpectedly".to_string());
                    pane.status = SessionState::Closed { code: None };
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Folds one session event into the pane's state.
    ///
    /// Returns true when the session has reached a terminal state and the watch
    /// task should stop.
    fn apply(&mut self, event: SessionEvent, window: &mut Window, cx: &mut Context<Self>) -> bool {
        // The channel is bounded and consumed one event at a time, and the only
        // state that grows is the terminal's own capped scrollback.
        let stop = match event {
            SessionEvent::Data(bytes) => {
                self.terminal.feed(&bytes);
                false
            }
            SessionEvent::Connected => {
                self.status = SessionState::Connected;
                // The PTY was opened at the transport's placeholder size; this
                // is the first real one it learns.
                self.sync_size();
                false
            }
            SessionEvent::State(state) => {
                let stop = state.is_terminal();
                self.status = state;
                stop
            }
            SessionEvent::Closed(code) => {
                self.status = SessionState::Closed { code };
                self.ended = Some(match code {
                    Some(code) => format!("remote shell exited with status {code}"),
                    None => "remote shell closed the session".to_string(),
                });
                true
            }
            SessionEvent::Error(message) => {
                self.ended = Some(message.clone());
                self.status = SessionState::Failed {
                    message: message.clone(),
                };
                // A failed session must never be silent.
                window.push_notification(Notification::error(message), cx);
                true
            }
        };
        self.title = self.terminal.title().or_else(|| self.title.clone());
        cx.notify();
        stop
    }

    /// Reports the current grid size to the remote PTY if it changed.
    fn sync_size(&mut self) {
        let size = (self.terminal.cols(), self.terminal.rows());
        if size == self.sent {
            return;
        }
        // A closed transport cannot accept a resize and there is nothing left to
        // report, so a dead pane stays quiet.
        if let Some(session) = &self.session {
            let _ = session.resize(size.0, size.1);
        }
        self.sent = size;
    }

    pub fn status(&self) -> PaneStatus {
        PaneStatus {
            state: self.status.clone(),
            title: self.title.clone(),
        }
    }

    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_handle.focus(window, cx);
    }

    /// Applies a new grid size to both the local parser and the remote PTY.
    ///
    /// Returns whether the grid actually changed shape; a `false` return means
    /// the caller should not schedule a re-render for it.
    pub fn resize(&mut self, cols: u16, rows: u16) -> bool {
        let cols = cols.max(1);
        let rows = rows.max(1);
        if self.terminal.cols() == cols && self.terminal.rows() == rows {
            return false;
        }
        self.terminal.resize(cols, rows);
        if let Some(session) = &self.session {
            let _ = session.resize(cols, rows);
        }
        self.sent = (cols, rows);
        true
    }

    /// Writes one keystroke to the remote shell.
    fn forward_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        // Cmd-shortcuts and fn-chords belong to the application, not the shell.
        if keystroke.modifiers.platform || keystroke.modifiers.function {
            return;
        }
        let Some(key) = map_key(&keystroke.key) else {
            return;
        };
        let Some(session) = &self.session else {
            return;
        };
        let mods = Modifiers::new(
            keystroke.modifiers.shift,
            keystroke.modifiers.alt,
            keystroke.modifiers.control,
        );
        let bytes = encode_key(key, mods);
        if bytes.is_empty() {
            return;
        }
        // Backpressure or a closed transport must not wedge the UI; the next
        // keystroke simply tries again.
        if session.write(&bytes).is_ok() {
            cx.notify();
        }
    }

    /// Builds one row: a backdrop for any run with a background colour, the
    /// coalesced text runs positioned at their own columns, and the cursor.
    fn render_row(
        &self,
        row: u16,
        runs: &[Run],
        cursor_col: Option<u16>,
        cx: &mut Context<Self>,
    ) -> Div {
        let (cell_w, cell_h) = self.cell;
        let theme_fg = cx.theme().foreground;
        let theme_bg = cx.theme().background;

        let mut line = div()
            .absolute()
            .left(px(0.0))
            .top(px(f32::from(row) * cell_h))
            .w_full()
            .h(px(cell_h));

        for run in runs {
            let left = f32::from(run.start) * cell_w;
            let width = run.len as f32 * cell_w;
            // Inverse video swaps the two colours, which is how a terminal draws
            // a selection or a highlighted menu row. A `Default` slot resolves to
            // the theme colour it is standing in for, so the swap stays legible
            // on either theme.
            // Theme slots are `Hsla`; a cell colour is `Rgba`. Convert the theme
            // default so both arms of each pair are the same type.
            let (fg, bg) = if run.style.inverse {
                (
                    run.style.bg.unwrap_or(theme_bg.into()),
                    run.style.fg.unwrap_or(theme_fg.into()),
                )
            } else {
                (
                    run.style.fg.unwrap_or(theme_fg.into()),
                    run.style.bg.unwrap_or(theme_bg.into()),
                )
            };
            // A background is only painted when the cell actually asked for one
            // (or is inverted); otherwise every default cell would need its own
            // quad and the pane would stop being cheap to draw.
            if run.style.bg.is_some() || run.style.inverse {
                line = line.child(
                    div()
                        .absolute()
                        .left(px(left))
                        .top(px(0.0))
                        .w(px(width))
                        .h(px(cell_h))
                        .bg(bg),
                );
            }
            if run.text.is_empty() {
                continue;
            }
            line = line.child(
                div()
                    .absolute()
                    .left(px(left))
                    .top(px(0.0))
                    .w(px(width))
                    .h(px(cell_h))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .font_family("Menlo")
                    .text_size(px(FONT_SIZE))
                    .line_height(px(cell_h))
                    .text_color(fg)
                    .when(run.style.bold, |el| el.font_weight(FontWeight::BOLD))
                    .when(run.style.italic, |el| el.italic())
                    .when(run.style.underline, |el| el.underline())
                    .child(SharedString::from(run.text.clone())),
            );
        }

        if let Some(col) = cursor_col {
            line = line.child(
                div()
                    .absolute()
                    .left(px(f32::from(col) * cell_w))
                    .top(px(0.0))
                    .w(px(cell_w))
                    .h(px(cell_h))
                    .bg(theme_fg.opacity(0.4)),
            );
        }

        line
    }
}

/// What the pane tells the shell about its session. The shell owns the tab and
/// status chrome, so the pane reports rather than renders it.
#[derive(Clone, Debug)]
pub struct PaneStatus {
    pub state: SessionState,
    pub title: Option<String>,
}

/// Maps a GPUI key code to a [`Key`].
///
/// Only key codes the terminal crate understands are returned. IME output
/// arrives as `key_char` rather than a code, so nothing here sends a composed
/// character twice.
fn map_key(key: &str) -> Option<Key> {
    Some(match key {
        "enter" => Key::Enter,
        "tab" => Key::Tab,
        "backspace" => Key::Backspace,
        "delete" => Key::Delete,
        "escape" => Key::Escape,
        "up" => Key::Up,
        "down" => Key::Down,
        "left" => Key::Left,
        "right" => Key::Right,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" => Key::PageUp,
        "pagedown" => Key::PageDown,
        "insert" => Key::Insert,
        "f1" => Key::F1,
        "f2" => Key::F2,
        "f3" => Key::F3,
        "f4" => Key::F4,
        "f5" => Key::F5,
        "f6" => Key::F6,
        "f7" => Key::F7,
        "f8" => Key::F8,
        "f9" => Key::F9,
        "f10" => Key::F10,
        "f11" => Key::F11,
        "f12" => Key::F12,
        // A space has no single-character key code, so GPUI names it.
        "space" => Key::Char(' '),
        // Anything longer than one character is a named key we do not encode.
        other => {
            let mut chars = other.chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            Key::Char(c)
        }
    })
}

/// Measures the advance width and line height of Menlo at [`FONT_SIZE`].
///
/// Both come from the real text system, so the grid stays aligned on a HiDPI
/// display or when the font is missing; a wrong advance is exactly the
/// "everything is shifted" failure that makes a terminal unreadable.
fn measure_cell(window: &mut Window) -> (f32, f32) {
    let text_system = window.text_system().clone();
    let font_id = text_system.resolve_font(&font("Menlo"));
    let bounds = text_system.bounding_box(font_id, px(FONT_SIZE));
    let height = (f32::from(bounds.size.height) / 0.7).max(FONT_SIZE).round();
    let advance = f32::from(text_system.layout_width(font_id, px(FONT_SIZE), 'M'));
    let advance = if advance > 0.0 {
        advance.round()
    } else {
        // Unreachable for a monospace face; a zero advance would collapse the
        // grid, so derive one from the measured height instead.
        (height * 0.6).round()
    };
    (advance, height)
}

impl Render for TerminalPane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme_fg = cx.theme().foreground;
        let theme_bg = cx.theme().background;
        let muted = cx.theme().muted_foreground;

        // Re-measure each frame: a couple of cached font lookups, and it is what
        // keeps the grid aligned when the scale factor or theme font changes.
        self.cell = measure_cell(window);
        let (cell_w, cell_h) = self.cell;

        let (cursor_col, cursor_row, cursor_visible) = self.terminal.cursor();
        let rows = self.terminal.rows();

        let mut grid = div().relative().w_full().h(px(f32::from(rows) * cell_h));
        for row in 0..rows {
            let runs = row_runs(&self.terminal, row);
            let cursor = (cursor_visible && cursor_row == row).then_some(cursor_col);
            grid = grid.child(self.render_row(row, &runs, cursor, cx));
        }

        let ended = self.ended.clone();
        let connecting = matches!(self.status, SessionState::Connecting);

        div()
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .h_full()
            .overflow_hidden()
            .bg(theme_bg)
            .text_color(theme_fg)
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _window, cx| {
                this.forward_key(event, cx);
            }))
            // A canvas child reports the space the pane was actually given, so
            // the grid reflows with the window instead of keeping its last size.
            // It draws nothing; the rows below do the painting, and the resize is
            // a no-op unless the column or row count really changed.
            .child(div().absolute().size_full().child(gpui_kit::canvas(
                move |bounds: Bounds<gpui_kit::Pixels>, _, _| bounds.size,
                {
                    // A canvas paints with `&mut App`, so the pane is reached
                    // through a weak handle rather than `Context::listener`.
                    let view = cx.entity().downgrade();
                    move |_, size: gpui_kit::Size<gpui_kit::Pixels>, _, cx| {
                        view.update(cx, |pane, cx| {
                            let cols = (f32::from(size.width) / cell_w).floor().max(1.0) as u16;
                            let rows = (f32::from(size.height) / cell_h).floor().max(1.0) as u16;
                            // Re-render only when the grid actually changed shape;
                            // otherwise every frame would schedule the next one.
                            if pane.resize(cols, rows) {
                                cx.notify();
                            }
                        })
                        .ok();
                    }
                },
            )))
            .child(grid)
            .when_some(ended, |el, message| {
                el.child(
                    div()
                        .absolute()
                        .left(px(12.0))
                        .bottom(px(12.0))
                        .text_xs()
                        .text_color(muted)
                        .child(SharedString::from(message)),
                )
            })
            .when(connecting, |el| {
                el.child(
                    div()
                        .absolute()
                        .left(px(12.0))
                        .bottom(px(12.0))
                        .text_xs()
                        .text_color(muted)
                        .child("connecting…"),
                )
            })
    }
}

/// The text runs for one visible row.
fn row_runs(terminal: &Terminal, row: u16) -> Vec<Run> {
    let cols = terminal.cols();
    coalesce_row((0..cols).filter_map(move |col| terminal.cell(row, col)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The channel components of a rendered colour, rounded back to bytes.
    fn bytes(color: Rgba) -> [u8; 3] {
        [
            (color.r * 255.0).round() as u8,
            (color.g * 255.0).round() as u8,
            (color.b * 255.0).round() as u8,
        ]
    }

    /// Every conversion below is expected to produce a colour.
    fn bytes_of(color: Color) -> [u8; 3] {
        let rgb = color_to_rgb(color).expect("colour has a value");
        bytes(rgb)
    }

    #[test]
    fn default_color_defers_to_the_theme() {
        assert_eq!(color_to_rgb(Color::Default), None);
    }

    #[test]
    fn rgb_passes_through_unchanged() {
        assert_eq!(bytes_of(Color::Rgb(18, 52, 86)), [18, 52, 86]);
    }

    #[test]
    fn basic_ansi_indices_use_the_standard_palette() {
        assert_eq!(bytes_of(Color::Idx(1)), [205, 0, 0]);
        assert_eq!(bytes_of(Color::Idx(15)), [255, 255, 255]);
    }

    #[test]
    fn cube_indices_walk_the_6x6x6_levels() {
        // The cube's own corners.
        assert_eq!(bytes_of(Color::Idx(16)), [0, 0, 0]);
        assert_eq!(bytes_of(Color::Idx(231)), [255, 255, 255]);
        // 196 = 16 + 180, the pure-red corner of the cube.
        assert_eq!(bytes_of(Color::Idx(196)), [255, 0, 0]);
    }

    #[test]
    fn grey_ramp_spans_8_to_238() {
        assert_eq!(bytes_of(Color::Idx(232)), [8, 8, 8]);
        assert_eq!(bytes_of(Color::Idx(255)), [238, 238, 238]);
    }

    #[test]
    fn every_index_resolves_to_a_color() {
        for index in 0..=MAX_IDX {
            assert!(
                color_to_rgb(Color::Idx(index)).is_some(),
                "index {index} has no colour"
            );
        }
    }

    #[test]
    fn named_keys_map_and_unknown_keys_do_not() {
        assert_eq!(map_key("enter"), Some(Key::Enter));
        assert_eq!(map_key("pageup"), Some(Key::PageUp));
        assert_eq!(map_key("f5"), Some(Key::F5));
        assert_eq!(map_key("space"), Some(Key::Char(' ')));
        assert_eq!(map_key("a"), Some(Key::Char('a')));
        // A named key we do not encode must not be mistaken for a character.
        assert_eq!(map_key("capslock"), None);
        assert_eq!(map_key(""), None);
    }

    /// Ctrl+C must produce the C0 interrupt byte, not the letter.
    #[test]
    fn ctrl_letter_encodes_to_a_control_byte() {
        let key = map_key("c").expect("letter maps");
        let bytes = encode_key(key, Modifiers::new(false, false, true));
        assert_eq!(bytes, vec![0x03]);
    }

    /// The whole point of coalescing: a same-styled row collapses to one run per
    /// style change, and the runs' column spans still cover the full width.
    #[test]
    fn runs_coalesce_and_keep_their_column_offsets() {
        // Exactly wide enough for the text, so there are no trailing blanks.
        let mut terminal = Terminal::new(9, 1, 10);
        terminal.feed(b"hello \x1b[31mred");

        let runs = row_runs(&terminal, 0);
        assert_eq!(runs.len(), 2, "expected one run per style change");
        assert_eq!(runs[0].start, 0);
        assert_eq!(runs[0].len, 6);
        assert_eq!(runs[0].text, "hello ");
        assert_eq!(runs[1].start, 6);
        assert_eq!(runs[1].len, 3);
        assert_eq!(runs[1].text, "red");

        // Every column is covered exactly once, so nothing shifts sideways.
        let covered: usize = runs.iter().map(|run| run.len).sum();
        assert_eq!(covered, usize::from(terminal.cols()));
    }

    /// A row wider than its text still ends in one run: blanks must coalesce
    /// rather than become an element each.
    #[test]
    fn trailing_blanks_coalesce_into_a_single_run() {
        let mut terminal = Terminal::new(80, 1, 10);
        terminal.feed(b"hi");

        let runs = row_runs(&terminal, 0);
        assert_eq!(runs.len(), 1, "an all-default row is one run");
        assert_eq!(runs[0].len, 80);
        assert_eq!(runs[0].text, "hi");
    }
}
