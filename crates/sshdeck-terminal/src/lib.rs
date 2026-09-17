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
// indexed / RGB), so it is re-exported rather than mirrored. The mouse enums
// are re-exported the same way: the engine already parses `DECSET`/`DECRST`
// into them, so mirroring them would only add a conversion.
pub use vt100::{Color, MouseProtocolEncoding, MouseProtocolMode};

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

    /// Whether the remote has enabled bracketed paste (`DECSET 2004`). When it
    /// is on, a paste must be framed with `ESC[200~`/`ESC[201~`; use
    /// [`Terminal::encode_paste`] so that framing cannot be forgotten.
    #[must_use]
    pub fn bracketed_paste(&self) -> bool {
        self.parser.screen().bracketed_paste()
    }

    /// Whether application-cursor-keys mode (`DECSET 1`) is on. It makes the
    /// arrow-shaped keys emit `ESC O A` instead of `ESC [ A`; use
    /// [`Terminal::encode_key`] so the bytes match the mode.
    #[must_use]
    pub fn application_cursor_keys(&self) -> bool {
        self.parser.screen().application_cursor()
    }

    /// Whether application-keypad mode (`ESC =`) is on.
    #[must_use]
    pub fn application_keypad(&self) -> bool {
        self.parser.screen().application_keypad()
    }

    /// Whether the cursor is visible (`DECSET 25`).
    #[must_use]
    pub fn cursor_visible(&self) -> bool {
        !self.parser.screen().hide_cursor()
    }

    /// The remote's mouse-tracking mode (`DECSET 9`/`1000`/`1002`/`1003`).
    /// Mouse events should only be reported while this is not
    /// [`MouseProtocolMode::None`].
    #[must_use]
    pub fn mouse_mode(&self) -> MouseProtocolMode {
        self.parser.screen().mouse_protocol_mode()
    }

    /// The remote's mouse-report encoding (`DECSET 1005`/`1006`). SGR reports
    /// from [`encode_sgr_mouse`] are only correct while this is
    /// [`MouseProtocolEncoding::Sgr`].
    #[must_use]
    pub fn mouse_encoding(&self) -> MouseProtocolEncoding {
        self.parser.screen().mouse_protocol_encoding()
    }

    /// Whether the alternate screen is in use (`DECSET 47`/`1049`).
    // ponytail: the engine tracks 47 and 1049 but ignores 1047; 1049 is what
    // full-screen apps use, and the caller only needs the distinction.
    #[must_use]
    pub fn alternate_screen(&self) -> bool {
        self.parser.screen().alternate_screen()
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

    /// Encodes a key press using this terminal's current input modes. Prefer
    /// this over the free [`encode_key`], which assumes the default modes.
    #[must_use]
    pub fn encode_key(&self, key: Key, mods: Modifiers) -> Vec<u8> {
        encode_key_with(key, mods, self.application_cursor_keys())
    }

    /// Bytes to send for a paste of `text`, framed according to this
    /// terminal's current bracketed-paste mode. Prefer this over the free
    /// [`encode_paste`], which needs the mode passed in by hand.
    #[must_use]
    pub fn encode_paste(&self, text: &str) -> Vec<u8> {
        encode_paste(text, self.bracketed_paste())
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

/// Encodes a key press as xterm-256color input bytes, assuming the default
/// (non-application) cursor mode.
///
/// Cursor and function keys use the xterm modifier parameter
/// (`1 + shift + 2*alt + 4*ctrl`) when any modifier is held. Ctrl+letter
/// yields the C0 control byte (Ctrl+C -> `0x03`); Alt prefixes `ESC`.
///
/// This signature is retained for callers that hold no [`Terminal`]; a
/// terminal-aware caller should use [`Terminal::encode_key`] instead, which
/// consults [`Terminal::application_cursor_keys`].
#[must_use]
pub fn encode_key(key: Key, mods: Modifiers) -> Vec<u8> {
    encode_key_with(key, mods, false)
}

/// Encodes a key press as xterm-256color input bytes for a terminal in the
/// given input mode.
///
/// Application-cursor-keys mode (`DECSET 1`) is the only mode that changes a
/// key's bytes today: the arrow-shaped keys then emit `ESC O A`-style (SS3)
/// sequences when no modifier is held. With a modifier, xterm uses the `CSI`
/// form in both modes.
#[must_use]
pub fn encode_key_with(key: Key, mods: Modifiers, application_cursor_keys: bool) -> Vec<u8> {
    let m = 1 + u8::from(mods.shift()) + 2 * u8::from(mods.alt()) + 4 * u8::from(mods.ctrl());
    match key {
        Key::Char(c) => encode_char(c, mods),
        Key::Enter => escape_prefix(0x0d, mods),
        Key::Tab => escape_prefix(0x09, mods),
        Key::Backspace => escape_prefix(0x7f, mods),
        Key::Escape => escape_prefix(0x1b, mods),
        Key::Up => cursor_key(b'A', m, application_cursor_keys),
        Key::Down => cursor_key(b'B', m, application_cursor_keys),
        Key::Right => cursor_key(b'C', m, application_cursor_keys),
        Key::Left => cursor_key(b'D', m, application_cursor_keys),
        Key::Home => cursor_key(b'H', m, application_cursor_keys),
        Key::End => cursor_key(b'F', m, application_cursor_keys),
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

/// Arrow-shaped cursor keys. Under application-cursor-keys with no modifier
/// xterm uses the SS3 form (`ESC O A`); every other case uses `CSI`.
fn cursor_key(final_byte: u8, m: u8, application_cursor_keys: bool) -> Vec<u8> {
    if application_cursor_keys && m == 1 {
        ss3(final_byte, m)
    } else {
        csi(final_byte, m)
    }
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

/// Bytes to send for pasting `text`, given the bracketed-paste mode.
///
/// When `bracketed_paste` is on the text is framed with `ESC[200~` /
/// `ESC[201~` so the remote can tell a paste from typing. When it is off the
/// framing would be echoed literally, so the text is sent unwrapped — but
/// first every newline is normalised to the `\r` a Return key sends. Sending a
/// raw `\n` instead would leave the line feed without a carriage return (the
/// column does not reset) and a multi-line paste would trigger line by line.
///
/// A terminal-aware caller should use [`Terminal::encode_paste`], which reads
/// the mode itself.
#[must_use]
pub fn encode_paste(text: &str, bracketed_paste: bool) -> Vec<u8> {
    if bracketed_paste {
        let mut out = Vec::with_capacity(text.len() + 12);
        out.extend_from_slice(b"\x1b[200~");
        out.extend_from_slice(text.as_bytes());
        out.extend_from_slice(b"\x1b[201~");
        out
    } else {
        let mut out = Vec::with_capacity(text.len());
        let mut pending_cr = false;
        for c in text.chars() {
            match c {
                // The `\r` of a CRLF pair is already in `out`; drop the `\n`.
                '\n' if pending_cr => {}
                '\n' => out.push(b'\r'),
                _ => {
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                }
            }
            pending_cr = c == '\r';
        }
        out
    }
}

/// Encodes a mouse event as an SGR (1006) report: `CSI < b ; x ; y M` for a
/// press and `... m` for a release. Only correct while the remote is in
/// [`MouseProtocolEncoding::Sgr`] (see [`Terminal::mouse_encoding`]).
///
/// `button` is the SGR button code (0 left, 1 middle, 2 right, 64/65 wheel;
/// add 32 for a motion report). `col` and `row` are zero-based terminal cells;
/// the protocol reports them one-based, which is added here.
#[must_use]
pub fn encode_sgr_mouse(button: u8, col: u16, row: u16, release: bool) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"\x1b[<");
    out.extend_from_slice(button.to_string().as_bytes());
    out.push(b';');
    out.extend_from_slice((u32::from(col) + 1).to_string().as_bytes());
    out.push(b';');
    out.extend_from_slice((u32::from(row) + 1).to_string().as_bytes());
    out.push(if release { b'm' } else { b'M' });
    out
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

    #[test]
    fn bracketed_paste_defaults_off_and_toggles() {
        let mut terminal = Terminal::new(20, 5, 100);
        assert!(!terminal.bracketed_paste());
        terminal.feed(b"\x1b[?2004h");
        assert!(terminal.bracketed_paste());
        terminal.feed(b"\x1b[?2004l");
        assert!(!terminal.bracketed_paste());
    }

    #[test]
    fn paste_helper_wraps_only_in_bracketed_mode_and_normalises_off() {
        let mut terminal = Terminal::new(20, 5, 100);
        // Off: a newline becomes the Return byte so the paste is not submitted
        // line by line, and a CRLF collapses to a single Return.
        assert_eq!(terminal.encode_paste("one\ntwo"), b"one\rtwo".to_vec());
        assert_eq!(terminal.encode_paste("a\r\nb"), b"a\rb".to_vec());
        terminal.feed(b"\x1b[?2004h");
        assert!(terminal.bracketed_paste());
        // On: framed verbatim; the remote handles the embedded newline.
        assert_eq!(
            terminal.encode_paste("one\ntwo"),
            b"\x1b[200~one\ntwo\x1b[201~".to_vec()
        );
    }

    #[test]
    fn application_cursor_keys_switches_arrow_form() {
        let mut terminal = Terminal::new(20, 5, 100);
        assert!(!terminal.application_cursor_keys());
        assert_eq!(
            terminal.encode_key(Key::Up, Modifiers::default()),
            b"\x1b[A".to_vec()
        );
        terminal.feed(b"\x1b[?1h");
        assert!(terminal.application_cursor_keys());
        assert_eq!(
            terminal.encode_key(Key::Up, Modifiers::default()),
            b"\x1bOA".to_vec()
        );
        assert_eq!(
            terminal.encode_key(Key::Left, Modifiers::default()),
            b"\x1bOD".to_vec()
        );
        // A held modifier uses the CSI form in either mode.
        assert_eq!(
            terminal.encode_key(Key::Up, Modifiers::new(false, false, true)),
            b"\x1b[1;5A".to_vec()
        );
        terminal.feed(b"\x1b[?1l");
        assert!(!terminal.application_cursor_keys());
        assert_eq!(
            terminal.encode_key(Key::Up, Modifiers::default()),
            b"\x1b[A".to_vec()
        );
    }

    #[test]
    fn cursor_visibility_reflects_mode() {
        let mut terminal = Terminal::new(20, 5, 100);
        assert!(terminal.cursor_visible());
        terminal.feed(b"\x1b[?25l");
        assert!(!terminal.cursor_visible());
        terminal.feed(b"\x1b[?25h");
        assert!(terminal.cursor_visible());
    }

    #[test]
    fn mouse_mode_and_sgr_report_encode() {
        let mut terminal = Terminal::new(20, 5, 100);
        assert_eq!(terminal.mouse_mode(), MouseProtocolMode::None);
        terminal.feed(b"\x1b[?1000h");
        assert_eq!(terminal.mouse_mode(), MouseProtocolMode::PressRelease);
        terminal.feed(b"\x1b[?1006h");
        assert_eq!(terminal.mouse_encoding(), MouseProtocolEncoding::Sgr);
        // A left press at zero-based cell (4, 9) is reported one-based.
        assert_eq!(encode_sgr_mouse(0, 4, 9, false), b"\x1b[<0;5;10M".to_vec());
        assert_eq!(encode_sgr_mouse(0, 4, 9, true), b"\x1b[<0;5;10m".to_vec());
        terminal.feed(b"\x1b[?1000l");
        assert_eq!(terminal.mouse_mode(), MouseProtocolMode::None);
    }

    #[test]
    fn mode_survives_chunk_boundary() {
        let mut terminal = Terminal::new(20, 5, 100);
        // The escape sequence is split mid-parameter across two feeds.
        terminal.feed(b"\x1b[?20");
        assert!(!terminal.bracketed_paste());
        terminal.feed(b"04h");
        assert!(terminal.bracketed_paste());
        terminal.feed(b"\x1b[?1");
        terminal.feed(b"h");
        assert!(terminal.application_cursor_keys());
    }
}
