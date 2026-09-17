//! The live terminal pane: one PTY-backed session rendered as text runs.
//!
//! This view owns the transport handle, the character grid, and the keyboard for
//! a single connection. It knows nothing about the host inventory; the shell
//! above it supplies a [`SessionConfig`] and reads [`PaneStatus`] back for the
//! tab strip and status bar.
//!
//! # Interaction
//!
//! - The mouse wheel scrolls the viewport through the grid's capped scrollback;
//!   while scrolled back, a chip reports the offset and pressing `End` (or any
//!   key sent to the shell) returns to the live screen.
//! - Dragging with the left button selects cells; `Cmd-C` copies the selection
//!   and `Cmd-V` pastes the clipboard into the session.
//! - `Cmd-F` opens an in-pane find box over the visible grid: type a query,
//!   `Enter` / `Shift-Enter` walk the matches, `Escape` closes it.
//!
//! # Construction
//!
//! [`TerminalPane::new`] keeps the compiled defaults. To apply the settings
//! surface's values, use [`TerminalPane::new_with_options`]:
//!
//! ```ignore
//! let options = TerminalOptions::new(
//!     settings_view.read(cx).font_size(),
//!     settings_view.read(cx).scrollback_lines(),
//!     settings_view.read(cx).cursor_blink(),
//! );
//! let pane = cx.new(|cx| TerminalPane::new_with_options(config, options, window, cx));
//! ```
//!
//! The scrollback cap is fixed when the grid is created, so changing it later
//! requires rebuilding the pane; `TerminalOptions` never raises it above
//! [`SCROLLBACK_LINES`].

use std::sync::Arc;
use std::time::Duration;

use gpui_kit::component::notification::Notification;
use gpui_kit::component::ActiveTheme as _;
// `push_notification` is a `WindowExt` method; without the trait in scope the
// window has no such method (see AGENTS.md errata on missing trait imports).
use gpui_kit::component::WindowExt as _;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    div, font, point, px, Bounds, ClipboardItem, Context, Div, FocusHandle, FontWeight,
    KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement as _,
    Pixels, Point, Render, Rgba, ScrollDelta, ScrollWheelEvent, SharedString, Styled as _, Task,
    Window,
};
use gpui_kit::{InteractiveElement as _, IntoElement};
use sshdeck_core::session::{Session, SessionConfig, SessionEvent};
use sshdeck_core::SessionState;
use sshdeck_terminal::{encode_key, Cell, Color, Key, Modifiers, Terminal};

/// Scrollback cap. Must stay bounded so a long build log cannot grow the parser
/// without limit (docs/BUDGET.md: "scrollback is the memory cliff").
const SCROLLBACK_LINES: usize = 10_000;

/// Default terminal font size in pixels, alongside the shell's `text_sm`.
const FONT_SIZE: f32 = 13.0;

/// Font-size bounds. The floor keeps glyphs legible; the ceiling stops a single
/// line from swallowing the pane. `TerminalOptions` clamps into this range so a
/// bad value can never collapse or explode the grid.
const MIN_FONT_SIZE: f32 = 6.0;
const MAX_FONT_SIZE: f32 = 72.0;

/// Cursor blink half-period. Cursor blink is the one timer docs/BUDGET.md
/// accepts in the terminal; it is parked entirely while the window is
/// unfocused, so a static app still has zero idle wake-ups.
const CURSOR_BLINK_INTERVAL: Duration = Duration::from_millis(500);

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

/// Tunables a [`TerminalPane`] takes at construction.
///
/// Fields are private; read them with the accessors. Values are clamped on the
/// way in, so a reader always reports what the pane actually uses.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TerminalOptions {
    font_size: f32,
    scrollback_lines: usize,
    cursor_blink: bool,
}

impl TerminalOptions {
    /// Clamps `font_size` into `MIN_FONT_SIZE..=MAX_FONT_SIZE` (a non-finite
    /// size falls back to [`FONT_SIZE`]) and `scrollback_lines` into
    /// `1..=SCROLLBACK_LINES`. There is deliberately no unbounded value.
    #[must_use]
    pub fn new(font_size: f32, scrollback_lines: usize, cursor_blink: bool) -> Self {
        let font_size = if font_size.is_finite() {
            font_size.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE)
        } else {
            FONT_SIZE
        };
        Self {
            font_size,
            scrollback_lines: scrollback_lines.clamp(1, SCROLLBACK_LINES),
            cursor_blink,
        }
    }

    /// Font size in pixels.
    #[must_use]
    pub const fn font_size(self) -> f32 {
        self.font_size
    }

    /// Scrollback cap, in lines, fixed at construction.
    #[must_use]
    pub const fn scrollback_lines(self) -> usize {
        self.scrollback_lines
    }

    /// Whether the cursor should blink while the window is focused.
    #[must_use]
    pub const fn cursor_blink(self) -> bool {
        self.cursor_blink
    }
}

impl Default for TerminalOptions {
    fn default() -> Self {
        Self {
            font_size: FONT_SIZE,
            scrollback_lines: SCROLLBACK_LINES,
            cursor_blink: true,
        }
    }
}

/// A cell position inside the visible grid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct GridPos {
    row: u16,
    col: u16,
}

/// A two-endpoint selection in grid coordinates.
#[derive(Clone, Copy, Debug)]
struct Selection {
    /// Where the drag started.
    anchor: GridPos,
    /// Where the pointer is now.
    head: GridPos,
}

impl Selection {
    /// The endpoints in reading order, so a backwards drag selects the same
    /// cells as a forwards one.
    fn normalized(self) -> (GridPos, GridPos) {
        if (self.anchor.row, self.anchor.col) <= (self.head.row, self.head.col) {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }
}

/// One occurrence of the find query in the visible grid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MatchSpan {
    row: u16,
    col: u16,
    len: u16,
}

/// The in-pane find state. Matches themselves are recomputed from the grid on
/// every render, so they can never go stale against new output.
#[derive(Default)]
struct Search {
    query: String,
    /// Index into the current render's matches, kept modulo that count.
    current: usize,
}

/// The visible grid of a session, rendered as text runs.
pub struct TerminalPane {
    /// `None` when the transport rejected the configuration outright; the pane
    /// then renders the failure instead of a live grid. Shared so the shell can
    /// open an SFTP subsystem on the same authenticated connection.
    session: Option<Arc<Session>>,
    terminal: Terminal,
    options: TerminalOptions,
    focus_handle: FocusHandle,
    /// Cell metrics in pixels, measured from the real font rather than guessed.
    cell: (f32, f32),
    /// Top-left of the grid in window coordinates, captured at paint so a mouse
    /// position can be turned back into a cell.
    grid_origin: Point<Pixels>,
    /// Grid size last sent to the transport, so a resize only crosses threads
    /// when the size actually changed.
    sent: (u16, u16),
    status: SessionState,
    title: Option<String>,
    /// Why the session ended, when it did not end cleanly.
    ended: Option<String>,
    /// True while a left-button drag is extending the selection.
    dragging: bool,
    selection: Option<Selection>,
    search: Option<Search>,
    /// Wheel lines not yet worth a whole row, so a trackpad's fine deltas still
    /// scroll instead of rounding to nothing.
    pending_scroll: f32,
    /// Blink phase: true while the cursor is drawn.
    blink_on: bool,
    /// Set by the blink task when it parks on an unfocused window, so the next
    /// active frame knows it may start a fresh task.
    blink_parked: bool,
    /// The blink timer, held so it is dropped (and cancelled) with the pane.
    blink_task: Option<Task<()>>,
}

impl TerminalPane {
    /// Connects and returns the pane with the compiled defaults. A configuration
    /// the transport rejects synchronously (a missing secret, an unsupported
    /// auth method) is kept as a failed state rather than a returned error, so
    /// the pane can show why.
    ///
    /// Construct it with `cx.new(|cx| TerminalPane::new(config, window, cx))`
    /// from the owning view, so construction runs in this pane's own context and
    /// the event-watch task is spawned against this pane's handle.
    pub fn new(config: SessionConfig, window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::new_with_options(config, TerminalOptions::default(), window, cx)
    }

    /// Connects and returns the pane with the given tunables.
    ///
    /// The shell applies the settings surface here:
    ///
    /// ```ignore
    /// let pane = cx.new(|cx| TerminalPane::new_with_options(config, options, window, cx));
    /// ```
    ///
    /// `options.scrollback_lines()` is fixed when the grid is created; to change
    /// it, rebuild the pane. `font_size()` and `cursor_blink()` are read live.
    pub fn new_with_options(
        config: SessionConfig,
        options: TerminalOptions,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let cell = measure_cell(window, options.font_size());
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
            session: connected.ok().map(Arc::new),
            terminal: Terminal::new(80, 24, options.scrollback_lines()),
            options,
            focus_handle,
            cell,
            grid_origin: point(px(0.0), px(0.0)),
            sent: (80, 24),
            status,
            title: None,
            ended,
            dragging: false,
            selection: None,
            search: None,
            pending_scroll: 0.0,
            blink_on: true,
            blink_parked: false,
            blink_task: None,
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

    /// A shared handle to the live transport, if the configuration was accepted.
    ///
    /// The shell clones this to open an SFTP subsystem on the same session; the
    /// `Arc` is what lets the SFTP connect task own the handle across an await.
    pub fn session(&self) -> Option<Arc<Session>> {
        self.session.clone()
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

    /// Writes one keystroke to the remote shell, after the pane's own
    /// shortcuts and the find box have had their turn.
    fn forward_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;

        // The find box owns the keyboard until Escape or a second Cmd-F.
        if self.search.is_some() {
            self.search_key(event, cx);
            return;
        }

        // Cmd-shortcuts belong to the application, not the shell.
        if keystroke.modifiers.platform {
            match keystroke.key.as_str() {
                "c" => self.copy_selection(cx),
                "v" => self.paste(cx),
                "f" => {
                    self.search = Some(Search::default());
                    cx.notify();
                }
                _ => {}
            }
            return;
        }
        // fn-chords belong to the application too.
        if keystroke.modifiers.function {
            return;
        }
        let Some(key) = map_key(&keystroke.key) else {
            return;
        };
        // `End` at the bottom goes to the shell; while scrolled back it means
        // "return to the live screen" instead.
        if keystroke.key == "end" && self.terminal.scrollback_offset() > 0 {
            self.scroll_to_bottom(cx);
            return;
        }
        // Any other live keypress means the user is interacting with the shell,
        // so snap back to the bottom before sending it.
        if self.terminal.scrollback_offset() > 0 {
            self.scroll_to_bottom(cx);
        }
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

    /// Copies the current selection to the platform clipboard.
    fn copy_selection(&mut self, cx: &mut Context<Self>) {
        let Some(selection) = self.selection else {
            return;
        };
        let text = selection_text(&self.terminal, selection);
        if text.is_empty() {
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    /// Writes the clipboard's text into the session.
    ///
    // ponytail: `sshdeck_terminal::Terminal` does not expose the grid's
    // bracketed-paste state, so paste is plain. Upgrade path: re-export
    // `vt100::Screen::bracketed_paste` from the terminal crate and frame the
    // text with `ESC[200~ .. ESC[201~` when it is set.
    fn paste(&mut self, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        if text.is_empty() {
            return;
        }
        if let Some(session) = &self.session {
            let _ = session.write(text.as_bytes());
        }
    }

    /// Handles a keystroke while the find box is open.
    fn search_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;

        if keystroke.modifiers.platform {
            if keystroke.key == "f" {
                self.search = None;
                cx.notify();
            }
            return;
        }
        if keystroke.key == "escape" {
            self.search = None;
            cx.notify();
            return;
        }
        if keystroke.key == "enter" {
            self.advance_match(!keystroke.modifiers.shift, cx);
            return;
        }

        let Some(search) = self.search.as_mut() else {
            return;
        };
        if keystroke.key == "backspace" {
            search.query.pop();
            search.current = 0;
            cx.notify();
            return;
        }
        if keystroke.modifiers.control {
            return;
        }
        // A space has no single-character key code, so GPUI names it; other
        // printable keys arrive as `key_char` (which respects shift/alt).
        let text = if keystroke.key == "space" {
            Some(" ".to_string())
        } else {
            keystroke.key_char.clone()
        };
        if let Some(text) = text {
            if !text.is_empty() && !text.chars().any(char::is_control) {
                search.query.push_str(&text);
                search.current = 0;
                cx.notify();
            }
        }
    }

    /// Moves the focused match, wrapping at either end.
    fn advance_match(&mut self, forward: bool, cx: &mut Context<Self>) {
        let count = match &self.search {
            Some(search) => find_matches(&self.terminal, &search.query).len(),
            None => return,
        };
        if count == 0 {
            return;
        }
        if let Some(search) = self.search.as_mut() {
            search.current = if forward {
                (search.current + 1) % count
            } else {
                (search.current + count - 1) % count
            };
        }
        cx.notify();
    }

    /// Starts a drag-select at the pointer.
    fn begin_selection(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let position = self.grid_pos(event.position);
        self.dragging = true;
        self.selection = Some(Selection {
            anchor: position,
            head: position,
        });
        // A click in the terminal should also give it the keyboard.
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    /// Extends the selection while the left button is held.
    fn extend_selection(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        // `dragging()` false means the button was released, possibly outside the
        // pane, so the drag ends here even if we missed the mouse-up.
        if !self.dragging || !event.dragging() {
            self.dragging = false;
            return;
        }
        let position = self.grid_pos(event.position);
        if let Some(selection) = self.selection.as_mut() {
            selection.head = position;
            cx.notify();
        }
    }

    /// Ends a drag-select; a click without movement clears the selection.
    fn end_selection(&mut self, cx: &mut Context<Self>) {
        self.dragging = false;
        if self
            .selection
            .is_some_and(|selection| selection.anchor == selection.head)
        {
            self.selection = None;
        }
        cx.notify();
    }

    /// The grid cell under a window-space point.
    fn grid_pos(&self, position: Point<Pixels>) -> GridPos {
        let relative = position - self.grid_origin;
        cell_at(
            f32::from(relative.x),
            f32::from(relative.y),
            self.cell,
            self.terminal.cols(),
            self.terminal.rows(),
        )
    }

    /// Scrolls the viewport by one wheel event.
    fn scroll_wheel(&mut self, event: &ScrollWheelEvent, cx: &mut Context<Self>) {
        let lines = match event.delta {
            ScrollDelta::Lines(delta) => delta.y,
            ScrollDelta::Pixels(delta) => f32::from(delta.y) / self.cell.1.max(1.0),
        };
        let (offset, pending) = fold_scroll(
            self.terminal.scrollback_offset(),
            self.pending_scroll,
            lines,
        );
        self.pending_scroll = pending;
        self.set_scroll(offset, cx);
    }

    /// Moves the scrollback viewport, dropping a selection that would otherwise
    /// highlight whatever scrolled into its place.
    fn set_scroll(&mut self, offset: usize, cx: &mut Context<Self>) {
        if self.terminal.scrollback_offset() == offset {
            return;
        }
        // The engine clamps to the retained rows, so `offset` cannot exceed the
        // 10 000-line cap.
        self.terminal.set_scrollback_offset(offset);
        self.selection = None;
        self.dragging = false;
        cx.notify();
    }

    /// Returns the viewport to the live screen.
    fn scroll_to_bottom(&mut self, cx: &mut Context<Self>) {
        self.pending_scroll = 0.0;
        self.set_scroll(0, cx);
    }

    /// Starts the cursor-blink timer when it is wanted and not already running.
    ///
    /// The task parks as soon as it sees an unfocused window, and `render`
    /// restarts it from the frame the activation refresh schedules, so no timer
    /// runs while the app is in the background.
    fn start_blink(&mut self, window: &Window, cx: &mut Context<Self>) {
        if !self.options.cursor_blink() {
            return;
        }
        self.blink_task = Some(cx.spawn_in(window, async move |pane, cx| loop {
            cx.background_executor().timer(CURSOR_BLINK_INTERVAL).await;
            let running = pane.update_in(cx, |pane, window, cx| {
                if !window.is_window_active() {
                    pane.blink_on = true;
                    pane.blink_parked = true;
                    return false;
                }
                pane.blink_on = !pane.blink_on;
                cx.notify();
                true
            });
            if !matches!(running, Ok(true)) {
                break;
            }
        }));
    }

    /// Builds one row: a backdrop for any run with a background colour, the
    /// selection and find highlights, the coalesced text runs positioned at
    /// their own columns, and the cursor.
    #[allow(clippy::too_many_arguments)]
    fn render_row(
        &self,
        row: u16,
        runs: &[Run],
        selection: Option<(u16, u16)>,
        matches: &[MatchSpan],
        active_match: Option<usize>,
        cursor_col: Option<u16>,
        cx: &mut Context<Self>,
    ) -> Div {
        let (cell_w, cell_h) = self.cell;
        let theme_fg = cx.theme().foreground;
        let theme_bg = cx.theme().background;
        let selection_color = cx.theme().primary.opacity(0.30);
        let match_color = cx.theme().danger.opacity(0.28);
        let active_match_color = cx.theme().danger.opacity(0.55);

        // Inverse video swaps the two colours, which is how a terminal draws a
        // selection or a highlighted menu row. A `Default` slot resolves to the
        // theme colour it is standing in for, so the swap stays legible on
        // either theme. Theme slots are `Hsla`; a cell colour is `Rgba`, so
        // convert the theme default to keep both arms the same type.
        let colors = |run: &Run| -> (Rgba, Rgba) {
            if run.style.inverse {
                (
                    run.style.bg.unwrap_or(theme_bg.into()),
                    run.style.fg.unwrap_or(theme_fg.into()),
                )
            } else {
                (
                    run.style.fg.unwrap_or(theme_fg.into()),
                    run.style.bg.unwrap_or(theme_bg.into()),
                )
            }
        };

        let mut line = div()
            .absolute()
            .left(px(0.0))
            .top(px(f32::from(row) * cell_h))
            .w_full()
            .h(px(cell_h));

        // Run backgrounds first, so a highlight draws over a coloured cell.
        // A background is only painted when the cell actually asked for one
        // (or is inverted); otherwise every default cell would need its own
        // quad and the pane would stop being cheap to draw.
        for run in runs {
            let (_, bg) = colors(run);
            if run.style.bg.is_some() || run.style.inverse {
                line = line.child(
                    div()
                        .absolute()
                        .left(px(f32::from(run.start) * cell_w))
                        .top(px(0.0))
                        .w(px(run.len as f32 * cell_w))
                        .h(px(cell_h))
                        .bg(bg),
                );
            }
        }

        // Selection, then find matches: at most one quad per row for the
        // selection and one per match, both proportional to the grid width.
        if let Some((first, last)) = selection {
            let width = last.saturating_sub(first) + 1;
            line = line.child(
                div()
                    .absolute()
                    .left(px(f32::from(first) * cell_w))
                    .top(px(0.0))
                    .w(px(f32::from(width) * cell_w))
                    .h(px(cell_h))
                    .bg(selection_color),
            );
        }
        for (index, span) in matches.iter().enumerate() {
            if span.row != row {
                continue;
            }
            let color = if Some(index) == active_match {
                active_match_color
            } else {
                match_color
            };
            line = line.child(
                div()
                    .absolute()
                    .left(px(f32::from(span.col) * cell_w))
                    .top(px(0.0))
                    .w(px(f32::from(span.len) * cell_w))
                    .h(px(cell_h))
                    .bg(color),
            );
        }

        // Text last, so glyphs sit on top of every highlight.
        for run in runs {
            if run.text.is_empty() {
                continue;
            }
            let (fg, _) = colors(run);
            line = line.child(
                div()
                    .absolute()
                    .left(px(f32::from(run.start) * cell_w))
                    .top(px(0.0))
                    .w(px(run.len as f32 * cell_w))
                    .h(px(cell_h))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .font_family("Menlo")
                    .text_size(px(self.options.font_size()))
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

/// The selected columns of one row, inclusive, or `None` when the row is not
/// touched by the selection. Clamped to `cols`, since a selection can outlive a
/// narrower resize.
fn selection_span(selection: Selection, row: u16, cols: u16) -> Option<(u16, u16)> {
    let (start, end) = selection.normalized();
    if cols == 0 || row < start.row || row > end.row {
        return None;
    }
    let last_col = cols - 1;
    let first = if row == start.row { start.col } else { 0 };
    let last = if row == end.row { end.col } else { last_col };
    let first = first.min(last_col);
    let last = last.min(last_col);
    (first <= last).then_some((first, last))
}

/// Copies the selected cells out of the grid.
///
/// Trailing blanks are trimmed and rows are joined with `\n`, the shape a
/// terminal emulator puts on the clipboard.
fn selection_text(terminal: &Terminal, selection: Selection) -> String {
    let cols = terminal.cols();
    if cols == 0 {
        return String::new();
    }
    let (start, end) = selection.normalized();
    let mut out = String::new();
    for row in start.row..=end.row {
        let Some((first, last)) = selection_span(selection, row, cols) else {
            continue;
        };
        let mut line = String::new();
        for col in first..=last {
            if let Some(cell) = terminal.cell(row, col) {
                line.push_str(cell.text());
            }
        }
        out.push_str(line.trim_end());
        if row != end.row {
            out.push('\n');
        }
    }
    out
}

/// A row's characters and the column each came from.
fn row_text(terminal: &Terminal, row: u16) -> (String, Vec<u16>) {
    let mut text = String::new();
    let mut columns = Vec::new();
    for col in 0..terminal.cols() {
        if let Some(cell) = terminal.cell(row, col) {
            for character in cell.text().chars() {
                text.push(character);
                columns.push(col);
            }
        }
    }
    (text, columns)
}

/// Every occurrence of `query` in the visible grid.
///
/// The search is limited to the visible grid on purpose: scrollback lives in the
/// engine and is not addressable by row, so an "all history" search would need a
/// separately bounded index.
fn find_matches(terminal: &Terminal, query: &str) -> Vec<MatchSpan> {
    let mut matches = Vec::new();
    if query.is_empty() {
        return matches;
    }
    let query_chars = query.chars().count();
    for row in 0..terminal.rows() {
        let (text, columns) = row_text(terminal, row);
        // `match_indices` walks byte offsets; the column map is per character.
        for (byte, _) in text.match_indices(query) {
            let start = text[..byte].chars().count();
            let Some(&col) = columns.get(start) else {
                continue;
            };
            let Some(&end_col) = columns.get(start + query_chars - 1) else {
                continue;
            };
            matches.push(MatchSpan {
                row,
                col,
                len: end_col - col + 1,
            });
        }
    }
    matches
}

/// Folds a wheel delta, in lines, into a scrollback offset.
///
/// The fractional remainder is returned so a trackpad's sub-line deltas
/// accumulate until they add up to a whole line instead of being rounded away.
fn fold_scroll(offset: usize, pending: f32, lines: f32) -> (usize, f32) {
    let mut pending = pending + lines;
    let whole = pending.trunc();
    if whole == 0.0 {
        return (offset, pending);
    }
    pending -= whole;
    let next = if whole > 0.0 {
        offset.saturating_add(whole as usize)
    } else {
        offset.saturating_sub((-whole) as usize)
    };
    (next, pending)
}

/// Maps a point in grid-local pixels to a cell, clamped to the grid.
fn cell_at(x: f32, y: f32, cell: (f32, f32), cols: u16, rows: u16) -> GridPos {
    let (cell_w, cell_h) = cell;
    let col = (x / cell_w.max(1.0)).floor();
    let row = (y / cell_h.max(1.0)).floor();
    GridPos {
        row: row.clamp(0.0, f32::from(rows.saturating_sub(1))) as u16,
        col: col.clamp(0.0, f32::from(cols.saturating_sub(1))) as u16,
    }
}

/// Measures the advance width and line height of Menlo at `font_size`.
///
/// Both come from the real text system, so the grid stays aligned on a HiDPI
/// display or when the font is missing; a wrong advance is exactly the
/// "everything is shifted" failure that makes a terminal unreadable.
fn measure_cell(window: &mut Window, font_size: f32) -> (f32, f32) {
    let text_system = window.text_system().clone();
    let font_id = text_system.resolve_font(&font("Menlo"));
    let bounds = text_system.bounding_box(font_id, px(font_size));
    let height = (f32::from(bounds.size.height) / 0.7).max(font_size).round();
    let advance = f32::from(text_system.layout_width(font_id, px(font_size), 'M'));
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
        let panel_bg = cx.theme().popover;
        let panel_fg = cx.theme().popover_foreground;
        let border = cx.theme().border;

        // Cursor blink is the one accepted terminal timer (docs/BUDGET.md). It
        // runs only while this window is focused; the task parks the moment it
        // sees an unfocused window, and this frame restarts it once activation
        // refreshes the view, so a background window has no timer at all.
        if self.options.cursor_blink()
            && window.is_window_active()
            && (self.blink_task.is_none() || self.blink_parked)
        {
            self.blink_parked = false;
            self.blink_on = true;
            self.start_blink(window, cx);
        }

        // Re-measure each frame: a couple of cached font lookups, and it is what
        // keeps the grid aligned when the scale factor or the configured font
        // size changes.
        self.cell = measure_cell(window, self.options.font_size());
        let (cell_w, cell_h) = self.cell;

        let (cursor_col, cursor_row, cursor_visible) = self.terminal.cursor();
        let rows = self.terminal.rows();
        let cols = self.terminal.cols();
        let offset = self.terminal.scrollback_offset();

        // Matches are recomputed from the grid every frame, so new output can
        // never leave a stale highlight behind.
        let matches = match &self.search {
            Some(search) => find_matches(&self.terminal, &search.query),
            None => Vec::new(),
        };
        let active_match = match &self.search {
            Some(search) if !matches.is_empty() => Some(search.current % matches.len()),
            _ => None,
        };
        let cursor_lit = !self.options.cursor_blink() || self.blink_on;

        let mut grid = div().relative().w_full().h(px(f32::from(rows) * cell_h));
        for row in 0..rows {
            let runs = row_runs(&self.terminal, row);
            let selection = self
                .selection
                .and_then(|selection| selection_span(selection, row, cols));
            // The live cursor is not on screen while the viewport is scrolled
            // back, so it is only drawn at the bottom.
            let cursor = (offset == 0 && cursor_visible && cursor_lit && cursor_row == row)
                .then_some(cursor_col);
            grid = grid.child(self.render_row(
                row,
                &runs,
                selection,
                &matches,
                active_match,
                cursor,
                cx,
            ));
        }

        let search_label = self.search.as_ref().map(|search| {
            if matches.is_empty() {
                format!("/{}  no matches", search.query)
            } else {
                format!(
                    "/{}  {}/{}",
                    search.query,
                    active_match.map_or(0, |index| index + 1),
                    matches.len()
                )
            }
        });
        let ended = self.ended.clone();
        let connecting = matches!(self.status, SessionState::Connecting);

        let mut root = div()
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
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    this.begin_selection(event, window, cx);
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _window, cx| {
                this.extend_selection(event, cx);
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _event: &MouseUpEvent, _window, cx| {
                    this.end_selection(cx);
                }),
            )
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _window, cx| {
                this.scroll_wheel(event, cx);
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
                    move |bounds: Bounds<gpui_kit::Pixels>,
                          size: gpui_kit::Size<gpui_kit::Pixels>,
                          _,
                          cx| {
                        view.update(cx, |pane, cx| {
                            // The grid shares the pane's top-left, so this is
                            // the origin a mouse position is measured from.
                            pane.grid_origin = bounds.origin;
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
            .child(grid);

        if let Some(label) = search_label {
            root = root.child(
                div()
                    .absolute()
                    .top(px(8.0))
                    .right(px(8.0))
                    .px_2()
                    .py_1()
                    .border_1()
                    .border_color(border)
                    .rounded_sm()
                    .bg(panel_bg)
                    .text_xs()
                    .text_color(panel_fg)
                    .child(SharedString::from(label)),
            );
        }
        if offset > 0 {
            root = root.child(
                div()
                    .absolute()
                    .bottom(px(8.0))
                    .right(px(8.0))
                    .px_2()
                    .py_1()
                    .border_1()
                    .border_color(border)
                    .rounded_sm()
                    .bg(panel_bg)
                    .text_xs()
                    .text_color(muted)
                    .child(SharedString::from(format!(
                        "↑ {offset} lines · End returns to the live screen"
                    ))),
            );
        }
        if let Some(message) = ended {
            root = root.child(
                div()
                    .absolute()
                    .left(px(12.0))
                    .bottom(px(12.0))
                    .text_xs()
                    .text_color(muted)
                    .child(SharedString::from(message)),
            );
        }
        if connecting {
            root = root.child(
                div()
                    .absolute()
                    .left(px(12.0))
                    .bottom(px(12.0))
                    .text_xs()
                    .text_color(muted)
                    .child("connecting…"),
            );
        }
        root
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

    /// A settings value can never collapse or explode the grid, and scrollback
    /// can be lowered but never raised past the budgeted cap.
    #[test]
    fn options_clamp_font_size_and_cap_scrollback() {
        let tiny = TerminalOptions::new(0.5, 0, true);
        assert_eq!(tiny.font_size(), MIN_FONT_SIZE);
        assert_eq!(tiny.scrollback_lines(), 1);
        assert!(tiny.cursor_blink());

        let huge = TerminalOptions::new(400.0, usize::MAX, false);
        assert_eq!(huge.font_size(), MAX_FONT_SIZE);
        assert_eq!(huge.scrollback_lines(), SCROLLBACK_LINES);
        assert!(!huge.cursor_blink());

        let not_a_number = TerminalOptions::new(f32::NAN, 500, true);
        assert_eq!(not_a_number.font_size(), FONT_SIZE);
        assert_eq!(not_a_number.scrollback_lines(), 500);
    }

    /// Trackpad deltas arrive in sub-line steps; the fractional carry is what
    /// stops them from being rounded to nothing, and the offset never goes
    /// negative.
    #[test]
    fn fold_scroll_carries_fractions_and_saturates_at_zero() {
        assert_eq!(fold_scroll(0, 0.0, 3.0), (3, 0.0));
        assert_eq!(fold_scroll(5, 0.0, -2.0), (3, 0.0));
        assert_eq!(fold_scroll(0, 0.0, -2.0), (0, 0.0));
        assert_eq!(fold_scroll(0, 0.0, 1e30), (usize::MAX, 0.0));

        let (offset, pending) = fold_scroll(0, 0.0, 0.4);
        assert_eq!(offset, 0);
        assert!((pending - 0.4).abs() < 1e-6);
        let (offset, pending) = fold_scroll(0, pending, 0.7);
        assert_eq!(offset, 1);
        assert!((pending - 0.1).abs() < 1e-5);
    }

    #[test]
    fn selection_span_covers_every_touched_row() {
        let selection = Selection {
            anchor: GridPos { row: 0, col: 2 },
            head: GridPos { row: 2, col: 3 },
        };
        assert_eq!(selection_span(selection, 0, 10), Some((2, 9)));
        assert_eq!(selection_span(selection, 1, 10), Some((0, 9)));
        assert_eq!(selection_span(selection, 2, 10), Some((0, 3)));
        assert_eq!(selection_span(selection, 3, 10), None);

        // A reversed drag selects the same cells.
        let reversed = Selection {
            anchor: GridPos { row: 2, col: 3 },
            head: GridPos { row: 0, col: 2 },
        };
        assert_eq!(selection_span(reversed, 0, 10), Some((2, 9)));
    }

    #[test]
    fn selection_text_trims_trailing_blanks_and_joins_rows() {
        let mut terminal = Terminal::new(10, 3, 10);
        terminal.feed(b"abc\r\ndef");

        let selection = Selection {
            anchor: GridPos { row: 0, col: 0 },
            head: GridPos { row: 1, col: 2 },
        };
        assert_eq!(selection_text(&terminal, selection), "abc\ndef");

        let single = Selection {
            anchor: GridPos { row: 1, col: 1 },
            head: GridPos { row: 1, col: 1 },
        };
        assert_eq!(selection_text(&terminal, single), "e");
    }

    #[test]
    fn search_finds_query_spans_in_the_visible_grid() {
        let mut terminal = Terminal::new(20, 3, 10);
        terminal.feed(b"hello world");

        assert_eq!(
            find_matches(&terminal, "world"),
            vec![MatchSpan {
                row: 0,
                col: 6,
                len: 5
            }]
        );
        assert_eq!(
            find_matches(&terminal, "l"),
            vec![
                MatchSpan {
                    row: 0,
                    col: 2,
                    len: 1
                },
                MatchSpan {
                    row: 0,
                    col: 3,
                    len: 1
                },
                MatchSpan {
                    row: 0,
                    col: 9,
                    len: 1
                },
            ]
        );
        assert!(find_matches(&terminal, "").is_empty());
        assert!(find_matches(&terminal, "zzz").is_empty());
    }

    #[test]
    fn cell_at_clamps_to_the_grid() {
        let cell = (8.0, 16.0);
        assert_eq!(cell_at(0.0, 0.0, cell, 10, 5), GridPos { row: 0, col: 0 });
        assert_eq!(cell_at(8.0, 16.0, cell, 10, 5), GridPos { row: 1, col: 1 });
        assert_eq!(
            cell_at(-100.0, -100.0, cell, 10, 5),
            GridPos { row: 0, col: 0 }
        );
        assert_eq!(
            cell_at(10_000.0, 10_000.0, cell, 10, 5),
            GridPos { row: 4, col: 9 }
        );
    }
}
