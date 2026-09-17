//! Terminal emulation for sshdeck.
//!
//! Owns the character grid: bytes from a channel go in, a renderable screen
//! comes out. No UI and no transport dependency, so it can be tested without a
//! window or a network.
//!
//! Escape-sequence parsing is delegated to [`vt100`] rather than hand-rolled;
//! this crate adds the input side (key -> xterm bytes) and a UI-agnostic cell
//! view. Colours are reported as data; nothing here knows about a theme.

use vt100::Parser;

// The engine's colour enum is already the data shape the UI needs (default /
// indexed / RGB), so it is re-exported rather than mirrored.
pub use vt100::Color;

/// Scrollback and screen state for one session.
pub struct Terminal {
    parser: Parser<TitleCallback>,
}

impl Terminal {
    /// Creates a terminal of `cols` x `rows` cells, retaining at most
    /// `scrollback_lines` scrolled-off rows.
    #[must_use]
    pub fn new(cols: u16, rows: u16, scrollback_lines: usize) -> Self {
        Self {
            parser: Parser::new_with_callbacks(
                rows,
                cols,
                scrollback_lines,
                TitleCallback::default(),
            ),
        }
    }

    /// Feeds a chunk of the remote byte stream into the parser.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
    }

    /// Resizes the visible grid, preserving the scrollback.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.parser.screen_mut().set_size(rows, cols);
    }

    /// Number of columns in the visible grid.
    #[must_use]
    pub fn cols(&self) -> u16 {
        self.parser.screen().size().1
    }

    /// Number of rows in the visible grid.
    #[must_use]
    pub fn rows(&self) -> u16 {
        self.parser.screen().size().0
    }

    /// The cell at `(row, col)`, if it is inside the visible grid.
    #[must_use]
    pub fn cell(&self, row: u16, col: u16) -> Option<Cell<'_>> {
        let screen = self.parser.screen();
        let cell = screen.cell(row, col)?;
        let (cursor_row, cursor_col) = screen.cursor_position();
        let cursor = !screen.hide_cursor() && cursor_row == row && cursor_col == col;
        Some(Cell {
            text: cell.contents(),
            fg: cell.fgcolor(),
            bg: cell.bgcolor(),
            bold: cell.bold(),
            italic: cell.italic(),
            underline: cell.underline(),
            inverse: cell.inverse(),
            cursor,
        })
    }

    /// Every visible cell, row-major, for a renderer to walk.
    pub fn cells(&self) -> impl Iterator<Item = Cell<'_>> {
        let rows = self.rows();
        let cols = self.cols();
        (0..rows).flat_map(move |row| (0..cols).filter_map(move |col| self.cell(row, col)))
    }

    /// Cursor as `(column, row, visible)`.
    #[must_use]
    pub fn cursor(&self) -> (u16, u16, bool) {
        let screen = self.parser.screen();
        let (row, col) = screen.cursor_position();
        (col, row, !screen.hide_cursor())
    }

    /// The window title set by the remote via OSC 2, if any.
    #[must_use]
    pub fn title(&self) -> Option<String> {
        self.parser.callbacks().title.clone()
    }

    /// Current scrollback view offset, `0` when the live screen is in view.
    #[must_use]
    pub fn scrollback_offset(&self) -> usize {
        self.parser.screen().scrollback()
    }

    /// Scrolls the view `offset` rows back; the engine clamps it to the
    /// number of retained rows, so it can never exceed the scrollback cap.
    pub fn set_scrollback_offset(&mut self, offset: usize) {
        self.parser.screen_mut().set_scrollback(offset);
    }
}

/// A single rendered cell, borrowed from the grid.
#[derive(Clone, Copy, Debug)]
pub struct Cell<'a> {
    text: &'a str,
    fg: Color,
    bg: Color,
    bold: bool,
    italic: bool,
    underline: bool,
    inverse: bool,
    cursor: bool,
}

impl<'a> Cell<'a> {
    /// Text in the cell. Usually one character; may be several codepoints when
    /// combining marks are present.
    #[must_use]
    pub fn text(&self) -> &'a str {
        self.text
    }

    /// Foreground colour.
    #[must_use]
    pub fn fg(&self) -> Color {
        self.fg
    }

    /// Background colour.
    #[must_use]
    pub fn bg(&self) -> Color {
        self.bg
    }

    /// Bold.
    #[must_use]
    pub fn bold(&self) -> bool {
        self.bold
    }

    /// Italic.
    #[must_use]
    pub fn italic(&self) -> bool {
        self.italic
    }

    /// Underline.
    #[must_use]
    pub fn underline(&self) -> bool {
        self.underline
    }

    /// Inverse video.
    #[must_use]
    pub fn inverse(&self) -> bool {
        self.inverse
    }

    /// Whether this cell is the cursor. False while the cursor is hidden.
    #[must_use]
    pub fn is_cursor(&self) -> bool {
        self.cursor
    }
}

/// Captures the OSC 2 window title, which the engine does not store itself.
#[derive(Default)]
struct TitleCallback {
    title: Option<String>,
}

impl vt100::Callbacks for TitleCallback {
    fn set_window_title(&mut self, _screen: &mut vt100::Screen, title: &[u8]) {
        self.title = Some(String::from_utf8_lossy(title).into_owned());
    }
}

/// A key press, independent of any UI toolkit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter,
    Tab,
    Backspace,
    Delete,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,
}

/// Modifier state for a key press.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Modifiers {
    shift: bool,
    alt: bool,
    ctrl: bool,
}

impl Modifiers {
    #[must_use]
    pub const fn new(shift: bool, alt: bool, ctrl: bool) -> Self {
        Self { shift, alt, ctrl }
    }

    #[must_use]
    pub const fn shift(self) -> bool {
        self.shift
    }

    #[must_use]
    pub const fn alt(self) -> bool {
        self.alt
    }

    #[must_use]
    pub const fn ctrl(self) -> bool {
        self.ctrl
    }
}

/// Encodes a key press as xterm-256color input bytes.
///
/// Cursor and function keys use the xterm modifier parameter
/// (`1 + shift + 2*alt + 4*ctrl`) when any modifier is held. Ctrl+letter
/// yields the C0 control byte (Ctrl+C -> `0x03`); Alt prefixes `ESC`.
// ponytail: keys are encoded against xterm defaults. Application-cursor-mode
// (`ESC O`) variants and bracketed-paste framing are left to the caller, which
// knows the current terminal modes.
#[must_use]
pub fn encode_key(key: Key, mods: Modifiers) -> Vec<u8> {
    let m = 1 + u8::from(mods.shift()) + 2 * u8::from(mods.alt()) + 4 * u8::from(mods.ctrl());
    match key {
        Key::Char(c) => encode_char(c, mods),
        Key::Enter => escape_prefix(0x0d, mods),
        Key::Tab => escape_prefix(0x09, mods),
        Key::Backspace => escape_prefix(0x7f, mods),
        Key::Escape => escape_prefix(0x1b, mods),
        Key::Up => csi(b'A', m),
        Key::Down => csi(b'B', m),
        Key::Right => csi(b'C', m),
        Key::Left => csi(b'D', m),
        Key::Home => csi(b'H', m),
        Key::End => csi(b'F', m),
        Key::Insert => tilde(2, m),
        Key::Delete => tilde(3, m),
        Key::PageUp => tilde(5, m),
        Key::PageDown => tilde(6, m),
        Key::F1 => ss3(b'P', m),
        Key::F2 => ss3(b'Q', m),
        Key::F3 => ss3(b'R', m),
        Key::F4 => ss3(b'S', m),
        Key::F5 => tilde(15, m),
        Key::F6 => tilde(17, m),
        Key::F7 => tilde(18, m),
        Key::F8 => tilde(19, m),
        Key::F9 => tilde(20, m),
        Key::F10 => tilde(21, m),
        Key::F11 => tilde(23, m),
        Key::F12 => tilde(24, m),
    }
}

fn encode_char(c: char, mods: Modifiers) -> Vec<u8> {
    let mut out = Vec::new();
    if mods.alt() {
        out.push(0x1b);
    }
    let ctrl_byte = if mods.ctrl() { control_byte(c) } else { None };
    if let Some(byte) = ctrl_byte {
        out.push(byte);
        return out;
    }
    let mut buf = [0u8; 4];
    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
    out
}

/// The C0 byte a character produces under Ctrl, xterm style.
fn control_byte(c: char) -> Option<u8> {
    let upper = c.to_ascii_uppercase();
    match upper {
        'A'..='Z' => Some(upper as u8 - b'A' + 1),
        '@' | ' ' => Some(0x00),
        '[' => Some(0x1b),
        '\\' => Some(0x1c),
        ']' => Some(0x1d),
        '^' => Some(0x1e),
        '_' => Some(0x1f),
        '?' => Some(0x7f),
        _ => None,
    }
}

fn escape_prefix(byte: u8, mods: Modifiers) -> Vec<u8> {
    if mods.alt() {
        vec![0x1b, byte]
    } else {
        vec![byte]
    }
}

/// `CSI <final>` or `CSI 1;<m><final>` (arrows, Home, End).
fn csi(final_byte: u8, m: u8) -> Vec<u8> {
    let mut out = vec![0x1b, b'['];
    if m > 1 {
        out.push(b'1');
        out.push(b';');
        out.push(b'0' + m);
    }
    out.push(final_byte);
    out
}

/// `CSI <n>~` or `CSI <n>;<m>~` (Insert/Delete/PageUp/PageDown/F5-F12).
fn tilde(n: u8, m: u8) -> Vec<u8> {
    let mut out = vec![0x1b, b'['];
    out.extend_from_slice(n.to_string().as_bytes());
    if m > 1 {
        out.push(b';');
        out.push(b'0' + m);
    }
    out.push(b'~');
    out
}

/// `SS3 <final>` or `CSI 1;<m><final>` (F1-F4).
fn ss3(final_byte: u8, m: u8) -> Vec<u8> {
    if m > 1 {
        csi(final_byte, m)
    } else {
        vec![0x1b, b'O', final_byte]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell_text(terminal: &Terminal, row: u16, col: u16) -> String {
        terminal
            .cell(row, col)
            .map(|c| c.text().to_string())
            .unwrap_or_default()
    }

    #[test]
    fn plain_text_lands_and_advances_cursor() {
        let mut terminal = Terminal::new(20, 5, 100);
        terminal.feed(b"hi");
        assert_eq!(cell_text(&terminal, 0, 0), "h");
        assert_eq!(cell_text(&terminal, 0, 1), "i");
        assert_eq!(terminal.cursor(), (2, 0, true));
    }

    #[test]
    fn crlf_moves_to_next_line() {
        let mut terminal = Terminal::new(20, 5, 100);
        terminal.feed(b"ab\r\ncd");
        assert_eq!(cell_text(&terminal, 0, 0), "a");
        assert_eq!(cell_text(&terminal, 1, 0), "c");
        assert_eq!(cell_text(&terminal, 1, 1), "d");
    }

    #[test]
    fn lone_newline_keeps_column() {
        let mut terminal = Terminal::new(20, 5, 100);
        terminal.feed(b"x\ny");
        assert_eq!(cell_text(&terminal, 1, 1), "y");
        assert_eq!(cell_text(&terminal, 1, 0), "");
    }

    #[test]
    fn lone_carriage_return_resets_column() {
        let mut terminal = Terminal::new(20, 5, 100);
        terminal.feed(b"ab\rc");
        assert_eq!(cell_text(&terminal, 0, 0), "c");
        assert_eq!(cell_text(&terminal, 0, 1), "b");
    }

    #[test]
    fn sgr_color_is_parsed() {
        let mut terminal = Terminal::new(20, 5, 100);
        terminal.feed(b"\x1b[31mR");
        let red = terminal.cell(0, 0).expect("cell exists");
        assert_eq!(red.fg(), Color::Idx(1));
        let blank = terminal.cell(0, 1).expect("cell exists");
        assert_eq!(blank.fg(), Color::Default);
    }

    #[test]
    fn resize_updates_dimensions() {
        let mut terminal = Terminal::new(20, 5, 100);
        terminal.resize(30, 8);
        assert_eq!(terminal.cols(), 30);
        assert_eq!(terminal.rows(), 8);
        terminal.feed(b"z");
        assert_eq!(cell_text(&terminal, 0, 0), "z");
    }

    #[test]
    fn scrollback_is_capped() {
        let mut terminal = Terminal::new(10, 2, 5);
        let mut input = Vec::new();
        for i in 0..40 {
            input.extend_from_slice(format!("line{i}\r\n").as_bytes());
        }
        terminal.feed(&input);
        terminal.set_scrollback_offset(usize::MAX);
        assert_eq!(terminal.scrollback_offset(), 5);
    }

    #[test]
    fn ctrl_c_is_control_byte() {
        assert_eq!(
            encode_key(Key::Char('c'), Modifiers::new(false, false, true)),
            vec![0x03]
        );
    }

    #[test]
    fn ctrl_alt_arrow_is_modifier_encoded() {
        assert_eq!(
            encode_key(Key::Up, Modifiers::new(false, true, true)),
            b"\x1b[1;7A".to_vec()
        );
    }
}
