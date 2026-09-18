//! sshdeck — a GPUI desktop SSH client.
//!
//! The shell (host inventory, filtering, selection, persistence, theming) is
//! milestone 1. This is milestone 2: the pane next to it is a real terminal
//! backed by the `sshdeck-core` transport and the `sshdeck-terminal` grid.

mod forward_pane;
pub mod glyph;
mod keys_pane;
mod local_session;
mod logs_pane;
mod palette;
mod settings;
mod sftp_pane;
mod snippets_pane;
mod terminal;

use forward_pane::ForwardPane;
use gpui_kit::component::{
    button::{Button, ButtonVariants as _},
    input::{Input, InputContentType, InputEvent, InputState},
    notification::Notification,
    scroll::ScrollableElement as _,
    tooltip::Tooltip,
    ActiveTheme as _, Disableable as _, Icon, IconName, InteractiveElementExt as _, Root,
    Selectable as _, Sizable as _, Size, Theme, ThemeMode, ThemeRegistry, WindowExt,
};
use gpui_kit::prelude::{FluentBuilder as _, StatefulInteractiveElement as _};
use gpui_kit::{
    actions, div, point, px, rgb, rgba, AnyElement, App, AppContext as _, ClipboardItem, Context,
    Entity, Focusable as _, Hsla, InteractiveElement as _, IntoElement, KeyBinding, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement as _, Pixels, Point, Render, Rgba,
    SharedString, Styled as _, Subscription, TitlebarOptions, Window, WindowControlArea,
    WindowOptions,
};
use keys_pane::KeysPane;
use logs_pane::LogsPane;
use palette::PaletteView;
use settings::SettingsView;
use sftp_pane::SftpPane;
use snippets_pane::SnippetsPane;
use sshdeck_core::session::{Session as SshSession, SessionConfig, SessionEvent};
use sshdeck_core::{AuthMethod, Host, HostId, HostStore, SessionState};
use sshdeck_sftp::SftpClient;
use sshdeck_vault::Vault;
use terminal::{
    PaneHeaderAction, PaneStatus, TerminalOptions, TerminalPane, TerminalScheme, SCHEMES,
};

actions!(
    sshdeck,
    [
        OpenPalette,
        FindInTerminal,
        NewTabAction,
        CloseTabAction,
        ToggleSidebarAction,
        OpenSettingsAction,
        ZoomInAction,
        ZoomOutAction,
        ResetZoomAction
    ]
);

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match parse_args(&args) {
        // Headless `--connect … --exec …`: no gpui and no window at all.
        Ok(Mode::Exec { host, command }) => std::process::exit(run_headless(&host, &command)),
        Err(message) => {
            eprintln!("sshdeck: {message}");
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
        // No headless flags: the desktop app, exactly as before.
        Ok(Mode::Gui) => {}
    }

    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(|cx| {
            gpui_kit::init(cx);
            init_theme(cx);
            cx.bind_keys([
                KeyBinding::new("cmd-k", OpenPalette, None),
                KeyBinding::new("ctrl-k", OpenPalette, None),
                KeyBinding::new("cmd-f", FindInTerminal, None),
                KeyBinding::new("ctrl-f", FindInTerminal, None),
                KeyBinding::new("cmd-t", NewTabAction, None),
                KeyBinding::new("ctrl-t", NewTabAction, None),
                KeyBinding::new("cmd-w", CloseTabAction, None),
                KeyBinding::new("ctrl-w", CloseTabAction, None),
                KeyBinding::new("cmd-b", ToggleSidebarAction, None),
                KeyBinding::new("ctrl-b", ToggleSidebarAction, None),
                KeyBinding::new("cmd-,", OpenSettingsAction, None),
                KeyBinding::new("ctrl-,", OpenSettingsAction, None),
                KeyBinding::new("cmd-=", ZoomInAction, None),
                KeyBinding::new("cmd-+", ZoomInAction, None),
                KeyBinding::new("cmd--", ZoomOutAction, None),
                KeyBinding::new("cmd-0", ResetZoomAction, None),
            ]);
            cx.spawn(async move |cx| {
                cx.open_window(window_options(), |window, cx| {
                    let view = cx.new(|cx| SshDeck::new(window, cx));
                    cx.new(|cx| Root::new(view, window, cx))
                })
                .expect("failed to open window");
            })
            .detach();
        });
}

/// The window shape: frameless, with the native traffic lights still present.
///
/// Termius is a 10px-rounded frameless window. `WindowOptions` has **no**
/// corner-radius field, so that exact radius cannot be expressed by GPUI and
/// the native macOS window radius stands. `appears_transparent` is the part
/// GPUI does express: it hides the system title bar and draws the content under
/// it, while the window is still created with the closable / minimizable /
/// resizable style masks (because `titlebar` is `Some`), so the traffic lights
/// and the close button stay — the window always has a way to close.
///
/// `traffic_light_position` is set explicitly so the lights sit level with the
/// 30px tab pills inside the 40px header (the [gpui-component `TitleBar`] uses
/// `(9, 9)` for its 34px bar; `(9, 13)` centres a ~14px light group in 40px).
/// The header marks itself as `WindowControlArea::Drag`, so the window can be
/// dragged from the whole header even though the system title bar is hidden.
///
/// [gpui-component `TitleBar`]: https://docs.rs/gpui-component
fn window_options() -> WindowOptions {
    WindowOptions {
        titlebar: Some(TitlebarOptions {
            title: None,
            appears_transparent: true,
            traffic_light_position: Some(point(px(9.), px(13.))),
        }),
        app_owns_titlebar_drag: false,
        ..WindowOptions::default()
    }
}

/// The command-line forms the binary understands.
#[derive(Debug, PartialEq, Eq)]
enum Mode {
    /// No `--connect`/`--exec`: start the desktop app.
    Gui,
    /// Run one command over SSH and stream its output to stdout.
    Exec { host: String, command: String },
}

/// Usage for the `--connect`/`--exec` form, printed on any argument error.
const USAGE: &str = "\
usage: sshdeck [--connect <host-id-or-label> --exec \"<command>\"]

  (no arguments)   start the desktop app
  --connect        host id or label from the saved inventory
  --exec           run one command over SSH and stream its output to stdout";

/// Parses the command line with the program name removed.
///
/// `--connect` and `--exec` are accepted in either order; no flags at all means
/// the GUI. Anything else is a usage error whose message `main` prints before
/// exiting 2. Kept as a pure function so it is testable without a process.
fn parse_args(args: &[String]) -> Result<Mode, String> {
    let mut host: Option<String> = None;
    let mut command: Option<String> = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--connect" => {
                index += 1;
                host = Some(
                    args.get(index)
                        .ok_or("--connect needs a host id or label")?
                        .clone(),
                );
            }
            "--exec" => {
                index += 1;
                command = Some(args.get(index).ok_or("--exec needs a command")?.clone());
            }
            other => return Err(format!("unexpected argument: {other}")),
        }
        index += 1;
    }

    match (host, command) {
        (None, None) => Ok(Mode::Gui),
        (None, Some(_)) => Err("--exec requires --connect".to_string()),
        (Some(_), None) => Err("--connect requires --exec".to_string()),
        (Some(host), Some(command)) => Ok(Mode::Exec { host, command }),
    }
}

/// The `SSHDECK_PASSWORD` override, if one is set.
///
/// Development and test affordance; see `connect` for the full note. The value
/// goes straight to [`SessionConfig::with_password`] and is never logged or
/// written to disk.
fn env_password() -> Option<String> {
    std::env::var("SSHDECK_PASSWORD").ok()
}

/// The name of the dark theme in the bundled theme set.
const DEFAULT_THEME: &str = "sshdeck Dark";

/// The light theme name — the app now defaults to Light, header stays dark.
#[allow(dead_code)]
const DEFAULT_LIGHT_THEME: &str = "sshdeck Light";

/// Registers the bundled Termius-matched theme and makes [`DEFAULT_THEME`] the
/// startup theme.
///
/// Route: the `ThemeSet` is **embedded** with `include_str!` and loaded through
/// `ThemeRegistry::load_themes_from_str` rather than `ThemeRegistry::watch_dir`.
/// A bundled macOS app has no reliable working directory, so a path-based
/// registry would look for `themes/sshdeck.json` relative to wherever the
/// process happened to launch and miss it; the embedded string cannot miss.
/// `load_themes_from_str` is public in `gpui-component` 0.6.1 (`theme/registry.rs`)
/// even though `theme.md` only documents the directory watcher.
///
/// `Theme::change` re-applies the selected mode's config **and** projects it to
/// the Base layer (scrollbars, resize handles); applying the loaded config with
/// `apply_config` alone would leave that projection stale.
fn init_theme(cx: &mut App) {
    const THEME_SET: &str = include_str!("../themes/sshdeck.json");

    if let Err(error) = ThemeRegistry::global_mut(cx).load_themes_from_str(THEME_SET) {
        // Non-fatal: a theme that will not parse should not stop the app; it
        // starts in the built-in default instead of the Termius-matched one.
        eprintln!("sshdeck: could not load the bundled theme: {error}");
        return;
    }

    // Clone the configs out of the registry in one immutable borrow, before
    // touching the mutable globals below.
    let (light, dark) = {
        let themes = ThemeRegistry::global(cx).themes();
        (
            themes.get(&SharedString::from("sshdeck Light")).cloned(),
            themes.get(&SharedString::from(DEFAULT_THEME)).cloned(),
        )
    };
    if let Some(light) = light {
        Theme::global_mut(cx).light_theme = light;
    }
    if let Some(dark) = dark {
        Theme::global_mut(cx).dark_theme = dark;
    }

    // Light is the default; header stays dark navy explicitly (see render_header).
    Theme::change(ThemeMode::Light, None, cx);
}

/// Splits the `SSHDECK_AUTOCONNECT` value into individual targets.
///
/// Development and test affordance only: a comma-separated list opens one
/// session per entry so a multi-session footprint can be measured. Whitespace
/// is trimmed and empty entries are dropped, so the original single-host form
/// keeps working unchanged.
fn autoconnect_targets(value: &str) -> Vec<&str> {
    value
        .split(',')
        .map(str::trim)
        .filter(|target| !target.is_empty())
        .collect()
}

/// Parses a quick-connect string (`user@host:port`, `ssh ...`, `ip:port`, `ip`, or domain)
/// into an ephemeral [`Host`] ready for connection.
///
/// Returns `None` if the input is a plain search term rather than a connection target.
pub fn parse_quick_connect(raw: &str) -> Option<Host> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }

    let mut username = String::new();
    let address;
    let mut port = 22u16;

    if let Some(rest) = raw.strip_prefix("ssh ") {
        let parts: Vec<&str> = rest.split_whitespace().collect();
        let mut idx = 0;
        let mut target = None;
        while idx < parts.len() {
            let part = parts[idx];
            if part == "-p" && idx + 1 < parts.len() {
                if let Ok(p) = parts[idx + 1].parse::<u16>() {
                    port = p;
                }
                idx += 2;
            } else if part.starts_with("-p") && part.len() > 2 {
                if let Ok(p) = part[2..].parse::<u16>() {
                    port = p;
                }
                idx += 1;
            } else if part == "-l" && idx + 1 < parts.len() {
                username = parts[idx + 1].to_string();
                idx += 2;
            } else if !part.starts_with('-') && target.is_none() {
                target = Some(part);
                idx += 1;
            } else {
                idx += 1;
            }
        }
        let target = target?;
        if let Some((user, host_part)) = target.split_once('@') {
            if username.is_empty() {
                username = user.to_string();
            }
            if let Some((host, p)) = host_part.rsplit_once(':') {
                if let Ok(parsed_port) = p.parse::<u16>() {
                    address = host.to_string();
                    port = parsed_port;
                } else {
                    address = host_part.to_string();
                }
            } else {
                address = host_part.to_string();
            }
        } else if let Some((host, p)) = target.rsplit_once(':') {
            if let Ok(parsed_port) = p.parse::<u16>() {
                address = host.to_string();
                port = parsed_port;
            } else {
                address = target.to_string();
            }
        } else {
            address = target.to_string();
        }
    } else if raw.contains('@') {
        let (user, host_part) = raw.split_once('@')?;
        username = user.to_string();
        if let Some((host, p)) = host_part.rsplit_once(':') {
            if let Ok(parsed_port) = p.parse::<u16>() {
                address = host.to_string();
                port = parsed_port;
            } else {
                address = host_part.to_string();
            }
        } else {
            address = host_part.to_string();
        }
    } else if let Some((host, p)) = raw.rsplit_once(':') {
        if let Ok(parsed_port) = p.parse::<u16>() {
            address = host.to_string();
            port = parsed_port;
        } else {
            return None;
        }
    } else if raw.parse::<std::net::IpAddr>().is_ok() {
        address = raw.to_string();
    } else if raw.eq_ignore_ascii_case("localhost") {
        address = "localhost".to_string();
    } else if raw.contains('.')
        && !raw.contains(' ')
        && raw
            .chars()
            .all(|c| c.is_alphanumeric() || c == '.' || c == '-' || c == '_')
    {
        address = raw.to_string();
    } else {
        return None;
    }

    if address.is_empty() {
        return None;
    }

    let label = if !username.is_empty() && port != 22 {
        format!("{username}@{address}:{port}")
    } else if !username.is_empty() {
        format!("{username}@{address}")
    } else if port != 22 {
        format!("{address}:{port}")
    } else {
        address.clone()
    };

    let mut host = Host::new(label, address);
    host.username = username;
    host.port = port;
    host.auth = AuthMethod::Agent;
    Some(host)
}

/// A surface `SSHDECK_START_PANE` can open at startup: a left-rail pane, or the
/// SFTP tab.
///
/// Development and test affordance only — it names an existing destination, so
/// it can select a pane and nothing else.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StartPane {
    Nav(LeftNav),
    Sftp,
}

/// The pane names `SSHDECK_START_PANE` accepts, in the order they are listed to
/// a user when one is not recognised.
const START_PANE_VALUES: &str =
    "hosts, keychain, forward/port-forwarding, snippets, known-hosts, logs, sftp";

/// Parses an `SSHDECK_START_PANE` value.
///
/// Case-insensitive, and `-`/`_`/spaces are tolerated so `port-forwarding`,
/// `port_forwarding` and `Port Forwarding` all mean the same pane. `None` means
/// the value is not a known pane; the caller warns and keeps the default rather
/// than refusing to start.
fn parse_start_pane(value: &str) -> Option<StartPane> {
    let normalized = value.trim().to_lowercase().replace(['-', '_', ' '], "");
    match normalized.as_str() {
        "hosts" => Some(StartPane::Nav(LeftNav::Hosts)),
        "keychain" => Some(StartPane::Nav(LeftNav::Keychain)),
        "forward" | "forwarding" | "portforward" | "portforwarding" => {
            Some(StartPane::Nav(LeftNav::PortForwarding))
        }
        "snippets" => Some(StartPane::Nav(LeftNav::Snippets)),
        "knownhosts" => Some(StartPane::Nav(LeftNav::KnownHosts)),
        "logs" => Some(StartPane::Nav(LeftNav::Logs)),
        "settings" => Some(StartPane::Nav(LeftNav::Settings)),
        "sftp" => Some(StartPane::Sftp),
        _ => None,
    }
}

/// Parses the optional port field of the add-host form.
///
/// An empty field means the SSH default (22). A non-numeric or out-of-range
/// value is a real error, so the caller can show a message instead of silently
/// defaulting to 22 — otherwise `host:2222` would quietly connect to 22.
fn parse_port(raw: &str) -> Result<u16, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(22);
    }
    match raw.parse::<u16>() {
        Ok(port) if port != 0 => Ok(port),
        _ => Err(format!(
            "Port must be a number between 1 and 65535, got \"{raw}\""
        )),
    }
}

/// The subtitle under a host card's label: protocols, then the login for each.
///
/// Tags live in the Host Details tags box, not here — appending them to the
/// subtitle duplicated the tag row and pushed real connection info out.
fn host_subtitle(host: &Host) -> String {
    let mut tokens: Vec<String> = host.protocols().to_vec();
    if tokens.is_empty() {
        tokens.push("ssh".to_string());
    }
    if !host.username.is_empty() {
        // One login per protocol: `ssh, telnet, root, root`.
        let logins = std::iter::repeat_n(host.username.clone(), tokens.len());
        tokens.extend(logins);
    }
    tokens.join(", ")
}

/// A Termius Host Details input box: h40, radius 8, `#d5dde0` border.
///
/// The caller drops a bare `Input` (with `appearance(false)`) or a static
/// row inside; the box carries the sizing, border, and padding.
fn details_box() -> gpui_kit::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .h(px(40.))
        .px_3()
        .w_full()
        .rounded(px(8.))
        .border_1()
        .border_color(rgb(0xd5dde0))
}

/// Vault grid columns for the available centre width, matching Termius's
/// 1/2/3/4-column breakpoints at 360/700/1200px (3 columns proven at ~1038px
/// available width in the 3.08.00 capture).
fn grid_columns(avail_w: f32) -> usize {
    if avail_w < 360.0 {
        1
    } else if avail_w < 700.0 {
        2
    } else if avail_w < 1200.0 {
        3
    } else {
        4
    }
}

/// Left-rail width for the window width: a 60px icon strip when manually
/// collapsed or the window is narrower than ~900px, otherwise the 185px rail.
fn rail_width(win_w: f32, collapsed: bool) -> f32 {
    if collapsed || win_w < 900.0 {
        60.0
    } else {
        185.0
    }
}

/// Whether the host-details drawer floats over the vault instead of docking
/// beside it: below ~800px there is no room for a docked drawer.
fn details_overlay(win_w: f32) -> bool {
    win_w < 800.0
}

/// Docked host-details drawer width: 300px at 800–1100px, 360px above 1100.
/// Below ~800px the caller floats it (see `details_overlay`) instead.
fn details_width(win_w: f32) -> f32 {
    if win_w < 1100.0 {
        300.0
    } else {
        360.0
    }
}

/// Max popover width: 320px capped at 90vw so popovers never overflow narrow
/// windows. Pure so the clamp has a test.
fn popover_max_w(win_w: f32) -> f32 {
    (win_w * 0.9).clamp(160.0, 320.0)
}

/// Max popover height: 70vh, always paired with `overflow_y_scrollbar` at the
/// call site. Pure so the factor has a test.
fn popover_max_h(win_h: f32) -> f32 {
    (win_h * 0.7).max(160.0)
}

/// Below ~600px width anchored popovers become full-width drawers instead of
/// floating cards. Pure so the breakpoint has a test.
fn use_full_drawer(win_w: f32) -> bool {
    win_w < 600.0
}

/// Host card height: list rows stay 48px; grid cards are 68px, 56px on narrow
/// windows. 44px inputs keep their touch target (see `sidebar_row`). Pure.
fn host_card_height(is_list: bool, win_w: f32) -> f32 {
    if is_list {
        48.0
    } else if use_full_drawer(win_w) {
        56.0
    } else {
        68.0
    }
}

/// The OS brand colour for a host icon, from the 25 values recovered in
/// `docs/UI-PARITY.md` ("Host OS brand colours").
///
/// `alpine` has no recovered value on purpose: a brand colour is a fact about
/// the platform, not something to invent, so it returns `None` and the caller
/// falls back to the muted foreground.
fn os_brand_color(name: &str) -> Option<Rgba> {
    let hex = match name.trim().to_lowercase().as_str() {
        "ubuntu" => 0xe95420,
        "debian" => 0xce0056,
        "arch" => 0x1793d1,
        "fedora" => 0x3c6eb4,
        "centos" => 0xefa720,
        "redhat" => 0xee0000,
        "rockylinux" => 0x34d399,
        "suse" => 0x30ba78,
        "mageia" => 0x2397d4,
        "gentoo" => 0x54487a,
        "freebsd" => 0xf60006,
        "openbsd" => 0xf2ca30,
        "netbsd" => 0xf26711,
        "routeros" => 0x164aaa,
        "linux" => 0xffcc33,
        "macos" => 0x49a3f2,
        "windows" => 0x00a1f1,
        "android" => 0x3ddc84,
        "apple" => 0x171719,
        "aws" => 0xff9900,
        "digitalocean" => 0x0080ff,
        "cisco" => 0x00bceb,
        "pi" | "raspbian" => 0xbe3956,
        "gloria" => 0x16b8f0,
        _ => return None,
    };
    Some(rgb(hex))
}

/// Tints a host icon with its platform's brand colour.
///
/// [`Host`] has no OS field yet (ROADMAP P5), so the platform name is read from
/// the host's group or tags if one is present. When nothing names a known
/// platform the icon keeps `fallback` (`#8d91a5`); the OS is never guessed.
fn host_os_tint(host: &Host, fallback: Hsla) -> Hsla {
    host.group
        .iter()
        .chain(host.tags.iter())
        .find_map(|name| os_brand_color(name).map(Hsla::from))
        .unwrap_or(fallback)
}

/// Runs one command over SSH with no window, no gpui, and no async runtime.
///
/// This is the same transport the terminal pane uses, but on the SSH `exec`
/// request instead of a PTY + login shell: the command is sent once, the
/// server's stdout and stderr stream back verbatim, and nothing is echoed, so
/// the bytes are exactly what the command wrote. `events()` is an
/// `async_channel::Receiver`; `recv_blocking` (verified on the pinned
/// async-channel 2.5.0) lets that stream be drained on the main thread.
///
/// Returns the process exit code: the remote exit status when the server sends
/// one (like `ssh host cmd`), otherwise 0 on a clean close, or 1 on a connect
/// failure or a [`SessionEvent::Error`].
fn run_headless(target: &str, command: &str) -> i32 {
    use std::io::Write as _;

    let mut store = HostStore::at_default_path();
    if let Err(error) = store.load() {
        eprintln!("sshdeck: could not load hosts: {error}");
        return 1;
    }
    let Some(host) = store
        .inventory()
        .hosts()
        .iter()
        .find(|host| host.id.as_str() == target || host.label.eq_ignore_ascii_case(target))
    else {
        eprintln!("sshdeck: no host matches {target}");
        return 1;
    };

    // See `connect`: `SSHDECK_PASSWORD` is a development and test affordance,
    // never logged, never written to disk, never shown in the UI. Without it a
    // password host fails `Session::connect` with a missing-secret error, which
    // is reported below.
    let mut config = SessionConfig::from_host(host);
    if matches!(&host.auth, sshdeck_core::AuthMethod::Password { .. }) {
        if let Some(password) = env_password() {
            config = config.with_password(password);
        }
    }

    // No PTY and no login shell: the command is the whole session, so it is
    // never echoed and stdout is not polluted by a prompt.
    let session = match SshSession::exec(config, command) {
        Ok(session) => session,
        Err(error) => {
            eprintln!("sshdeck: {error}");
            return 1;
        }
    };

    // One receiver for the whole session: `events()` clones, and the channel is
    // competing-consumer, so calling it per iteration would split the stream.
    let events = session.events();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut failed = false;
    let mut exit_code = 0;
    while let Ok(event) = events.recv_blocking() {
        match event {
            // Verbatim: no decoding and no line-splitting.
            SessionEvent::Data(bytes) => {
                let _ = out.write_all(&bytes);
                let _ = out.flush();
            }
            SessionEvent::Error(message) => {
                eprintln!("sshdeck: {message}");
                failed = true;
            }
            // The remote status is the command's own.
            SessionEvent::Closed(code) => exit_code = code.unwrap_or(0),
            // Other lifecycle events carry no bytes or status.
            _ => {}
        }
    }

    if failed {
        1
    } else {
        exit_code
    }
}

/// One open tab: the host it points at and the pane rendering it.
struct Session {
    host: HostId,
    pane: Entity<TerminalPane>,
    /// Mirrors the pane's state so the tab strip and status bar can render it
    /// without reaching into the pane every frame.
    status: PaneStatus,
}

/// Which surface the main region shows: the fixed tabs are Vaults and SFTP,
/// followed by one session tab per open terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MainTab {
    /// The host inventory (the sidebar surface, shown full-region).
    Vaults,
    /// The SFTP browser for the focused session.
    Sftp,
    /// The new tab screen (hosts picker and workspaces).
    NewTab,
    /// The tiled workspace: one or more session panes side by side.
    Workspace,
    /// The terminal for `sessions[index]`.
    Session(usize),
}

/// The placeholder tab to show after the session at `closed` is removed.
///
/// Pure so the tab bookkeeping can be checked without a window.
fn tab_after_close(tab: MainTab, closed: usize) -> MainTab {
    match tab {
        MainTab::Session(index) if index == closed => MainTab::Vaults,
        MainTab::Session(index) if index > closed => MainTab::Session(index - 1),
        other => other,
    }
}

/// The session right-sidebar tab.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SidebarTab {
    Snippets,
    History,
    Autocomplete,
    Appearance,
}

/// Cap for the session sidebar's suggestion history. Like the logs pane's ring
/// buffer, it evicts the oldest entry past the cap instead of growing.
const MAX_HISTORY_ENTRIES: usize = 200;

/// Pushes one suggestion onto the bounded sidebar history, evicting the oldest
/// past [`MAX_HISTORY_ENTRIES`]. Pure so the bound can be checked without a
/// window.
fn push_history(history: &mut Vec<String>, entry: String) {
    if history.len() >= MAX_HISTORY_ENTRIES {
        history.remove(0);
    }
    history.push(entry);
}

/// A workspace tiling: one session or a split of nested tilings.
///
/// A `Split` lays its children side by side (`Row`) or stacked (`Col`), with
/// `weights` as each child's `flex_grow` share: nesting mirrors the reference
/// (a column of two beside a full-height pane) and uneven weights widen one
/// tile (three columns at 1/1/2). New splits default to equal shares.
/// ponytail: no drag-resize yet — ceiling: a gutter handle writing user
/// shares back into `weights`.
#[derive(Clone, Debug, PartialEq)]
enum WorkspaceNode {
    /// No tiles yet; the workspace seeds it from the focused session.
    Empty,
    /// One tiled pane with one or more tabs and the active tab index.
    Pane { tabs: Vec<usize>, active: usize },
    /// A split of two or more nested tilings.
    Split {
        dir: SplitDir,
        weights: Vec<f32>,
        children: Vec<WorkspaceNode>,
    },
}

/// Split direction: `Row` lays children side by side, `Col` stacks them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SplitDir {
    Row,
    Col,
}

impl SplitDir {
    /// The perpendicular axis, so splitting a focused tile nests the new
    /// tile across its parent's direction.
    fn other(self) -> Self {
        match self {
            Self::Row => Self::Col,
            Self::Col => Self::Row,
        }
    }
}

/// Flattens the tiling into session indexes in tab order. Pure so the tree
/// bookkeeping has a test.
fn workspace_leaves(node: &WorkspaceNode) -> Vec<usize> {
    match node {
        WorkspaceNode::Empty => Vec::new(),
        WorkspaceNode::Pane { tabs, .. } => tabs.clone(),
        WorkspaceNode::Split { children, .. } => {
            children.iter().flat_map(workspace_leaves).collect()
        }
    }
}

/// Returns the total number of pane leaves in the split tree.
fn workspace_panes_count(node: &WorkspaceNode) -> usize {
    match node {
        WorkspaceNode::Empty => 0,
        WorkspaceNode::Pane { .. } => 1,
        WorkspaceNode::Split { children, .. } => children.iter().map(workspace_panes_count).sum(),
    }
}

/// Returns the active session in each pane leaf.
#[allow(dead_code)]
fn workspace_active_leaves(node: &WorkspaceNode) -> Vec<usize> {
    match node {
        WorkspaceNode::Empty => Vec::new(),
        WorkspaceNode::Pane { tabs, active } => {
            if let Some(&s) = tabs.get(*active) {
                vec![s]
            } else if let Some(&s) = tabs.first() {
                vec![s]
            } else {
                Vec::new()
            }
        }
        WorkspaceNode::Split { children, .. } => {
            children.iter().flat_map(workspace_active_leaves).collect()
        }
    }
}

/// Activates `target` session in whichever pane contains it.
/// Returns the updated tree and the pane index if found.
fn workspace_activate_session(
    node: &WorkspaceNode,
    target: usize,
) -> (WorkspaceNode, Option<usize>) {
    let mut pane_counter = 0;
    let mut found_pane = None;

    fn rec(
        node: &WorkspaceNode,
        target: usize,
        counter: &mut usize,
        found: &mut Option<usize>,
    ) -> WorkspaceNode {
        match node {
            WorkspaceNode::Empty => WorkspaceNode::Empty,
            WorkspaceNode::Pane { tabs, active } => {
                let current_pane = *counter;
                *counter += 1;
                if let Some(pos) = tabs.iter().position(|&s| s == target) {
                    *found = Some(current_pane);
                    WorkspaceNode::Pane {
                        tabs: tabs.clone(),
                        active: pos,
                    }
                } else {
                    WorkspaceNode::Pane {
                        tabs: tabs.clone(),
                        active: *active,
                    }
                }
            }
            WorkspaceNode::Split {
                dir,
                weights,
                children,
            } => {
                let next_children = children
                    .iter()
                    .map(|c| rec(c, target, counter, found))
                    .collect();
                WorkspaceNode::Split {
                    dir: *dir,
                    weights: weights.clone(),
                    children: next_children,
                }
            }
        }
    }

    let out = rec(node, target, &mut pane_counter, &mut found_pane);
    (out, found_pane)
}

/// Splits the focused pane or seeds the workspace with `index`.
/// If `index` is already tiled, its tab is activated.
fn workspace_insert(node: &WorkspaceNode, focus: usize, index: usize) -> (WorkspaceNode, usize) {
    let (activated, found) = workspace_activate_session(node, index);
    if let Some(p) = found {
        return (activated, p);
    }
    let panes = workspace_panes_count(node);
    if panes == 0 {
        return (
            WorkspaceNode::Pane {
                tabs: vec![index],
                active: 0,
            },
            0,
        );
    }
    let target = focus.min(panes.saturating_sub(1));
    let mut counter = 0;
    let mut new_focus = 0;

    fn split_at(
        node: &WorkspaceNode,
        target_pane: usize,
        new_session: usize,
        dir: SplitDir,
        counter: &mut usize,
        new_focus: &mut usize,
    ) -> WorkspaceNode {
        match node {
            WorkspaceNode::Empty => WorkspaceNode::Pane {
                tabs: vec![new_session],
                active: 0,
            },
            WorkspaceNode::Pane { tabs, active } => {
                let current = *counter;
                *counter += 1;
                if current == target_pane {
                    *new_focus = current + 1;
                    WorkspaceNode::Split {
                        dir,
                        weights: vec![1.0, 1.0],
                        children: vec![
                            WorkspaceNode::Pane {
                                tabs: tabs.clone(),
                                active: *active,
                            },
                            WorkspaceNode::Pane {
                                tabs: vec![new_session],
                                active: 0,
                            },
                        ],
                    }
                } else {
                    WorkspaceNode::Pane {
                        tabs: tabs.clone(),
                        active: *active,
                    }
                }
            }
            WorkspaceNode::Split {
                dir: parent_dir,
                weights,
                children,
            } => {
                let next_dir = parent_dir.other();
                let next_children = children
                    .iter()
                    .map(|c| split_at(c, target_pane, new_session, next_dir, counter, new_focus))
                    .collect();
                WorkspaceNode::Split {
                    dir: *parent_dir,
                    weights: weights.clone(),
                    children: next_children,
                }
            }
        }
    }

    let out = split_at(
        node,
        target,
        index,
        SplitDir::Row,
        &mut counter,
        &mut new_focus,
    );
    (out, new_focus)
}

/// Adds a session tab into a specific pane leaf.
#[allow(dead_code)]
fn workspace_add_tab_to_pane(
    node: &WorkspaceNode,
    target_pane: usize,
    session: usize,
) -> WorkspaceNode {
    let mut counter = 0;
    fn rec(
        node: &WorkspaceNode,
        target: usize,
        session: usize,
        counter: &mut usize,
    ) -> WorkspaceNode {
        match node {
            WorkspaceNode::Empty => WorkspaceNode::Pane {
                tabs: vec![session],
                active: 0,
            },
            WorkspaceNode::Pane { tabs, .. } => {
                let cur = *counter;
                *counter += 1;
                if cur == target {
                    let mut next_tabs = tabs.clone();
                    if !next_tabs.contains(&session) {
                        next_tabs.push(session);
                    }
                    let active = next_tabs.iter().position(|&s| s == session).unwrap_or(0);
                    WorkspaceNode::Pane {
                        tabs: next_tabs,
                        active,
                    }
                } else {
                    node.clone()
                }
            }
            WorkspaceNode::Split {
                dir,
                weights,
                children,
            } => {
                let next_ch = children
                    .iter()
                    .map(|c| rec(c, target, session, counter))
                    .collect();
                WorkspaceNode::Split {
                    dir: *dir,
                    weights: weights.clone(),
                    children: next_ch,
                }
            }
        }
    }
    rec(node, target_pane, session, &mut counter)
}

/// Switches active tab in a specific pane leaf.
fn workspace_switch_tab(
    node: &WorkspaceNode,
    target_pane: usize,
    tab_index: usize,
) -> WorkspaceNode {
    let mut counter = 0;
    fn rec(node: &WorkspaceNode, target: usize, tab: usize, counter: &mut usize) -> WorkspaceNode {
        match node {
            WorkspaceNode::Empty => WorkspaceNode::Empty,
            WorkspaceNode::Pane { tabs, .. } => {
                let cur = *counter;
                *counter += 1;
                if cur == target {
                    let active = tab.min(tabs.len().saturating_sub(1));
                    WorkspaceNode::Pane {
                        tabs: tabs.clone(),
                        active,
                    }
                } else {
                    node.clone()
                }
            }
            WorkspaceNode::Split {
                dir,
                weights,
                children,
            } => {
                let next_ch = children
                    .iter()
                    .map(|c| rec(c, target, tab, counter))
                    .collect();
                WorkspaceNode::Split {
                    dir: *dir,
                    weights: weights.clone(),
                    children: next_ch,
                }
            }
        }
    }
    rec(node, target_pane, tab_index, &mut counter)
}

/// Moves a tab from `(from_pane, from_tab)` to `(to_pane, to_pos)`.
fn workspace_move_tab(
    node: &WorkspaceNode,
    from_pane: usize,
    from_tab: usize,
    to_pane: usize,
    to_pos: usize,
) -> WorkspaceNode {
    let mut counter = 0;
    let mut extracted_session = None;
    fn extract(
        node: &WorkspaceNode,
        target_pane: usize,
        tab_idx: usize,
        counter: &mut usize,
        extracted: &mut Option<usize>,
    ) -> Option<WorkspaceNode> {
        match node {
            WorkspaceNode::Empty => None,
            WorkspaceNode::Pane { tabs, active } => {
                let cur = *counter;
                *counter += 1;
                if cur == target_pane {
                    let mut next_tabs = tabs.clone();
                    if tab_idx < next_tabs.len() {
                        let sess = next_tabs.remove(tab_idx);
                        *extracted = Some(sess);
                    }
                    if next_tabs.is_empty() {
                        None
                    } else {
                        let next_active = (*active).min(next_tabs.len() - 1);
                        Some(WorkspaceNode::Pane {
                            tabs: next_tabs,
                            active: next_active,
                        })
                    }
                } else {
                    Some(node.clone())
                }
            }
            WorkspaceNode::Split { dir, children, .. } => {
                let kept: Vec<WorkspaceNode> = children
                    .iter()
                    .filter_map(|c| extract(c, target_pane, tab_idx, counter, extracted))
                    .collect();
                match kept.len() {
                    0 => None,
                    1 => kept.into_iter().next(),
                    _ => Some(WorkspaceNode::Split {
                        dir: *dir,
                        weights: vec![1.0; kept.len()],
                        children: kept,
                    }),
                }
            }
        }
    }

    let without_tab = extract(
        node,
        from_pane,
        from_tab,
        &mut counter,
        &mut extracted_session,
    )
    .unwrap_or(WorkspaceNode::Empty);
    let Some(session) = extracted_session else {
        return node.clone();
    };

    let mut insert_counter = 0;
    fn insert(
        node: &WorkspaceNode,
        target_pane: usize,
        pos: usize,
        session: usize,
        counter: &mut usize,
    ) -> WorkspaceNode {
        match node {
            WorkspaceNode::Empty => WorkspaceNode::Pane {
                tabs: vec![session],
                active: 0,
            },
            WorkspaceNode::Pane { tabs, .. } => {
                let cur = *counter;
                *counter += 1;
                if cur == target_pane {
                    let mut next_tabs = tabs.clone();
                    let clamped = pos.min(next_tabs.len());
                    next_tabs.insert(clamped, session);
                    WorkspaceNode::Pane {
                        tabs: next_tabs,
                        active: clamped,
                    }
                } else {
                    node.clone()
                }
            }
            WorkspaceNode::Split {
                dir,
                weights,
                children,
            } => {
                let next_ch = children
                    .iter()
                    .map(|c| insert(c, target_pane, pos, session, counter))
                    .collect();
                WorkspaceNode::Split {
                    dir: *dir,
                    weights: weights.clone(),
                    children: next_ch,
                }
            }
        }
    }

    insert(&without_tab, to_pane, to_pos, session, &mut insert_counter)
}

/// Inserts a session as a new tab into `target_pane`.
fn workspace_insert_tab_into_pane(
    node: &WorkspaceNode,
    target_pane: usize,
    session: usize,
) -> WorkspaceNode {
    let clean = remove_session_no_renumber(node, session).unwrap_or(WorkspaceNode::Empty);
    if clean == WorkspaceNode::Empty {
        return WorkspaceNode::Pane {
            tabs: vec![session],
            active: 0,
        };
    }
    let mut counter = 0;
    fn rec(node: &WorkspaceNode, target: usize, sess: usize, counter: &mut usize) -> WorkspaceNode {
        match node {
            WorkspaceNode::Empty => WorkspaceNode::Pane {
                tabs: vec![sess],
                active: 0,
            },
            WorkspaceNode::Pane { tabs, .. } => {
                let cur = *counter;
                *counter += 1;
                if cur == target {
                    let mut next_tabs = tabs.clone();
                    next_tabs.push(sess);
                    let active_idx = next_tabs.len() - 1;
                    WorkspaceNode::Pane {
                        tabs: next_tabs,
                        active: active_idx,
                    }
                } else {
                    node.clone()
                }
            }
            WorkspaceNode::Split {
                dir,
                weights,
                children,
            } => {
                let next_ch = children
                    .iter()
                    .map(|c| rec(c, target, sess, counter))
                    .collect();
                WorkspaceNode::Split {
                    dir: *dir,
                    weights: weights.clone(),
                    children: next_ch,
                }
            }
        }
    }
    rec(&clean, target_pane, session, &mut counter)
}

/// Splits `target_pane` with `session` across `dir` (before or after).
fn workspace_split_pane_with_session(
    node: &WorkspaceNode,
    target_pane: usize,
    session: usize,
    dir: SplitDir,
    place_after: bool,
) -> WorkspaceNode {
    let clean = remove_session_no_renumber(node, session).unwrap_or(WorkspaceNode::Empty);
    if clean == WorkspaceNode::Empty {
        return WorkspaceNode::Pane {
            tabs: vec![session],
            active: 0,
        };
    }

    let mut counter = 0;
    fn split_pane(
        node: &WorkspaceNode,
        target: usize,
        sess: usize,
        dir: SplitDir,
        place_after: bool,
        counter: &mut usize,
    ) -> WorkspaceNode {
        match node {
            WorkspaceNode::Empty => WorkspaceNode::Pane {
                tabs: vec![sess],
                active: 0,
            },
            WorkspaceNode::Pane { tabs, active } => {
                let cur = *counter;
                *counter += 1;
                if cur == target {
                    let old_pane = WorkspaceNode::Pane {
                        tabs: tabs.clone(),
                        active: *active,
                    };
                    let new_pane = WorkspaceNode::Pane {
                        tabs: vec![sess],
                        active: 0,
                    };
                    let children = if place_after {
                        vec![old_pane, new_pane]
                    } else {
                        vec![new_pane, old_pane]
                    };
                    WorkspaceNode::Split {
                        dir,
                        weights: vec![1.0, 1.0],
                        children,
                    }
                } else {
                    node.clone()
                }
            }
            WorkspaceNode::Split {
                dir: parent_dir,
                weights,
                children,
            } => {
                let next_ch = children
                    .iter()
                    .map(|c| split_pane(c, target, sess, dir, place_after, counter))
                    .collect();
                WorkspaceNode::Split {
                    dir: *parent_dir,
                    weights: weights.clone(),
                    children: next_ch,
                }
            }
        }
    }

    split_pane(&clean, target_pane, session, dir, place_after, &mut counter)
}

/// Removes a session without renumbering other sessions (used for drag & drop).
fn remove_session_no_renumber(node: &WorkspaceNode, target: usize) -> Option<WorkspaceNode> {
    match node {
        WorkspaceNode::Empty => None,
        WorkspaceNode::Pane { tabs, active } => {
            let next_tabs: Vec<usize> = tabs.iter().copied().filter(|&s| s != target).collect();
            if next_tabs.is_empty() {
                None
            } else {
                let next_active = (*active).min(next_tabs.len() - 1);
                Some(WorkspaceNode::Pane {
                    tabs: next_tabs,
                    active: next_active,
                })
            }
        }
        WorkspaceNode::Split { dir, children, .. } => {
            let kept: Vec<WorkspaceNode> = children
                .iter()
                .filter_map(|c| remove_session_no_renumber(c, target))
                .collect();
            match kept.len() {
                0 => None,
                1 => kept.into_iter().next(),
                _ => Some(WorkspaceNode::Split {
                    dir: *dir,
                    weights: vec![1.0; kept.len()],
                    children: kept,
                }),
            }
        }
    }
}

/// Repairs the tiling after session `closed` is removed:
/// closed tab is dropped, later indexes shift down, empty panes pruned,
/// single-child splits collapse.
fn workspace_remove(node: &WorkspaceNode, focus: usize, closed: usize) -> (WorkspaceNode, usize) {
    let out = remove_session(node, closed).unwrap_or(WorkspaceNode::Empty);
    let panes = workspace_panes_count(&out);
    (out, focus.min(panes.saturating_sub(1)))
}

fn remove_session(node: &WorkspaceNode, closed: usize) -> Option<WorkspaceNode> {
    match node {
        WorkspaceNode::Empty => None,
        WorkspaceNode::Pane { tabs, active } => {
            let mut next_tabs: Vec<usize> = Vec::new();
            for &s in tabs {
                if s == closed {
                    continue;
                }
                if s > closed {
                    next_tabs.push(s - 1);
                } else {
                    next_tabs.push(s);
                }
            }
            if next_tabs.is_empty() {
                None
            } else {
                let next_active = (*active).min(next_tabs.len() - 1);
                Some(WorkspaceNode::Pane {
                    tabs: next_tabs,
                    active: next_active,
                })
            }
        }
        WorkspaceNode::Split { dir, children, .. } => {
            let kept: Vec<WorkspaceNode> = children
                .iter()
                .filter_map(|child| remove_session(child, closed))
                .collect();
            match kept.len() {
                0 => None,
                1 => kept.into_iter().next(),
                _ => Some(WorkspaceNode::Split {
                    dir: *dir,
                    weights: vec![1.0; kept.len()],
                    children: kept,
                }),
            }
        }
    }
}

/// Settles `weights` into one positive share per child: a short table pads
/// with equal shares and any zero, negative, or non-finite entry falls back
/// to all-equal. Pure so the ratio math has a test.
fn normalize_weights(weights: &[f32], len: usize) -> Vec<f32> {
    let mut out: Vec<f32> = weights.iter().take(len).copied().collect();
    out.resize(len, 1.0);
    if out.iter().all(|weight| *weight > 0.0 && weight.is_finite()) {
        out
    } else {
        vec![1.0; len]
    }
}

/// Below this window width the workspace stacks tiles vertically instead of
/// side by side: two 80-column panes cannot fit, so a row would squeeze each
/// canvas below a usable grid.
const WORKSPACE_STACK_WIDTH: f32 = 700.0;

/// Below this window width the 300pt session sidebar becomes an overlay drawer
/// instead of an inset panel: at 300pt plus a usable terminal the content
/// would squeeze below a usable grid.
const SIDEBAR_OVERLAY_WIDTH: f32 = 900.0;

/// Left navigation rail entries — mirrors Termius sidebar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LeftNav {
    Hosts,
    Keychain,
    PortForwarding,
    Snippets,
    KnownHosts,
    Logs,
    Settings,
}

/// Sort mode for the hosts view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HostSort {
    #[default]
    LabelAsc,
    LabelDesc,
    Address,
    Recent,
}

impl HostSort {
    pub fn next(self) -> Self {
        match self {
            Self::LabelAsc => Self::LabelDesc,
            Self::LabelDesc => Self::Address,
            Self::Address => Self::Recent,
            Self::Recent => Self::LabelAsc,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::LabelAsc => "Sorted: Name (A-Z)",
            Self::LabelDesc => "Sorted: Name (Z-A)",
            Self::Address => "Sorted: Address",
            Self::Recent => "Sorted: Default order",
        }
    }
}

/// A full-region pane that shows over the session content while it is open.
///
/// Each variant is created on demand and dropped when it closes, so a pane that
/// holds a transport never outlives its own visibility.
enum Overlay {
    Settings(Entity<SettingsView>),
    #[allow(dead_code)]
    Keys(Entity<KeysPane>),
}

/// How host inventory is presented: 2/3/4-col card grid or compact rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewMode {
    Grid,
    List,
}

struct SshDeck {
    store: HostStore,
    selected: Option<HostId>,
    /// Which main-region tab is showing.
    tab: MainTab,
    /// Left rail selection — drives the content region when no overlay covers it.
    left_nav: LeftNav,
    /// The focused session, if any: the SFTP tab and the Reconnect action follow
    /// it even while another tab is showing.
    active: Option<usize>,
    sessions: Vec<Session>,
    filter: Entity<InputState>,
    draft_label: Entity<InputState>,
    draft_address: Entity<InputState>,
    draft_username: Entity<InputState>,
    draft_port: Entity<InputState>,
    /// The in-flight secret prompt, when a host needs a password we do not have.
    secret: Option<(HostId, Entity<InputState>)>,
    /// Encrypted password store (OS-keychain master key). `None` when the
    /// vault could not be opened — password hosts then fall back to the
    /// per-connection prompt instead of persisting anything.
    vault: Option<Vault>,
    /// The command palette while it is open, if at all.
    palette: Option<Entity<PaletteView>>,
    /// The settings or keys pane showing over the session, if at all.
    overlay: Option<Overlay>,
    /// The SFTP pane, created when the SFTP tab is first selected so a transport
    /// is not opened until then.
    sftp_pane: Option<Entity<SftpPane>>,
    /// Lazily created panes for the left nav — each is built once and cached so
    /// in-progress edits survive switching away and back.
    keys_pane: Option<Entity<KeysPane>>,
    forward_pane: Option<Entity<ForwardPane>>,
    snippets_pane: Option<Entity<SnippetsPane>>,
    logs_pane: Option<Entity<LogsPane>>,
    /// Whether the host sidebar is collapsed to its narrow rail.
    sidebar_collapsed: bool,
    /// Host list view mode: responsive card grid or compact list rows.
    view_mode: ViewMode,
    /// Host sorting mode.
    host_sort: HostSort,
    /// Last connected host id for session restoration.
    last_session_host: Option<HostId>,
    /// Whether the add-host sheet is open. Host creation lives behind the
    /// header's `+` control rather than an always-visible form.
    add_host_open: bool,
    /// Index of the session the SFTP pane is attached to, if any.
    sftp_attached: Option<usize>,
    /// Bumped per attach attempt so a superseded SFTP connect is ignored.
    sftp_generation: u64,
    /// Whether the right host details panel is open.
    details_open: bool,
    /// Overflow menu inside host details.
    details_menu_open: bool,
    /// Theme picker view inside host details.
    theme_picker_open: bool,
    /// Credentials popover inside host details (+ SSH ID, Key, etc.).
    credentials_popover_open: bool,
    /// Whether "Show more" collapsible is expanded in host details.
    show_more: bool,
    /// Label/address editors for the Host Details panel, synced to the selected
    /// host by [`SshDeck::sync_details_inputs`] (real `Input`s, not boxes).
    details_label: Entity<InputState>,
    details_address: Entity<InputState>,
    details_port: Entity<InputState>,
    details_username: Entity<InputState>,
    details_password: Entity<InputState>,
    details_group: Entity<InputState>,
    details_tags: Entity<InputState>,
    /// Which host the details inputs are synced to; `None` means they are stale
    /// and must be re-synced before the panel renders them.
    details_edit_host: Option<HostId>,
    /// Whether the personal vault info dialog is open.
    vault_info_open: bool,
    /// Search input for the New Tab screen.
    new_tab_query: Entity<InputState>,
    /// Selected terminal color theme.
    selected_theme: String,
    /// Terminal font size in pixels. New panes are constructed with it and the
    /// stepper applies it live to existing panes; only the scrollback cap
    /// still needs a pane rebuild (it is fixed when the grid is created).
    terminal_font_size: f32,
    /// Session right-sidebar tab and visibility. Open by default next to the
    /// terminal, mirroring the reference workspace layout.
    sidebar_tab: SidebarTab,
    sidebar_open: bool,
    /// Workspace tiling: a recursive split tree of session indexes, with the
    /// focused tile as a flattened leaf position.
    workspace: WorkspaceNode,
    workspace_focus: usize,
    /// Toolbar direction override for the root split: `None` follows the
    /// tree's own directions, `Some` forces one axis. Narrow windows stack
    /// vertically regardless.
    workspace_direction: Option<SplitDir>,
    /// When true the workspace shows only the focused tile.
    workspace_maximized: bool,
    /// When true, input in any workspace pane or snippet execution broadcasts to all open panes.
    broadcast_mode: bool,
    /// Dragged tab state `(from_pane_leaf_index, from_tab_index)` when moving/splitting.
    dragged_tab: Option<(usize, usize)>,
    /// Session index currently being dragged (from top tab bar or pane).
    dragged_session: Option<usize>,
    /// Initial mouse down position on a tab to detect drag threshold.
    tab_drag_start: Option<(usize, Point<Pixels>)>,
    /// Sidebar suggestion history (bounded by [`MAX_HISTORY_ENTRIES`]) and its
    /// inline form: three inputs plus a visibility flag.
    history: Vec<String>,
    history_form_open: bool,
    autocomplete_enabled: bool,
    hist_who: Entity<InputState>,
    hist_where: Entity<InputState>,
    hist_what: Entity<InputState>,
    /// Subscription handles must outlive construction, so they are owned here.
    _subscriptions: Vec<Subscription>,
}

/// Stable vault id for a host's login password: `login:{host-id}`.
///
/// The inventory only ever stores this id. The secret itself lives in the
/// encrypted vault, never in `hosts.json` in plaintext.
fn password_secret_id(host_id: &HostId) -> String {
    format!("login:{}", host_id.as_str())
}

/// Resolves a vault reference to its secret. Vault errors and missing entries
/// both mean "no secret" — the caller falls back to the prompt, never to a
/// guess.
fn resolve_password(vault: Option<&Vault>, secret_ref: &str) -> Option<String> {
    vault
        .and_then(|vault| vault.get(secret_ref).ok())
        .flatten()
        .map(|secret| secret.to_string())
}

/// One-time rescue for inventories written when the details field stored its
/// text as the reference: any `Password` reference the vault cannot resolve
/// (and which is not already a stable id) is treated as the legacy secret,
/// re-stored under the stable id, and the host is pointed at it. Returns the
/// number of hosts migrated.
fn migrate_legacy_password_refs(store: &mut HostStore, vault: &mut Vault) -> usize {
    let mut migrated = 0;
    for mut host in store.inventory().hosts().to_vec() {
        let sshdeck_core::AuthMethod::Password { secret_ref } = &host.auth else {
            continue;
        };
        let stable = password_secret_id(&host.id);
        if secret_ref == &stable {
            continue;
        }
        if vault.get(secret_ref).ok().flatten().is_some() {
            continue;
        }
        if secret_ref.is_empty() {
            continue;
        }
        let legacy = secret_ref.clone();
        if vault.set(stable.clone(), &legacy).is_err() {
            continue;
        }
        host.auth = sshdeck_core::AuthMethod::Password { secret_ref: stable };
        store.inventory_mut().upsert(host);
        migrated += 1;
    }
    if migrated > 0 {
        let _ = vault.save();
        let _ = store.save();
    }
    migrated
}

fn current_time_str() -> String {
    use std::time::SystemTime;
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let s = secs % 60;
    let m = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    format!("{h:02}:{m:02}:{s:02}")
}

fn current_user_initials() -> String {
    if let Ok(user) = std::env::var("USER") {
        let parts: Vec<&str> = user.split('.').collect();
        if parts.len() >= 2 {
            let first = parts[0].chars().next().unwrap_or('U').to_ascii_uppercase();
            let second = parts[1].chars().next().unwrap_or('S').to_ascii_uppercase();
            return format!("{first}{second}");
        }
        let mut chars = user.chars();
        if let Some(c) = chars.next() {
            let second = chars.next().unwrap_or(' ').to_ascii_uppercase();
            return format!(
                "{}{}",
                c.to_ascii_uppercase(),
                if second != ' ' {
                    second.to_string()
                } else {
                    String::new()
                }
            );
        }
    }
    "U".to_string()
}

impl SshDeck {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut store = HostStore::at_default_path();
        // A store that cannot be read is empty, not fatal: the UI surfaces the
        // problem on the next save rather than refusing to start.
        let load_error = store.load().err();

        // The vault owns every login secret; the inventory keeps only opaque
        // ids. Legacy plaintext references are rescued into it on startup so
        // `hosts.json` ends up secret-free after one launch.
        let mut vault_error = None;
        let vault = match Vault::open_default() {
            Ok(mut vault) => {
                migrate_legacy_password_refs(&mut store, &mut vault);
                Some(vault)
            }
            Err(error) => {
                vault_error = Some(format!("Password vault unavailable: {error}"));
                None
            }
        };

        let filter = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Find a host or ssh user@hostname...")
        });
        let draft_label = cx.new(|cx| InputState::new(window, cx).placeholder("Label"));
        let draft_address = cx.new(|cx| InputState::new(window, cx).placeholder("hostname or IP"));
        let draft_username =
            cx.new(|cx| InputState::new(window, cx).placeholder("username (optional)"));
        let draft_port = cx.new(|cx| InputState::new(window, cx).placeholder("port (default 22)"));
        let new_tab_query =
            cx.new(|cx| InputState::new(window, cx).placeholder("Search hosts or tabs"));
        let hist_who = cx.new(|cx| InputState::new(window, cx).placeholder("User"));
        let hist_where = cx.new(|cx| InputState::new(window, cx).placeholder("Host"));
        let hist_what = cx.new(|cx| InputState::new(window, cx).placeholder("Suggestion..."));
        let details_label = cx.new(|cx| InputState::new(window, cx).placeholder("Label"));
        let details_address =
            cx.new(|cx| InputState::new(window, cx).placeholder("hostname or IP"));
        let details_port = cx.new(|cx| InputState::new(window, cx).placeholder("22"));
        let details_username = cx.new(|cx| InputState::new(window, cx).placeholder("Username"));
        let details_password = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Password")
                .masked(true)
        });
        let details_group = cx.new(|cx| InputState::new(window, cx).placeholder("Parent Group"));
        let details_tags = cx.new(|cx| InputState::new(window, cx).placeholder("Tags"));

        // Re-render the list as the query changes; the filter itself is applied
        // in `render`, so no filtered copy needs to be kept in state.
        let subscriptions = vec![
            cx.subscribe_in(&filter, window, |this, _, event, window, cx| match event {
                InputEvent::Change => cx.notify(),
                InputEvent::PressEnter { .. } => {
                    let filter_val = this.filter.read(cx).value().trim().to_string();
                    if let Some(host) = parse_quick_connect(&filter_val) {
                        this.connect(host, window, cx);
                    }
                }
                _ => {}
            }),
            cx.subscribe_in(
                &new_tab_query,
                window,
                |this, _, event, window, cx| match event {
                    InputEvent::Change => cx.notify(),
                    InputEvent::PressEnter { .. } => {
                        let query = this.new_tab_query.read(cx).value().trim().to_string();
                        if !query.is_empty() {
                            if let Some(host) = parse_quick_connect(&query) {
                                this.connect(host, window, cx);
                            } else {
                                let filtered = this.store.inventory().filtered(&query);
                                if let Some(first) = filtered.first() {
                                    this.connect((*first).clone(), window, cx);
                                }
                            }
                        }
                    }
                    _ => {}
                },
            ),
            // Details editors commit each keystroke to the selected host, so the
            // card label follows the edit live. The store save is the same
            // write-every-mutation rule the rest of the view follows.
            cx.subscribe_in(&details_label, window, |this, _, event, window, cx| {
                if matches!(event, InputEvent::Change) {
                    this.commit_details_label(window, cx);
                }
            }),
            cx.subscribe_in(&details_address, window, |this, _, event, window, cx| {
                if matches!(event, InputEvent::Change) {
                    this.commit_details_address(window, cx);
                }
            }),
            cx.subscribe_in(&details_port, window, |this, _, event, window, cx| {
                if matches!(event, InputEvent::Change) {
                    this.commit_details_port(window, cx);
                }
            }),
            cx.subscribe_in(&details_username, window, |this, _, event, window, cx| {
                if matches!(event, InputEvent::Change) {
                    this.commit_details_username(window, cx);
                }
            }),
            cx.subscribe_in(&details_password, window, |this, _, event, window, cx| {
                if matches!(event, InputEvent::Change) {
                    this.commit_details_password(window, cx);
                }
            }),
            cx.subscribe_in(&details_group, window, |this, _, event, window, cx| {
                if matches!(event, InputEvent::Change) {
                    this.commit_details_group(window, cx);
                }
            }),
            cx.subscribe_in(&details_tags, window, |this, _, event, window, cx| {
                if matches!(event, InputEvent::Change) {
                    this.commit_details_tags(window, cx);
                }
            }),
        ];

        if let Some(error) = load_error {
            let message = SharedString::from(format!("Could not load hosts: {error}"));
            cx.defer_in(window, move |_, window, cx| {
                window.push_notification(Notification::error(message), cx);
            });
        }
        if let Some(error) = vault_error {
            let message = SharedString::from(error);
            cx.defer_in(window, move |_, window, cx| {
                window.push_notification(Notification::error(message), cx);
            });
        }

        let initial_selected = store.inventory().hosts().first().map(|h| h.id.clone());

        let view = Self {
            store,
            vault,
            selected: initial_selected.clone(),
            tab: MainTab::Vaults,
            left_nav: LeftNav::Hosts,
            active: None,
            sessions: Vec::new(),
            filter,
            draft_label,
            draft_address,
            draft_username,
            draft_port,
            secret: None,
            palette: None,
            overlay: None,
            sftp_pane: None,
            keys_pane: None,
            forward_pane: None,
            snippets_pane: None,
            logs_pane: None,
            sidebar_collapsed: false,
            view_mode: ViewMode::Grid,
            host_sort: HostSort::default(),
            last_session_host: None,
            add_host_open: false,
            sftp_attached: None,
            sftp_generation: 0,
            details_open: true,
            details_menu_open: false,
            theme_picker_open: false,
            credentials_popover_open: false,
            show_more: false,
            details_label,
            details_address,
            details_port,
            details_username,
            details_password,
            details_group,
            details_tags,
            details_edit_host: None,
            vault_info_open: false,
            new_tab_query,
            selected_theme: "Termius Dark".to_string(),
            terminal_font_size: 14.0,
            sidebar_tab: SidebarTab::Snippets,
            sidebar_open: true,
            workspace: WorkspaceNode::Empty,
            workspace_focus: 0,
            workspace_direction: None,
            workspace_maximized: false,
            broadcast_mode: false,
            dragged_tab: None,
            dragged_session: None,
            tab_drag_start: None,
            history: crate::snippets_pane::read_recent_shell_history(),
            history_form_open: false,
            autocomplete_enabled: sshdeck_config::Settings::load().autocomplete_enabled(),
            hist_who,
            hist_where,
            hist_what,
            _subscriptions: subscriptions,
        };

        if initial_selected.is_some() {
            cx.defer_in(window, move |this, window, cx| {
                if this.selected.is_some() {
                    this.details_open = true;
                    this.sync_details_inputs(window, cx);
                    cx.notify();
                }
            });
        }

        // Development and test affordance, not a user feature: with
        // `SSHDECK_AUTOCONNECT` set to a comma-separated list of host ids or
        // labels, connect one session per entry as soon as the view exists so a
        // multi-session footprint can be measured without a human. The original
        // single-host form (one id or label, no comma) keeps working. A missing
        // host is a notification, never a startup failure.
        if let Ok(targets) = std::env::var("SSHDECK_AUTOCONNECT") {
            cx.defer_in(window, move |this, window, cx| {
                for target in autoconnect_targets(&targets) {
                    this.auto_connect(target, window, cx);
                }
                if std::env::var("SSHDECK_START_TILED").is_ok() && this.sessions.len() >= 2 {
                    this.tile_sessions_side_by_side(1, window, cx);
                } else if std::env::var("SSHDECK_DRAG_OVERLAY").is_ok() && this.sessions.len() >= 2
                {
                    this.dragged_session = Some(1);
                    cx.notify();
                }
            });
        }

        // Development and test affordance, not a user feature: with
        // `SSHDECK_START_PANE` set to a pane name, open that pane as soon as the
        // view exists, exactly as its rail button or tab does, so each pane can
        // be inspected without a click. It selects a pane and nothing else. An
        // unknown name warns and leaves the default (Hosts) showing; startup
        // never fails.
        if let Ok(value) = std::env::var("SSHDECK_START_PANE") {
            cx.defer_in(window, move |this, window, cx| {
                match parse_start_pane(&value) {
                    Some(StartPane::Nav(nav)) => this.select_left_nav(nav, window, cx),
                    Some(StartPane::Sftp) => this.select_tab(MainTab::Sftp, window, cx),
                    None => window.push_notification(
                        Notification::warning(format!(
                        "SSHDECK_START_PANE: unknown pane \"{value}\"; expected {START_PANE_VALUES}"
                    )),
                        cx,
                    ),
                }
            });
        }
        view
    }

    fn handle_open_palette(
        &mut self,
        _: &OpenPalette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_palette(window, cx);
    }

    fn handle_find_in_terminal(
        &mut self,
        _: &FindInTerminal,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(active) = self.active {
            if let Some(session) = self.sessions.get(active) {
                session.pane.update(cx, |pane, cx| pane.open_search(cx));
            }
        }
    }

    fn handle_new_tab(&mut self, _: &NewTabAction, window: &mut Window, cx: &mut Context<Self>) {
        self.tab = MainTab::NewTab;
        self.new_tab_query.update(cx, |input, cx| {
            input.focus(window, cx);
        });
        cx.notify();
    }

    fn handle_close_tab(
        &mut self,
        _: &CloseTabAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.overlay.is_some() {
            self.close_overlay(window, cx);
            return;
        }
        if self.palette.is_some() {
            self.palette = None;
            cx.notify();
            return;
        }
        if self.add_host_open {
            self.close_add_host(cx);
            return;
        }
        if self.tab == MainTab::NewTab {
            let fallback = self.active.map(MainTab::Session).unwrap_or(MainTab::Vaults);
            self.select_tab(fallback, window, cx);
            return;
        }
        if let Some(active) = self.active {
            if active < self.sessions.len() {
                self.close_session(active, window, cx);
            }
        }
    }

    fn handle_toggle_sidebar(
        &mut self,
        _: &ToggleSidebarAction,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if matches!(self.tab, MainTab::Session(_) | MainTab::Workspace) {
            self.sidebar_open = !self.sidebar_open;
            cx.notify();
        }
    }

    fn handle_open_settings(
        &mut self,
        _: &OpenSettingsAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_settings(window, cx);
    }

    fn handle_zoom_in(&mut self, _: &ZoomInAction, window: &mut Window, cx: &mut Context<Self>) {
        self.set_terminal_font_size(self.terminal_font_size + 1.0, window, cx);
        cx.notify();
    }

    fn handle_zoom_out(&mut self, _: &ZoomOutAction, window: &mut Window, cx: &mut Context<Self>) {
        self.set_terminal_font_size(self.terminal_font_size - 1.0, window, cx);
        cx.notify();
    }

    fn handle_reset_zoom(
        &mut self,
        _: &ResetZoomAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_terminal_font_size(14.0, window, cx);
        cx.notify();
    }

    /// Connects to one `SSHDECK_AUTOCONNECT` target by id or label, if it exists.
    ///
    /// The env var may hold a comma-separated list; the caller splits it and
    /// invokes this once per entry. Reuses the normal [`Self::connect`] path, so
    /// password handling and the tab/pane setup are identical to a human
    /// double-click. A target with no host is a notification; startup continues
    /// regardless.
    fn auto_connect(&mut self, target: &str, window: &mut Window, cx: &mut Context<Self>) {
        let host = self
            .store
            .inventory()
            .hosts()
            .iter()
            .find(|host| host.id.as_str() == target || host.label.eq_ignore_ascii_case(target))
            .cloned();
        match host {
            Some(host) => self.connect(host, window, cx),
            None => window.push_notification(
                Notification::warning(format!("No host matches {target}")),
                cx,
            ),
        }
    }

    /// Adds the draft host to the inventory and selects it.
    ///
    /// Username and port are both optional in the form, but a port that is
    /// present must parse: a non-numeric or out-of-range value is a message,
    /// never a silent fall back to 22 (which would misroute the connection).
    /// Returns whether a host was added, so the sheet only closes on success.
    fn add_draft_host(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let label = self.draft_label.read(cx).value().trim().to_string();
        let address = self.draft_address.read(cx).value().trim().to_string();
        let username = self.draft_username.read(cx).value().trim().to_string();
        let port_raw = self.draft_port.read(cx).value().to_string();
        let port = match parse_port(&port_raw) {
            Ok(port) => port,
            Err(message) => {
                window.push_notification(Notification::warning(message), cx);
                return false;
            }
        };

        if label.is_empty() || address.is_empty() {
            window.push_notification(
                Notification::warning("A label and an address are both required"),
                cx,
            );
            return false;
        }

        let mut host = Host::new(&label, &address);
        host.username = username;
        host.port = port;
        let id = self.store.inventory_mut().insert(host);
        self.selected = Some(id);
        self.sync_details_inputs(window, cx);

        if let Err(error) = self.store.save() {
            window.push_notification(Notification::error(format!("Could not save: {error}")), cx);
        }

        self.draft_label
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.draft_address
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.draft_username
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.draft_port
            .update(cx, |state, cx| state.set_value("", window, cx));
        cx.notify();
        true
    }

    fn remove_host(&mut self, id: &HostId, window: &mut Window, cx: &mut Context<Self>) {
        self.store.inventory_mut().remove(id);
        if self.selected.as_ref() == Some(id) {
            self.selected = None;
        }
        self.details_edit_host = None;
        if let Err(error) = self.store.save() {
            window.push_notification(Notification::error(format!("Could not save: {error}")), cx);
        }
        cx.notify();
    }

    /// Copies the selected host's label/address into the Host Details inputs.
    ///
    /// Called wherever the selection is assigned (card click, add, duplicate),
    /// because the inputs outlive any one host. The edit-host marker is set
    /// **before** the values so a `Change` event emitted by `set_value` commits
    /// back to the newly selected host with identical values — idempotent.
    fn sync_details_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.selected.clone() else {
            self.details_edit_host = None;
            return;
        };
        if self.details_edit_host.as_ref() == Some(&id) {
            return;
        }
        if let Some(host) = self.store.inventory().get(&id).cloned() {
            self.details_edit_host = Some(id);
            self.details_label.update(cx, |state, cx| {
                state.set_value(host.label.as_str(), window, cx)
            });
            self.details_address.update(cx, |state, cx| {
                state.set_value(host.address.as_str(), window, cx);
            });
            self.details_port.update(cx, |state, cx| {
                state.set_value(host.port.to_string().as_str(), window, cx);
            });
            self.details_username.update(cx, |state, cx| {
                state.set_value(host.username.as_str(), window, cx);
            });
            let password = match &host.auth {
                // The box shows the secret itself (Termius behaviour), resolved
                // from the vault — the inventory id is never user-facing.
                sshdeck_core::AuthMethod::Password { secret_ref } => {
                    resolve_password(self.vault.as_ref(), secret_ref).unwrap_or_default()
                }
                _ => String::new(),
            };
            self.details_password.update(cx, |state, cx| {
                state.set_value(password.as_str(), window, cx);
            });
            self.details_group.update(cx, |state, cx| {
                state.set_value(host.group.as_deref().unwrap_or(""), window, cx);
            });
            self.details_tags.update(cx, |state, cx| {
                state.set_value(host.tags.join(", ").as_str(), window, cx);
            });
        }
    }

    /// Writes the details Label input back to the selected host.
    fn commit_details_label(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.details_edit_host.clone() else {
            return;
        };
        let label = self.details_label.read(cx).value().to_string();
        if let Some(mut host) = self.store.inventory().get(&id).cloned() {
            if host.label != label {
                host.label = label;
                self.store.inventory_mut().upsert(host);
                if let Err(error) = self.store.save() {
                    window.push_notification(
                        Notification::error(format!("Could not save: {error}")),
                        cx,
                    );
                }
                cx.notify();
            }
        }
    }

    /// Writes the details Address input back to the selected host.
    fn commit_details_address(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.details_edit_host.clone() else {
            return;
        };
        let address = self.details_address.read(cx).value().trim().to_string();
        if address.is_empty() {
            return;
        }
        if let Some(mut host) = self.store.inventory().get(&id).cloned() {
            if host.address != address {
                host.address = address;
                self.store.inventory_mut().upsert(host);
                if let Err(error) = self.store.save() {
                    window.push_notification(
                        Notification::error(format!("Could not save: {error}")),
                        cx,
                    );
                }
                cx.notify();
            }
        }
    }

    /// Writes the details Port input back to the selected host.
    ///
    /// Intermediate keystrokes (`""`, partial numbers) do not parse, so they
    /// are ignored rather than clobbering the port or spamming warnings — the
    /// last good value stays until the field parses again.
    fn commit_details_port(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.details_edit_host.clone() else {
            return;
        };
        let raw = self.details_port.read(cx).value().to_string();
        let Ok(port) = parse_port(&raw) else {
            return;
        };
        if let Some(mut host) = self.store.inventory().get(&id).cloned() {
            if host.port != port {
                host.port = port;
                self.store.inventory_mut().upsert(host);
                if let Err(error) = self.store.save() {
                    window.push_notification(
                        Notification::error(format!("Could not save: {error}")),
                        cx,
                    );
                }
                cx.notify();
            }
        }
    }

    /// Writes the details Username input back to the selected host.
    fn commit_details_username(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.details_edit_host.clone() else {
            return;
        };
        let username = self.details_username.read(cx).value().trim().to_string();
        if let Some(mut host) = self.store.inventory().get(&id).cloned() {
            if host.username != username {
                host.username = username;
                self.store.inventory_mut().upsert(host);
                if let Err(error) = self.store.save() {
                    window.push_notification(
                        Notification::error(format!("Could not save: {error}")),
                        cx,
                    );
                }
                cx.notify();
            }
        }
    }

    /// Writes the details Password input back to the selected host.
    ///
    /// Termius behaviour: this box IS the password. A non-empty value is
    /// sealed into the encrypted vault under the host's stable id and the
    /// inventory keeps only that opaque id — never the secret. Clearing the
    /// field drops a password-auth host back to agent auth and removes the
    /// vault entry. Key-based methods are left alone until their own editors
    /// exist. Without an open vault there is no honest place to keep a
    /// secret, so the edit is reverted with an error instead of persisting
    /// plaintext.
    fn commit_details_password(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.details_edit_host.clone() else {
            return;
        };
        let password = self.details_password.read(cx).value().to_string();
        let secret_id = password_secret_id(&id);
        let Some(vault) = self.vault.as_mut() else {
            self.details_password.update(cx, |state, cx| {
                state.set_value("", window, cx);
            });
            window.push_notification(
                Notification::error("Password vault unavailable: cannot save password"),
                cx,
            );
            return;
        };
        let current = vault.get(&secret_id).ok().flatten();
        if current.as_deref().map(|secret| secret.as_str()) == Some(password.as_str()) {
            return;
        }
        if let Some(mut host) = self.store.inventory().get(&id).cloned() {
            if password.is_empty() {
                let _ = vault.remove(&secret_id);
                if matches!(host.auth, sshdeck_core::AuthMethod::Password { .. }) {
                    host.auth = sshdeck_core::AuthMethod::Agent;
                    self.store.inventory_mut().upsert(host);
                }
            } else {
                if let Err(error) = vault.set(secret_id.clone(), &password) {
                    window.push_notification(
                        Notification::error(format!("Could not save password: {error}")),
                        cx,
                    );
                    return;
                }
                // Key-based and other methods are left alone: typing here
                // must not silently discard a key path — but a host on agent
                // auth with a fresh password means password auth.
                if !matches!(host.auth, sshdeck_core::AuthMethod::Password { .. })
                    && !matches!(host.auth, sshdeck_core::AuthMethod::Agent)
                {
                    return;
                }
                host.auth = sshdeck_core::AuthMethod::Password {
                    secret_ref: secret_id,
                };
                self.store.inventory_mut().upsert(host);
            }
            if let Err(error) = vault.save() {
                window.push_notification(
                    Notification::error(format!("Could not save password: {error}")),
                    cx,
                );
            } else if let Err(error) = self.store.save() {
                window
                    .push_notification(Notification::error(format!("Could not save: {error}")), cx);
            }
            cx.notify();
        }
    }

    /// Writes the details Parent Group input back to the selected host.
    fn commit_details_group(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.details_edit_host.clone() else {
            return;
        };
        let raw = self.details_group.read(cx).value().trim().to_string();
        let group = if raw.is_empty() { None } else { Some(raw) };
        if let Some(mut host) = self.store.inventory().get(&id).cloned() {
            if host.group != group {
                host.group = group;
                self.store.inventory_mut().upsert(host);
                if let Err(error) = self.store.save() {
                    window.push_notification(
                        Notification::error(format!("Could not save: {error}")),
                        cx,
                    );
                }
                cx.notify();
            }
        }
    }

    /// Writes the details Tags input back to the selected host.
    ///
    /// The box holds a comma-separated list (`prod, edge`); splitting keeps
    /// editing to one row instead of a pill editor.
    fn commit_details_tags(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.details_edit_host.clone() else {
            return;
        };
        let tags: Vec<String> = self
            .details_tags
            .read(cx)
            .value()
            .split(',')
            .map(|tag| tag.trim().to_string())
            .filter(|tag| !tag.is_empty())
            .collect();
        if let Some(mut host) = self.store.inventory().get(&id).cloned() {
            if host.tags != tags {
                host.tags = tags;
                self.store.inventory_mut().upsert(host);
                if let Err(error) = self.store.save() {
                    window.push_notification(
                        Notification::error(format!("Could not save: {error}")),
                        cx,
                    );
                }
                cx.notify();
            }
        }
    }

    /// Opens a session for `host`, prompting for a password first when the host
    /// authenticates with one and no secret is available yet.
    fn connect(&mut self, host: Host, window: &mut Window, cx: &mut Context<Self>) {
        let config = match &host.auth {
            // The sidebar password box is the password (Termius behaviour):
            // resolve the vaulted secret and connect directly. The prompt is
            // only the fallback for hosts with no stored secret.
            //
            // Development and test affordance, not a user feature: when
            // `SSHDECK_PASSWORD` is set, use it instead of prompting so an
            // automated run can connect. It is read straight from the
            // environment into the connection config — never logged, never
            // written to disk, never shown in the UI. Without it, the prompt is
            // exactly as before.
            sshdeck_core::AuthMethod::Password { secret_ref } => {
                match resolve_password(self.vault.as_ref(), secret_ref).or_else(env_password) {
                    Some(password) => SessionConfig::from_host(&host).with_password(password),
                    None => {
                        self.prompt_for_secret(&host, window, cx);
                        return;
                    }
                }
            }
            _ => SessionConfig::from_host(&host),
        };

        self.open_pane(&host, config, window, cx);
    }

    /// Asks for the host's password before connecting.
    ///
    /// Fallback for hosts with no vaulted secret. Submitting seals the secret
    /// into the vault (see `submit_secret`), so this prompt appears at most
    /// once per host.
    fn prompt_for_secret(&mut self, host: &Host, window: &mut Window, cx: &mut Context<Self>) {
        let prompt = cx.new(|cx| InputState::new(window, cx).placeholder("Password"));
        let label = host.label.clone();
        self.secret = Some((host.id.clone(), prompt));
        window.push_notification(Notification::info(format!("{label} needs a password")), cx);
        cx.notify();
    }

    /// Connects the host whose password was just entered, then clears the prompt.
    ///
    /// The password is also sealed into the vault under the host's stable id
    /// (Termius remembers it), so the next Connect goes straight through.
    /// Without an open vault it stays connection-only, as before.
    fn submit_secret(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((host_id, input)) = self.secret.take() else {
            return;
        };
        let Some(host) = self.store.inventory().get(&host_id).cloned() else {
            return;
        };
        let password = input.read(cx).value().to_string();
        if !password.is_empty() {
            let secret_id = password_secret_id(&host_id);
            if let Some(vault) = self.vault.as_mut() {
                if vault.set(secret_id.clone(), &password).is_ok() {
                    let _ = vault.save();
                    let mut host = host.clone();
                    host.auth = sshdeck_core::AuthMethod::Password {
                        secret_ref: secret_id,
                    };
                    self.store.inventory_mut().upsert(host);
                    let _ = self.store.save();
                }
            }
        }
        let host = self
            .store
            .inventory()
            .get(&host_id)
            .cloned()
            .unwrap_or(host);
        let config = SessionConfig::from_host(&host).with_password(password);
        self.open_pane(&host, config, window, cx);
    }

    /// Creates the pane and appends it as a tab.
    fn open_pane(
        &mut self,
        host: &Host,
        config: SessionConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Construct the pane inside its own entity context; the shell only holds
        // the handle. `TerminalPane::new` returns the pane state, not an entity.
        // The sidebar's font size and theme apply here so a new pane opens
        // looking like the existing ones; unknown theme names fall back to the
        // pane default rather than refusing to connect.
        let font_size = self.terminal_font_size;
        let scheme = TerminalScheme::by_name(&self.selected_theme).unwrap_or_default();
        let options = TerminalOptions::new(font_size, 10_000, true).with_scheme(scheme);
        // The default constructor covers the default options; anything else
        // goes through the options form so the sidebar's font size and theme
        // apply to the new pane.
        let pane = if options == TerminalOptions::default() {
            cx.new(|cx| TerminalPane::new(config, window, cx))
        } else {
            cx.new(|cx| TerminalPane::new_with_options(config, options, window, cx))
        };
        // The pane header reports split/max/close back through a weak handle,
        // the same shape as the palette's `set_on_select`.
        let weak = cx.entity().downgrade();
        let pane_handle = pane.clone();
        pane.update(cx, |pane, _| {
            pane.set_header_title(host.label.clone());
            let weak_action = weak.clone();
            pane.set_on_pane_action(move |action, pane, window, cx| {
                weak_action
                    .update(cx, |this, cx| {
                        this.pane_action(action, pane, window, cx);
                    })
                    .ok();
            });
            let weak_bc = weak.clone();
            let pane_entity = pane_handle.clone();
            pane.set_on_broadcast(move |bytes, _pane, cx| {
                weak_bc
                    .update(cx, |this, cx| {
                        if this.broadcast_mode {
                            let leaves = workspace_leaves(&this.workspace);
                            for &idx in &leaves {
                                if let Some(s) = this.sessions.get(idx) {
                                    if s.pane != pane_entity {
                                        s.pane.update(cx, |p, _| {
                                            p.write(bytes);
                                        });
                                    }
                                }
                            }
                        }
                    })
                    .ok();
            });
        });
        // Re-render the chrome whenever the pane's status changes. The pane is
        // the only thing that reads the event channel; the shell just mirrors.
        let subscription = cx.observe_in(&pane, window, |this, pane, window, cx| {
            let status = pane.read(cx).status();
            // Match by handle rather than trusting the active index: the pane can
            // notify before the tab is registered.
            if let Some(session) = this.sessions.iter_mut().find(|s| s.pane == pane) {
                session.status = status;
            }
            // A session that just authenticated is what the SFTP pane attaches
            // to; a session that ended is what it detaches from.
            this.reconcile_sftp(window, cx);
            this.reconcile_forward(cx);
            cx.notify();
        });
        self._subscriptions.push(subscription);

        let status = pane.read(cx).status();
        self.sessions.push(Session {
            host: host.id.clone(),
            pane: pane.clone(),
            status: status.clone(),
        });
        self.last_session_host = Some(host.id.clone());
        let index = self.sessions.len() - 1;
        self.active = Some(index);
        self.tab = MainTab::Session(index);
        self.overlay = None;
        pane.update(cx, |pane, cx| pane.focus(window, cx));
        // The new tab is active now; the SFTP pane must follow it even before the
        // session reaches `Connected` (it detaches until then).
        self.reconcile_sftp(window, cx);
        self.reconcile_forward(cx);

        let host_lbl = host.label.clone();
        let host_ep = host.endpoint();
        let user = if host.username.is_empty() {
            "user".to_string()
        } else {
            host.username.clone()
        };
        let logs_pane = self.ensure_logs_pane(window, cx);
        let time_str = current_time_str();
        logs_pane.update(cx, |p, cx| {
            p.append(
                &time_str,
                "Connected",
                user,
                "session",
                host_lbl,
                host_ep,
                logs_pane::LogLevel::Success,
                cx,
            );
        });

        let failed = match &status.state {
            SessionState::Failed { message } => Some(message.clone()),
            _ => None,
        };
        if let Some(message) = failed {
            window.push_notification(
                Notification::error(format!("{} could not connect: {message}", host.label)),
                cx,
            );
        }
        cx.notify();
    }

    /// Spawns a local terminal session running the system shell and adds it as a tab.
    fn open_local_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let font_size = self.terminal_font_size;
        let scheme = TerminalScheme::by_name(&self.selected_theme).unwrap_or_default();
        let options = TerminalOptions::new(font_size, 10_000, true).with_scheme(scheme);

        let pane = cx.new(|cx| TerminalPane::new_local_with_options(options, window, cx));

        let mut instance_num = 1;
        while self.sessions.iter().any(|s| {
            s.status.title.as_deref() == Some(&format!("Local Terminal ({instance_num})"))
                || s.host.as_str() == format!("local-terminal-{instance_num}")
        }) {
            instance_num += 1;
        }
        let title = format!("Local Terminal ({instance_num})");
        let host_id = HostId::new(format!("local-terminal-{instance_num}"));

        let weak = cx.entity().downgrade();
        let pane_handle = pane.clone();
        let title_for_header = title.clone();
        pane.update(cx, |pane, _| {
            pane.set_title(title_for_header.clone());
            pane.set_header_title(title_for_header);
            let weak_action = weak.clone();
            pane.set_on_pane_action(move |action, pane, window, cx| {
                weak_action
                    .update(cx, |this, cx| {
                        this.pane_action(action, pane, window, cx);
                    })
                    .ok();
            });
            let weak_bc = weak.clone();
            let pane_entity = pane_handle.clone();
            pane.set_on_broadcast(move |bytes, _pane, cx| {
                weak_bc
                    .update(cx, |this, cx| {
                        if this.broadcast_mode {
                            let leaves = workspace_leaves(&this.workspace);
                            for &idx in &leaves {
                                if let Some(s) = this.sessions.get(idx) {
                                    if s.pane != pane_entity {
                                        s.pane.update(cx, |p, _| {
                                            p.write(bytes);
                                        });
                                    }
                                }
                            }
                        }
                    })
                    .ok();
            });
        });

        let subscription = cx.observe_in(&pane, window, |this, pane, window, cx| {
            let status = pane.read(cx).status();
            if let Some(session) = this.sessions.iter_mut().find(|s| s.pane == pane) {
                session.status = status;
            }
            this.reconcile_sftp(window, cx);
            this.reconcile_forward(cx);
            cx.notify();
        });
        self._subscriptions.push(subscription);

        let mut status = pane.read(cx).status();
        if status.title.is_none() {
            status.title = Some(title.clone());
        }
        self.sessions.push(Session {
            host: host_id.clone(),
            pane: pane.clone(),
            status,
        });
        self.last_session_host = Some(host_id);
        let index = self.sessions.len() - 1;
        self.active = Some(index);
        self.tab = MainTab::Session(index);
        self.overlay = None;
        pane.update(cx, |pane, cx| pane.focus(window, cx));
        self.reconcile_sftp(window, cx);
        self.reconcile_forward(cx);

        let logs_pane = self.ensure_logs_pane(window, cx);
        let time_str = current_time_str();
        logs_pane.update(cx, |p, cx| {
            p.append(
                &time_str,
                "Connected",
                "local".to_string(),
                "terminal",
                title,
                "local".to_string(),
                logs_pane::LogLevel::Success,
                cx,
            );
        });

        cx.notify();
    }

    #[allow(dead_code)]
    fn connected_count(&self) -> usize {
        self.sessions
            .iter()
            .filter(|s| s.status.state.is_active())
            .count()
    }

    /// Replaces the main region with `overlay` (or restores it with `None`).
    ///
    /// A pane that is being replaced is dropped, closing any transport it held.
    fn show_overlay(
        &mut self,
        overlay: Option<Overlay>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.overlay = overlay;
        self.reconcile_sftp(window, cx);
        cx.notify();
    }

    fn close_overlay(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_overlay(None, window, cx);
    }

    fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.overlay, Some(Overlay::Settings(_))) {
            self.close_overlay(window, cx);
            return;
        }
        let view = cx.new(|cx| SettingsView::new(window, cx));
        view.update(cx, |view, cx| view.focus(window, cx));
        self.show_overlay(Some(Overlay::Settings(view)), window, cx);
    }

    #[allow(dead_code)]
    fn open_keys(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.overlay, Some(Overlay::Keys(_))) {
            self.close_overlay(window, cx);
            return;
        }
        let view = cx.new(|cx| KeysPane::new(window, cx));
        self.show_overlay(Some(Overlay::Keys(view)), window, cx);
    }

    /// Selects a left-rail pane. The single path both the rail buttons and
    /// `SSHDECK_START_PANE` go through, so the startup affordance cannot drift
    /// from a real click.
    fn select_left_nav(&mut self, nav: LeftNav, window: &mut Window, cx: &mut Context<Self>) {
        self.left_nav = nav;
        self.tab = MainTab::Vaults;
        match nav {
            LeftNav::Keychain => {
                let pane = self.ensure_keys_pane(window, cx);
                pane.update(cx, |p, cx| p.show(keys_pane::KeysSection::Keys, cx));
            }
            LeftNav::KnownHosts => {
                let pane = self.ensure_keys_pane(window, cx);
                pane.update(cx, |p, cx| p.show(keys_pane::KeysSection::Hosts, cx));
            }
            LeftNav::PortForwarding => {
                self.ensure_forward_pane(window, cx);
                self.reconcile_forward(cx);
            }
            LeftNav::Snippets => {
                self.ensure_snippets_pane(window, cx);
            }
            LeftNav::Logs => {
                self.ensure_logs_pane(window, cx);
            }
            LeftNav::Settings => {
                self.open_settings(window, cx);
                return;
            }
            LeftNav::Hosts => {}
        }
        cx.notify();
    }

    fn ensure_keys_pane(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<KeysPane> {
        if let Some(pane) = self.keys_pane.clone() {
            return pane;
        }
        let pane = cx.new(|cx| KeysPane::new(window, cx));
        self.keys_pane = Some(pane.clone());
        pane
    }

    fn ensure_forward_pane(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<ForwardPane> {
        if let Some(pane) = self.forward_pane.clone() {
            return pane;
        }
        let pane = cx.new(|cx| ForwardPane::new(window, cx));
        self.forward_pane = Some(pane.clone());
        pane
    }

    /// Sends text into the active session terminal, or broadcasts to all open
    /// workspace leaves if broadcast mode is active.
    pub fn broadcast_send_text(&self, text: &str, cx: &mut Context<Self>) {
        if self.broadcast_mode {
            let leaves = workspace_leaves(&self.workspace);
            if !leaves.is_empty() {
                for &index in &leaves {
                    if let Some(session) = self.sessions.get(index) {
                        session.pane.update(cx, |p, _| {
                            p.send_text(text);
                        });
                    }
                }
                return;
            }
        }
        if let Some(active) = self.active {
            if let Some(session) = self.sessions.get(active) {
                session.pane.update(cx, |p, _| {
                    p.send_text(text);
                });
            }
        }
    }

    fn ensure_snippets_pane(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<SnippetsPane> {
        if let Some(pane) = self.snippets_pane.clone() {
            return pane;
        }
        let pane = cx.new(|cx| SnippetsPane::new(window, cx));
        let root = cx.entity().downgrade();
        pane.update(cx, |p, _| {
            p.set_on_execute(move |text, window, cx| {
                let text = text.to_string();
                root.update(cx, |this, cx| {
                    this.broadcast_send_text(&text, cx);
                    if this.broadcast_mode {
                        window.push_notification(
                            Notification::success("Snippet broadcasted to workspace terminals"),
                            cx,
                        );
                    } else if this.active.is_some() {
                        window.push_notification(
                            Notification::success("Snippet sent to terminal"),
                            cx,
                        );
                    } else {
                        window.push_notification(
                            Notification::warning("No active terminal session"),
                            cx,
                        );
                    }
                })
                .ok();
            });
        });
        self.snippets_pane = Some(pane.clone());
        pane
    }

    fn ensure_logs_pane(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<LogsPane> {
        if let Some(pane) = self.logs_pane.clone() {
            return pane;
        }
        // The buffer starts empty: entries appear only as the app pushes real
        // connect/close/forward/transfer events. Nothing is fabricated here.
        let pane = cx.new(|cx| LogsPane::new(window, cx));
        self.logs_pane = Some(pane.clone());
        pane
    }

    /// Keeps the port-forwarding pane attached to the focused session, like the
    /// SFTP pane. Called on session connected/closed/tab switched.
    fn reconcile_forward(&mut self, cx: &mut Context<Self>) {
        let Some(pane) = self.forward_pane.clone() else {
            return;
        };
        let session = self
            .active
            .and_then(|index| self.sessions.get(index))
            .filter(|session| matches!(session.status.state, SessionState::Connected))
            .and_then(|session| session.pane.read(cx).session());
        pane.update(cx, |pane, cx| pane.set_session(session, cx));
    }

    /// Switches the main region to `tab`, creating the SFTP pane on first use.
    fn select_tab(&mut self, tab: MainTab, window: &mut Window, cx: &mut Context<Self>) {
        self.overlay = None;
        self.tab = tab;
        if matches!(tab, MainTab::Sftp) && self.sftp_pane.is_none() {
            let root = cx.entity().downgrade();
            self.sftp_pane = Some(cx.new(|cx| SftpPane::new(window, cx)));
            if let Some(pane) = self.sftp_pane.clone() {
                pane.update(cx, |pane, _| {
                    pane.set_on_show_logs(move |window, cx| {
                        root.update(cx, |this, cx| {
                            this.select_left_nav(LeftNav::Logs, window, cx);
                        })
                        .ok();
                    });
                });
            }
        }
        if let MainTab::Session(index) = tab {
            self.active = Some(index);
            if let Some(session) = self.sessions.get(index) {
                let pane = session.pane.clone();
                pane.update(cx, |pane, cx| pane.focus(window, cx));
            }
            self.set_workspace_chrome(false, cx);
        }
        if matches!(tab, MainTab::Workspace) {
            // Seed the workspace from the focused session on first entry.
            if workspace_leaves(&self.workspace).is_empty() {
                if let Some(active) = self.active {
                    self.workspace = WorkspaceNode::Pane {
                        tabs: vec![active],
                        active: 0,
                    };
                    self.workspace_focus = 0;
                }
            }
            self.set_workspace_chrome(false, cx);
            if let Some(&index) = workspace_leaves(&self.workspace).get(self.workspace_focus) {
                self.active = Some(index);
                if let Some(session) = self.sessions.get(index) {
                    let pane = session.pane.clone();
                    pane.update(cx, |pane, cx| pane.focus(window, cx));
                }
            }
        }
        self.reconcile_sftp(window, cx);
        self.reconcile_forward(cx);
        cx.notify();
    }

    /// Applies the sidebar's terminal theme to every open pane live. Unknown
    /// names keep the pane default; the grid parser holds no colours, so no
    /// rebuild is needed. Called from event handlers only.
    fn apply_terminal_scheme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let scheme = TerminalScheme::by_name(&self.selected_theme).unwrap_or_default();
        for session in &self.sessions {
            session.pane.update(cx, |pane, _| pane.set_scheme(scheme));
        }
        let _ = window;
    }

    /// Applies a sidebar font size to new and existing panes, clamped to the
    /// terminal's 10–24px bounds. The cell is re-measured from it every frame,
    /// so `line_height` scales with the glyphs. Called from event handlers.
    fn set_terminal_font_size(&mut self, size: f32, window: &mut Window, cx: &mut Context<Self>) {
        let size = if size.is_finite() {
            size.clamp(10.0, 24.0)
        } else {
            14.0
        };
        self.terminal_font_size = size;
        for session in &self.sessions {
            session.pane.update(cx, |pane, _| pane.set_font_size(size));
        }
        let _ = window;
    }

    /// Shows or hides the 28pt per-pane header on every session pane, with a
    /// fresh title snapshot. Called from event handlers only — never from
    /// render or the pane observer, where an entity update would re-notify.
    fn set_workspace_chrome(&mut self, show: bool, cx: &mut Context<Self>) {
        for session in &self.sessions {
            let title = session.status.title.clone().unwrap_or_else(|| {
                self.store
                    .inventory()
                    .get(&session.host)
                    .map(|host| host.label.clone())
                    .unwrap_or_else(|| session.host.to_string())
            });
            session.pane.update(cx, |pane, _| {
                pane.set_show_header(show);
                pane.set_header_title(title.clone());
            });
        }
    }

    /// Handles a workspace pane header action for the tile that owns `pane`.
    fn pane_action(
        &mut self,
        action: PaneHeaderAction,
        pane: Entity<TerminalPane>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.sessions.iter().position(|s| s.pane == pane) else {
            return;
        };
        match action {
            PaneHeaderAction::Split => self.workspace_split(index, window, cx),
            PaneHeaderAction::Maximize => {
                self.workspace_maximized = !self.workspace_maximized;
                self.select_tab(MainTab::Workspace, window, cx);
            }
            PaneHeaderAction::Close => self.close_session(index, window, cx),
        }
    }

    /// Tiles `index` next to the focused tile and enters the workspace.
    ///
    /// An already-tiled session is focused, not duplicated, so every tile keeps
    /// its own `TerminalPane` entity. A per-tile second PTY on the same host is
    /// the upgrade path; it needs the host's `SessionConfig` (and possibly a
    /// second password) and is deliberately not opened here.
    fn workspace_split(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let (workspace, focus) = workspace_insert(&self.workspace, self.workspace_focus, index);
        self.workspace = workspace;
        self.workspace_focus = focus;
        self.workspace_maximized = false;
        self.select_tab(MainTab::Workspace, window, cx);
    }

    /// Tiles two sessions side-by-side (active session and `other_index`) in a 2-pane workspace.
    fn tile_sessions_side_by_side(
        &mut self,
        other_index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.sessions.is_empty() {
            return;
        }
        let current_active = self.active.unwrap_or(0);
        let left = current_active;
        let right = if other_index != current_active && other_index < self.sessions.len() {
            other_index
        } else if self.sessions.len() > 1 {
            (0..self.sessions.len())
                .find(|&i| i != current_active)
                .unwrap_or(0)
        } else {
            current_active
        };

        if left == right {
            self.workspace = WorkspaceNode::Pane {
                tabs: vec![left],
                active: 0,
            };
            self.workspace_focus = 0;
        } else {
            self.workspace = WorkspaceNode::Split {
                dir: SplitDir::Row,
                weights: vec![1.0, 1.0],
                children: vec![
                    WorkspaceNode::Pane {
                        tabs: vec![left],
                        active: 0,
                    },
                    WorkspaceNode::Pane {
                        tabs: vec![right],
                        active: 0,
                    },
                ],
            };
            self.workspace_focus = 1;
        }
        self.workspace_maximized = false;
        self.dragged_session = None;
        self.dragged_tab = None;
        self.tab_drag_start = None;
        self.select_tab(MainTab::Workspace, window, cx);
    }

    /// Splits the current view or workspace with `dragged_session` in direction `dir`.
    fn split_with_dragged_session(
        &mut self,
        dragged: usize,
        dir: SplitDir,
        place_after: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.sessions.is_empty() {
            return;
        }
        let current_active = self.active.unwrap_or(0);
        let (first, second) = if place_after {
            (current_active, dragged)
        } else {
            (dragged, current_active)
        };
        self.workspace = WorkspaceNode::Split {
            dir,
            weights: vec![1.0, 1.0],
            children: vec![
                WorkspaceNode::Pane {
                    tabs: vec![first],
                    active: 0,
                },
                WorkspaceNode::Pane {
                    tabs: vec![second],
                    active: 0,
                },
            ],
        };
        self.workspace_focus = if place_after { 1 } else { 0 };
        self.workspace_maximized = false;
        self.dragged_session = None;
        self.dragged_tab = None;
        self.tab_drag_start = None;
        self.select_tab(MainTab::Workspace, window, cx);
    }

    /// Closes one session tab and repairs the focused-tab bookkeeping.
    fn close_session(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.dragged_session = None;
        self.dragged_tab = None;
        self.tab_drag_start = None;
        if index >= self.sessions.len() {
            return;
        }
        let session = self.sessions.remove(index);
        let host_opt = self.store.inventory().get(&session.host).cloned();
        let host_lbl = host_opt
            .as_ref()
            .map(|h| h.label.clone())
            .unwrap_or_else(|| session.host.to_string());
        let host_ep = host_opt.as_ref().map(|h| h.endpoint()).unwrap_or_default();
        let logs_pane = self.ensure_logs_pane(window, cx);
        let time_str = current_time_str();
        logs_pane.update(cx, |p, cx| {
            p.append(
                &time_str,
                "Closed",
                "user",
                "session",
                host_lbl,
                host_ep,
                logs_pane::LogLevel::Info,
                cx,
            );
        });
        self.tab = tab_after_close(self.tab, index);
        self.active = match self.active {
            Some(active) if active == index => None,
            Some(active) if active > index => Some(active - 1),
            other => other,
        };
        // Tiles point at session indexes, so they shift down past the removal
        // and drop tiles that showed the closed session.
        let (workspace, workspace_focus) =
            workspace_remove(&self.workspace, self.workspace_focus, index);
        self.workspace = workspace;
        self.workspace_focus = workspace_focus;
        if matches!(self.tab, MainTab::Workspace) {
            let leaves = workspace_leaves(&self.workspace);
            if leaves.is_empty() {
                self.tab = MainTab::Vaults;
            } else {
                self.active = leaves.get(self.workspace_focus).copied();
            }
        }
        self.reconcile_sftp(window, cx);
        self.reconcile_forward(cx);
        cx.notify();
    }

    /// Attaches the SFTP pane to the focused session's transport, or detaches it.
    ///
    /// Called whenever the tab selection or a session's state changes. At most
    /// one SFTP session is live: a client is opened only while the SFTP tab is
    /// showing a session that is authenticated, and dropped as soon as that
    /// session ends, another tab becomes active, or a settings/keys pane opens
    /// (docs/BUDGET.md: no connection per tab).
    fn reconcile_sftp(&mut self, window: &Window, cx: &mut Context<Self>) {
        // The pane only exists once the SFTP tab has been opened.
        let Some(pane) = self.sftp_pane.clone() else {
            return;
        };

        let visible = self.overlay.is_none() && matches!(self.tab, MainTab::Sftp);
        let target = if visible {
            self.active.filter(|&index| {
                self.sessions
                    .get(index)
                    .is_some_and(|session| matches!(session.status.state, SessionState::Connected))
            })
        } else {
            None
        };
        if self.sftp_attached == target {
            return;
        }
        self.sftp_attached = target;
        self.sftp_generation = self.sftp_generation.wrapping_add(1);
        let generation = self.sftp_generation;

        // Drop the previous transport before opening the next one.
        pane.update(cx, |pane, cx| pane.set_client(None, cx));

        let Some(index) = target else {
            return;
        };
        let Some(entry) = self.sessions.get(index) else {
            return;
        };
        let host_id = entry.host.clone();
        let session = entry.pane.read(cx).session();
        let Some(session) = session else {
            return;
        };

        // Show the pane's connecting branch while the transport opens.
        let label = self
            .store
            .inventory()
            .get(&host_id)
            .map(|host| host.label.clone())
            .unwrap_or_else(|| host_id.to_string());
        pane.update(cx, |pane, cx| pane.set_connecting(Some(label), cx));

        cx.spawn_in(window, async move |this, cx| {
            let connected = SftpClient::connect(&session).await;
            this.update_in(cx, |this, window, cx| {
                // A newer attach, or a closed pane, has superseded this attempt.
                if this.sftp_generation != generation {
                    return;
                }
                match connected {
                    Ok(client) => pane.update(cx, |pane, cx| pane.set_client(Some(client), cx)),
                    Err(error) => {
                        // Leave the pane in its "not connected" state. Retrying is
                        // a user action, so no loop runs here.
                        this.sftp_attached = None;
                        pane.update(cx, |pane, cx| pane.set_connecting(None, cx));
                        window.push_notification(
                            Notification::warning(format!("SFTP could not open: {error}")),
                            cx,
                        );
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    /// Opens the palette, or moves focus back to it when it is already open.
    fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(palette) = self.palette.clone() {
            palette.update(cx, |palette, cx| palette.focus(window, cx));
            cx.notify();
            return;
        }

        let root = cx.entity().downgrade();
        let palette = cx.new(|cx| PaletteView::new(window, cx));

        let select_root = root.clone();
        let cancel_root = root;
        palette.update(cx, |palette, _cx| {
            palette.set_on_select(move |command, window, cx| {
                select_root
                    .update(cx, |this, cx| this.dispatch_command(command, window, cx))
                    .ok();
            });
            palette.set_on_cancel(move |_window, cx| {
                cancel_root
                    .update(cx, |this, cx| {
                        this.palette = None;
                        cx.notify();
                    })
                    .ok();
            });
        });

        self.palette = Some(palette);
        cx.notify();
    }

    /// Runs one palette command against the handlers this view already has.
    fn dispatch_command(
        &mut self,
        command: palette::CommandId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Choosing a command closes the palette, whatever its outcome.
        self.palette = None;
        match command {
            palette::CommandId::AddHost => {
                let _ = self.add_draft_host(window, cx);
            }
            palette::CommandId::ConnectSelectedHost => match self.selected.clone() {
                Some(id) => {
                    if let Some(host) = self.store.inventory().get(&id).cloned() {
                        self.connect(host, window, cx);
                    }
                }
                None => window.push_notification(Notification::warning("Select a host first"), cx),
            },
            palette::CommandId::RemoveSelectedHost => match self.selected.clone() {
                Some(id) => self.remove_host(&id, window, cx),
                None => window.push_notification(Notification::warning("Select a host first"), cx),
            },
            // The palette performs the theme toggle itself; kept so a dispatched
            // toggle still works if that ever changes.
            palette::CommandId::ToggleTheme => {
                let next = if Theme::global(cx).is_dark() {
                    ThemeMode::Light
                } else {
                    ThemeMode::Dark
                };
                Theme::change(next, Some(window), cx);
            }
            palette::CommandId::OpenSftp => {
                self.select_tab(MainTab::Sftp, window, cx);
            }
            palette::CommandId::ManageKeys => {
                self.select_left_nav(LeftNav::Keychain, window, cx);
            }
            palette::CommandId::OpenSettings => {
                self.open_settings(window, cx);
            }
            palette::CommandId::PortForwarding => {
                self.select_left_nav(LeftNav::PortForwarding, window, cx);
            }
            palette::CommandId::Snippets => {
                self.select_left_nav(LeftNav::Snippets, window, cx);
            }
            palette::CommandId::KnownHosts => {
                self.select_left_nav(LeftNav::KnownHosts, window, cx);
            }
            palette::CommandId::Logs => {
                self.select_left_nav(LeftNav::Logs, window, cx);
            }
            palette::CommandId::ToggleSidebar => {
                self.sidebar_collapsed = !self.sidebar_collapsed;
            }
            palette::CommandId::OpenLocalTerminal => {
                self.open_local_terminal(window, cx);
            }
        }
        cx.notify();
    }

    fn on_palette_up(
        &mut self,
        _: &palette::PaletteUp,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.nudge_palette(-1, cx);
    }

    fn on_palette_down(
        &mut self,
        _: &palette::PaletteDown,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.nudge_palette(1, cx);
    }

    fn on_palette_cancel(
        &mut self,
        _: &palette::PaletteCancel,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(palette) = self.palette.clone() {
            palette.update(cx, |palette, cx| palette.dismiss(window, cx));
        }
    }

    fn nudge_palette(&mut self, delta: isize, cx: &mut Context<Self>) {
        if let Some(palette) = self.palette.clone() {
            palette.update(cx, |palette, cx| palette.nudge(delta, cx));
        }
    }

    /// The single 40px app header: sidebar toggle, the session tabs, the add
    /// control, then the right-aligned pane actions.
    ///
    /// This replaces the old three bands (title bar + tab strip + status bar);
    /// Termius draws one header and no status bar. The product name is gone from
    /// the chrome, and the status bar's host/state/connection-count information
    /// now lives in the selected tab and the header's right cluster.
    ///
    /// The right side is intentionally quiet: no filled primary button, no
    /// two-line status block. Termius shows a text button, a bell, and an
    /// account control there; sshdeck shows three ghost icons at 16px
    /// (command palette, notifications, account). The filled `Reconnect`
    /// action now lives on the session tab itself (RotateCw) plus the palette
    /// and double-click, and connection state is the glyph tint on the tab
    /// with a tooltip. Removed from header but still reachable:
    /// - `Reconnect`/`Connect` → session tab's RotateCw, palette
    ///   "Connect to Selected Host", or double-clicking the host row.
    /// - `SSH keys & known hosts` → sidebar "Keys" button and the palette.
    /// - Theme toggle → palette "Toggle Light / Dark Theme".
    fn render_header(&mut self, _window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        // ponytail: the header is an always-dark chrome strip (`--main-bg`
        // `#1d2033`), so it keeps fixed dark hexes instead of theme tokens —
        // in Light mode the theme's `muted`/`foreground` would turn the tabs
        // light-grey-on-light. Ceiling: a dedicated header token in
        // `themes/sshdeck.json`; upgrade by adding one and using it here.
        let header_bg = rgb(0x1a1d2d);
        let header_fg = rgb(0xffffff);

        let update_btn = Button::new("header-update-btn")
            .ghost()
            .small()
            .label("Update")
            .tooltip("Check for updates")
            .on_click(cx.listener(|_this, _, window, cx| {
                window.push_notification(Notification::info("SshDeck is up to date (v0.1.0)"), cx);
            }));

        let actions = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .flex_shrink_0()
            .child(update_btn)
            .child(
                Button::new("notifications")
                    .ghost()
                    .icon(Icon::default().data(glyph::BELL_SOLID).size(px(16.)))
                    .tooltip("Connection logs & events")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.select_left_nav(LeftNav::Logs, window, cx);
                    })),
            )
            .when(
                matches!(self.tab, MainTab::Session(_) | MainTab::Workspace),
                |this| {
                    let open = self.sidebar_open;
                    this.child(
                        Button::new("right-sidebar-toggle")
                            .ghost()
                            .icon(
                                Icon::new(IconName::PanelRight)
                                    .size(px(16.))
                                    .text_color(if open { rgb(0x10b981) } else { rgb(0x8d91a5) }),
                            )
                            .tooltip(if open {
                                "Hide sidebar (⌘B)"
                            } else {
                                "Show sidebar (⌘B)"
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.sidebar_open = !this.sidebar_open;
                                cx.notify();
                            })),
                    )
                },
            );

        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .flex_shrink_0()
            .h(px(40.))
            .bg(header_bg)
            .text_color(header_fg)
            .pl(if cfg!(target_os = "macos") {
                px(80.)
            } else {
                px(12.)
            })
            .pr_3()
            .border_b_1()
            .border_color(rgba(0xffffff14))
            .window_control_area(WindowControlArea::Drag)
            .child(self.render_tabs(cx))
            .child(actions)
    }

    /// Opens the add-host sheet. Host creation lives behind the header's `+`
    /// control instead of an always-visible form in the sidebar.
    fn open_add_host(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.add_host_open = true;
        // The label field owns no focus itself until the sheet has rendered, so
        // queue the focus for after this frame.
        cx.defer_in(window, |this, window, cx| {
            let handle = this.draft_label.read(cx).focus_handle(cx);
            handle.focus(window, cx);
        });
        cx.notify();
    }

    fn close_add_host(&mut self, cx: &mut Context<Self>) {
        self.add_host_open = false;
        cx.notify();
    }

    fn render_sidebar(&mut self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.render_left_rail(window, cx)
    }

    fn render_left_rail(&mut self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        // ponytail: the rail is white in Light mode but `sidebar.background`
        // is shared with the window root and the add-host sheet, so it cannot
        // move to white on its own — the rail rides `popover` (`#ffffff` in
        // Light) instead. Ceiling: a dedicated `rail.background` token in
        // `themes/sshdeck.json`; upgrade by adding one and using it here.
        let rail_bg = cx.theme().popover;
        let border = cx.theme().sidebar_border;
        let active_bg = cx.theme().muted; // #e6ebed in light
        let fg = cx.theme().foreground; // #141729
        let muted = cx.theme().muted_foreground; // #798c94
        let win_w = f32::from(window.bounds().size.width);
        let collapsed = self.sidebar_collapsed || win_w < 900.0;

        let nav_item = move |label: &'static str, glyph_data: &'static [u8], is_active: bool| {
            div()
                .id(SharedString::from(format!("nav-{label}")))
                .flex()
                .flex_row()
                .items_center()
                .when(collapsed, |el| el.justify_center().px_0())
                .when(!collapsed, |el| el.gap_2().px_3())
                .h(px(44.))
                .rounded(px(8.))
                .cursor_pointer()
                .when(is_active, |el| el.bg(active_bg))
                .text_color(if is_active { fg } else { muted })
                .child(
                    Icon::default()
                        .data(glyph_data)
                        .size(px(16.))
                        .text_color(if is_active { fg } else { muted }),
                )
                .when(!collapsed, |el| el.child(label))
                .hover(|s| s.bg(active_bg))
        };

        let hosts_active = self.left_nav == LeftNav::Hosts;
        let keychain_active = self.left_nav == LeftNav::Keychain;
        let pf_active = self.left_nav == LeftNav::PortForwarding;
        let snippets_active = self.left_nav == LeftNav::Snippets;
        let known_active = self.left_nav == LeftNav::KnownHosts;
        let logs_active = self.left_nav == LeftNav::Logs;

        div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .w(px(rail_width(win_w, self.sidebar_collapsed)))
            .h_full()
            .bg(rail_bg)
            .border_r_1()
            .border_color(border)
            .p_2()
            .gap_1()
            .child(
                nav_item("Hosts", glyph::HOST, hosts_active).on_click(cx.listener(
                    |this, _, window, cx| {
                        this.select_left_nav(LeftNav::Hosts, window, cx);
                    },
                )),
            )
            .child(
                nav_item("Keychain", glyph::KEY, keychain_active).on_click(cx.listener(
                    |this, _, window, cx| {
                        this.select_left_nav(LeftNav::Keychain, window, cx);
                    },
                )),
            )
            .child(
                nav_item("Port Forwarding", glyph::FORWARD, pf_active).on_click(cx.listener(
                    |this, _, window, cx| {
                        this.select_left_nav(LeftNav::PortForwarding, window, cx);
                    },
                )),
            )
            .child(
                nav_item("Snippets", glyph::SNIPPET, snippets_active).on_click(cx.listener(
                    |this, _, window, cx| {
                        this.select_left_nav(LeftNav::Snippets, window, cx);
                    },
                )),
            )
            .child(
                nav_item("Known Hosts", glyph::RADIOWAVES, known_active).on_click(cx.listener(
                    |this, _, window, cx| {
                        this.select_left_nav(LeftNav::KnownHosts, window, cx);
                    },
                )),
            )
            .child(
                nav_item("Logs", glyph::CLOCK, logs_active).on_click(cx.listener(
                    |this, _, window, cx| {
                        this.select_left_nav(LeftNav::Logs, window, cx);
                    },
                )),
            )
    }

    /// The add-host sheet: a right-hand panel over a scrim, carrying the same
    /// four fields and the same validation the always-visible sidebar form had.
    /// Opening and closing is the header `+` and the sheet's close control.
    ///
    /// Below ~600px window width the panel becomes a full-width drawer so the
    /// 360px card never overflows a narrow window.
    fn render_add_host_sheet(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        if !self.add_host_open {
            return None;
        }
        let win_w = f32::from(window.bounds().size.width);
        let narrow = use_full_drawer(win_w);

        let panel = div()
            .absolute()
            .top_0()
            .when(!narrow, |el| el.right_0().w(px(360.)))
            .when(narrow, |el| el.left_0().right_0().w_full())
            .h_full()
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .overflow_y_scrollbar()
            .border_l_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().sidebar)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(div().text_color(cx.theme().foreground).child("Add host"))
                    .child(
                        Button::new("add-host-close")
                            .ghost()
                            .icon(IconName::Close)
                            .tooltip("Close")
                            .on_click(cx.listener(|this, _, _, cx| this.close_add_host(cx))),
                    ),
            )
            .child(Input::new(&self.draft_label).small())
            .child(Input::new(&self.draft_address).small())
            .child(Input::new(&self.draft_username).small())
            .child(Input::new(&self.draft_port).small())
            .child(
                Button::new("add-host-submit")
                    .small()
                    .primary()
                    .label("Add host")
                    .icon(IconName::Plus)
                    .on_click(cx.listener(|this, _, window, cx| {
                        if this.add_draft_host(window, cx) {
                            this.close_add_host(cx);
                        }
                    })),
            );

        Some(
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                // The scrim is a sibling under the panel, so a click on the
                // panel never reaches it; a click on the scrim dismisses.
                .child(
                    div()
                        .id("add-host-scrim")
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full()
                        .bg(rgba(0x00000080))
                        .on_click(cx.listener(|this, _, _, cx| this.close_add_host(cx))),
                )
                .child(panel),
        )
    }

    /// One fixed header tab (Vaults or SFTP): 30px tall inside the 40px header,
    /// 6px radius, transparent until selected. Uses 16px icons throughout the
    /// chrome (medium, the default) with the heavier glyph choice for each tab.
    /// The tab row: Vaults dropdown pill, SFTP tab, Workspace tab (always present),
    /// open session tabs, New Tab (+ New Tab without close button), and the quick-add + button.
    fn render_tabs(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = rgb(0x8d91a5);
        let unselected_fg = rgb(0xd0d4e4);
        let selected_bg = rgb(0x25293d);
        let selected_border = rgba(0xffffff1a);
        let selected_fg = rgb(0xffffff);
        let hover_bg = rgb(0x222638);
        let active = self.active;

        // 1. [ ⚿ Vaults ☁ ⌵ ] dropdown button pill
        let is_vaults_active = self.tab == MainTab::Vaults && self.overlay.is_none();
        let vaults_btn = div()
            .id("tab-vaults")
            .flex()
            .flex_row()
            .items_center()
            .gap_1p5()
            .h(px(28.))
            .px_2p5()
            .rounded(px(7.))
            .border_1()
            .border_color(if is_vaults_active {
                rgba(0xffffff2e)
            } else {
                rgba(0xffffff1a)
            })
            .bg(if is_vaults_active {
                rgb(0x282c3f)
            } else {
                rgb(0x222638)
            })
            .cursor_pointer()
            .flex_shrink_0()
            .hover(|s| s.bg(rgb(0x2a2e44)).border_color(rgba(0xffffff2e)))
            .child(
                Icon::default()
                    .data(glyph::ID_BADGE)
                    .size(px(14.))
                    .text_color(if is_vaults_active {
                        selected_fg
                    } else {
                        unselected_fg
                    }),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .font_weight(gpui_kit::FontWeight::MEDIUM)
                    .text_color(if is_vaults_active {
                        selected_fg
                    } else {
                        unselected_fg
                    })
                    .child("Vaults"),
            )
            .child(
                Icon::default()
                    .data(glyph::CLOUD)
                    .size(px(13.))
                    .text_color(muted),
            )
            .child(div().w(px(1.)).h(px(12.)).bg(rgba(0xffffff20)))
            .child(
                Icon::new(IconName::ChevronDown)
                    .size(px(12.))
                    .text_color(muted),
            )
            .on_click(cx.listener(|this, _, window, cx| {
                this.select_tab(MainTab::Vaults, window, cx);
            }));

        // 2. [ 📁 SFTP ] tab button
        let is_sftp_active = self.tab == MainTab::Sftp && self.overlay.is_none();
        let sftp_btn = div()
            .id("tab-sftp")
            .flex()
            .flex_row()
            .items_center()
            .gap_1p5()
            .h(px(28.))
            .px_2p5()
            .rounded(px(6.))
            .cursor_pointer()
            .flex_shrink_0()
            .text_color(if is_sftp_active {
                selected_fg
            } else {
                unselected_fg
            })
            .when(is_sftp_active, |el| {
                el.bg(selected_bg).border_1().border_color(selected_border)
            })
            .when(!is_sftp_active, |el| el.hover(move |s| s.bg(hover_bg)))
            .child(
                Icon::default()
                    .data(glyph::FOLDER)
                    .size(px(14.))
                    .text_color(if is_sftp_active {
                        selected_fg
                    } else {
                        unselected_fg
                    }),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .font_weight(gpui_kit::FontWeight::MEDIUM)
                    .child("SFTP"),
            )
            .on_click(cx.listener(|this, _, window, cx| {
                this.select_tab(MainTab::Sftp, window, cx);
            }));

        // 3. [ ⚏ Workspace ] tab button (only when workspace is active or has panes)
        let is_ws_active = self.tab == MainTab::Workspace && self.overlay.is_none();
        let ws_panes = workspace_panes_count(&self.workspace);
        let show_ws_tab = ws_panes > 0 || is_ws_active;
        let ws_title = if ws_panes > 1 {
            format!("Workspace ({ws_panes})")
        } else {
            "Workspace".to_string()
        };
        let ws_btn = if show_ws_tab {
            Some(
                div()
                    .id("tab-workspace")
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1p5()
                    .h(px(28.))
                    .px_2p5()
                    .rounded(px(6.))
                    .cursor_pointer()
                    .flex_shrink_0()
                    .text_color(if is_ws_active {
                        rgb(0x10b981)
                    } else {
                        unselected_fg
                    })
                    .when(is_ws_active, |el| {
                        el.bg(rgb(0x0e3a2f))
                            .border_1()
                            .border_color(rgba(0x10b98160))
                    })
                    .when(!is_ws_active, |el| el.hover(move |s| s.bg(hover_bg)))
                    .child(
                        Button::new("close-workspace-tab")
                            .ghost()
                            .xsmall()
                            .icon(Icon::new(IconName::Close).size(px(12.)).text_color(
                                if is_ws_active {
                                    rgb(0x10b981)
                                } else {
                                    unselected_fg
                                },
                            ))
                            .tooltip("Close workspace")
                            .on_click(cx.listener(|this, _, window, cx| {
                                cx.stop_propagation();
                                this.workspace = WorkspaceNode::Empty;
                                let fallback =
                                    this.active.map(MainTab::Session).unwrap_or(MainTab::Vaults);
                                this.select_tab(fallback, window, cx);
                            })),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .font_weight(gpui_kit::FontWeight::MEDIUM)
                            .child(ws_title),
                    )
                    .child(div().size(px(5.)).rounded_full().bg(rgb(0x10b981)))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.select_tab(MainTab::Workspace, window, cx);
                    })),
            )
        } else {
            None
        };

        let mut strip = div()
            .id("tab-strip")
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .h(px(30.))
            .min_w(px(0.))
            .overflow_x_scrollbar()
            .child(vaults_btn)
            .child(sftp_btn)
            .when_some(ws_btn, |el, btn| el.child(btn));

        // 4. Session tabs
        let has_multi = self.sessions.len() > 1;
        for (index, session) in self.sessions.iter().enumerate() {
            let is_local = session.host.as_str().starts_with("local-terminal");
            let host_label = if is_local {
                "Local Terminal".to_string()
            } else {
                self.store
                    .inventory()
                    .get(&session.host)
                    .map(|host| host.label.clone())
                    .unwrap_or_else(|| session.host.to_string())
            };
            let label = session.status.title.clone().unwrap_or_else(|| {
                if is_local {
                    let num = session
                        .host
                        .as_str()
                        .strip_prefix("local-terminal-")
                        .unwrap_or("1");
                    format!("Local Terminal ({num})")
                } else {
                    let same_host_count = self
                        .sessions
                        .iter()
                        .filter(|s| s.host == session.host)
                        .count();
                    if same_host_count > 1 {
                        let instance_num = self.sessions[..=index]
                            .iter()
                            .filter(|s| s.host == session.host)
                            .count();
                        format!("{host_label} ({instance_num})")
                    } else {
                        host_label
                    }
                }
            });

            let is_active = self.overlay.is_none() && active == Some(index) && !is_ws_active;
            let is_this_dragged = self.dragged_session == Some(index);
            let state_label = session.status.state.label();
            let id = session.host.clone();
            let close_id = id.clone();

            let tab_bg = if is_active {
                rgb(0x0e3a2f)
            } else {
                rgb(0x222638)
            };
            let tab_border = if is_active {
                rgba(0x10b98160)
            } else {
                rgba(0xffffff14)
            };
            let tab_fg = if is_active {
                rgb(0x10b981)
            } else {
                unselected_fg
            };

            let host_opt = self.store.inventory().get(&session.host);
            let os_tile = if let Some(host) = host_opt {
                let tint = host_os_tint(host, rgb(0xe95420).into());
                div()
                    .size(px(18.))
                    .rounded(px(4.))
                    .bg(tint)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        Icon::default()
                            .data(glyph::UBUNTU_SOLID)
                            .size(px(12.))
                            .text_color(rgb(0xffffff)),
                    )
            } else {
                div()
                    .size(px(18.))
                    .rounded(px(4.))
                    .bg(if is_active {
                        rgba(0x10b98124)
                    } else {
                        rgba(0xffffff14)
                    })
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        Icon::default()
                            .data(glyph::TERMINAL_PROMPT)
                            .size(px(11.))
                            .text_color(tab_fg),
                    )
            };

            let split_btn = Button::new(SharedString::from(format!("split-tab-{index}")))
                .ghost()
                .xsmall()
                .icon(
                    Icon::default()
                        .data(glyph::SPLIT_HORIZONTAL)
                        .size(px(12.))
                        .text_color(if is_active {
                            rgb(0x10b981)
                        } else {
                            unselected_fg
                        }),
                )
                .tooltip("Tile side-by-side with active session")
                .on_click(cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    this.tile_sessions_side_by_side(index, window, cx);
                }));

            let mut tab_div = div()
                .id(SharedString::from(format!("tab-{index}-{id}")))
                .flex()
                .flex_row()
                .items_center()
                .gap_1p5()
                .px_2p5()
                .h(px(28.))
                .rounded(px(6.))
                .cursor_pointer()
                .min_w(px(0.))
                .flex_shrink_0()
                .bg(tab_bg)
                .border_1()
                .border_color(tab_border)
                .text_color(tab_fg)
                .when(!is_active, |el| el.hover(move |s| s.bg(hover_bg)))
                .when(is_this_dragged, |el| {
                    el.bg(rgba(0x2091f630))
                        .border_1()
                        .border_color(rgb(0x2091f6))
                })
                .tooltip({
                    let tip = state_label.clone();
                    move |window, cx| Tooltip::new(tip.clone()).build(window, cx)
                })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, event: &MouseDownEvent, _window, cx| {
                        this.tab_drag_start = Some((index, event.position));
                        cx.notify();
                    }),
                )
                .on_mouse_move(
                    cx.listener(move |this, event: &MouseMoveEvent, _window, cx| {
                        if let Some((start_idx, start_pos)) = this.tab_drag_start {
                            if event.dragging() && start_idx == index {
                                let dx =
                                    (f32::from(event.position.x) - f32::from(start_pos.x)).abs();
                                let dy =
                                    (f32::from(event.position.y) - f32::from(start_pos.y)).abs();
                                if dx > 4.0 || dy > 4.0 {
                                    this.dragged_session = Some(index);
                                    this.dragged_tab = None;
                                    cx.notify();
                                }
                            }
                        }
                    }),
                );

            // Close button on the LEFT for active tab (matching Termius Screenshot 13, 22),
            // and host/terminal icon on the left for inactive tabs (matching Termius Screenshot 1, 12).
            if is_active {
                tab_div = tab_div.child(
                    Button::new(SharedString::from(format!("close-tab-{index}-{close_id}")))
                        .ghost()
                        .xsmall()
                        .icon(
                            Icon::new(IconName::Close)
                                .size(px(12.))
                                .text_color(rgb(0x10b981)),
                        )
                        .tooltip("Close session")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.close_session(index, window, cx);
                        })),
                );
            } else {
                tab_div = tab_div.child(os_tile);
            }

            tab_div = tab_div.child(
                div()
                    .text_size(px(12.))
                    .font_weight(gpui_kit::FontWeight::MEDIUM)
                    .min_w(px(0.))
                    .truncate()
                    .child(SharedString::from(label)),
            );

            if is_active {
                tab_div = tab_div.child(div().size(px(5.)).rounded_full().bg(rgb(0x10b981)));
            }

            if has_multi {
                tab_div = tab_div.child(split_btn);
            }

            strip = strip.child(tab_div.on_click(cx.listener(move |this, _, window, cx| {
                this.select_tab(MainTab::Session(index), window, cx);
            })));
        }

        // 5. [ ✕ New Tab ] / [ + New Tab ] tab pill (matching Termius Screenshot 12, 19, 22, 27)
        let is_new_tab_active = self.tab == MainTab::NewTab && self.overlay.is_none();
        let show_new_tab_pill = is_new_tab_active || !self.sessions.is_empty() || show_ws_tab;
        if show_new_tab_pill {
            let new_tab_item = div()
                .id("tab-new-tab")
                .flex()
                .flex_row()
                .items_center()
                .gap_1p5()
                .h(px(28.))
                .px_2p5()
                .rounded(px(6.))
                .cursor_pointer()
                .flex_shrink_0()
                .bg(if is_new_tab_active {
                    selected_bg
                } else {
                    rgb(0x222638)
                })
                .border_1()
                .border_color(if is_new_tab_active {
                    selected_border
                } else {
                    rgba(0xffffff14)
                })
                .text_color(if is_new_tab_active {
                    selected_fg
                } else {
                    unselected_fg
                })
                .when(!is_new_tab_active, |el| el.hover(move |s| s.bg(hover_bg)))
                .when(is_new_tab_active, |el| {
                    el.child(
                        Button::new("close-new-tab")
                            .ghost()
                            .xsmall()
                            .icon(IconName::Close)
                            .tooltip("Close tab")
                            .on_click(cx.listener(|this, _, window, cx| {
                                cx.stop_propagation();
                                let fallback =
                                    this.active.map(MainTab::Session).unwrap_or(MainTab::Vaults);
                                this.select_tab(fallback, window, cx);
                            })),
                    )
                })
                .when(!is_new_tab_active, |el| {
                    el.child(
                        Icon::new(IconName::Plus)
                            .size(px(12.))
                            .text_color(unselected_fg),
                    )
                })
                .child(
                    div()
                        .text_size(px(12.))
                        .font_weight(gpui_kit::FontWeight::MEDIUM)
                        .min_w(px(0.))
                        .truncate()
                        .child("New Tab"),
                )
                .on_click(cx.listener(|this, _, window, cx| {
                    this.select_tab(MainTab::NewTab, window, cx);
                }));

            strip = strip.child(new_tab_item);
        }

        // 6. Quick add tab `+` button right next to `New Tab`
        let add_btn = Button::new("add-tab-btn")
            .ghost()
            .xsmall()
            .icon(
                Icon::new(IconName::Plus)
                    .size(px(14.))
                    .text_color(unselected_fg),
            )
            .tooltip("New tab (⌘T)")
            .on_click(cx.listener(|this, _, window, cx| {
                this.select_tab(MainTab::NewTab, window, cx);
            }));

        strip = strip.child(div().flex_shrink_0().child(add_btn));

        div()
            .flex()
            .flex_row()
            .items_center()
            .flex_1()
            .min_w(px(0.))
            .child(strip)
    }

    /// ~28px band directly under the 40px header, spanning **only the pane
    /// area, not the sidebar**. Holds the focused pane's own tabs plus a `+`.
    /// Visually quiet: no background fill of its own, a lighter active tab
    /// (`#282b3d`, `tab.active.background`), `12px` text. Always rendered,
    /// even with a single tab — the original always shows it.
    #[allow(dead_code)]
    fn render_pane_tabs(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let active_bg = cx.theme().muted; // #282b3d `tab.active.background`
        let active_fg = cx.theme().foreground; // #ffffff `tab.active.foreground`
        let hover_bg = cx.theme().list_hover; // `--list-hover`, mode-aware

        // One tab reflecting the focused pane. For a session the label is the
        // pane title or host label and the glyph is SquareTerminal tinted by
        // connection state; for Vaults/SFTP the fixed icon is shown with
        // muted tint. Count is always at least one, so the row never collapses.
        let (label, icon, glyph_color, state_label): (String, IconName, Hsla, String) = match self
            .tab
        {
            MainTab::Vaults => (
                "Vaults".to_string(),
                IconName::LayoutDashboard,
                cx.theme().muted_foreground,
                "vaults".to_string(),
            ),
            MainTab::Sftp => (
                "SFTP".to_string(),
                IconName::Folder,
                cx.theme().muted_foreground,
                "sftp".to_string(),
            ),
            MainTab::NewTab => (
                "New Tab".to_string(),
                IconName::SquareTerminal,
                cx.theme().muted_foreground,
                "new tab".to_string(),
            ),
            MainTab::Workspace => (
                "Workspace".to_string(),
                IconName::PanelRight,
                cx.theme().muted_foreground,
                "workspace".to_string(),
            ),
            MainTab::Session(index) => {
                if let Some(session) = self.sessions.get(index) {
                    let title = session.status.title.clone().unwrap_or_else(|| {
                        self.store
                            .inventory()
                            .get(&session.host)
                            .map(|host| host.label.clone())
                            .unwrap_or_else(|| session.host.to_string())
                    });
                    let (slabel, color) = match &session.status.state {
                        SessionState::Connected => ("connected".to_string(), cx.theme().success),
                        SessionState::Connecting | SessionState::Authenticating => {
                            ("connecting".to_string(), cx.theme().warning)
                        }
                        SessionState::Failed { message } => {
                            (format!("failed: {message}"), cx.theme().danger)
                        }
                        SessionState::Disconnected | SessionState::Closed { .. } => {
                            ("disconnected".to_string(), cx.theme().muted_foreground)
                        }
                    };
                    (title, IconName::SquareTerminal, color, slabel)
                } else {
                    (
                        "Vaults".to_string(),
                        IconName::LayoutDashboard,
                        cx.theme().muted_foreground,
                        "vaults".to_string(),
                    )
                }
            }
        };

        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .h(px(28.))
            .px_2()
            .flex_shrink_0()
            .border_b_1()
            .border_color(cx.theme().sidebar_border) // --border-light
            // No background fill on the row itself — only the active tab is
            // elevated (`#282b3d`, radius 6px) with primary text; inactive
            // would be transparent with `#8d91a5` (here only one tab, so active).
            .child(
                div()
                    .id("pane-tab-active")
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .h(px(22.))
                    .rounded_md()
                    .bg(active_bg)
                    .text_color(active_fg)
                    .text_size(px(12.))
                    .hover(|s| s.bg(hover_bg))
                    // (unconfirmed) tooltip via closure mirrors header session
                    // tab; Button::tooltip(string) is the verified path.
                    .tooltip({
                        let tip = state_label.clone();
                        move |window, cx| Tooltip::new(tip.clone()).build(window, cx)
                    })
                    .child(Icon::new(icon).text_color(glyph_color))
                    .child(SharedString::from(label))
                    .child(
                        Button::new("pane-tab-close")
                            .ghost()
                            .icon(IconName::Close)
                            .tooltip("Close")
                            .on_click(cx.listener(|this, _, window, cx| match this.tab {
                                MainTab::Session(index) => {
                                    this.close_session(index, window, cx);
                                }
                                _ => this.select_tab(MainTab::Vaults, window, cx),
                            })),
                    ),
            )
            .child(
                Button::new("pane-new-tab")
                    .ghost()
                    .icon(IconName::Plus)
                    .tooltip("New tab")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_add_host(window, cx);
                    })),
            )
    }

    /// The password prompt, rendered while a secret is missing.
    fn render_secret_prompt(&mut self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        // Clone the handle so no borrow of `self` outlives this method.
        let (host_id, input) = self.secret.clone()?;
        let label = self
            .store
            .inventory()
            .get(&host_id)
            .map(|host| host.endpoint())
            .unwrap_or_default();
        Some(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .px_3()
                .py_2()
                .flex_shrink_0()
                .border_b_1()
                .border_color(cx.theme().border)
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(SharedString::from(format!("{label} · password"))),
                )
                .child(
                    div().flex_1().child(
                        Input::new(&input)
                            .small()
                            .content_type(InputContentType::Password),
                    ),
                )
                .child(
                    Button::new("secret-connect")
                        .small()
                        .primary()
                        .label("Connect")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.submit_secret(window, cx);
                        })),
                )
                .child(
                    Button::new("secret-cancel")
                        .small()
                        .ghost()
                        .label("Cancel")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.secret = None;
                            cx.notify();
                        })),
                ),
        )
    }

    fn render_main(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        if self.overlay.is_some() {
            return self.render_overlay(cx);
        }
        match self.tab {
            MainTab::Vaults => {
                match self.left_nav {
                    LeftNav::Hosts => self.render_vault(window, cx),
                    LeftNav::Keychain | LeftNav::KnownHosts => {
                        let pane = self.ensure_keys_pane(window, cx);
                        let section = match self.left_nav {
                            LeftNav::KnownHosts => keys_pane::KeysSection::Hosts,
                            _ => keys_pane::KeysSection::Keys,
                        };
                        pane.update(cx, |p, cx| p.show(section, cx));
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .size_full()
                            .overflow_hidden()
                            .child(pane)
                            .into_any_element()
                    }
                    LeftNav::PortForwarding => {
                        let pane = self.ensure_forward_pane(window, cx);
                        // Ensure session is reconciled even if pane was just created
                        // outside the nav selection path (e.g., via render).
                        self.reconcile_forward(cx);
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .size_full()
                            .overflow_hidden()
                            .child(pane)
                            .into_any_element()
                    }
                    LeftNav::Snippets => {
                        let pane = self.ensure_snippets_pane(window, cx);
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .size_full()
                            .overflow_hidden()
                            .child(pane)
                            .into_any_element()
                    }
                    LeftNav::Logs => {
                        let pane = self.ensure_logs_pane(window, cx);
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .size_full()
                            .overflow_hidden()
                            .child(pane)
                            .into_any_element()
                    }
                    LeftNav::Settings => self.render_overlay(cx),
                }
            }
            MainTab::Sftp => self.render_sftp(window, cx),
            MainTab::NewTab => self.render_new_tab(window, cx),
            MainTab::Workspace => self.render_workspace(window, cx),
            MainTab::Session(_) => self.render_session(window, cx),
        }
    }

    /// The Vaults/Hosts screen — Termius light: search + Connect, toolbar, Hosts header, responsive 1/2/3/4-col white cards, right Host Details drawer.
    fn render_vault(&mut self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let content_bg = cx.theme().accent; // #edf1f2 in light
        let muted = cx.theme().muted_foreground; // #798c94
        let has_selection = self.selected.is_some();
        let selected_id = self.selected.clone();

        let filter_val = self.filter.read(cx).value().trim().to_string();
        let quick_connect_host = parse_quick_connect(&filter_val);

        // Top search row: the Connect pill lives inside the search container's
        // right edge. When a quick-connect string is typed, it illuminates
        // as an active primary blue "Quick Connect" button. Otherwise it is
        // grey and disabled-looking until a saved host is selected.
        let mut connect_pill = div()
            .id("vault-connect")
            .h(px(32.))
            .px_4()
            .rounded(px(8.))
            .flex()
            .items_center()
            .justify_center()
            .flex_shrink_0()
            .text_size(px(13.))
            .font_weight(gpui_kit::FontWeight::MEDIUM);

        if let Some(quick_host) = quick_connect_host {
            connect_pill = connect_pill
                .bg(rgb(0x2091f6))
                .text_color(rgb(0xffffff))
                .cursor_pointer()
                .hover(|s| s.bg(rgb(0x1976d2)))
                .child("Quick Connect")
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.connect(quick_host.clone(), window, cx);
                }));
        } else if has_selection {
            let connect_target = selected_id.clone();
            connect_pill = connect_pill
                .bg(rgb(0x2091f6))
                .text_color(rgb(0xffffff))
                .cursor_pointer()
                .hover(|s| s.bg(rgb(0x1976d2)))
                .child("Connect")
                .on_click(cx.listener(move |this, _, window, cx| {
                    if let Some(host) = connect_target
                        .clone()
                        .and_then(|id| this.store.inventory().get(&id).cloned())
                    {
                        this.connect(host, window, cx);
                    }
                }));
        } else {
            connect_pill = connect_pill
                .bg(rgb(0xeef2f4))
                .text_color(rgb(0x8d9ba3))
                .child("Connect");
        }

        let search_bar = div()
            .flex()
            .flex_row()
            .items_center()
            .w_full()
            .h(px(42.))
            .pl_4()
            .pr(px(5.))
            .rounded(px(10.))
            .bg(rgb(0xffffff))
            .border_1()
            .border_color(rgb(0xe2e8f0))
            .shadow_xs()
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .child(Input::new(&self.filter).small().bordered(false)),
            )
            .child(connect_pill);

        let search_row = div()
            .flex()
            .flex_row()
            .items_center()
            .w_full()
            .px_6()
            .pt_4()
            .pb_2()
            .child(search_bar);

        // Toolbar row: + New host (merged split), Terminal; right view toggles + MH avatar.
        // Termius tokens: split-button bg #ffffff, hairline border #d5dde0,
        // accent #2091f6, avatar orange #e67e22.
        let toolbar = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .w_full()
            .px_6()
            .py_1()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_0()
                    .h(px(32.))
                    .rounded(px(8.))
                    .border_1()
                    .border_color(rgb(0xd5dde0))
                    .bg(rgb(0xffffff))
                    .child(
                        div()
                            .id("vault-new-host")
                            .flex()
                            .flex_row()
                            .items_center()
                            .px_2p5()
                            .h_full()
                            .text_size(px(13.))
                            .font_weight(gpui_kit::FontWeight::MEDIUM)
                            .text_color(rgb(0x1d2033))
                            .cursor_pointer()
                            .hover(|s| s.bg(rgb(0xf1f5f9)))
                            .child("+ New host")
                            .on_click(
                                cx.listener(|this, _, window, cx| this.open_add_host(window, cx)),
                            ),
                    )
                    .child(div().w(px(1.)).h(px(18.)).bg(rgb(0xd5dde0)))
                    .child(
                        div()
                            .id("vault-new-host-caret")
                            .flex()
                            .items_center()
                            .px_2()
                            .h_full()
                            .cursor_pointer()
                            .hover(|s| s.bg(rgb(0xf1f5f9)))
                            .child(
                                Icon::new(IconName::ChevronDown)
                                    .size(px(14.))
                                    .text_color(muted),
                            )
                            .on_click(
                                cx.listener(|this, _, window, cx| this.open_add_host(window, cx)),
                            ),
                    ),
            )
            .child(
                div()
                    .id("vault-terminal")
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .h(px(32.))
                    .px_2p5()
                    .rounded(px(8.))
                    .border_1()
                    .border_color(rgb(0xd5dde0))
                    .bg(rgb(0xffffff))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(0xf1f5f9)))
                    .child(
                        div()
                            .size(px(18.))
                            .rounded(px(4.))
                            .bg(rgb(0x1d2033))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .font_weight(gpui_kit::FontWeight::BOLD)
                                    .text_color(rgb(0xffffff))
                                    .child(">_"),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(13.))
                            .font_weight(gpui_kit::FontWeight::MEDIUM)
                            .text_color(rgb(0x1d2033))
                            .child("Terminal"),
                    )
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_local_terminal(window, cx);
                    })),
            )
            .child(div().flex_1())
            .child(
                Button::new("vault-view-grid")
                    .ghost()
                    .icon(
                        Icon::default()
                            .data(match self.view_mode {
                                ViewMode::Grid => glyph::GRID,
                                ViewMode::List => glyph::LIST,
                            })
                            .size(px(18.)),
                    )
                    .tooltip(match self.view_mode {
                        ViewMode::Grid => "Switch to list view",
                        ViewMode::List => "Switch to grid view",
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.view_mode = match this.view_mode {
                            ViewMode::Grid => ViewMode::List,
                            ViewMode::List => ViewMode::Grid,
                        };
                        cx.notify();
                    })),
            )
            .child(
                Button::new("vault-filter")
                    .ghost()
                    .icon(Icon::default().data(glyph::TAG).size(px(18.)))
                    .tooltip("Filter by tags")
                    .on_click(cx.listener(|this, _, window, cx| {
                        let mut all_tags: Vec<String> = this
                            .store
                            .inventory()
                            .hosts()
                            .iter()
                            .flat_map(|h| h.tags.clone())
                            .collect();
                        all_tags.sort();
                        all_tags.dedup();
                        if all_tags.is_empty() {
                            let handle = this.filter.read(cx).focus_handle(cx);
                            handle.focus(window, cx);
                            window.push_notification(
                                Notification::info("No tags found on hosts yet"),
                                cx,
                            );
                        } else {
                            let cur = this.filter.read(cx).value().to_string();
                            let next_tag = all_tags
                                .iter()
                                .find(|t| !cur.contains(t.as_str()))
                                .unwrap_or(&all_tags[0]);
                            this.filter.update(cx, |input, cx| {
                                input.set_value(next_tag, window, cx);
                            });
                            window.push_notification(
                                Notification::info(format!("Filtered by tag: {next_tag}")),
                                cx,
                            );
                            cx.notify();
                        }
                    })),
            )
            .child(
                Button::new("vault-calendar")
                    .ghost()
                    .icon(Icon::default().data(glyph::CALENDAR).size(px(18.)))
                    .tooltip(self.host_sort.label())
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.host_sort = this.host_sort.next();
                        window.push_notification(Notification::info(this.host_sort.label()), cx);
                        cx.notify();
                    })),
            )
            .child(
                div().flex().flex_row().items_center().gap_1().child(
                    div()
                        .size(px(32.))
                        .rounded_full()
                        .bg(rgb(0xe67e22))
                        .border_2()
                        .border_color(rgb(0x2091f6))
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_xs()
                        .font_weight(gpui_kit::FontWeight::BOLD)
                        .text_color(rgb(0xffffff))
                        .child(current_user_initials()),
                ),
            );

        // Host cards — responsive column chunking based on available width
        let win_w = f32::from(window.bounds().size.width);
        let sidebar_w = rail_width(win_w, self.sidebar_collapsed);
        // Below ~800px the drawer floats over the grid instead of docking,
        // so the grid keeps the full centre width.
        let overlay_drawer = details_overlay(win_w) && self.details_open;
        let details_w = if self.details_open {
            if overlay_drawer {
                (win_w - sidebar_w).max(280.0)
            } else {
                details_width(win_w)
            }
        } else {
            0.0
        };
        let docked_details_w = if overlay_drawer { 0.0 } else { details_w };
        let avail_w = (win_w - sidebar_w - docked_details_w - 32.0).max(180.0);

        let chunk_size = match self.view_mode {
            ViewMode::List => 1,
            ViewMode::Grid => grid_columns(avail_w),
        };

        let query = self.filter.read(cx).value().to_string();
        let mut filtered: Vec<Host> = self
            .store
            .inventory()
            .filtered(&query)
            .into_iter()
            .cloned()
            .collect();

        match self.host_sort {
            HostSort::LabelAsc => {
                filtered.sort_by_key(|a| a.label.to_lowercase());
            }
            HostSort::LabelDesc => {
                filtered.sort_by_key(|b| std::cmp::Reverse(b.label.to_lowercase()));
            }
            HostSort::Address => {
                filtered.sort_by(|a, b| a.address.cmp(&b.address));
            }
            HostSort::Recent => {}
        }

        let mut rows: Vec<AnyElement> = Vec::new();
        for chunk in filtered.chunks(chunk_size) {
            let mut row_cards: Vec<AnyElement> = Vec::new();
            for host in chunk {
                row_cards.push(self.render_host_card(host, window, cx));
            }
            for _ in chunk.len()..chunk_size {
                row_cards.push(
                    div()
                        .flex_1()
                        .max_w(px(380.))
                        .min_w(px(0.))
                        .into_any_element(),
                );
            }
            rows.push(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap_3()
                    .w_full()
                    .min_w(px(0.))
                    .children(row_cards)
                    .into_any_element(),
            );
        }

        let grid = div()
            .id("vault-grid")
            .flex()
            .flex_col()
            .gap_3()
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scrollbar()
            .px_6()
            .py_3()
            .child(
                div()
                    .text_size(px(16.))
                    .font_weight(gpui_kit::FontWeight::BOLD)
                    .text_color(rgb(0x1d2033))
                    .child("Hosts"),
            )
            .children(rows);

        // The secret prompt must render here, not only in session view:
        // password hosts stop at `prompt_for_secret` without opening a tab,
        // so without this row the toast is a dead end with nowhere to type.
        let secret = self.render_secret_prompt(cx);
        let centre = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(0.))
            .bg(content_bg)
            .overflow_hidden()
            .when_some(secret, |el, prompt| el.child(prompt))
            .child(search_row)
            .child(toolbar)
            .child(grid);

        let details_open = self.details_open;
        let details = if details_open {
            Some(self.render_host_details_panel(details_w, window, cx))
        } else {
            None
        };

        let vault_info_modal = if self.vault_info_open {
            let host_count = self.store.inventory().hosts().len();
            let fg = cx.theme().foreground;
            let win_h = f32::from(window.bounds().size.height);
            Some(
                div()
                    .absolute()
                    .top(px(80.))
                    .left(px(12.))
                    .w(px(320.))
                    .max_w(px(popover_max_w(win_w)))
                    .max_h(px(popover_max_h(win_h)))
                    .overflow_y_scrollbar()
                    .p_4()
                    .rounded(px(12.))
                    .bg(cx.theme().popover)
                    .border_1()
                    .border_color(cx.theme().border)
                    .shadow_lg()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_size(px(14.))
                                    .font_weight(gpui_kit::FontWeight::BOLD)
                                    .text_color(fg)
                                    .child("Personal Vault"),
                            )
                            .child(
                                Button::new("close-vault-info")
                                    .ghost()
                                    .small()
                                    .label("✕")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.vault_info_open = false;
                                        cx.notify();
                                    })),
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child("All hosts, keys, and credentials are saved locally on your device with 100% privacy."),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .p_2()
                            .rounded_md()
                            .bg(cx.theme().background)
                            .border_1()
                            .border_color(cx.theme().border)
                            .child(div().text_xs().text_color(muted).child("Location: ~/.config/sshdeck/hosts.json"))
                            .child(div().text_xs().text_color(fg).child(format!("Total hosts: {host_count}")))
                            .child(div().text_xs().text_color(rgb(0x21b568)).child("Network: 100% Offline (0 cloud tracking)")),
                    ),
            )
        } else {
            None
        };

        div()
            .flex()
            .flex_row()
            .flex_1()
            .h_full()
            .min_h(px(0.))
            .overflow_hidden()
            .relative()
            .bg(content_bg)
            .child(centre)
            .when_some(details, |el, panel| {
                if overlay_drawer {
                    // Narrow window: the drawer floats over the grid at full
                    // remaining width instead of squeezing it.
                    el.child(div().absolute().top_0().right_0().bottom_0().child(panel))
                } else {
                    el.child(panel)
                }
            })
            .when_some(vault_info_modal, |el, modal| el.child(modal))
            .into_any_element()
    }

    fn render_host_card(
        &mut self,
        host: &Host,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let is_selected = self.selected.as_ref() == Some(&host.id);
        let connect_host = host.clone();
        let select_id = host.id.clone();
        let id = host.id.clone();
        let muted = rgb(0x8d9ba3);
        let fg = rgb(0x1d2033);
        let card_bg = rgb(0xffffff);
        let border_selected = rgb(0x2091f6);
        let border_default = rgb(0xe2e8f0);
        let orange = rgb(0xe95420);
        let is_list = self.view_mode == ViewMode::List;
        let win_w = f32::from(window.bounds().size.width);
        let card_h = host_card_height(is_list, win_w);

        let tags_str = host_subtitle(host);

        let tint = host_os_tint(host, orange.into());

        div()
            .id(SharedString::from(format!("vault-card-{id}")))
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .flex_1()
            .max_w(px(380.))
            .min_w(px(0.))
            .overflow_hidden()
            .h(px(card_h))
            .px_3p5()
            .rounded(px(12.))
            .bg(card_bg)
            .border_1()
            .border_color(if is_selected {
                border_selected
            } else {
                border_default
            })
            .when(is_selected, |el| el.border_2())
            .shadow_xs()
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, window, cx| {
                this.selected = Some(select_id.clone());
                this.details_open = true;
                this.sync_details_inputs(window, cx);
                cx.notify();
            }))
            .on_double_click(cx.listener(move |this, _, window, cx| {
                this.connect(connect_host.clone(), window, cx);
            }))
            .child(
                div()
                    .size(if is_list { px(32.) } else { px(40.) })
                    .rounded(px(10.))
                    .bg(tint)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        Icon::default()
                            .data(glyph::UBUNTU_SOLID)
                            .size(if is_list { px(18.) } else { px(22.) })
                            .text_color(rgb(0xffffff)),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w(px(0.))
                    .overflow_hidden()
                    .child(
                        div()
                            .text_size(px(14.))
                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            .text_color(fg)
                            .truncate()
                            .child(SharedString::from(host.label.clone())),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(muted)
                            .truncate()
                            .child(SharedString::from(tags_str)),
                    ),
            )
            .into_any_element()
    }

    /// Full-panel theme browser: back header plus one h56 row per scheme.
    ///
    /// The inline dropdown could not show swatches, so the theme row
    /// navigates here instead. Rows reuse the `SCHEMES` registry and the same
    /// apply path as the sidebar theme list.
    fn render_theme_browser(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let fg = cx.theme().foreground;
        let muted = cx.theme().muted_foreground;
        let accent = cx.theme().primary;
        let card_bg = cx.theme().popover;
        let selected = self.selected_theme.clone();
        div()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .overflow_y_scrollbar()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new("theme-browser-back")
                            .ghost()
                            .small()
                            .label("‹ Back")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.theme_picker_open = false;
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .text_size(px(17.))
                            .font_weight(gpui_kit::FontWeight::BOLD)
                            .text_color(fg)
                            .child("Select Color Theme"),
                    ),
            )
            .children(SCHEMES.iter().map(|(name, scheme)| {
                let chosen = *name == selected;
                let swatch_bg: Hsla = scheme.background().into();
                let swatch_fg: Hsla = scheme.foreground().into();
                let theme_name = name.to_string();
                div()
                    .id(SharedString::from(format!("details-theme-{name}")))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_3()
                    .w_full()
                    .h(px(56.))
                    .p_2()
                    .rounded(px(14.))
                    .bg(card_bg)
                    .when(chosen, |s| s.border_1().border_color(accent))
                    .cursor_pointer()
                    .hover(|s| s.bg(cx.theme().muted))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.selected_theme = theme_name.clone();
                        this.theme_picker_open = false;
                        this.apply_terminal_scheme(window, cx);
                        cx.notify();
                    }))
                    .child(
                        div()
                            .w(px(64.))
                            .h(px(40.))
                            .flex_shrink_0()
                            .rounded(px(6.))
                            .bg(swatch_bg)
                            .border_1()
                            .border_color(swatch_fg)
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(div().text_size(px(10.)).text_color(swatch_fg).child("$▮")),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .flex_1()
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(fg)
                                    .child(SharedString::from(name.to_string())),
                            )
                            .child(div().text_xs().text_color(muted).child("16 colors")),
                    )
                    .when(chosen, |s| {
                        s.child(Icon::new(IconName::Check).text_color(accent))
                    })
            }))
            .into_any_element()
    }

    fn render_host_details_panel(
        &mut self,
        details_w: f32,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let border = cx.theme().border;
        let muted = cx.theme().muted_foreground;
        let fg = cx.theme().foreground;
        let card_bg = cx.theme().popover;
        let orange = rgb(0xe95420);
        let win_size = window.bounds().size;
        let win_w = f32::from(win_size.width);
        let win_h = f32::from(win_size.height);
        let narrow = use_full_drawer(win_w);
        let pop_max_w = popover_max_w(win_w);
        let pop_max_h = popover_max_h(win_h);

        let selected_host = self
            .selected
            .as_ref()
            .and_then(|id| self.store.inventory().get(id).cloned());

        let menu_open = self.details_menu_open;
        let header = div()
            .flex()
            .flex_col()
            .w_full()
            .relative()
            .border_b_1()
            .border_color(border)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .w_full()
                    .p_3()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .text_size(px(17.))
                                    .font_weight(gpui_kit::FontWeight::BOLD)
                                    .text_color(fg)
                                    .child("Host Details"),
                            )
                            .child(div().text_xs().text_color(muted).child("Personal vault ▾")),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_1()
                            .child(
                                Button::new("details-overflow")
                                    .ghost()
                                    .icon(IconName::Ellipsis)
                                    .tooltip("More actions")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.details_menu_open = !this.details_menu_open;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("details-collapse")
                                    .ghost()
                                    .icon(
                                        Icon::default().data(glyph::COLLAPSE_DRAWER).size(px(16.)),
                                    )
                                    .tooltip("Collapse")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.details_open = false;
                                        this.details_menu_open = false;
                                        this.selected = None;
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
            .when(menu_open, |el| {
                let host_for_action = selected_host.clone();
                el.child(
                    div()
                        .absolute()
                        .top(px(60.))
                        // Flip/clamp on narrow windows: stretch to both edges
                        // instead of overflowing off the right.
                        .when(!narrow, |el| el.right(px(12.)).w(px(220.)))
                        .when(narrow, |el| el.left(px(12.)).right(px(12.)))
                        .max_w(px(pop_max_w))
                        .max_h(px(pop_max_h))
                        .overflow_y_scrollbar()
                        .flex()
                        .flex_col()
                        .p_2()
                        .rounded(px(12.))
                        .bg(cx.theme().popover)
                        .border_1()
                        .border_color(border)
                        .shadow_lg()
                        .gap_1()
                        .child(
                            Button::new("menu-connect")
                                .ghost()
                                .small()
                                .w_full()
                                .icon(Icon::default().data(glyph::PLUG).size(px(16.)))
                                .label("Connect")
                                .on_click(cx.listener({
                                    let host = host_for_action.clone();
                                    move |this, _, window, cx| {
                                        this.details_menu_open = false;
                                        if let Some(host) = host.clone() {
                                            this.connect(host, window, cx);
                                        }
                                    }
                                })),
                        )
                        .child(
                            Button::new("menu-telnet")
                                .ghost()
                                .small()
                                .w_full()
                                .icon(Icon::default().data(glyph::HOST).size(px(16.)))
                                .label("Add Telnet")
                                .on_click(cx.listener({
                                    let host = host_for_action.clone();
                                    move |this, _, window, cx| {
                                        this.details_menu_open = false;
                                        if let Some(mut h) = host.clone() {
                                            if !h.protocols.iter().any(|p| p == "telnet") {
                                                h.protocols.push("telnet".to_string());
                                                this.store.inventory_mut().upsert(h);
                                                let _ = this.store.save();
                                                window.push_notification(
                                                    Notification::success(
                                                        "Added Telnet protocol to host",
                                                    ),
                                                    cx,
                                                );
                                                cx.notify();
                                            } else {
                                                window.push_notification(
                                                    Notification::info(
                                                        "Host already has Telnet configured",
                                                    ),
                                                    cx,
                                                );
                                            }
                                        }
                                    }
                                })),
                        )
                        .child(
                            Button::new("menu-duplicate")
                                .ghost()
                                .small()
                                .w_full()
                                .icon(Icon::default().data(glyph::COPY).size(px(16.)))
                                .label("Duplicate")
                                .on_click(cx.listener({
                                    let host = host_for_action.clone();
                                    move |this, _, window, cx| {
                                        this.details_menu_open = false;
                                        if let Some(host) = host.clone() {
                                            let mut copy = host.clone();
                                            copy.label = format!("{} (Copy)", host.label);
                                            let copy_id = this.store.inventory_mut().insert(copy);
                                            let _ = this.store.save();
                                            this.selected = Some(copy_id);
                                            this.sync_details_inputs(window, cx);
                                            window.push_notification(
                                                Notification::success("Host duplicated"),
                                                cx,
                                            );
                                            cx.notify();
                                        }
                                    }
                                })),
                        )
                        .child(
                            Button::new("menu-remove")
                                .ghost()
                                .small()
                                .w_full()
                                .icon(Icon::default().data(glyph::TRASH).size(px(16.)))
                                .label("Remove")
                                .on_click(cx.listener({
                                    let host = host_for_action.clone();
                                    move |this, _, window, cx| {
                                        this.details_menu_open = false;
                                        if let Some(host) = host.clone() {
                                            this.remove_host(&host.id, window, cx);
                                        }
                                    }
                                })),
                        ),
                )
            });

        let popover_open = self.credentials_popover_open;
        let show_more = self.show_more;
        let theme_picker_open = self.theme_picker_open;
        let selected_theme = self.selected_theme.clone();

        let (mut body, mut connect_btn): (AnyElement, Option<AnyElement>) = match selected_host
            .clone()
        {
            None => (
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .p_6()
                    .text_color(muted)
                    .child("Select a host to see details")
                    .into_any_element(),
                None,
            ),
            Some(host) => {
                let host_for_connect = host.clone();
                (
                    div()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .p_3()
                        .overflow_y_scrollbar()
                        // Card 1: Address
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_2()
                                .p_3()
                                .rounded(px(14.))
                                .bg(card_bg)
                                .child(
                                    div()
                                        .text_sm()
                                        .font_weight(gpui_kit::FontWeight::MEDIUM)
                                        .text_color(fg)
                                        .child("Address"),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .items_center()
                                        .gap_3()
                                        .child(
                                            div()
                                                .size(px(40.))
                                                .rounded(px(8.))
                                                .bg(orange)
                                                .flex()
                                                .items_center()
                                                .justify_center()
                                                .child(
                                                    Icon::default()
                                                        .data(glyph::UBUNTU_SOLID)
                                                        .size(px(24.))
                                                        .text_color(rgb(0xffffff)),
                                                ),
                                        )
                                        .child(
                                            details_box()
                                                .flex_1()
                                                .child(
                                                    Input::new(&self.details_address)
                                                        .small()
                                                        .appearance(false)
                                                        .flex_1(),
                                                ),
                                        ),
                                ),
                        )
                        // Card 2: General
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_2()
                                .p_3()
                                .rounded(px(14.))
                                .bg(card_bg)
                                .child(
                                    div()
                                        .text_sm()
                                        .font_weight(gpui_kit::FontWeight::MEDIUM)
                                        .text_color(fg)
                                        .child("General"),
                                )
                                .child(
                                    details_box().child(
                                        Input::new(&self.details_label)
                                            .small()
                                            .appearance(false)
                                            .flex_1(),
                                    ),
                                )
                                .child(
                                    details_box()
                                        .child(
                                            Icon::default()
                                                .data(glyph::FOLDER)
                                                .size(px(14.))
                                                .text_color(muted),
                                        )
                                        .child(
                                            Input::new(&self.details_group)
                                                .small()
                                                .appearance(false)
                                                .flex_1(),
                                        ),
                                )
                                .child(
                                    details_box()
                                        .child(
                                            Icon::default()
                                                .data(glyph::TAG)
                                                .size(px(14.))
                                                .text_color(muted),
                                        )
                                        .child(
                                            Input::new(&self.details_tags)
                                                .small()
                                                .appearance(false)
                                                .flex_1(),
                                        ),
                                )
                                // Key-setting row: Backspace Default at bottom of General card
                                .child(
                                    details_box()
                                        .justify_between()
                                        .child(
                                            div()
                                                .flex()
                                                .flex_row()
                                                .items_center()
                                                .gap_2()
                                                .child(
                                                    Icon::default()
                                                        .data(glyph::BACKSPACE)
                                                        .size(px(14.))
                                                        .text_color(muted),
                                                )
                                                .child(
                                                    div()
                                                        .text_sm()
                                                        .text_color(muted)
                                                        .child("Backspace"),
                                                ),
                                        )
                                        .child(
                                            div()
                                                .text_sm()
                                                .text_color(muted)
                                                .child("Default"),
                                        ),
                                )
                        )
                        // Share this host: its own card, not a row of General.
                        .child(
                            div()
                                .id("share-this-host-btn")
                                .flex()
                                .flex_row()
                                .items_center()
                                .justify_center()
                                .gap_2()
                                .p_3()
                                .rounded(px(14.))
                                .bg(card_bg)
                                .text_color(rgb(0x2091f6))
                                .cursor_pointer()
                                // Termius list-hover #f0f3f5.
                                .hover(|s| s.bg(rgb(0xf0f3f5)))
                                .child(Icon::default().data(glyph::SHARE).size(px(16.)))
                                .child(
                                    div()
                                        .text_size(px(14.))
                                        .font_weight(gpui_kit::FontWeight::MEDIUM)
                                        .child("Share this host"),
                                )
                                .on_click(cx.listener({
                                    let cmd = format!("ssh -p {} {}@{}", host.port, host.username, host.address);
                                    move |_, _, window, cx| {
                                        cx.write_to_clipboard(ClipboardItem::new_string(cmd.clone()));
                                        window.push_notification(
                                            Notification::success(format!("Copied to clipboard: {cmd}")),
                                            cx,
                                        );
                                    }
                                })),
                        )
                        // Card 3: SSH & Credentials
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_2p5()
                                .p_3()
                                .rounded(px(14.))
                                .bg(card_bg)
                                .child(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .items_center()
                                        .gap_1p5()
                                        .child(
                                            div()
                                                .text_sm()
                                                .font_weight(gpui_kit::FontWeight::MEDIUM)
                                                .text_color(fg)
                                                .child("SSH on"),
                                        )
                                        .child(
                                            div()
                                                .flex()
                                                .flex_row()
                                                .items_center()
                                                .w(px(60.))
                                                .h(px(28.))
                                                .px_2()
                                                .rounded(px(6.))
                                                .border_1()
                                                .border_color(rgb(0xd5dde0))
                                                .child(
                                                    Input::new(&self.details_port)
                                                        .small()
                                                        .appearance(false)
                                                        .flex_1(),
                                                ),
                                        )
                                        .child(
                                            div()
                                                .text_sm()
                                                .font_weight(gpui_kit::FontWeight::MEDIUM)
                                                .text_color(fg)
                                                .child("port"),
                                        ),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .font_weight(gpui_kit::FontWeight::MEDIUM)
                                        .text_color(muted)
                                        .child("Credentials"),
                                )
                                .child(
                                    details_box()
                                        .child(
                                            Icon::new(IconName::User)
                                                .size(px(14.))
                                                .text_color(muted),
                                        )
                                        .child(
                                            Input::new(&self.details_username)
                                                .small()
                                                .appearance(false)
                                                .flex_1(),
                                        )
                                        .child(
                                            Button::new("btn-identity-picker")
                                                .ghost()
                                                .small()
                                                .icon(
                                                    Icon::default()
                                                        .data(glyph::PENCIL)
                                                        .size(px(14.)),
                                                )
                                                .on_click(cx.listener(
                                                    |this, _, window, cx| {
                                                        this.select_left_nav(
                                                            LeftNav::Keychain,
                                                            window,
                                                            cx,
                                                        );
                                                        let pane = this.ensure_keys_pane(
                                                            window, cx,
                                                        );
                                                        pane.update(cx, |p, cx| {
                                                            p.open_identity_picker(
                                                                window, cx,
                                                            )
                                                        });
                                                        cx.notify();
                                                    },
                                                )),
                                        ),
                                )
                                .child(
                                    details_box()
                                        .child(
                                            Icon::default()
                                                .data(glyph::KEY)
                                                .size(px(14.))
                                                .text_color(muted),
                                        )
                                        .child(
                                            Input::new(&self.details_password)
                                                .small()
                                                .appearance(false)
                                                .mask_toggle()
                                                .flex_1(),
                                        ),
                                )
                                .child(
                                    div().flex().flex_row().items_center().pt_1().child(
                                        Button::new("btn-ssh-id")
                                            .ghost()
                                            .small()
                                            .label("+ SSH ID, Key, Certificate, FIDO2")
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.credentials_popover_open =
                                                    !this.credentials_popover_open;
                                                cx.notify();
                                            })),
                                    ),
                                )
                                .when(popover_open, |el| {
                                    el.child(
                                        div()
                                            .flex()
                                            .flex_col()
                                            .w_full()
                                            .max_w(px(pop_max_w))
                                            .max_h(px(pop_max_h))
                                            .overflow_y_scrollbar()
                                            .p_3()
                                            .rounded(px(12.))
                                            .bg(cx.theme().popover)
                                            .border_1()
                                            .border_color(border)
                                            .shadow_lg()
                                            .gap_1()
                                            .child(
                                                div()
                                                    .flex()
                                                    .flex_row()
                                                    .items_center()
                                                    .justify_between()
                                                    .w_full()
                                                    .pb_1()
                                                    .child(
                                                        div()
                                                            .flex()
                                                            .flex_col()
                                                            .gap_0p5()
                                                            .child(
                                                                div()
                                                                    .text_sm()
                                                                    .font_weight(gpui_kit::FontWeight::MEDIUM)
                                                                    .text_color(fg)
                                                                    .child("SSH ID"),
                                                            )
                                                            .child(
                                                                div()
                                                                    .text_xs()
                                                                    .text_color(muted)
                                                                    .child("Passkeys for SSH"),
                                                            ),
                                                    )
                                                    // Lavender Set-up pill.
                                                    .child(
                                                        div()
                                                            .id("creds-setup-pill")
                                                            .px_2()
                                                            .py_0p5()
                                                            .rounded_full()
                                                            .bg(rgb(0xe8e8ff))
                                                            .text_xs()
                                                            .text_color(rgb(0x6666d2))
                                                            .cursor_pointer()
                                                            .child("Set up")
                                                            .on_click(cx.listener(
                                                                |this, _, window, cx| {
                                                                    this.credentials_popover_open =
                                                                        false;
                                                                    this.select_left_nav(
                                                                        LeftNav::Keychain,
                                                                        window,
                                                                        cx,
                                                                    );
                                                                },
                                                            )),
                                                    ),
                                            )
                                            .child(
                                                Button::new("add-ssh-key")
                                                    .ghost()
                                                    .small()
                                                    .w_full()
                                                    .icon(
                                                        Icon::default().data(glyph::KEY).size(px(16.)),
                                                    )
                                                    .label("Key")
                                                    .on_click(cx.listener(|this, _, window, cx| {
                                                        this.credentials_popover_open = false;
                                                        this.select_left_nav(LeftNav::Keychain, window, cx);
                                                    })),
                                            )
                                            .child(
                                                Button::new("add-cert")
                                                    .ghost()
                                                    .small()
                                                    .w_full()
                                                    .icon(
                                                        Icon::default()
                                                            .data(glyph::CERTIFICATE)
                                                            .size(px(16.)),
                                                    )
                                                    .label("Certificate")
                                                    .on_click(cx.listener(|this, _, window, cx| {
                                                        this.credentials_popover_open = false;
                                                        this.select_left_nav(LeftNav::Keychain, window, cx);
                                                    })),
                                            )
                                            .child(
                                                Button::new("add-fido2")
                                                    .ghost()
                                                    .small()
                                                    .w_full()
                                                    .icon(
                                                        Icon::default()
                                                            .data(glyph::SECURITY_KEY)
                                                            .size(px(16.)),
                                                    )
                                                    .label("FIDO2")
                                                    .on_click(cx.listener(|this, _, window, cx| {
                                                        this.credentials_popover_open = false;
                                                        this.select_left_nav(LeftNav::Keychain, window, cx);
                                                    })),
                                            ),
                                    )
                                })
                                // Show more lives in this card, not its own.
                                .child(
                                    Button::new("btn-show-more")
                                        .ghost()
                                        .small()
                                        .label(if show_more {
                                            "Show less ▴"
                                        } else {
                                            "Show more ▾"
                                        })
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.show_more = !this.show_more;
                                            cx.notify();
                                        })),
                                )
                                .when(show_more, |el| {
                                    // Agent Forwarding and Host Chaining read
                                    // live model state (`AuthMethod::Agent`,
                                    // `Host::proxy_jump`). The rest have no
                                    // `Host` field, so they render as dimmed
                                    // non-interactive disclosures, never as
                                    // controls that look settable but write
                                    // nowhere.
                                    let more_row = |icon: &'static [u8],
                                                    label: &str,
                                                    value: Option<String>,
                                                    live: bool|
                                     -> AnyElement {
                                        details_box()
                                            .justify_between()
                                            .when(!live, |s| s.opacity(0.55))
                                            .child(
                                                div()
                                                    .flex()
                                                    .flex_row()
                                                    .items_center()
                                                    .gap_2()
                                                    .flex_shrink_0()
                                                    .child(
                                                        Icon::default()
                                                            .data(icon)
                                                            .size(px(14.))
                                                            .text_color(muted),
                                                    )
                                                    .child(
                                                        div()
                                                            .text_sm()
                                                            .text_color(muted)
                                                            .child(label.to_string()),
                                                    ),
                                            )
                                            .when_some(value, |s, v| {
                                                s.child(
                                                    div()
                                                        .text_sm()
                                                        .text_color(fg)
                                                        .min_w(px(0.))
                                                        .truncate()
                                                        .child(v),
                                                )
                                            })
                                            .into_any_element()
                                    };
                                    let agent_forwarding =
                                        if matches!(host.auth, sshdeck_core::AuthMethod::Agent) {
                                            "Enabled".to_string()
                                        } else {
                                            "Disabled".to_string()
                                        };
                                    el.child(
                                        div()
                                            .flex()
                                            .flex_col()
                                            .gap_2()
                                            .pt_2()
                                            .border_t_1()
                                            .border_color(cx.theme().sidebar_border)
                                            .child(more_row(
                                                glyph::KEY,
                                                "Agent Forwarding",
                                                Some(agent_forwarding),
                                                true,
                                            ))
                                            // ponytail: `Host` has no
                                            // per-host startup-snippet field;
                                            // the row stays a dimmed
                                            // disclosure until the model
                                            // grows one. Upgrade by adding
                                            // the field and passing its value.
                                            .child(more_row(
                                                glyph::SNIPPET,
                                                "Startup snippet",
                                                None,
                                                false,
                                            ))
                                            .child(more_row(
                                                glyph::HOST,
                                                "Host Chaining",
                                                host.proxy_jump.clone(),
                                                host.proxy_jump.is_some(),
                                            ))
                                            // ponytail: command-like jumps are
                                            // refused by `sshdeck_core::jump`
                                            // (`ChainError::ProxyCommandSkipped`);
                                            // `Host` carries no ProxyCommand.
                                            .child(more_row(glyph::FORWARD, "Proxy", None, false))
                                            // ponytail: `Host` has no env,
                                            // charset, or mosh fields; dimmed
                                            // disclosures until it does.
                                            .child(more_row(
                                                glyph::CODE,
                                                "Environment Variable",
                                                None,
                                                false,
                                            ))
                                            .child(more_row(
                                                glyph::TERMINAL_PROMPT,
                                                "UTF-8",
                                                None,
                                                false,
                                            ))
                                            .child(more_row(
                                                glyph::PLUG,
                                                "Mosh",
                                                Some("Disabled".to_string()),
                                                false,
                                            ))
                                            .child(
                                                details_box()
                                                    .id("btn-terminal-theme-select")
                                                    .cursor_pointer()
                                                    .hover(|s| s.bg(cx.theme().muted))
                                                    .on_click(cx.listener(
                                                        |this, _, _, cx| {
                                                            this.theme_picker_open = true;
                                                            cx.notify();
                                                        },
                                                    ))
                                                    .child(
                                                        // Termius terminal preview
                                                        // (#1d2033) with green bars.
                                                        div()
                                                            .w(px(56.))
                                                            .h(px(36.))
                                                            .rounded(px(6.))
                                                            .bg(rgb(0x1d2033))
                                                            .flex()
                                                            .flex_col()
                                                            .justify_center()
                                                            .gap_1()
                                                            .px_2()
                                                            .child(
                                                                div()
                                                                    .w(px(28.))
                                                                    .h(px(3.))
                                                                    .rounded_full()
                                                                    .bg(rgb(0x21b568)),
                                                            )
                                                            .child(
                                                                div()
                                                                    .w(px(36.))
                                                                    .h(px(3.))
                                                                    .rounded_full()
                                                                    .bg(rgb(0x21b568)),
                                                            )
                                                            .child(
                                                                div()
                                                                    .w(px(20.))
                                                                    .h(px(3.))
                                                                    .rounded_full()
                                                                    .bg(rgb(0x21b568)),
                                                            ),
                                                    )
                                                    .child(
                                                        div()
                                                            .text_sm()
                                                            .text_color(fg)
                                                            .flex_1()
                                                            .truncate()
                                                            .child(selected_theme.clone()),
                                                    )
                                                    .child(
                                                        div()
                                                            .text_sm()
                                                            .text_color(muted)
                                                            .child("›"),
                                                    ),
                                            )
                                    )
                                }),
                        )
                        // Protocol row lives in the scroll body; the footer
                        // keeps only the pinned Connect CTA.
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .justify_center()
                                .p_3()
                                .rounded(px(14.))
                                .bg(card_bg)
                                .child(
                                    Button::new("details-add-telnet")
                                        .ghost()
                                        .label("⊕ Add Telnet")
                                        .w_full()
                                        .on_click(cx.listener({
                                            let host_id = host.id.clone();
                                            move |this, _, window, cx| {
                                                if let Some(mut h) = this.store.inventory().get(&host_id).cloned() {
                                                    if !h.protocols.iter().any(|p| p == "telnet") {
                                                        h.protocols.push("telnet".to_string());
                                                        this.store.inventory_mut().upsert(h);
                                                        let _ = this.store.save();
                                                        window.push_notification(
                                                            Notification::success("Added Telnet protocol to host"),
                                                            cx,
                                                        );
                                                        cx.notify();
                                                    } else {
                                                        window.push_notification(
                                                            Notification::info("Host already has Telnet configured"),
                                                            cx,
                                                        );
                                                    }
                                                }
                                            }
                                        })),
                                ),
                        )
                        .into_any_element(),
                    Some(
                        Button::new("details-connect")
                            .primary()
                            .rounded(px(10.))
                            .with_size(Size::Size(px(17.)))
                            .label("Connect")
                            .w_full()
                            .h(px(46.))
                            .bg(rgb(0x2091f6))
                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.connect(host_for_connect.clone(), window, cx);
                            }))
                            .into_any_element(),
                    ),
                )
            }
        };

        // Full-panel theme browser replaces the scroll body (and its footer
        // Connect, which belongs to the details view) when open.
        if theme_picker_open && selected_host.is_some() {
            body = self.render_theme_browser(cx);
            connect_btn = None;
        }

        div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .w(px(details_w))
            .min_w(px(0.))
            .h_full()
            .bg(cx.theme().accent)
            .border_l_1()
            .border_color(border)
            .overflow_hidden()
            .child(header)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_hidden()
                    .child(body),
            )
            .when_some(connect_btn, |this, btn| {
                this.child(
                    div()
                        .flex_shrink_0()
                        .p_3()
                        .border_t_1()
                        .border_color(border)
                        .child(btn),
                )
            })
            .into_any_element()
    }

    /// The New Tab screen (Screenshot 12): search box with ⌘+K, recent connections, host cards.
    fn render_new_tab(&mut self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let content_bg = cx.theme().accent; // #edf1f2 in light
        let fg = cx.theme().foreground;
        let muted = cx.theme().muted_foreground;
        let card_bg = cx.theme().popover;
        let win_w = f32::from(window.bounds().size.width);

        let query = self.new_tab_query.read(cx).value().to_string();
        let filtered: Vec<Host> = self
            .store
            .inventory()
            .filtered(&query)
            .into_iter()
            .cloned()
            .collect();

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(content_bg)
            .overflow_y_scrollbar()
            // Full padding on normal windows, tighter below ~700px so the
            // centred column keeps its width.
            .p_4()
            .when(win_w >= 700.0, |el| el.p_8())
            .child(
                div()
                    .flex()
                    .flex_col()
                    .w_full()
                    .min_w(px(0.))
                    .max_w(px(720.))
                    .mx_auto()
                    .gap_6()
                    // Search bar with ⌘+K: one flat `#e8edf0` surface, no inner
                    // Input box (`bordered(false)`); the blue focus ring is
                    // the Input's own (`focus_bordered`, on by default).
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .w_full()
                            .px_4()
                            .h(px(44.))
                            .rounded(px(10.))
                            .bg(rgb(0xe8edf0))
                            .child(
                                div().flex_1().child(
                                    Input::new(&self.new_tab_query)
                                        .bordered(false)
                                        .cleanable(true),
                                ),
                            )
                            .child(
                                div()
                                    .id("btn-new-tab-cmd-k")
                                    .px_2()
                                    .py_1()
                                    .rounded(px(4.))
                                    .bg(cx.theme().muted)
                                    .text_xs()
                                    .text_color(muted)
                                    .cursor_pointer()
                                    .hover(|s| s.bg(cx.theme().border))
                                    .child("⌘+K")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.open_palette(window, cx);
                                    })),
                            ),
                    )
                    // Recent connections header
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_size(px(16.))
                                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                    .text_color(fg)
                                    .child("Recent connections"),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap_2()
                                    // `#e6ebed` pills behind the ghost buttons.
                                    .child(
                                        div().bg(rgb(0xe6ebed)).rounded(px(8.)).child(
                                            Button::new("new-tab-terminal")
                                                .ghost()
                                                .small()
                                                .icon(
                                                    Icon::default()
                                                        .data(glyph::TERMINAL_PROMPT)
                                                        .size(px(14.)),
                                                )
                                                .label("Terminal")
                                                .tooltip("Open Local Terminal")
                                                .on_click(cx.listener(|this, _, window, cx| {
                                                    this.open_local_terminal(window, cx);
                                                })),
                                        ),
                                    )
                                    .child(
                                        div().bg(rgb(0xe6ebed)).rounded(px(8.)).child(
                                            Button::new("new-tab-workspace")
                                                .ghost()
                                                .small()
                                                .label("Create a workspace")
                                                .on_click(cx.listener(|this, _, window, cx| {
                                                    this.select_tab(MainTab::Workspace, window, cx);
                                                })),
                                        ),
                                    )
                                    .child(
                                        div().bg(rgb(0xe6ebed)).rounded(px(8.)).child(
                                            Button::new("new-tab-restore")
                                                .ghost()
                                                .small()
                                                .label("Restore")
                                                .on_click(cx.listener(|this, _, window, cx| {
                                                    if let Some(host_id) =
                                                        this.last_session_host.clone()
                                                    {
                                                        if let Some(host) = this
                                                            .store
                                                            .inventory()
                                                            .get(&host_id)
                                                            .cloned()
                                                        {
                                                            this.connect(host, window, cx);
                                                            return;
                                                        }
                                                    }
                                                    if let Some(first) = this
                                                        .store
                                                        .inventory()
                                                        .hosts()
                                                        .first()
                                                        .cloned()
                                                    {
                                                        this.connect(first, window, cx);
                                                    } else {
                                                        window.push_notification(
                                                            Notification::warning(
                                                                "No host available to restore",
                                                            ),
                                                            cx,
                                                        );
                                                    }
                                                })),
                                        ),
                                    ),
                            ),
                    )
                    // Recent connections unified card container matching Termius 3.09.49 PM.
                    .child({
                        let filtered_count = filtered.len();
                        div()
                            .flex()
                            .flex_col()
                            .rounded(px(14.))
                            .bg(card_bg)
                            .border_1()
                            .border_color(cx.theme().border)
                            .shadow_xs()
                            .overflow_hidden()
                            .children(filtered.into_iter().enumerate().map(move |(row, host)| {
                                let connect_host = host.clone();
                                let id = host.id.clone();
                                let is_last = row + 1 == filtered_count;
                                div()
                                    .id(SharedString::from(format!("new-tab-host-{id}")))
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .justify_between()
                                    .gap_2()
                                    .min_w(px(0.))
                                    .px_4()
                                    .py_3()
                                    .when(!is_last, |el| {
                                        el.border_b_1().border_color(rgba(0xd5dde060))
                                    })
                                    .hover(|s| s.bg(rgb(0xf4f7f9)))
                                    .cursor_pointer()
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.connect(connect_host.clone(), window, cx);
                                    }))
                                    .child(
                                        div()
                                            .flex()
                                            .flex_row()
                                            .items_center()
                                            .gap_3()
                                            .flex_1()
                                            .min_w(px(0.))
                                            .overflow_hidden()
                                            .child(
                                                div()
                                                    .size(px(26.))
                                                    .flex_shrink_0()
                                                    .rounded(px(7.))
                                                    .bg(rgb(0xe95420))
                                                    .flex()
                                                    .items_center()
                                                    .justify_center()
                                                    .child(
                                                        Icon::default()
                                                            .data(glyph::UBUNTU_SOLID)
                                                            .size(px(16.))
                                                            .text_color(rgb(0xffffff)),
                                                    ),
                                            )
                                            .child(
                                                div()
                                                    .text_size(px(14.))
                                                    .font_weight(gpui_kit::FontWeight::MEDIUM)
                                                    .text_color(fg)
                                                    .flex_1()
                                                    .min_w(px(0.))
                                                    .truncate()
                                                    .child(SharedString::from(host.label)),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .flex_shrink_0()
                                            .text_size(px(12.))
                                            .text_color(muted)
                                            .child("Personal"),
                                    )
                            }))
                    }),
            )
            .into_any_element()
    }

    /// The SFTP tab: the browser, attached to the focused session. While that
    /// session is still connecting, the connecting rail docks on the right.
    fn render_sftp(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let browser = match self.sftp_pane.clone() {
            Some(pane) => div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w(px(0.))
                .size_full()
                .overflow_hidden()
                .child(pane)
                .into_any_element(),
            None => {
                let muted = cx.theme().muted_foreground;
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .size_full()
                    .child(Icon::new(IconName::FolderOpen).large().text_color(muted))
                    .child(div().text_color(muted).child("SFTP is not open"))
                    .into_any_element()
            }
        };

        let rail = self.active.and_then(|index| {
            self.sessions.get(index).filter(|session| {
                matches!(
                    session.status.state,
                    SessionState::Connecting | SessionState::Authenticating
                )
            })?;
            Some(self.render_connecting_rail(index, window, cx))
        });

        div()
            .flex()
            .flex_row()
            .flex_1()
            .size_full()
            .overflow_hidden()
            .child(browser)
            .when_some(rail, |el, rail| el.child(rail))
            .into_any_element()
    }

    /// The settings or keys pane, with a header that closes it.
    ///
    /// `render_main` calls this only while an overlay is present; the `None`
    /// arm exists so the match is total and the borrow of `self.overlay` ends
    /// before the element tree is built.
    fn render_overlay(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let border = cx.theme().border;
        let (title, body) = match self.overlay.as_ref() {
            Some(Overlay::Settings(view)) => ("Settings", view.clone().into_any_element()),
            Some(Overlay::Keys(view)) => {
                ("SSH keys & known hosts", view.clone().into_any_element())
            }
            None => ("", div().into_any_element()),
        };

        div()
            .flex()
            .flex_col()
            .flex_1()
            .h_full()
            .overflow_hidden()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .flex_shrink_0()
                    .h(px(38.))
                    .px_3()
                    .border_b_1()
                    .border_color(border)
                    .child(div().text_sm().child(SharedString::from(title)))
                    .child(
                        Button::new("overlay-close")
                            .small()
                            .ghost()
                            .label("Close")
                            .on_click(
                                cx.listener(|this, _, window, cx| this.close_overlay(window, cx)),
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_hidden()
                    .child(body),
            )
            .into_any_element()
    }

    /// The live terminal (or connecting screen/empty state) for the focused session,
    /// with the 300pt right sidebar docked beside it while open.
    fn render_session(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;

        // Bring the focused pane's status onto the tab before rendering chrome.
        if let Some(session) = self.active.and_then(|index| self.sessions.get_mut(index)) {
            session.status = session.pane.read(cx).status();
        }

        let content = match self.active {
            Some(index) => {
                if let Some(session) = self.sessions.get(index) {
                    let is_connecting = matches!(
                        session.status.state,
                        SessionState::Connecting | SessionState::Authenticating
                    );
                    if is_connecting {
                        self.render_connecting_screen(index, window, cx)
                    } else {
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .size_full()
                            .overflow_hidden()
                            .child(session.pane.clone())
                            .into_any_element()
                    }
                } else {
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .justify_center()
                        .gap_2()
                        .size_full()
                        .child(Icon::new(IconName::Inbox).large().text_color(muted))
                        .child(div().text_color(muted).child("Select a host to begin"))
                        .into_any_element()
                }
            }
            None => {
                let selected = self
                    .selected
                    .as_ref()
                    .and_then(|id| self.store.inventory().get(id))
                    .cloned();
                match selected {
                    None => div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .justify_center()
                        .gap_2()
                        .size_full()
                        .child(Icon::new(IconName::Inbox).large().text_color(muted))
                        .child(div().text_color(muted).child("Select a host to begin"))
                        .into_any_element(),
                    Some(host) => div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .justify_center()
                        .gap_2()
                        .size_full()
                        .child(Icon::new(IconName::Globe).large().text_color(muted))
                        .child(div().text_color(muted).child("Not connected"))
                        .child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .child(SharedString::from(format!("$ ssh {}", host.endpoint()))),
                        )
                        .into_any_element(),
                }
            }
        };

        let drop_overlay: Option<AnyElement> = if let Some(dragged_sess) = self.dragged_session {
            let dragged_label = if let Some(session) = self.sessions.get(dragged_sess) {
                let host_opt = self.store.inventory().get(&session.host);
                host_opt
                    .map(|h| h.label.clone())
                    .or_else(|| session.status.title.clone())
                    .unwrap_or_else(|| session.host.to_string())
            } else {
                format!("Session {dragged_sess}")
            };

            let cancel_bar = div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .px_4()
                .py_1p5()
                .bg(rgb(0x1d2033))
                .border_b_1()
                .border_color(rgb(0x2091f6))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .text_sm()
                        .text_color(rgb(0xffffff))
                        .child(div().size(px(8.)).rounded_full().bg(rgb(0x2091f6)))
                        .child(SharedString::from(format!(
                            "Tiling \"{dragged_label}\" — Drop or click a zone to run side-by-side"
                        ))),
                )
                .child(
                    Button::new("cancel-session-drag")
                        .ghost()
                        .small()
                        .label("Cancel")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.dragged_session = None;
                            this.dragged_tab = None;
                            this.tab_drag_start = None;
                            cx.notify();
                        })),
                );

            let drop_left = div()
                .id("session-drop-left")
                .flex_1()
                .h_full()
                .bg(rgba(0x2091f625))
                .border_r_2()
                .border_color(rgb(0x2091f6))
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_2()
                .cursor_pointer()
                .hover(|s| s.bg(rgba(0x2091f655)))
                .child(
                    Icon::default()
                        .data(glyph::SPLIT_HORIZONTAL)
                        .size(px(28.))
                        .text_color(rgb(0xffffff)),
                )
                .child(
                    div()
                        .text_size(px(15.))
                        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                        .text_color(rgb(0xffffff))
                        .child("Split Left"),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(0x93c5fd))
                        .child(SharedString::from(format!(
                            "Run \"{dragged_label}\" on left"
                        ))),
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.split_with_dragged_session(dragged_sess, SplitDir::Row, false, window, cx);
                }))
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(move |this, _, window, cx| {
                        this.split_with_dragged_session(
                            dragged_sess,
                            SplitDir::Row,
                            false,
                            window,
                            cx,
                        );
                    }),
                );

            let drop_right = div()
                .id("session-drop-right")
                .flex_1()
                .h_full()
                .bg(rgba(0x2091f625))
                .border_l_2()
                .border_color(rgb(0x2091f6))
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_2()
                .cursor_pointer()
                .hover(|s| s.bg(rgba(0x2091f655)))
                .child(
                    Icon::default()
                        .data(glyph::SPLIT_HORIZONTAL)
                        .size(px(28.))
                        .text_color(rgb(0xffffff)),
                )
                .child(
                    div()
                        .text_size(px(15.))
                        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                        .text_color(rgb(0xffffff))
                        .child("Split Right (Side by Side)"),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(0x93c5fd))
                        .child(SharedString::from(format!(
                            "Run \"{dragged_label}\" on right"
                        ))),
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.split_with_dragged_session(dragged_sess, SplitDir::Row, true, window, cx);
                }))
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(move |this, _, window, cx| {
                        this.split_with_dragged_session(
                            dragged_sess,
                            SplitDir::Row,
                            true,
                            window,
                            cx,
                        );
                    }),
                );

            let drop_top = div()
                .id("session-drop-top")
                .w_full()
                .h(px(60.))
                .bg(rgba(0x2091f620))
                .border_b_2()
                .border_color(rgb(0x2091f6))
                .flex()
                .items_center()
                .justify_center()
                .gap_2()
                .cursor_pointer()
                .hover(|s| s.bg(rgba(0x2091f650)))
                .child(
                    Icon::default()
                        .data(glyph::SPLIT_VERTICAL)
                        .size(px(20.))
                        .text_color(rgb(0xffffff)),
                )
                .child(
                    div()
                        .text_sm()
                        .font_weight(gpui_kit::FontWeight::MEDIUM)
                        .text_color(rgb(0xffffff))
                        .child("Split Top (Stacked)"),
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.split_with_dragged_session(dragged_sess, SplitDir::Col, false, window, cx);
                }))
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(move |this, _, window, cx| {
                        this.split_with_dragged_session(
                            dragged_sess,
                            SplitDir::Col,
                            false,
                            window,
                            cx,
                        );
                    }),
                );

            let drop_bottom = div()
                .id("session-drop-bottom")
                .w_full()
                .h(px(60.))
                .bg(rgba(0x2091f620))
                .border_t_2()
                .border_color(rgb(0x2091f6))
                .flex()
                .items_center()
                .justify_center()
                .gap_2()
                .cursor_pointer()
                .hover(|s| s.bg(rgba(0x2091f650)))
                .child(
                    Icon::default()
                        .data(glyph::SPLIT_VERTICAL)
                        .size(px(20.))
                        .text_color(rgb(0xffffff)),
                )
                .child(
                    div()
                        .text_sm()
                        .font_weight(gpui_kit::FontWeight::MEDIUM)
                        .text_color(rgb(0xffffff))
                        .child("Split Bottom (Stacked)"),
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.split_with_dragged_session(dragged_sess, SplitDir::Col, true, window, cx);
                }))
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(move |this, _, window, cx| {
                        this.split_with_dragged_session(
                            dragged_sess,
                            SplitDir::Col,
                            true,
                            window,
                            cx,
                        );
                    }),
                );

            Some(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .flex_col()
                    .bg(rgba(0x0f172a80))
                    .child(cancel_bar)
                    .child(drop_top)
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_row()
                            .child(drop_left)
                            .child(drop_right),
                    )
                    .child(drop_bottom)
                    .into_any_element(),
            )
        } else {
            None
        };

        let secret = self.render_secret_prompt(cx);
        let sidebar = self.render_session_sidebar(window, cx);

        div()
            .flex()
            .flex_col()
            .flex_1()
            .h_full()
            .min_h(px(0.))
            .overflow_hidden()
            .when_some(secret, |el, prompt| el.child(prompt))
            .child(
                div()
                    .relative()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_hidden()
                    .child(
                        div()
                            .relative()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w(px(0.))
                            .overflow_hidden()
                            .child(content)
                            .when_some(drop_overlay, |el, overlay| el.child(overlay)),
                    )
                    .child(sidebar),
            )
            .into_any_element()
    }

    /// The session right sidebar: a 300pt inset panel (10px radius, theme
    /// background, 7pt margins) with a 40pt 4-icon tab strip and one body per
    /// tab. Hidden entirely while `sidebar_open` is false; the header's panel
    /// button toggles it back. Below [`SIDEBAR_OVERLAY_WIDTH`] it becomes an
    /// overlay drawer (absolute right, bordered, `shadow_lg`) so the terminal
    /// keeps a usable grid instead of squeezing beside a 300pt panel.
    fn render_session_sidebar(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if !self.sidebar_open {
            return div().into_any_element();
        }
        let overlay = f32::from(window.bounds().size.width) < SIDEBAR_OVERLAY_WIDTH;
        let active = self.sidebar_tab;
        // `#223636` has no theme token (see AGENTS.md errata on chrome
        // colours); it is the recovered active-pill fill from the reference.
        let pill = rgb(0x223636);
        let green = cx.theme().success;
        let muted = cx.theme().muted_foreground;

        let tab_button = |id: &'static str,
                          tip: &'static str,
                          icon: Icon,
                          tab: SidebarTab,
                          cx: &mut Context<Self>| {
            let selected = active == tab;
            div()
                .flex_1()
                .flex()
                .flex_row()
                .items_center()
                .justify_center()
                .h(px(32.))
                .rounded(px(8.))
                .when(selected, |el| el.bg(pill))
                .child(
                    Button::new(id)
                        .ghost()
                        .icon(icon.text_color(if selected { green } else { muted }))
                        .tooltip(tip)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if this.sidebar_tab == tab && this.sidebar_open {
                                this.sidebar_open = false;
                            } else {
                                this.sidebar_tab = tab;
                                this.sidebar_open = true;
                            }
                            cx.notify();
                        })),
                )
        };

        let body = match active {
            SidebarTab::Snippets => self.render_sidebar_snippets(window, cx),
            SidebarTab::History => self.render_sidebar_history(cx),
            SidebarTab::Autocomplete => self.render_sidebar_autocomplete(cx),
            SidebarTab::Appearance => self.render_sidebar_appearance(cx),
        };

        div()
            .w(px(300.))
            .flex_shrink_0()
            .h_full()
            .when(!overlay, |el| el.py(px(7.)).pr(px(7.)))
            // Overlay drawer: absolute right over the content with a border and
            // shadow; the parent row is `relative`, and closing is the same
            // header toggle that closed the inset panel.
            .when(overlay, |el| {
                el.absolute()
                    .top_0()
                    .right_0()
                    .py(px(0.))
                    .pr(px(0.))
                    .border_l_1()
                    .border_color(cx.theme().border)
                    .shadow_lg()
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .h_full()
                    .w_full()
                    .rounded(px(10.))
                    .bg(cx.theme().background)
                    .overflow_hidden()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .flex_shrink_0()
                            .h(px(40.))
                            .px_2()
                            .gap_1()
                            .border_b_1()
                            .border_color(cx.theme().border)
                            // Reference order: rocket (autocomplete), braces
                            // (snippets), clock (history), palette
                            // (appearance). Rocket, braces, and clock ship as
                            // raw-SVG `Icon::data` glyphs — the bundled asset
                            // set has no such icons — while palette reuses
                            // the bundled `IconName::Palette`.
                            .child(tab_button(
                                "side-tab-autocomplete",
                                "Autocomplete",
                                Icon::default().data(glyph::ROCKET),
                                SidebarTab::Autocomplete,
                                cx,
                            ))
                            .child(tab_button(
                                "side-tab-snippets",
                                "Snippets",
                                Icon::default().data(glyph::BRACES),
                                SidebarTab::Snippets,
                                cx,
                            ))
                            .child(tab_button(
                                "side-tab-history",
                                "History",
                                Icon::default().data(glyph::CLOCK),
                                SidebarTab::History,
                                cx,
                            ))
                            .child(tab_button(
                                "side-tab-appearance",
                                "Appearance",
                                Icon::new(IconName::Palette),
                                SidebarTab::Appearance,
                                cx,
                            ))
                            .child(
                                div().flex_shrink_0().child(
                                    Button::new("side-close-btn")
                                        .ghost()
                                        .icon(
                                            Icon::new(IconName::Close)
                                                .size(px(14.))
                                                .text_color(muted),
                                        )
                                        .tooltip("Close sidebar (⌘B)")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.sidebar_open = false;
                                            cx.notify();
                                        })),
                                ),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_h(px(0.))
                            .flex()
                            .flex_col()
                            .overflow_y_scrollbar()
                            .child(body),
                    ),
            )
            .into_any_element()
    }

    /// One sidebar row: 14px label, 12px muted description, right-aligned
    /// control, hairline below. Mirrors the settings surface's row shape.
    /// 44px min height is the touch target and is kept on all widths; labels
    /// truncate with `min_w(0)` so long text never overflows a narrow panel.
    fn sidebar_row(
        label: &'static str,
        description: &'static str,
        control: impl IntoElement,
        cx: &App,
    ) -> impl IntoElement {
        div()
            .w_full()
            .min_h(px(44.))
            .py_2()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap_4()
            .border_b_1()
            .border_color(rgba(0x8d91a51a))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .flex_1()
                    .min_w(px(0.))
                    .overflow_hidden()
                    .child(
                        div()
                            .text_size(px(14.))
                            .text_color(cx.theme().foreground)
                            .truncate()
                            .child(label),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(cx.theme().muted_foreground)
                            .truncate()
                            .child(description),
                    ),
            )
            .child(control)
    }

    /// The Autocomplete body: one row with a BETA info chip.
    ///
    /// The old `Disabled` dropdown was a dead control (rendered `disabled` with
    /// no handler), so it is deleted rather than kept as a stub. The BETA chip
    /// is the info-token (same pattern as the Logs `Upgrade` chip): ghost-text
    /// suggestions are not implemented yet.
    /// The Snippets body: a compact 44px-row list with the pane's live search
    /// field. The rows share the pane's filter state (`search_field` /
    /// `compact_rows`), so typing here filters the same store the full pane
    /// shows; a tap selects the snippet in the pane.
    fn render_sidebar_snippets(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let pane = self.ensure_snippets_pane(window, cx);
        let search = pane.read(cx).search_field();
        let items = pane.read(cx).compact_rows(cx);
        let muted = cx.theme().muted_foreground;
        let border = cx.theme().border;
        let empty = items.is_empty();
        let rows: Vec<AnyElement> = items
            .into_iter()
            .map(|(id, label, secondary)| {
                let select_id = id.clone();
                let row_pane = pane.clone();
                let cmd = pane
                    .read(cx)
                    .command_of(&id)
                    .unwrap_or_else(|| secondary.clone());
                div()
                    .id(SharedString::from(format!(
                        "side-snippet-{}",
                        select_id.as_str()
                    )))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .w_full()
                    .min_h(px(44.))
                    .px_2()
                    .py_1()
                    .rounded(px(8.))
                    .border_b_1()
                    .border_color(border)
                    .cursor_pointer()
                    .hover(|s| s.bg(cx.theme().muted))
                    .on_click(cx.listener(move |_, _, window, cx| {
                        row_pane.update(cx, |pane, cx| {
                            pane.select(select_id.clone(), window, cx);
                        });
                    }))
                    .child(
                        div()
                            .size(px(28.))
                            .rounded(px(6.))
                            .bg(rgb(0x244a67))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(
                                Icon::default()
                                    .data(glyph::CODE)
                                    .size(px(16.))
                                    .text_color(rgb(0xffffff)),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w(px(0.))
                            .overflow_hidden()
                            .child(
                                div()
                                    .text_size(px(13.))
                                    .truncate()
                                    .child(SharedString::from(label)),
                            )
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(muted)
                                    .truncate()
                                    .font_family("Menlo")
                                    .child(SharedString::from(secondary)),
                            ),
                    )
                    .child(
                        Button::new(SharedString::from(format!("run-side-snip-{}", id.as_str())))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Play)
                            .tooltip("Run in Terminal")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.broadcast_send_text(&cmd, cx);
                                window.push_notification(
                                    Notification::success("Snippet sent to terminal"),
                                    cx,
                                );
                            })),
                    )
                    .into_any_element()
            })
            .collect();
        // Empty state mirrors the reference: a 72pt braces tile, the
        // catalogue copy, and a New Snippet affordance that opens the
        // library's blank form. Built before the chain so no `cx` borrow is
        // needed inside the `.when` closure below.
        let new_snippet = Button::new("side-snippet-new")
            .small()
            .label("New Snippet")
            .icon(Icon::default().data(glyph::BRACES))
            .on_click(cx.listener(|this, _, window, cx| {
                let pane = this.ensure_snippets_pane(window, cx);
                pane.update(cx, |pane, cx| {
                    pane.open_new(window, cx);
                });
                this.select_left_nav(LeftNav::Snippets, window, cx);
            }));
        let empty_state = div()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_2()
            .w_full()
            .py_6()
            .child(
                div()
                    .size(px(72.))
                    .rounded(px(16.))
                    .bg(cx.theme().muted)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        Icon::default()
                            .data(glyph::BRACES)
                            .size(px(28.))
                            .text_color(muted),
                    ),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().foreground)
                    .child("Create snippets from your commands"),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child("Store your most used commands to reuse them in one click."),
            )
            .child(new_snippet);
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.))
            .w_full()
            .child(
                div().flex_shrink_0().p_2().child(
                    div().flex_1().min_w(px(120.)).child(
                        Input::new(&search)
                            .small()
                            .cleanable(true)
                            .prefix(Icon::new(IconName::Search).small().text_color(muted)),
                    ),
                ),
            )
            .child(
                div()
                    .id("side-snippets-compact")
                    .flex()
                    .flex_col()
                    .gap_1()
                    .flex_1()
                    .min_h(px(0.))
                    .px_2()
                    .pb_2()
                    .overflow_y_scrollbar()
                    .when(empty, |el| el.child(empty_state))
                    .children(rows),
            )
            .into_any_element()
    }

    fn render_sidebar_autocomplete(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let chip = div()
            .px_2()
            .py_0p5()
            .rounded_full()
            .bg(rgb(0x223636))
            .text_size(px(10.))
            .text_color(cx.theme().success)
            .child("BETA");

        let is_enabled = self.autocomplete_enabled;
        let toggle_btn = Button::new("side-toggle-autocomplete")
            .small()
            .label(if is_enabled { "Enabled" } else { "Disabled" })
            .selected(is_enabled)
            .on_click(cx.listener(|this, _, window, cx| {
                this.autocomplete_enabled = !this.autocomplete_enabled;
                let mut cfg = sshdeck_config::Settings::load();
                cfg.set_autocomplete_enabled(this.autocomplete_enabled);
                let _ = cfg.save();
                window.push_notification(
                    Notification::info(if this.autocomplete_enabled {
                        "Autocomplete enabled"
                    } else {
                        "Autocomplete disabled"
                    }),
                    cx,
                );
                cx.notify();
            }));

        let control = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(chip)
            .child(toggle_btn);

        div()
            .flex()
            .flex_col()
            .w_full()
            .px_3()
            .py_2()
            .child(Self::sidebar_row(
                "Autocomplete",
                "Ghost-text suggestions as you type in terminal",
                control,
                cx,
            ))
            .into_any_element()
    }

    /// The History body: an add form (three inputs + Close/Save) over a flat
    /// list of saved suggestions, bounded like the logs pane.
    fn render_sidebar_history(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let form = self.history_form_open.then(|| {
            div()
                .flex()
                .flex_col()
                .w_full()
                .gap_2()
                .p_3()
                .border_b_1()
                .border_color(cx.theme().border)
                .child(Input::new(&self.hist_who).small())
                .child(Input::new(&self.hist_where).small())
                .child(Input::new(&self.hist_what).small())
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("side-history-close")
                                .ghost()
                                .small()
                                .label("Close")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.history_form_open = false;
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new("side-history-save")
                                .small()
                                .primary()
                                .label("Save")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    let who = this.hist_who.read(cx).value().trim().to_string();
                                    let whr = this.hist_where.read(cx).value().trim().to_string();
                                    let what = this.hist_what.read(cx).value().trim().to_string();
                                    let mut parts = Vec::new();
                                    if !what.is_empty() {
                                        parts.push(what.clone());
                                    }
                                    if !who.is_empty() {
                                        parts.push(who.clone());
                                    }
                                    if !whr.is_empty() {
                                        parts.push(whr.clone());
                                    }
                                    if !parts.is_empty() {
                                        push_history(&mut this.history, parts.join(" · "));
                                    }
                                    // Persist the suggestion as a snippet so it
                                    // survives restarts: the command template is
                                    // the entry, the label names who/where.
                                    if !what.is_empty() {
                                        let label = match (who.is_empty(), whr.is_empty()) {
                                            (false, false) => format!("{who} · {whr}"),
                                            (false, true) => who.clone(),
                                            (true, false) => whr.clone(),
                                            (true, true) => what.clone(),
                                        };
                                        let pane = this.ensure_snippets_pane(window, cx);
                                        let error = pane.update(cx, |pane, cx| {
                                            pane.add_quick_snippet(label, what, cx)
                                        });
                                        if let Some(message) = error {
                                            window.push_notification(
                                                Notification::warning(format!(
                                                    "Suggestion kept for this session, not saved: {message}"
                                                )),
                                                cx,
                                            );
                                        }
                                    }
                                    for input in [&this.hist_who, &this.hist_where, &this.hist_what]
                                    {
                                        input.update(cx, |state, cx| {
                                            state.set_value("", window, cx);
                                        });
                                    }
                                    this.history_form_open = false;
                                    cx.notify();
                                })),
                        ),
                )
        });

        let empty = self.history.is_empty() && !self.history_form_open;
        div()
            .flex()
            .flex_col()
            .w_full()
            .flex_1()
            .min_h(px(0.))
            .when_some(form, |el, form| el.child(form))
            .when(empty, |el| {
                el.child(
                    div()
                        .p_3()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child("No saved suggestions yet"),
                )
            })
            .children(self.history.iter().rev().enumerate().map(|(idx, entry)| {
                let cmd = entry.clone();
                let copy_cmd = entry.clone();
                div()
                    .id(SharedString::from(format!("side-history-{idx}")))
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .w_full()
                    .px_3()
                    .py_2()
                    .min_h(px(40.))
                    .border_b_1()
                    .border_color(rgba(0x8d91a51a))
                    .rounded(px(4.))
                    .hover(|s| s.bg(cx.theme().muted))
                    .child(
                        div()
                            .flex_1()
                            .text_sm()
                            .truncate()
                            .font_family("Menlo")
                            .text_color(cx.theme().foreground)
                            .child(SharedString::from(entry.clone())),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_1()
                            .child(
                                Button::new(SharedString::from(format!("copy-hist-{idx}")))
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::Copy)
                                    .tooltip("Copy command")
                                    .on_click(cx.listener(move |_, _, window, cx| {
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            copy_cmd.clone(),
                                        ));
                                        window.push_notification(
                                            Notification::info("Copied to clipboard"),
                                            cx,
                                        );
                                    })),
                            )
                            .child(
                                Button::new(SharedString::from(format!("run-hist-{idx}")))
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::Play)
                                    .tooltip("Run in Terminal")
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.broadcast_send_text(&cmd, cx);
                                        window.push_notification(
                                            Notification::success("Command sent to terminal"),
                                            cx,
                                        );
                                    })),
                            ),
                    )
            }))
            .child(
                div().p_3().child(
                    Button::new("side-history-add")
                        .ghost()
                        .small()
                        .label("Add suggestion")
                        .icon(IconName::Plus)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.history_form_open = true;
                            cx.notify();
                        })),
                ),
            )
            .into_any_element()
    }

    /// The Appearance body: a Font stepper (10–24px, matching the reference
    /// range) plus the terminal theme list with 64x40 mini-preview swatches
    /// and a selected border. The ten schemes are the reference catalogue
    /// with clean-room re-derived colours; picking one applies it live to
    /// every pane.
    fn render_sidebar_appearance(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let selected = self.selected_theme.clone();
        let font_size = self.terminal_font_size;
        // Live values: the cell is re-measured from the font size every frame,
        // so existing panes scale at once; only scrollback still needs a pane
        // rebuild (fixed when the grid is created).
        let font_control = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .child(
                Button::new("side-font-dec")
                    .ghost()
                    .small()
                    .label("−")
                    .tooltip("Smaller terminal text")
                    .disabled(font_size <= 10.0)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.set_terminal_font_size(this.terminal_font_size - 1.0, window, cx);
                    })),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(SharedString::from(format!("{font_size:.0} px"))),
            )
            .child(
                Button::new("side-font-inc")
                    .ghost()
                    .small()
                    .label("+")
                    .tooltip("Larger terminal text")
                    .disabled(font_size >= 24.0)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.set_terminal_font_size(this.terminal_font_size + 1.0, window, cx);
                    })),
            );
        div()
            .flex()
            .flex_col()
            .w_full()
            .px_3()
            .child(Self::sidebar_row(
                "Font",
                "Terminal text size · applies live; scrollback needs a new pane",
                font_control,
                cx,
            ))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .pt_3()
                    .pb_2()
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(cx.theme().muted_foreground)
                            .child("TERMINAL THEMES"),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!("{} themes", SCHEMES.len())),
                    ),
            )
            .children(SCHEMES.iter().map(|(name, scheme)| {
                let chosen = selected == *name;
                let swatch_bg: Hsla = scheme.background().into();
                let swatch_fg: Hsla = scheme.foreground().into();
                let swatch_cursor: Hsla = scheme.cursor().into();
                let theme_name = name.to_string();
                let theme_meta = scheme.meta();
                div()
                    .id(SharedString::from(format!("side-theme-{name}")))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_3()
                    .w_full()
                    .p_2()
                    .rounded(px(8.))
                    .border_1()
                    .border_color(if chosen {
                        cx.theme().success
                    } else {
                        rgba(0x8d91a51a).into()
                    })
                    .cursor_pointer()
                    .hover(|s| s.bg(cx.theme().muted))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.selected_theme = theme_name.clone();
                        this.apply_terminal_scheme(window, cx);
                        cx.notify();
                    }))
                    .child(
                        div()
                            .w(px(64.))
                            .h(px(40.))
                            .flex_shrink_0()
                            .rounded(px(6.))
                            .bg(swatch_bg)
                            .border_1()
                            .border_color(swatch_fg)
                            .flex()
                            .flex_col()
                            .justify_center()
                            .gap(px(4.))
                            .px(px(8.))
                            // Mini terminal preview: two text bars plus a
                            // prompt row with a cursor block, in the scheme's
                            // own colours on its background.
                            .child(div().w_full().h(px(4.)).rounded_full().bg(swatch_fg))
                            .child(div().w(px(36.)).h(px(4.)).rounded_full().bg(swatch_fg))
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap(px(3.))
                                    .child(
                                        div().w(px(10.)).h(px(4.)).rounded_full().bg(swatch_cursor),
                                    )
                                    .child(
                                        div().w(px(6.)).h(px(8.)).rounded(px(1.)).bg(swatch_cursor),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .flex_1()
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(cx.theme().foreground)
                                    .child(SharedString::from(name.to_string())),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(theme_meta),
                            ),
                    )
                    .when(chosen, |el| {
                        el.child(Icon::new(IconName::Check).text_color(cx.theme().success))
                    })
            }))
            .into_any_element()
    }

    /// The tiled workspace: a recursive split tree of session panes, every
    /// split laying its children with an 8px gutter. Each tile is a borderless
    /// click-to-focus wrapper: the pane draws its own 28pt header and green
    /// focus border, so the wrapper adds no chrome. Tiles flex by their
    /// settled `weights` share with `min_w/min_h` 0 so panes resize with the
    /// window. Below [`WORKSPACE_STACK_WIDTH`] every split stacks vertically
    /// so each canvas keeps a usable grid. Maximize shows only the focused
    /// tile and toggles back. The right sidebar docks beside the tiles, as in
    /// the session view.
    fn render_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        if workspace_panes_count(&self.workspace) == 0 {
            if let Some(active) = self.active {
                self.workspace = WorkspaceNode::Pane {
                    tabs: vec![active],
                    active: 0,
                };
                self.workspace_focus = 0;
            }
        }
        let muted = cx.theme().muted_foreground;
        let narrow = f32::from(window.bounds().size.width) < WORKSPACE_STACK_WIDTH;
        let maximized = self.workspace_maximized;
        let focus = self.workspace_focus;
        let override_dir = self.workspace_direction;
        let root_dir = match &self.workspace {
            WorkspaceNode::Split { dir, .. } => *dir,
            _ => SplitDir::Row,
        };
        let effective = if narrow {
            SplitDir::Col
        } else {
            override_dir.unwrap_or(root_dir)
        };
        let vertical = effective == SplitDir::Col;

        let leaves = workspace_leaves(&self.workspace);
        let body: AnyElement = if leaves.is_empty() {
            div()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_2()
                .size_full()
                .child(
                    Icon::new(IconName::LayoutDashboard)
                        .large()
                        .text_color(muted),
                )
                .child(
                    div()
                        .text_color(muted)
                        .child("Split a terminal to start the workspace"),
                )
                .into_any_element()
        } else if maximized {
            let panes = workspace_panes_count(&self.workspace);
            let at = focus.min(panes.saturating_sub(1));
            let mut counter = 0;
            fn find_pane(
                node: &WorkspaceNode,
                target: usize,
                counter: &mut usize,
            ) -> Option<(Vec<usize>, usize)> {
                match node {
                    WorkspaceNode::Empty => None,
                    WorkspaceNode::Pane { tabs, active } => {
                        let cur = *counter;
                        *counter += 1;
                        if cur == target {
                            Some((tabs.clone(), *active))
                        } else {
                            None
                        }
                    }
                    WorkspaceNode::Split { children, .. } => {
                        for c in children {
                            if let Some(res) = find_pane(c, target, counter) {
                                return Some(res);
                            }
                        }
                        None
                    }
                }
            }
            match find_pane(&self.workspace, at, &mut counter) {
                Some((tabs, active)) => self.render_workspace_pane(&tabs, active, at, window, cx),
                None => div().flex_1().into_any_element(),
            }
        } else {
            let node = self.workspace.clone();
            let mut pos = 0;
            self.render_workspace_node(&node, override_dir, narrow, &mut pos, window, cx)
        };

        let toolbar = div()
            .flex()
            .flex_row()
            .items_center()
            .flex_shrink_0()
            .h(px(28.))
            .px_2()
            .gap_2()
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(muted)
                    .child("Workspace"),
            )
            .when(
                self.dragged_tab.is_some() || self.dragged_session.is_some(),
                |el| {
                    el.child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_1()
                            .px_2()
                            .py_0p5()
                            .rounded(px(4.))
                            .bg(rgb(0x2091f6))
                            .text_xs()
                            .text_color(rgb(0xffffff))
                            .child("Moving tab — click a drop zone or")
                            .child(
                                Button::new("cancel-drag")
                                    .ghost()
                                    .xsmall()
                                    .label("Cancel")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.dragged_tab = None;
                                        this.dragged_session = None;
                                        this.tab_drag_start = None;
                                        cx.notify();
                                    })),
                            ),
                    )
                },
            )
            .child(div().flex_1())
            .child(
                Button::new("ws-broadcast")
                    .small()
                    .when(self.broadcast_mode, |btn| btn.primary())
                    .when(!self.broadcast_mode, |btn| btn.ghost())
                    .label(if self.broadcast_mode {
                        "Broadcast: ON"
                    } else {
                        "Broadcast"
                    })
                    .tooltip(if self.broadcast_mode {
                        "Broadcast mode is active: typing goes to all panes"
                    } else {
                        "Toggle broadcast mode (send input to all open panes)"
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.broadcast_mode = !this.broadcast_mode;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("ws-direction")
                    .ghost()
                    .small()
                    .label(if vertical { "Stacked" } else { "Side by side" })
                    .tooltip("Toggle split direction")
                    .on_click(cx.listener(|this, _, window, cx| {
                        let narrow = f32::from(window.bounds().size.width) < WORKSPACE_STACK_WIDTH;
                        let root = match &this.workspace {
                            WorkspaceNode::Split { dir, .. } => *dir,
                            _ => SplitDir::Row,
                        };
                        let effective = if narrow {
                            SplitDir::Col
                        } else {
                            this.workspace_direction.unwrap_or(root)
                        };
                        this.workspace_direction = Some(if effective == SplitDir::Row {
                            SplitDir::Col
                        } else {
                            SplitDir::Row
                        });
                        cx.notify();
                    })),
            )
            .child(
                Button::new("ws-max")
                    .ghost()
                    .small()
                    .label(if maximized { "Tile all" } else { "Maximize" })
                    .tooltip("Maximize the focused tile")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.workspace_maximized = !this.workspace_maximized;
                        cx.notify();
                    })),
            );

        let sidebar = self.render_session_sidebar(window, cx);

        div()
            .flex()
            .flex_col()
            .flex_1()
            .h_full()
            .min_h(px(0.))
            .overflow_hidden()
            .child(toolbar)
            .child(
                div()
                    .relative()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_hidden()
                    .p(px(8.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w(px(0.))
                            .overflow_hidden()
                            .child(body),
                    )
                    .child(sidebar),
            )
            .into_any_element()
    }

    /// Renders one tiling node: a multi-tab pane leaf or a split of children.
    fn render_workspace_node(
        &mut self,
        node: &WorkspaceNode,
        root_dir: Option<SplitDir>,
        narrow: bool,
        pos: &mut usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match node {
            WorkspaceNode::Empty => div().flex_1().into_any_element(),
            WorkspaceNode::Pane { tabs, active } => {
                let position = *pos;
                *pos += 1;
                self.render_workspace_pane(tabs, *active, position, window, cx)
            }
            WorkspaceNode::Split {
                dir,
                weights,
                children,
            } => {
                let axis = root_dir.unwrap_or(*dir);
                let column = narrow || axis == SplitDir::Col;
                let shares = normalize_weights(weights, children.len());
                let mut items = Vec::with_capacity(children.len());
                for (child, share) in children.iter().zip(shares) {
                    let element = self.render_workspace_node(child, None, narrow, pos, window, cx);
                    items.push(
                        div()
                            .flex_1()
                            .flex_grow(share)
                            .min_w(px(0.))
                            .min_h(px(0.))
                            .flex()
                            .flex_col()
                            .overflow_hidden()
                            .child(element)
                            .into_any_element(),
                    );
                }
                div()
                    .flex()
                    .flex_1()
                    .min_w(px(0.))
                    .min_h(px(0.))
                    .overflow_hidden()
                    .gap_2()
                    .when(column, |el| el.flex_col())
                    .when(!column, |el| el.flex_row())
                    .children(items)
                    .into_any_element()
            }
        }
    }

    /// One workspace pane leaf: a multi-tab container holding one or more session
    /// tabs, a tab bar with add/split/maximize/close actions, and the active session's
    /// `TerminalPane`. Supports drag-and-drop between panes and edge splitting.
    fn render_workspace_pane(
        &mut self,
        tabs: &[usize],
        active_tab: usize,
        pane_index: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let is_focused = self.workspace_focus == pane_index;
        let tabs_vec = tabs.to_vec();

        // 1. Tab headers
        let mut tab_elements = Vec::with_capacity(tabs.len());
        for (tab_idx, &session_idx) in tabs.iter().enumerate() {
            let is_active = tab_idx == active_tab;
            let (label, is_connected) = if let Some(session) = self.sessions.get(session_idx) {
                let host_opt = self.store.inventory().get(&session.host);
                let title = host_opt
                    .map(|h| h.label.clone())
                    .or_else(|| session.status.title.clone())
                    .unwrap_or_else(|| session.host.to_string());
                let connected = matches!(session.status.state, SessionState::Connected);
                (title, connected)
            } else {
                (format!("Session {session_idx}"), false)
            };

            let tab_bg = if is_active {
                rgb(0x0e3a2f)
            } else {
                rgb(0x1d2033)
            };
            let tab_fg = if is_active {
                rgb(0x10b981)
            } else {
                rgb(0x8d91a5)
            };

            let drag_btn = Button::new(SharedString::from(format!("drag-{pane_index}-{tab_idx}")))
                .ghost()
                .xsmall()
                .label("⋮⋮")
                .tooltip("Drag tab to move or split")
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.dragged_tab = Some((pane_index, tab_idx));
                    this.dragged_session = Some(session_idx);
                    cx.notify();
                }));

            let close_btn = Button::new(SharedString::from(format!(
                "close-tab-{pane_index}-{tab_idx}"
            )))
            .ghost()
            .xsmall()
            .icon(
                Icon::new(IconName::Close)
                    .size(px(11.))
                    .text_color(if is_active {
                        rgb(0x10b981)
                    } else {
                        rgb(0x8d91a5)
                    }),
            )
            .tooltip("Close tab")
            .on_click(cx.listener(move |this, _, window, cx| {
                this.close_session(session_idx, window, cx);
            }));

            let mut tab_item = div()
                .id(SharedString::from(format!(
                    "pane-{pane_index}-tab-{tab_idx}"
                )))
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .h(px(26.))
                .px_2()
                .rounded_t(px(4.))
                .bg(tab_bg)
                .text_color(tab_fg)
                .text_size(px(12.))
                .cursor_pointer()
                .hover(|s| s.bg(rgb(0x32364c)))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, event: &MouseDownEvent, _window, cx| {
                        this.tab_drag_start = Some((session_idx, event.position));
                        cx.notify();
                    }),
                )
                .on_mouse_move(
                    cx.listener(move |this, event: &MouseMoveEvent, _window, cx| {
                        if let Some((start_idx, start_pos)) = this.tab_drag_start {
                            if event.dragging() && start_idx == session_idx {
                                let dx =
                                    (f32::from(event.position.x) - f32::from(start_pos.x)).abs();
                                let dy =
                                    (f32::from(event.position.y) - f32::from(start_pos.y)).abs();
                                if dx > 4.0 || dy > 4.0 {
                                    this.dragged_tab = Some((pane_index, tab_idx));
                                    this.dragged_session = Some(session_idx);
                                    cx.notify();
                                }
                            }
                        }
                    }),
                );

            if is_active {
                tab_item = tab_item.child(close_btn);
            } else {
                tab_item = tab_item.child(drag_btn);
            }

            tab_item = tab_item
                .child(div().max_w(px(140.)).overflow_hidden().child(label))
                .when(is_connected, |el| {
                    el.child(div().size(px(5.)).rounded_full().bg(rgb(0x10b981)))
                });

            if !is_active {
                tab_item = tab_item.child(close_btn);
            }

            tab_item = tab_item.on_click(cx.listener(move |this, _, window, cx| {
                this.workspace = workspace_switch_tab(&this.workspace, pane_index, tab_idx);
                this.workspace_focus = pane_index;
                this.active = Some(session_idx);
                if let Some(session) = this.sessions.get(session_idx) {
                    session.pane.update(cx, |pane, cx| pane.focus(window, cx));
                }
                cx.notify();
            }));

            tab_elements.push(tab_item.into_any_element());
        }

        // 2. Pane controls on the right
        let add_btn = Button::new(SharedString::from(format!("pane-add-{pane_index}")))
            .ghost()
            .xsmall()
            .icon(IconName::Plus)
            .tooltip("New tab in this pane")
            .on_click(cx.listener(move |this, _, window, cx| {
                this.select_tab(MainTab::NewTab, window, cx);
            }));

        let split_h_btn = Button::new(SharedString::from(format!("pane-splith-{pane_index}")))
            .ghost()
            .xsmall()
            .icon(Icon::default().data(glyph::SPLIT_HORIZONTAL).size(px(13.)))
            .tooltip("Split side by side")
            .on_click(cx.listener(move |this, _, _, cx| {
                if let Some(active_sess) = this.active {
                    this.workspace = workspace_split_pane_with_session(
                        &this.workspace,
                        pane_index,
                        active_sess,
                        SplitDir::Row,
                        true,
                    );
                    cx.notify();
                }
            }));

        let split_v_btn = Button::new(SharedString::from(format!("pane-splitv-{pane_index}")))
            .ghost()
            .xsmall()
            .icon(Icon::default().data(glyph::SPLIT_VERTICAL).size(px(13.)))
            .tooltip("Split stacked")
            .on_click(cx.listener(move |this, _, _, cx| {
                if let Some(active_sess) = this.active {
                    this.workspace = workspace_split_pane_with_session(
                        &this.workspace,
                        pane_index,
                        active_sess,
                        SplitDir::Col,
                        true,
                    );
                    cx.notify();
                }
            }));

        let max_btn = Button::new(SharedString::from(format!("pane-max-{pane_index}")))
            .ghost()
            .xsmall()
            .icon(IconName::Maximize)
            .tooltip(if self.workspace_maximized {
                "Tile all"
            } else {
                "Maximize pane"
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                this.workspace_focus = pane_index;
                this.workspace_maximized = !this.workspace_maximized;
                cx.notify();
            }));

        let close_pane_tabs = tabs_vec.clone();
        let close_pane_btn = Button::new(SharedString::from(format!("pane-close-{pane_index}")))
            .ghost()
            .xsmall()
            .icon(
                Icon::new(IconName::Close)
                    .size(px(12.))
                    .text_color(rgb(0x10b981)),
            )
            .tooltip("Close pane")
            .on_click(cx.listener(move |this, _, window, cx| {
                for &s in close_pane_tabs.iter().rev() {
                    this.close_session(s, window, cx);
                }
            }));

        let single_session_info = if tabs.len() == 1 {
            let session_idx = tabs[0];
            let (label, username, host_opt, is_local, is_connected) =
                if let Some(session) = self.sessions.get(session_idx) {
                    let is_local = session.host == "local-terminal"
                        || session.host.starts_with("local-terminal-")
                        || session.is_local;
                    let host_opt = self.store.inventory().get(&session.host).cloned();
                    let title = host_opt
                        .as_ref()
                        .map(|h| h.label.clone())
                        .or_else(|| session.status.title.clone())
                        .unwrap_or_else(|| {
                            if is_local {
                                let num = session
                                    .host
                                    .as_str()
                                    .strip_prefix("local-terminal-")
                                    .unwrap_or("1");
                                if num == "1" || num.is_empty() {
                                    "Local Terminal".to_string()
                                } else {
                                    format!("Local Terminal ({num})")
                                }
                            } else {
                                session.host.to_string()
                            }
                        });
                    let user = host_opt
                        .as_ref()
                        .map(|h| {
                            if h.username.is_empty() {
                                "root".to_string()
                            } else {
                                h.username.clone()
                            }
                        })
                        .unwrap_or_else(|| {
                            if is_local {
                                "~".to_string()
                            } else {
                                "root".to_string()
                            }
                        });
                    let connected = matches!(session.status.state, SessionState::Connected);
                    (title, user, host_opt, is_local, connected)
                } else {
                    (
                        format!("Session {session_idx}"),
                        "root".to_string(),
                        None,
                        false,
                        false,
                    )
                };
            let orange = rgb(0xe95420);
            let tint = if is_local {
                rgb(0x282c3f)
            } else {
                host_opt
                    .as_ref()
                    .map(|h| host_os_tint(h, orange.into()))
                    .unwrap_or_else(|| orange.into())
            };
            Some((session_idx, label, username, tint, is_local, is_connected))
        } else {
            None
        };

        let header_left = if let Some((sess_idx, label, username, tint, is_local, is_connected)) =
            single_session_info
        {
            let title_color = if is_focused {
                rgb(0x10b981)
            } else {
                rgb(0xd0d4e4)
            };
            let icon_element = if is_local {
                div()
                    .size(px(18.))
                    .rounded(px(5.))
                    .bg(rgb(0x282c3f))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        Icon::default()
                            .data(glyph::TERMINAL_PROMPT)
                            .size(px(11.))
                            .text_color(if is_focused {
                                rgb(0x10b981)
                            } else {
                                rgb(0x8d91a5)
                            }),
                    )
            } else {
                div()
                    .size(px(18.))
                    .rounded(px(5.))
                    .bg(tint)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        Icon::default()
                            .data(glyph::UBUNTU_SOLID)
                            .size(px(12.))
                            .text_color(rgb(0xffffff)),
                    )
            };

            let subtitle_text = if is_local {
                "~".to_string()
            } else {
                format!("ssh, {username}")
            };

            div()
                .id(SharedString::from(format!("pane-hdr-{pane_index}")))
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .flex_1()
                .min_w(px(0.))
                .overflow_hidden()
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.workspace_focus = pane_index;
                    this.active = Some(sess_idx);
                    if let Some(session) = this.sessions.get(sess_idx) {
                        session.pane.update(cx, |pane, cx| pane.focus(window, cx));
                    }
                    cx.notify();
                }))
                .child(icon_element)
                .child(
                    div()
                        .text_size(px(13.))
                        .font_weight(gpui_kit::FontWeight::MEDIUM)
                        .text_color(title_color)
                        .truncate()
                        .child(SharedString::from(label)),
                )
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(0x8d91a5))
                        .child(SharedString::from(subtitle_text)),
                )
                .when(is_connected, |el| {
                    el.child(div().size(px(5.)).rounded_full().bg(rgb(0x10b981)))
                })
                .into_any_element()
        } else {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .overflow_hidden()
                .flex_1()
                .children(tab_elements)
                .into_any_element()
        };

        let tab_bar = div()
            .flex()
            .flex_row()
            .items_center()
            .h(px(32.))
            .bg(rgb(0x131722))
            .px_3()
            .child(header_left)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_0p5()
                    .flex_shrink_0()
                    .when(self.broadcast_mode, |el| {
                        el.child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap_1()
                                .px_1p5()
                                .py_0p5()
                                .rounded(px(3.))
                                .bg(rgba(0xf59e0b25))
                                .text_xs()
                                .text_color(rgb(0xf59e0b))
                                .child("● Broadcast"),
                        )
                    })
                    .when(tabs.len() > 1, |el| el.child(add_btn))
                    .when(is_focused, |el| {
                        el.child(split_h_btn)
                            .child(split_v_btn)
                            .child(max_btn)
                            .child(close_pane_btn)
                    }),
            );

        // 3. Active session pane
        let active_sess_idx = tabs
            .get(active_tab)
            .copied()
            .or_else(|| tabs.first().copied());
        let terminal_view = if let Some(idx) = active_sess_idx {
            if let Some(session) = self.sessions.get(idx) {
                session.pane.clone().into_any_element()
            } else {
                div().flex_1().into_any_element()
            }
        } else {
            div().flex_1().into_any_element()
        };

        // 4. Drop zones when dragging
        let moving_session_opt = self.dragged_session.or_else(|| {
            self.dragged_tab.and_then(|(from_pane, from_tab)| {
                if from_pane == pane_index {
                    tabs.get(from_tab).copied()
                } else {
                    let leaves = workspace_leaves(&self.workspace);
                    leaves.get(from_tab).copied()
                }
            })
        });

        let drop_overlay = if let Some(moving_session) = moving_session_opt {
            let from_pane_opt = self.dragged_tab;
            let drop_left = div()
                .id(SharedString::from(format!("ws-drop-left-{pane_index}")))
                .w(px(72.))
                .bg(rgba(0x2091f630))
                .border_r_2()
                .border_color(cx.theme().primary)
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_1()
                .cursor_pointer()
                .hover(|s| s.bg(rgba(0x2091f660)))
                .child(
                    Icon::default()
                        .data(glyph::SPLIT_HORIZONTAL)
                        .size(px(20.))
                        .text_color(rgb(0xffffff)),
                )
                .child(
                    div()
                        .text_xs()
                        .font_weight(gpui_kit::FontWeight::MEDIUM)
                        .text_color(rgb(0xffffff))
                        .child("Split Left"),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.workspace = workspace_split_pane_with_session(
                        &this.workspace,
                        pane_index,
                        moving_session,
                        SplitDir::Row,
                        false,
                    );
                    this.dragged_tab = None;
                    this.dragged_session = None;
                    this.tab_drag_start = None;
                    cx.notify();
                }))
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| {
                        this.workspace = workspace_split_pane_with_session(
                            &this.workspace,
                            pane_index,
                            moving_session,
                            SplitDir::Row,
                            false,
                        );
                        this.dragged_tab = None;
                        this.dragged_session = None;
                        this.tab_drag_start = None;
                        cx.notify();
                    }),
                );

            let drop_right = div()
                .id(SharedString::from(format!("ws-drop-right-{pane_index}")))
                .w(px(72.))
                .bg(rgba(0x2091f630))
                .border_l_2()
                .border_color(cx.theme().primary)
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_1()
                .cursor_pointer()
                .hover(|s| s.bg(rgba(0x2091f660)))
                .child(
                    Icon::default()
                        .data(glyph::SPLIT_HORIZONTAL)
                        .size(px(20.))
                        .text_color(rgb(0xffffff)),
                )
                .child(
                    div()
                        .text_xs()
                        .font_weight(gpui_kit::FontWeight::MEDIUM)
                        .text_color(rgb(0xffffff))
                        .child("Split Right"),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.workspace = workspace_split_pane_with_session(
                        &this.workspace,
                        pane_index,
                        moving_session,
                        SplitDir::Row,
                        true,
                    );
                    this.dragged_tab = None;
                    this.dragged_session = None;
                    this.tab_drag_start = None;
                    cx.notify();
                }))
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| {
                        this.workspace = workspace_split_pane_with_session(
                            &this.workspace,
                            pane_index,
                            moving_session,
                            SplitDir::Row,
                            true,
                        );
                        this.dragged_tab = None;
                        this.dragged_session = None;
                        this.tab_drag_start = None;
                        cx.notify();
                    }),
                );

            let drop_top = div()
                .id(SharedString::from(format!("ws-drop-top-{pane_index}")))
                .h(px(40.))
                .bg(rgba(0x2091f630))
                .border_b_2()
                .border_color(cx.theme().primary)
                .flex()
                .items_center()
                .justify_center()
                .gap_2()
                .cursor_pointer()
                .hover(|s| s.bg(rgba(0x2091f660)))
                .child(
                    Icon::default()
                        .data(glyph::SPLIT_VERTICAL)
                        .size(px(16.))
                        .text_color(rgb(0xffffff)),
                )
                .child(div().text_xs().text_color(rgb(0xffffff)).child("Split Top"))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.workspace = workspace_split_pane_with_session(
                        &this.workspace,
                        pane_index,
                        moving_session,
                        SplitDir::Col,
                        false,
                    );
                    this.dragged_tab = None;
                    this.dragged_session = None;
                    this.tab_drag_start = None;
                    cx.notify();
                }))
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| {
                        this.workspace = workspace_split_pane_with_session(
                            &this.workspace,
                            pane_index,
                            moving_session,
                            SplitDir::Col,
                            false,
                        );
                        this.dragged_tab = None;
                        this.dragged_session = None;
                        this.tab_drag_start = None;
                        cx.notify();
                    }),
                );

            let drop_bottom = div()
                .id(SharedString::from(format!("ws-drop-bottom-{pane_index}")))
                .h(px(40.))
                .bg(rgba(0x2091f630))
                .border_t_2()
                .border_color(cx.theme().primary)
                .flex()
                .items_center()
                .justify_center()
                .gap_2()
                .cursor_pointer()
                .hover(|s| s.bg(rgba(0x2091f660)))
                .child(
                    Icon::default()
                        .data(glyph::SPLIT_VERTICAL)
                        .size(px(16.))
                        .text_color(rgb(0xffffff)),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(0xffffff))
                        .child("Split Bottom"),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.workspace = workspace_split_pane_with_session(
                        &this.workspace,
                        pane_index,
                        moving_session,
                        SplitDir::Col,
                        true,
                    );
                    this.dragged_tab = None;
                    this.dragged_session = None;
                    this.tab_drag_start = None;
                    cx.notify();
                }))
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| {
                        this.workspace = workspace_split_pane_with_session(
                            &this.workspace,
                            pane_index,
                            moving_session,
                            SplitDir::Col,
                            true,
                        );
                        this.dragged_tab = None;
                        this.dragged_session = None;
                        this.tab_drag_start = None;
                        cx.notify();
                    }),
                );

            let tabs_len = tabs.len();
            let drop_center = div()
                .id(SharedString::from(format!("ws-drop-center-{pane_index}")))
                .flex_1()
                .bg(rgba(0x2091f615))
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .hover(|s| s.bg(rgba(0x2091f640)))
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(0xffffff))
                        .child("Move Tab Here"),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some((from_pane, from_tab)) = from_pane_opt {
                        this.workspace = workspace_move_tab(
                            &this.workspace,
                            from_pane,
                            from_tab,
                            pane_index,
                            tabs_len,
                        );
                    } else {
                        this.workspace = workspace_insert_tab_into_pane(
                            &this.workspace,
                            pane_index,
                            moving_session,
                        );
                    }
                    this.dragged_tab = None;
                    this.dragged_session = None;
                    this.tab_drag_start = None;
                    cx.notify();
                }))
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| {
                        if let Some((from_pane, from_tab)) = from_pane_opt {
                            this.workspace = workspace_move_tab(
                                &this.workspace,
                                from_pane,
                                from_tab,
                                pane_index,
                                tabs_len,
                            );
                        } else {
                            this.workspace = workspace_insert_tab_into_pane(
                                &this.workspace,
                                pane_index,
                                moving_session,
                            );
                        }
                        this.dragged_tab = None;
                        this.dragged_session = None;
                        this.tab_drag_start = None;
                        cx.notify();
                    }),
                );

            Some(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .flex_col()
                    .child(drop_top)
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_row()
                            .child(drop_left)
                            .child(drop_center)
                            .child(drop_right),
                    )
                    .child(drop_bottom),
            )
        } else {
            None
        };

        div()
            .id(SharedString::from(format!("ws-pane-{pane_index}")))
            .flex_1()
            .min_w(px(0.))
            .min_h(px(0.))
            .flex()
            .flex_col()
            .overflow_hidden()
            .rounded(px(6.))
            .when(is_focused, |el| el.border_2().border_color(rgb(0x10b981)))
            .when(!is_focused, |el| {
                el.border_1().border_color(rgba(0x8d91a530))
            })
            .child(tab_bar)
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_hidden()
                    .child(terminal_view)
                    .when_some(drop_overlay, |el, overlay| el.child(overlay)),
            )
            .into_any_element()
    }

    /// The connecting rail: a white 280pt panel with a `#5a5e73` header, a
    /// 32px status glyph, and Show logs / Close buttons on `#e6ebed` 32px
    /// pills. Shared by the connecting screen and the SFTP right pane.
    fn render_connecting_rail(
        &mut self,
        session_index: usize,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (host_id, state_label, title) = if let Some(session) = self.sessions.get(session_index)
        {
            (
                session.host.clone(),
                session.status.state.label(),
                session
                    .status
                    .title
                    .clone()
                    .unwrap_or_else(|| session.host.to_string()),
            )
        } else {
            return div().into_any_element();
        };
        let host = self.store.inventory().get(&host_id).cloned();
        let display_title = host.as_ref().map(|h| h.label.clone()).unwrap_or(title);
        let endpoint_str = host
            .as_ref()
            .map(|h| format!("{}:{}", h.address, h.port))
            .unwrap_or_else(|| host_id.to_string());
        let muted = cx.theme().muted_foreground;
        // 280px rail, capped at 90% of the window so it never squeezes the
        // connecting view out on narrow windows.
        let win_w = f32::from(window.bounds().size.width);
        let rail_max_w = (win_w * 0.9).max(220.0);

        // `#e6ebed` 32px pills wrapping ghost buttons, so the button chrome
        // stays flat while the pill carries the fill.
        let pill = |label: Button| {
            div()
                .h(px(32.))
                .flex_1()
                .flex()
                .flex_row()
                .items_center()
                .justify_center()
                .rounded(px(8.))
                .bg(rgb(0xe6ebed))
                .child(label)
        };

        div()
            .w(px(280.))
            .max_w(px(rail_max_w))
            .min_w(px(0.))
            .flex_shrink_0()
            .h_full()
            .flex()
            .flex_col()
            .bg(rgb(0xffffff))
            .border_l_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .w_full()
                    .min_w(px(0.))
                    .px_3()
                    .py_2()
                    .bg(rgb(0x5a5e73))
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(0xffffff))
                            .truncate()
                            .child(SharedString::from(display_title)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(0xd0d3e0))
                            .truncate()
                            .child(SharedString::from(endpoint_str)),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_3()
                    .p_6()
                    // ponytail: a static glyph, not an animation — the only
                    // budgeted timer is the cursor blink. Upgrade with a
                    // rotation step driven by the pane's notify loop.
                    .child(
                        Icon::new(IconName::RotateCw)
                            .size(px(32.))
                            .text_color(muted),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(muted)
                            .child(SharedString::from(format!("{state_label}..."))),
                    ),
            )
            .child(div().flex_1())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .p_3()
                    .child(pill(
                        Button::new("rail-logs")
                            .ghost()
                            .small()
                            .label("Show logs")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.select_left_nav(LeftNav::Logs, window, cx);
                            })),
                    ))
                    .child(pill(
                        Button::new("rail-close")
                            .ghost()
                            .small()
                            .label("Close")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.close_session(session_index, window, cx);
                            })),
                    )),
            )
            .into_any_element()
    }

    /// Connecting screen (Screenshot 13, light): emblem + name/endpoint row
    /// with a Show-logs pill, a static connecting rail, and a Close pill.
    /// No right rail on this screen; labels truncate with `min_w(0)`.
    fn render_connecting_screen(
        &mut self,
        session_index: usize,
        _window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (host_id, _state_label, title) = if let Some(session) = self.sessions.get(session_index)
        {
            (
                session.host.clone(),
                session.status.state.label(),
                session
                    .status
                    .title
                    .clone()
                    .unwrap_or_else(|| session.host.to_string()),
            )
        } else {
            return div().into_any_element();
        };

        let host = self.store.inventory().get(&host_id).cloned();
        let display_title = host.as_ref().map(|h| h.label.clone()).unwrap_or(title);
        let endpoint_str = host
            .as_ref()
            .map(|h| format!("SSH {}:{}", h.address, h.port))
            .unwrap_or_else(|| format!("SSH {host_id}"));

        let fg = cx.theme().foreground;
        let muted = cx.theme().muted_foreground;
        let rail_grey = rgb(0x5a5e73);

        // Neutral grey pills wrapping ghost buttons, so the button chrome
        // stays flat while the pill carries the fill.
        let pill = |label: Button| {
            div()
                .h(px(36.))
                .px_2()
                .flex()
                .flex_row()
                .items_center()
                .justify_center()
                .rounded(px(8.))
                .bg(rgb(0xe6ebed))
                .child(label)
        };

        div()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .size_full()
            .bg(cx.theme().background)
            .overflow_hidden()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .w_full()
                    .max_w(px(600.))
                    .min_w(px(0.))
                    .p_8()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_3()
                            .w_full()
                            .min_w(px(0.))
                            .child(
                                div()
                                    .size(px(40.))
                                    .flex_shrink_0()
                                    .rounded(px(10.))
                                    .bg(rgb(0xe95420))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(
                                        Icon::default()
                                            .data(glyph::UBUNTU)
                                            .size(px(24.))
                                            .text_color(rgb(0xffffff)),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .overflow_hidden()
                                    .child(
                                        div()
                                            .text_size(px(15.))
                                            .text_color(fg)
                                            .font_weight(gpui_kit::FontWeight::MEDIUM)
                                            .truncate()
                                            .child(SharedString::from(display_title.clone())),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(12.))
                                            .text_color(muted)
                                            .truncate()
                                            .child(SharedString::from(endpoint_str.clone())),
                                    ),
                            )
                            .child(pill(
                                Button::new("connecting-logs")
                                    .ghost()
                                    .small()
                                    .label("Show logs")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.select_left_nav(LeftNav::Logs, window, cx);
                                    })),
                            )),
                    )
                    // ponytail: static rail, not an animation — the only
                    // budgeted timer is the cursor blink. Upgrade with a
                    // rotation step driven by the pane's notify loop.
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .w_full()
                            .child(
                                div()
                                    .size(px(28.))
                                    .flex_shrink_0()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded_full()
                                    .bg(cx.theme().primary)
                                    .child(
                                        Icon::default()
                                            .data(glyph::PLUG)
                                            .size(px(16.))
                                            .text_color(rgb(0xffffff)),
                                    ),
                            )
                            .child(
                                div()
                                    .w(px(260.))
                                    .flex_1()
                                    .h(px(3.))
                                    .rounded_full()
                                    .bg(rail_grey),
                            )
                            .child(
                                div()
                                    .size(px(28.))
                                    .flex_shrink_0()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded_full()
                                    .bg(rail_grey)
                                    .child(
                                        Icon::default()
                                            .data(glyph::TERMINAL_PROMPT)
                                            .size(px(16.))
                                            .text_color(rgb(0xffffff)),
                                    ),
                            ),
                    )
                    .child(
                        div().flex().flex_row().items_center().child(pill(
                            Button::new("connecting-close")
                                .ghost()
                                .small()
                                .label("Close")
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.close_session(session_index, window, cx);
                                })),
                        )),
                    ),
            )
            .into_any_element()
    }
}

impl Render for SshDeck {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let foreground = cx.theme().foreground;

        let header = self.render_header(window, cx);
        let sidebar = self.render_sidebar(window, cx);
        let main = self.render_main(window, cx);
        let add_host_sheet = self.render_add_host_sheet(window, cx);
        let palette = self.palette.clone();

        let mut root = div()
            .flex()
            .flex_col()
            .size_full()
            // `--main-bg` is `#1d2033` (the theme's `sidebar` token); the
            // window's `background` token is the darker `--surface-lowest`.
            .bg(cx.theme().sidebar)
            .text_color(foreground)
            // The palette's navigation actions are bound in its own context. The
            // context is only active while the palette is open, so these keys are
            // never stolen from the terminal or an input.
            .when(palette.is_some(), |el| el.key_context(palette::CONTEXT))
            .on_action(cx.listener(Self::on_palette_up))
            .on_action(cx.listener(Self::on_palette_down))
            .on_action(cx.listener(Self::on_palette_cancel))
            .on_action(cx.listener(Self::handle_open_palette))
            .on_action(cx.listener(Self::handle_find_in_terminal))
            .on_action(cx.listener(Self::handle_new_tab))
            .on_action(cx.listener(Self::handle_close_tab))
            .on_action(cx.listener(Self::handle_toggle_sidebar))
            .on_action(cx.listener(Self::handle_open_settings))
            .on_action(cx.listener(Self::handle_zoom_in))
            .on_action(cx.listener(Self::handle_zoom_out))
            .on_action(cx.listener(Self::handle_reset_zoom))
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _window, cx| {
                if let Some((start_idx, start_pos)) = this.tab_drag_start {
                    if event.dragging() {
                        let dx = (f32::from(event.position.x) - f32::from(start_pos.x)).abs();
                        let dy = (f32::from(event.position.y) - f32::from(start_pos.y)).abs();
                        if (dx > 4.0 || dy > 4.0) && this.dragged_session != Some(start_idx) {
                            this.dragged_session = Some(start_idx);
                            this.dragged_tab = None;
                            cx.notify();
                        }
                    }
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _event: &MouseUpEvent, _window, _cx| {
                    this.tab_drag_start = None;
                }),
            )
            .child(header)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .overflow_hidden()
                    // Sessions are full-bleed: no left rail, no pane tab row.
                    .when(
                        !matches!(self.tab, MainTab::Session(_) | MainTab::Workspace),
                        |el| el.child(sidebar),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w(px(0.))
                            .overflow_hidden()
                            .child(main),
                    ),
            )
            .when_some(add_host_sheet, |el, sheet| el.child(sheet))
            .children(Root::render_dialog_layer(window, cx))
            .children(Root::render_sheet_layer(window, cx))
            .children(Root::render_notification_layer(window, cx));

        if let Some(palette) = palette {
            root = root.child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .flex()
                    .flex_col()
                    .items_center()
                    .pt(px(72.))
                    .child(palette),
            );
        }

        root
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| item.to_string()).collect()
    }

    fn exec(host: &str, command: &str) -> Mode {
        Mode::Exec {
            host: host.to_string(),
            command: command.to_string(),
        }
    }

    #[test]
    fn connect_and_exec_parse_in_either_order() {
        assert_eq!(
            parse_args(&argv(&["--connect", "prod", "--exec", "uptime"])),
            Ok(exec("prod", "uptime"))
        );
        assert_eq!(
            parse_args(&argv(&["--exec", "uptime", "--connect", "prod"])),
            Ok(exec("prod", "uptime"))
        );
    }

    #[test]
    fn no_flags_is_the_gui() {
        assert_eq!(parse_args(&argv(&[])), Ok(Mode::Gui));
    }

    #[test]
    fn missing_connect_is_a_usage_error() {
        assert!(parse_args(&argv(&["--exec", "uptime"])).is_err());
    }

    #[test]
    fn unknown_argument_is_a_usage_error() {
        assert!(parse_args(&argv(&["--connect", "prod", "--exec", "uptime", "--wat"])).is_err());
    }

    #[test]
    fn port_defaults_to_22_and_rejects_bad_values() {
        assert_eq!(parse_port(""), Ok(22));
        assert_eq!(parse_port("   "), Ok(22));
        assert_eq!(parse_port("2222"), Ok(2222));
        assert_eq!(parse_port(" 22 "), Ok(22));
        assert_eq!(parse_port("65535"), Ok(65535));
        assert!(parse_port("0").is_err());
        assert!(parse_port("65536").is_err());
        assert!(parse_port("-1").is_err());
        assert!(parse_port("ssh").is_err());
        assert!(parse_port("22a").is_err());
    }

    #[test]
    fn autoconnect_splits_a_comma_separated_list() {
        assert_eq!(autoconnect_targets("prod"), vec!["prod"]);
        assert_eq!(
            autoconnect_targets("prod, staging ,db-2"),
            vec!["prod", "staging", "db-2"]
        );
        assert!(autoconnect_targets("").is_empty());
        assert!(autoconnect_targets(" , ").is_empty());
    }

    #[test]
    fn start_pane_accepts_every_spelling_and_rejects_unknowns() {
        for spelling in ["hosts", "HOSTS", " hosts ", "Hosts"] {
            assert_eq!(
                parse_start_pane(spelling),
                Some(StartPane::Nav(LeftNav::Hosts))
            );
        }
        assert_eq!(
            parse_start_pane("Keychain"),
            Some(StartPane::Nav(LeftNav::Keychain))
        );
        for spelling in [
            "forward",
            "Forward",
            "port-forwarding",
            "port_forwarding",
            "Port Forwarding",
        ] {
            assert_eq!(
                parse_start_pane(spelling),
                Some(StartPane::Nav(LeftNav::PortForwarding))
            );
        }
        assert_eq!(
            parse_start_pane("snippets"),
            Some(StartPane::Nav(LeftNav::Snippets))
        );
        for spelling in ["known-hosts", "known_hosts", "Known Hosts", "KNOWN-HOSTS"] {
            assert_eq!(
                parse_start_pane(spelling),
                Some(StartPane::Nav(LeftNav::KnownHosts))
            );
        }
        assert_eq!(
            parse_start_pane("logs"),
            Some(StartPane::Nav(LeftNav::Logs))
        );
        assert_eq!(
            parse_start_pane("settings"),
            Some(StartPane::Nav(LeftNav::Settings))
        );
        assert_eq!(parse_start_pane("sftp"), Some(StartPane::Sftp));

        // Unknown and empty values fall back to the default instead of panicking.
        assert_eq!(parse_start_pane("keycahin"), None);
        assert_eq!(parse_start_pane(""), None);
        assert_eq!(parse_start_pane("   "), None);
    }

    #[test]
    fn closing_a_session_repairs_the_selected_tab() {
        assert_eq!(tab_after_close(MainTab::Session(2), 2), MainTab::Vaults);
        assert_eq!(tab_after_close(MainTab::Session(3), 1), MainTab::Session(2));
        assert_eq!(tab_after_close(MainTab::Session(1), 3), MainTab::Session(1));
        assert_eq!(tab_after_close(MainTab::Sftp, 0), MainTab::Sftp);
        assert_eq!(tab_after_close(MainTab::Vaults, 0), MainTab::Vaults);
        assert_eq!(tab_after_close(MainTab::NewTab, 0), MainTab::NewTab);
        assert_eq!(tab_after_close(MainTab::Workspace, 0), MainTab::Workspace);
    }

    #[test]
    fn sidebar_history_evicts_the_oldest_past_the_cap() {
        let mut history = Vec::new();
        for index in 0..MAX_HISTORY_ENTRIES {
            push_history(&mut history, format!("entry-{index}"));
        }
        assert_eq!(history.len(), MAX_HISTORY_ENTRIES);
        push_history(&mut history, "one-more".to_string());
        assert_eq!(history.len(), MAX_HISTORY_ENTRIES);
        assert_eq!(history[0], "entry-1");
        assert_eq!(history[MAX_HISTORY_ENTRIES - 1], "one-more");
    }

    fn pane(id: usize) -> WorkspaceNode {
        WorkspaceNode::Pane {
            tabs: vec![id],
            active: 0,
        }
    }

    #[test]
    fn closing_a_session_repairs_the_split_tree() {
        let flat = |tiles: &[usize]| WorkspaceNode::Split {
            dir: SplitDir::Row,
            weights: vec![1.0; tiles.len()],
            children: tiles.iter().map(|&tile| pane(tile)).collect(),
        };
        // The closed tile drops out; later indexes shift down.
        let (tree, focus) = workspace_remove(&flat(&[0, 1, 2]), 2, 1);
        assert_eq!((workspace_leaves(&tree), focus), (vec![0, 1], 1));
        // Focus into a removed tile clamps to the last remaining tile.
        let (tree, focus) = workspace_remove(&pane(3), 0, 3);
        assert_eq!((tree, focus), (WorkspaceNode::Empty, 0));
        // An unrelated close leaves tiles and focus alone.
        let (tree, focus) = workspace_remove(&flat(&[0, 2]), 1, 5);
        assert_eq!((workspace_leaves(&tree), focus), (vec![0, 2], 1));
        // Empty stays empty; a close cannot invent a focus.
        let (tree, focus) = workspace_remove(&WorkspaceNode::Empty, 0, 0);
        assert_eq!((tree, focus), (WorkspaceNode::Empty, 0));
        // A stale focus past the end clamps to the last tile.
        let (tree, focus) = workspace_remove(&flat(&[0, 1]), 9, 5);
        assert_eq!((workspace_leaves(&tree), focus), (vec![0, 1], 1));
        // Earlier indexes are untouched; only later ones shift.
        let (tree, focus) = workspace_remove(&flat(&[0, 1, 4]), 0, 1);
        assert_eq!((workspace_leaves(&tree), focus), (vec![0, 3], 0));
        // A split left with one child collapses into it.
        let nested = WorkspaceNode::Split {
            dir: SplitDir::Row,
            weights: vec![1.0, 1.0],
            children: vec![
                pane(0),
                WorkspaceNode::Split {
                    dir: SplitDir::Col,
                    weights: vec![1.0, 1.0],
                    children: vec![pane(1), pane(2)],
                },
            ],
        };
        let (tree, _) = workspace_remove(&nested, 0, 0);
        assert_eq!(
            tree,
            WorkspaceNode::Split {
                dir: SplitDir::Col,
                weights: vec![1.0, 1.0],
                children: vec![pane(0), pane(1)],
            }
        );
    }

    #[test]
    fn workspace_split_nests_without_duplicates() {
        // An empty tree tiles directly.
        assert_eq!(workspace_insert(&WorkspaceNode::Empty, 0, 1), (pane(1), 0));
        // Splitting a lone pane wraps it across the row axis.
        let (tree, focus) = workspace_insert(&pane(0), 0, 1);
        assert_eq!(focus, 1);
        assert_eq!(
            tree,
            WorkspaceNode::Split {
                dir: SplitDir::Row,
                weights: vec![1.0, 1.0],
                children: vec![pane(0), pane(1)],
            }
        );
        // Splitting inside a row nests a column: the reference's
        // full-height pane beside a stacked pair.
        let (tree, focus) = workspace_insert(&tree, 1, 2);
        assert_eq!((workspace_leaves(&tree), focus), (vec![0, 1, 2], 2));
        assert!(matches!(
            &tree,
            WorkspaceNode::Split {
                dir: SplitDir::Row,
                children,
                ..
            } if matches!(
                &children[1],
                WorkspaceNode::Split {
                    dir: SplitDir::Col,
                    ..
                }
            )
        ));
        // An already-tiled session focuses instead of duplicating, so every
        // tile keeps its own pane entity.
        let (same, focus) = workspace_insert(&tree, 0, 1);
        assert_eq!((same, focus), (tree, 1));
        // A stale focus splits the last tile rather than panicking.
        let (tree, focus) = workspace_insert(&pane(0), 9, 1);
        assert_eq!((workspace_leaves(&tree), focus), (vec![0, 1], 1));
    }

    #[test]
    fn workspace_insert_tab_into_pane_works() {
        let tree = pane(0);
        let updated = workspace_insert_tab_into_pane(&tree, 0, 1);
        assert_eq!(
            updated,
            WorkspaceNode::Pane {
                tabs: vec![0, 1],
                active: 1,
            }
        );
    }

    #[test]
    fn split_weights_default_to_equal_shares() {
        assert_eq!(normalize_weights(&[], 3), vec![1.0, 1.0, 1.0]);
        assert_eq!(normalize_weights(&[1.0, 1.0, 2.0], 3), vec![1.0, 1.0, 2.0]);
        // Short tables pad; zero, negative, and non-finite entries fall back
        // to all-equal rather than collapsing a tile.
        assert_eq!(normalize_weights(&[2.0], 2), vec![2.0, 1.0]);
        assert_eq!(normalize_weights(&[1.0, 0.0], 2), vec![1.0, 1.0]);
        assert_eq!(normalize_weights(&[1.0, -3.0], 2), vec![1.0, 1.0]);
        assert_eq!(normalize_weights(&[1.0, f32::NAN], 2), vec![1.0, 1.0]);
    }

    #[test]
    fn vault_subtitle_and_grid_columns_follow_termius() {
        let mut host = Host::new("prod", "10.0.0.1");
        host.username = "root".to_string();
        host.tags.push("prod".into());
        // Tags live in Host Details pills, never in the subtitle: protocols
        // first, then the login for each.
        assert_eq!(host_subtitle(&host), "ssh, root");
        host.protocols.push("telnet".to_string());
        assert_eq!(host_subtitle(&host), "ssh, telnet, root, root");
        host.username.clear();
        assert_eq!(host_subtitle(&host), "ssh, telnet");
        host.protocols.clear();
        assert_eq!(host_subtitle(&host), "ssh");

        assert_eq!(grid_columns(359.9), 1);
        assert_eq!(grid_columns(360.0), 2);
        assert_eq!(grid_columns(699.9), 2);
        assert_eq!(grid_columns(700.0), 3);
        assert_eq!(grid_columns(1199.9), 3);
        assert_eq!(grid_columns(1200.0), 4);
    }

    #[test]
    fn chrome_breakpoints_collapse_before_they_clip() {
        // Rail: 60px icons when manually collapsed or below ~900px.
        assert_eq!(rail_width(1400.0, false), 185.0);
        assert_eq!(rail_width(900.0, false), 185.0);
        assert_eq!(rail_width(899.9, false), 60.0);
        assert_eq!(rail_width(1400.0, true), 60.0);
        // Drawer: overlay below ~800px, 300px at 800–1100, 360px above.
        assert!(details_overlay(799.9));
        assert!(!details_overlay(800.0));
        assert_eq!(details_width(800.0), 300.0);
        assert_eq!(details_width(1099.9), 300.0);
        assert_eq!(details_width(1100.0), 360.0);
    }

    #[test]
    fn os_brand_colours_are_known_or_absent() {
        assert_eq!(os_brand_color("ubuntu"), Some(rgb(0xe95420)));
        assert_eq!(os_brand_color(" Ubuntu "), Some(rgb(0xe95420)));
        assert_eq!(os_brand_color("raspbian"), os_brand_color("pi"));
        // No recovered value: the tint must stay unknown, not invented.
        assert_eq!(os_brand_color("alpine"), None);
        assert_eq!(os_brand_color("plan9"), None);

        let fallback: Hsla = rgb(0x8d91a5).into();
        let mut host = Host::new("box", "10.0.0.1");
        assert_eq!(host_os_tint(&host, fallback), fallback);

        host.tags.push("debian".into());
        // Compare in `Hsla` space: an `Hsla -> Rgba -> Hsla` round trip loses a
        // least-significant bit (0xce0056 renders as 0xce0055 on the way back).
        assert_eq!(host_os_tint(&host, fallback), Hsla::from(rgb(0xce0056)));
    }

    #[test]
    fn popovers_clamp_and_drawers_replace_below_600px() {
        // max_w is min(320, 90vw) floored at 160 so a pill never collapses.
        assert_eq!(popover_max_w(1200.0), 320.0);
        assert_eq!(popover_max_w(300.0), 270.0);
        assert_eq!(popover_max_w(100.0), 160.0);
        // max_h is 70vh floored at 160, always paired with a scrollbar.
        assert_eq!(popover_max_h(1000.0), 700.0);
        assert_eq!(popover_max_h(100.0), 160.0);
        // Drawer breakpoint.
        assert!(use_full_drawer(599.9));
        assert!(!use_full_drawer(600.0));
        // Grid cards shrink, list rows and 44px touch targets do not.
        assert_eq!(host_card_height(true, 400.0), 48.0);
        assert_eq!(host_card_height(false, 400.0), 56.0);
        assert_eq!(host_card_height(false, 1200.0), 68.0);
    }

    fn test_vault(name: &str) -> (Vault, std::path::PathBuf) {
        // Passphrase vaults never touch the OS keychain: safe on CI runners.
        let dir = std::env::temp_dir().join(format!("sshdeck-vault-test-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        let vault =
            Vault::open_with_passphrase(dir.join("vault.json"), b"test-passphrase").unwrap();
        (vault, dir)
    }

    #[test]
    fn password_secret_id_is_stable_per_host() {
        let host = Host::new("vps", "10.0.0.1");
        let first = password_secret_id(&host.id);
        assert!(first.starts_with("login:"));
        assert_eq!(first, password_secret_id(&host.id));
        assert_ne!(
            first,
            password_secret_id(&Host::new("other", "10.0.0.2").id)
        );
    }

    #[test]
    fn vaulted_password_resolves_and_missing_is_none() {
        let (mut vault, dir) = test_vault("resolve");
        assert_eq!(resolve_password(Some(&vault), "login:x"), None);
        assert_eq!(resolve_password(None, "login:x"), None);
        vault.set("login:x", "s3cret").unwrap();
        assert_eq!(
            resolve_password(Some(&vault), "login:x").as_deref(),
            Some("s3cret")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_plaintext_refs_migrate_to_stable_ids() {
        let dir = std::env::temp_dir().join("sshdeck-vault-test-migrate");
        let _ = std::fs::remove_dir_all(&dir);
        let mut store = HostStore::new(dir.join("hosts.json"));
        let mut host = Host::new("vps", "10.0.0.1");
        host.auth = sshdeck_core::AuthMethod::Password {
            secret_ref: "legacy-typed-password".into(),
        };
        store.inventory_mut().upsert(host.clone());
        let (mut vault, _) = test_vault("migrate");
        assert_eq!(migrate_legacy_password_refs(&mut store, &mut vault), 1);
        let migrated = store.inventory().get(&host.id).unwrap();
        let sshdeck_core::AuthMethod::Password { secret_ref } = &migrated.auth else {
            panic!("auth must stay password");
        };
        assert_eq!(secret_ref, &password_secret_id(&host.id));
        assert_eq!(
            resolve_password(Some(&vault), secret_ref).as_deref(),
            Some("legacy-typed-password")
        );
        // Second run is a no-op: the stable id already resolves.
        assert_eq!(migrate_legacy_password_refs(&mut store, &mut vault), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_parse_quick_connect() {
        // User and host
        let h1 = parse_quick_connect("root@192.168.1.50").unwrap();
        assert_eq!(h1.username, "root");
        assert_eq!(h1.address, "192.168.1.50");
        assert_eq!(h1.port, 22);

        // User, host and custom port
        let h2 = parse_quick_connect("ubuntu@ec2.aws.com:2222").unwrap();
        assert_eq!(h2.username, "ubuntu");
        assert_eq!(h2.address, "ec2.aws.com");
        assert_eq!(h2.port, 2222);

        // ssh command syntax with -p
        let h3 = parse_quick_connect("ssh -p 2200 admin@myserver.local").unwrap();
        assert_eq!(h3.username, "admin");
        assert_eq!(h3.address, "myserver.local");
        assert_eq!(h3.port, 2200);

        let h4 = parse_quick_connect("ssh debian@example.com -p 222").unwrap();
        assert_eq!(h4.username, "debian");
        assert_eq!(h4.address, "example.com");
        assert_eq!(h4.port, 222);

        // IP with port
        let h5 = parse_quick_connect("10.0.0.1:8022").unwrap();
        assert_eq!(h5.username, "");
        assert_eq!(h5.address, "10.0.0.1");
        assert_eq!(h5.port, 8022);

        // Plain IP
        let h6 = parse_quick_connect("192.168.1.1").unwrap();
        assert_eq!(h6.address, "192.168.1.1");
        assert_eq!(h6.port, 22);

        // Domain
        let h7 = parse_quick_connect("my-box.lan").unwrap();
        assert_eq!(h7.address, "my-box.lan");
        assert_eq!(h7.port, 22);

        // Plain search query (should NOT parse as quick connect)
        assert!(parse_quick_connect("production").is_none());
        assert!(parse_quick_connect("database").is_none());
        assert!(parse_quick_connect("").is_none());
    }

    #[test]
    fn local_terminal_instance_naming() {
        let mut titles: Vec<String> = Vec::new();
        let get_next_title = |titles: &[String]| {
            let mut instance_num = 1;
            while titles
                .iter()
                .any(|t| t == &format!("Local Terminal ({instance_num})"))
            {
                instance_num += 1;
            }
            format!("Local Terminal ({instance_num})")
        };

        let t1 = get_next_title(&titles);
        assert_eq!(t1, "Local Terminal (1)");
        titles.push(t1);

        let t2 = get_next_title(&titles);
        assert_eq!(t2, "Local Terminal (2)");
        titles.push(t2);

        // Remove (1), next should reuse (1)
        titles.remove(0);
        let t3 = get_next_title(&titles);
        assert_eq!(t3, "Local Terminal (1)");
    }
}
