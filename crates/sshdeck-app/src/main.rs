//! sshdeck — a GPUI desktop SSH client.
//!
//! The shell (host inventory, filtering, selection, persistence, theming) is
//! milestone 1. This is milestone 2: the pane next to it is a real terminal
//! backed by the `sshdeck-core` transport and the `sshdeck-terminal` grid.

mod forward_pane;
mod keys_pane;
mod palette;
mod settings;
mod sftp_pane;
mod terminal;

use gpui_kit::component::{
    button::{Button, ButtonVariants as _},
    input::{Input, InputContentType, InputEvent, InputState},
    notification::Notification,
    scroll::ScrollableElement as _,
    tooltip::Tooltip,
    ActiveTheme as _, Disableable as _, Icon, IconName, InteractiveElementExt as _, Root,
    Sizable as _, Theme, ThemeMode, ThemeRegistry, WindowExt,
};
use gpui_kit::prelude::{FluentBuilder as _, StatefulInteractiveElement as _};
use gpui_kit::{
    div, point, px, rgb, rgba, AnyElement, App, AppContext as _, Context, Entity, Focusable as _,
    Hsla, InteractiveElement as _, IntoElement, ParentElement as _, Render, Rgba, SharedString,
    Styled as _, Subscription, TitlebarOptions, Window, WindowControlArea, WindowOptions,
};
use keys_pane::KeysPane;
use palette::PaletteView;
use settings::SettingsView;
use sftp_pane::SftpPane;
use sshdeck_core::session::{Session as SshSession, SessionConfig, SessionEvent};
use sshdeck_core::{Host, HostId, HostStore, SessionState};
use sshdeck_sftp::SftpClient;
use terminal::{PaneStatus, TerminalPane};

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
/// 51px tab row inside the 56px header (the [gpui-component `TitleBar`] uses
/// `(9, 9)` for its 34px bar; `(9, 20)` centres a ~14px light group in 56px).
/// The header marks itself as `WindowControlArea::Drag`, so the window can be
/// dragged from the whole header even though the system title bar is hidden.
///
/// [gpui-component `TitleBar`]: https://docs.rs/gpui-component
fn window_options() -> WindowOptions {
    WindowOptions {
        titlebar: Some(TitlebarOptions {
            title: None,
            appears_transparent: true,
            traffic_light_position: Some(point(px(9.), px(20.))),
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

/// Left navigation rail entries — mirrors Termius sidebar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LeftNav {
    Hosts,
    Keychain,
    PortForwarding,
    Snippets,
    KnownHosts,
    Logs,
}

/// A full-region pane that shows over the session content while it is open.
///
/// Each variant is created on demand and dropped when it closes, so a pane that
/// holds a transport never outlives its own visibility.
enum Overlay {
    Settings(Entity<SettingsView>),
    Keys(Entity<KeysPane>),
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
    /// The command palette while it is open, if at all.
    palette: Option<Entity<PaletteView>>,
    /// The settings or keys pane showing over the session, if at all.
    overlay: Option<Overlay>,
    /// The SFTP pane, created when the SFTP tab is first selected so a transport
    /// is not opened until then.
    sftp_pane: Option<Entity<SftpPane>>,
    /// Whether the host sidebar is collapsed to its narrow rail.
    sidebar_collapsed: bool,
    /// Whether the add-host sheet is open. Host creation lives behind the
    /// header's `+` control rather than an always-visible form.
    add_host_open: bool,
    /// Index of the session the SFTP pane is attached to, if any.
    sftp_attached: Option<usize>,
    /// Bumped per attach attempt so a superseded SFTP connect is ignored.
    sftp_generation: u64,
    /// Subscription handles must outlive construction, so they are owned here.
    _subscriptions: Vec<Subscription>,
}

impl SshDeck {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut store = HostStore::at_default_path();
        // A store that cannot be read is empty, not fatal: the UI surfaces the
        // problem on the next save rather than refusing to start.
        let load_error = store.load().err();

        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Search hosts"));
        let draft_label = cx.new(|cx| InputState::new(window, cx).placeholder("Label"));
        let draft_address = cx.new(|cx| InputState::new(window, cx).placeholder("hostname or IP"));
        let draft_username =
            cx.new(|cx| InputState::new(window, cx).placeholder("username (optional)"));
        let draft_port = cx.new(|cx| InputState::new(window, cx).placeholder("port (default 22)"));

        // Re-render the list as the query changes; the filter itself is applied
        // in `render`, so no filtered copy needs to be kept in state.
        let subscriptions = vec![cx.subscribe_in(&filter, window, |_, _, event, _, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        })];

        if let Some(error) = load_error {
            let message = SharedString::from(format!("Could not load hosts: {error}"));
            cx.defer_in(window, move |_, window, cx| {
                window.push_notification(Notification::error(message), cx);
            });
        }

        let view = Self {
            store,
            selected: None,
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
            sidebar_collapsed: false,
            add_host_open: false,
            sftp_attached: None,
            sftp_generation: 0,
            _subscriptions: subscriptions,
        };

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
            });
        }
        view
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
        if let Err(error) = self.store.save() {
            window.push_notification(Notification::error(format!("Could not save: {error}")), cx);
        }
        cx.notify();
    }

    /// Opens a session for `host`, prompting for a password first when the host
    /// authenticates with one and no secret is available yet.
    fn connect(&mut self, host: Host, window: &mut Window, cx: &mut Context<Self>) {
        // An existing tab for this host is focused instead of opening a second
        // connection to the same place.
        if let Some(index) = self.sessions.iter().position(|s| s.host == host.id) {
            self.active = Some(index);
            self.tab = MainTab::Session(index);
            self.overlay = None;
            let pane = self.sessions[index].pane.clone();
            pane.update(cx, |pane, cx| pane.focus(window, cx));
            cx.notify();
            return;
        }

        let config = match &host.auth {
            // The password lives in the OS keychain, which the vault crate owns.
            // Until that is wired into this view, ask for it once and use it for
            // this connection only: it is never written to the inventory.
            //
            // Development and test affordance, not a user feature: when
            // `SSHDECK_PASSWORD` is set, use it instead of prompting so an
            // automated run can connect. It is read straight from the
            // environment into the connection config — never logged, never
            // written to disk, never shown in the UI. Without it, the prompt is
            // exactly as before.
            sshdeck_core::AuthMethod::Password { .. } => match env_password() {
                Some(password) => SessionConfig::from_host(&host).with_password(password),
                None => {
                    self.prompt_for_secret(&host, window, cx);
                    return;
                }
            },
            _ => SessionConfig::from_host(&host),
        };

        self.open_pane(&host, config, window, cx);
    }

    /// Asks for the host's password before connecting.
    ///
    /// The password is used for this connection only and never reaches the
    /// inventory. The keychain-backed vault owns persistent secrets; until it is
    /// wired into this view, this is the honest way to connect at all.
    fn prompt_for_secret(&mut self, host: &Host, window: &mut Window, cx: &mut Context<Self>) {
        let prompt = cx.new(|cx| InputState::new(window, cx).placeholder("Password"));
        let label = host.label.clone();
        self.secret = Some((host.id.clone(), prompt));
        window.push_notification(Notification::info(format!("{label} needs a password")), cx);
        cx.notify();
    }

    /// Connects the host whose password was just entered, then clears the prompt.
    fn submit_secret(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((host_id, input)) = self.secret.take() else {
            return;
        };
        let Some(host) = self.store.inventory().get(&host_id).cloned() else {
            return;
        };
        // A password is not persisted; it exists for this connection only.
        let password = input.read(cx).value().to_string();
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
        let pane = cx.new(|cx| TerminalPane::new(config, window, cx));
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
            cx.notify();
        });
        self._subscriptions.push(subscription);

        let status = pane.read(cx).status();
        let failed = match &status.state {
            SessionState::Failed { message } => Some(message.clone()),
            _ => None,
        };
        self.sessions.push(Session {
            host: host.id.clone(),
            pane: pane.clone(),
            status,
        });
        let index = self.sessions.len() - 1;
        self.active = Some(index);
        self.tab = MainTab::Session(index);
        self.overlay = None;
        pane.update(cx, |pane, cx| pane.focus(window, cx));
        // The new tab is active now; the SFTP pane must follow it even before the
        // session reaches `Connected` (it detaches until then).
        self.reconcile_sftp(window, cx);

        if let Some(message) = failed {
            window.push_notification(
                Notification::error(format!("{} could not connect: {message}", host.label)),
                cx,
            );
        }
        cx.notify();
    }

    /// The tab the pane is showing, if any.
    fn active_session(&self) -> Option<&Session> {
        self.active.and_then(|index| self.sessions.get(index))
    }

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

    fn open_keys(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.overlay, Some(Overlay::Keys(_))) {
            self.close_overlay(window, cx);
            return;
        }
        let view = cx.new(|cx| KeysPane::new(window, cx));
        self.show_overlay(Some(Overlay::Keys(view)), window, cx);
    }

    /// Switches the main region to `tab`, creating the SFTP pane on first use.
    fn select_tab(&mut self, tab: MainTab, window: &mut Window, cx: &mut Context<Self>) {
        self.overlay = None;
        self.tab = tab;
        if matches!(tab, MainTab::Sftp) && self.sftp_pane.is_none() {
            self.sftp_pane = Some(cx.new(|cx| SftpPane::new(window, cx)));
        }
        if let MainTab::Session(index) = tab {
            self.active = Some(index);
            if let Some(session) = self.sessions.get(index) {
                let pane = session.pane.clone();
                pane.update(cx, |pane, cx| pane.focus(window, cx));
            }
        }
        self.reconcile_sftp(window, cx);
        cx.notify();
    }

    /// Closes one session tab and repairs the focused-tab bookkeeping.
    fn close_session(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index >= self.sessions.len() {
            return;
        }
        self.sessions.remove(index);
        self.tab = tab_after_close(self.tab, index);
        self.active = match self.active {
            Some(active) if active == index => None,
            Some(active) if active > index => Some(active - 1),
            other => other,
        };
        self.reconcile_sftp(window, cx);
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
        let session = match self.sessions.get(index) {
            Some(session) => session.pane.read(cx).session(),
            None => None,
        };
        let Some(session) = session else {
            return;
        };

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
            // The palette marks these unavailable and never dispatches them.
            palette::CommandId::OpenSftp
            | palette::CommandId::ManageKeys
            | palette::CommandId::OpenSettings => {}
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

    /// The single 56px app header: sidebar toggle, the session tabs, the add
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
    fn render_header(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let collapsed = self.sidebar_collapsed;
        // Header stays dark navy #1d2033 in both Light and Dark (hardcoded; no theme token covers it in Light).
        let header_bg = rgb(0x1d2033);
        let header_fg = rgb(0xffffff);
        let header_muted = rgb(0x8d91a5);

        let actions = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .flex_shrink_0()
            .child(
                Button::new("palette")
                    .ghost()
                    .icon(IconName::Search)
                    .tooltip("Command palette")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_palette(window, cx);
                    })),
            )
            .child(
                Button::new("notifications")
                    .ghost()
                    .icon(IconName::Bell)
                    .tooltip("Notifications")
                    .on_click(|_, window, cx| {
                        window
                            .push_notification(Notification::info("Notifications coming soon"), cx);
                    }),
            )
            .child(
                Button::new("account")
                    .ghost()
                    .icon(IconName::CircleUser)
                    .tooltip("Settings & account")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_settings(window, cx);
                    })),
            );

        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .flex_shrink_0()
            .h(px(56.))
            .bg(header_bg)
            .text_color(header_fg)
            .pl(if cfg!(target_os = "macos") {
                px(80.)
            } else {
                px(12.)
            })
            .pr_3()
            .border_b_1()
            .border_color(rgba(0x8d91a540))
            .window_control_area(WindowControlArea::Drag)
            .child(
                Button::new("sidebar-toggle")
                    .ghost()
                    .icon(if collapsed {
                        IconName::PanelLeftOpen
                    } else {
                        IconName::PanelLeftClose
                    })
                    .tooltip(if collapsed {
                        "Expand sidebar"
                    } else {
                        "Collapse sidebar"
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.sidebar_collapsed = !this.sidebar_collapsed;
                        cx.notify();
                    })),
            )
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

    /// One host card: `--entity-item-background` at rest, `--list-hover` on
    /// hover, `--list-select` when selected. The icon is tinted with the host's
    /// OS brand colour when a group or tag names a known platform; otherwise it
    /// keeps `--text-secondary` (`Host` has no OS field yet).
    ///
    /// `prefix` namespaces the element ids: the same host can be on screen in
    /// both the sidebar and the Vaults tab, and ids must be unique.
    fn render_host_row(&mut self, host: &Host, prefix: &str, cx: &mut Context<Self>) -> AnyElement {
        let id = host.id.clone();
        let is_selected = self.selected.as_ref() == Some(&id);
        let is_open = self.sessions.iter().any(|s| s.host == id);
        let remove_id = id.clone();
        let connect_host = host.clone();
        let muted = cx.theme().muted_foreground;
        // `--entity-item-background`.
        let card = cx.theme().muted;
        // `--list-select` / `--list-hover`. The selected value maps to
        // `list.active.background`; the hover token currently equals the card
        // background, so the recovered hex is used directly (AGENTS.md errata).
        let card_selected = cx.theme().list_active;
        let card_hover = rgb(0x3e4257);
        let tint = host_os_tint(host, muted);

        div()
            .id(SharedString::from(format!("{prefix}-host-{id}")))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.selected = Some(id.clone());
                cx.notify();
            }))
            .on_double_click(cx.listener(move |this, _, window, cx| {
                this.connect(connect_host.clone(), window, cx);
            }))
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .w_full()
            .px_2()
            .py_2()
            .rounded_md()
            .cursor_pointer()
            .bg(if is_selected { card_selected } else { card })
            // `--list-hover`: the card highlight, via the hover style
            // refinement (`StatefulInteractiveElement::hover`).
            .hover(|style| style.bg(card_hover))
            .child(Icon::new(IconName::Globe).text_color(tint))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .overflow_hidden()
                    .child(SharedString::from(host.label.clone()))
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(SharedString::from(host.endpoint())),
                    ),
            )
            .when(is_open, |el| {
                el.child(Icon::new(IconName::Check).text_color(cx.theme().success))
            })
            .child(
                Button::new(SharedString::from(format!("{prefix}-remove-{remove_id}")))
                    .ghost()
                    .icon(IconName::Close)
                    .tooltip("Remove host")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.remove_host(&remove_id, window, cx);
                    })),
            )
            .into_any_element()
    }

    fn render_sidebar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        self.render_left_rail(cx)
    }

    fn render_left_rail(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        // Hardcoded light rail colours (no token for #f7f9fa sidebar/list).
        let rail_bg = rgb(0xf7f9fa);
        let border = rgb(0xd5dde0);
        let active_bg = cx.theme().muted; // #e6ebed in light
        let fg = cx.theme().foreground; // #141729
        let muted = cx.theme().muted_foreground; // #798c94

        let nav_item = |label: &'static str, icon: IconName, _nav: LeftNav, is_active: bool| {
            div()
                .id(SharedString::from(format!("nav-{label}")))
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .h(px(44.))
                .px_3()
                .rounded(px(8.))
                .cursor_pointer()
                .when(is_active, |el| el.bg(active_bg))
                .text_color(if is_active { fg } else { muted })
                .child(Icon::new(icon).text_color(if is_active { fg } else { muted }))
                .child(label)
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
            .w(if self.sidebar_collapsed {
                px(60.)
            } else {
                px(230.)
            })
            .h_full()
            .bg(rail_bg)
            .border_r_1()
            .border_color(border)
            .p_2()
            .gap_1()
            .child(
                nav_item(
                    "Hosts",
                    IconName::LayoutDashboard,
                    LeftNav::Hosts,
                    hosts_active,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.left_nav = LeftNav::Hosts;
                    cx.notify();
                })),
            )
            .child(
                nav_item(
                    "Keychain",
                    IconName::HardDrive,
                    LeftNav::Keychain,
                    keychain_active,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.left_nav = LeftNav::Keychain;
                    cx.notify();
                })),
            )
            .child(
                nav_item(
                    "Port Forwarding",
                    IconName::ExternalLink,
                    LeftNav::PortForwarding,
                    pf_active,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.left_nav = LeftNav::PortForwarding;
                    cx.notify();
                })),
            )
            .child(
                nav_item(
                    "Snippets",
                    IconName::File,
                    LeftNav::Snippets,
                    snippets_active,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.left_nav = LeftNav::Snippets;
                    cx.notify();
                })),
            )
            .child(
                nav_item(
                    "Known Hosts",
                    IconName::Globe,
                    LeftNav::KnownHosts,
                    known_active,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.left_nav = LeftNav::KnownHosts;
                    cx.notify();
                })),
            )
            .child(
                nav_item("Logs", IconName::Inbox, LeftNav::Logs, logs_active).on_click(
                    cx.listener(|this, _, _, cx| {
                        this.left_nav = LeftNav::Logs;
                        cx.notify();
                    }),
                ),
            )
    }

    fn render_empty_state(&self, title: &str, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        div()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_2()
            .size_full()
            .p_8()
            .child(Icon::new(IconName::Inbox).large().text_color(muted))
            .child(
                div()
                    .text_color(muted)
                    .child(SharedString::from(title.to_string())),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child("No data yet — this pane is not wired."),
            )
            .into_any_element()
    }

    /// The add-host sheet: a right-hand panel over a scrim, carrying the same
    /// four fields and the same validation the always-visible sidebar form had.
    /// Opening and closing is the header `+` and the sheet's close control.
    fn render_add_host_sheet(&mut self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        if !self.add_host_open {
            return None;
        }

        let panel = div()
            .absolute()
            .top_0()
            .right_0()
            .h_full()
            .w(px(360.))
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
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

    /// One fixed header tab (Vaults or SFTP): 51px tall inside the 56px header,
    /// 6px radius, transparent until selected. Uses 16px icons throughout the
    /// chrome (medium, the default) with the heavier glyph choice for each tab.
    fn render_fixed_tab(
        &mut self,
        label: &'static str,
        icon: IconName,
        target: MainTab,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let is_active = self.tab == target;
        let muted = cx.theme().muted_foreground; // #8d91a5
        let selected_bg = cx.theme().muted; // #282b3d
        let selected_fg = cx.theme().foreground; // #ffffff
                                                 // `--list-hover` (#3e4257) — token `list.hover.background` still equals
                                                 // the card background in the bundled theme, so the recovered hex is used.
        let hover_bg = rgb(0x3e4257);

        div()
            .id(SharedString::from(format!("tab-{label}")))
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_3()
            .h(px(51.))
            .rounded_md()
            .cursor_pointer()
            .text_color(if is_active { selected_fg } else { muted })
            .when(is_active, |el| el.bg(selected_bg))
            .when(!is_active, |el| el.hover(move |s| s.bg(hover_bg)))
            .child(Icon::new(icon).text_color(if is_active { selected_fg } else { muted }))
            .child(label)
            .on_click(cx.listener(move |this, _, window, cx| {
                this.select_tab(target, window, cx);
            }))
            .into_any_element()
    }

    /// The tab row: the two fixed tabs (Vaults, SFTP), then one tab per open
    /// session, then the add control.
    ///
    /// `--horizontal-tabs-height` is 51px, centred in the 56px header. The
    /// selected tab is one step lighter (`--surface-high`, the theme's `muted`
    /// background, `#282b3d`) with primary text; unselected tabs are transparent
    /// with the secondary `#8d91a5` text and stay 6px-rounded. A session tab
    /// carries a terminal glyph (SquareTerminal) tinted by connection state
    /// instead of a dot, then the label, then the close `✕`.
    fn render_tabs(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground; // #8d91a5
        let selected_bg = cx.theme().muted; // #282b3d
        let selected_fg = cx.theme().foreground; // #ffffff
        let hover_bg = rgb(0x3e4257); // --list-hover
        let active = self.active;

        let vaults =
            self.render_fixed_tab("Vaults", IconName::LayoutDashboard, MainTab::Vaults, cx);
        // Heavier closed-folder glyph reads more solid than FolderOpen.
        let sftp = self.render_fixed_tab("SFTP", IconName::Folder, MainTab::Sftp, cx);

        let mut strip = div()
            .id("tab-strip")
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .h(px(51.))
            .flex_1()
            .min_w(px(0.))
            .overflow_x_scrollbar()
            .child(vaults)
            .child(sftp);

        for (index, session) in self.sessions.iter().enumerate() {
            let label = session.status.title.clone().unwrap_or_else(|| {
                self.store
                    .inventory()
                    .get(&session.host)
                    .map(|host| host.label.clone())
                    .unwrap_or_else(|| session.host.to_string())
            });
            // A session tab is only selected while no pane overlay covers it.
            let is_active = self.overlay.is_none() && active == Some(index);
            // Glyph tint carries connection state (connected #21b568 via
            // `success`, connecting #f2c94c via `warning`, failed #f25e61 via
            // `danger`, idle #8d91a5 via `muted_foreground`). The session tab
            // order is glyph, label, close `✕` — the dot is gone.
            let glyph_color = match &session.status.state {
                SessionState::Connected => cx.theme().success, // #21b568
                SessionState::Connecting | SessionState::Authenticating => cx.theme().warning, // #f2c94c
                SessionState::Failed { .. } => cx.theme().danger, // #f25e61
                SessionState::Disconnected | SessionState::Closed { .. } => {
                    cx.theme().muted_foreground // #8d91a5
                }
            };
            let state_label = session.status.state.label();
            let id = session.host.clone();
            let reconnect_id = id.clone();
            let close_id = id.clone();

            strip = strip.child(
                div()
                    .id(SharedString::from(format!("tab-{id}")))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .h(px(51.))
                    .rounded_md()
                    .cursor_pointer()
                    .text_color(if is_active { selected_fg } else { muted })
                    .when(is_active, |el| el.bg(selected_bg))
                    .when(!is_active, |el| el.hover(move |s| s.bg(hover_bg)))
                    // (unconfirmed) Div tooltip via closure; verified tooltip
                    // path is Button::tooltip(string) — this Div form mirrors
                    // docs/UI-PARITY.md "plus a tooltip" requirement.
                    .tooltip({
                        let tip = state_label.clone();
                        move |window, cx| Tooltip::new(tip.clone()).build(window, cx)
                    })
                    .child(Icon::new(IconName::SquareTerminal).text_color(glyph_color))
                    .child(SharedString::from(label))
                    .child(
                        Button::new(SharedString::from(format!("reconnect-tab-{reconnect_id}")))
                            .ghost()
                            .icon(IconName::RotateCw)
                            .tooltip("Reconnect")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                if let Some(host) =
                                    this.store.inventory().get(&reconnect_id).cloned()
                                {
                                    this.connect(host, window, cx);
                                }
                            })),
                    )
                    .child(
                        Button::new(SharedString::from(format!("close-tab-{close_id}")))
                            .ghost()
                            .icon(IconName::Close)
                            .tooltip("Close session")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.close_session(index, window, cx);
                            })),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.select_tab(MainTab::Session(index), window, cx);
                    })),
            );
        }

        strip.child(
            Button::new("add-host-tab")
                .ghost()
                .icon(IconName::Plus)
                .tooltip("Add host")
                .on_click(cx.listener(|this, _, window, cx| {
                    this.open_add_host(window, cx);
                })),
        )
    }

    /// ~28px band directly under the 56px header, spanning **only the pane
    /// area, not the sidebar**. Holds the focused pane's own tabs plus a `+`.
    /// Visually quiet: no background fill of its own, a lighter active tab
    /// (`#282b3d`, `tab.active.background`), `12px` text. Always rendered,
    /// even with a single tab — the original always shows it.
    fn render_pane_tabs(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let active_bg = cx.theme().muted; // #282b3d `tab.active.background`
        let active_fg = cx.theme().foreground; // #ffffff `tab.active.foreground`
        let hover_bg = rgb(0x3e4257); // --list-hover, no token

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
            .border_color(rgba(0x8d91a51a)) // --border-light
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

    fn render_main(&mut self, cx: &mut Context<Self>) -> AnyElement {
        if self.overlay.is_some() {
            return self.render_overlay(cx);
        }
        match self.tab {
            MainTab::Vaults => {
                match self.left_nav {
                    LeftNav::Hosts => self.render_vault(cx),
                    LeftNav::Keychain | LeftNav::KnownHosts => {
                        // KeysPane is reachable via the header Keys button and palette;
                        // render an honest placeholder that names it (no fake list).
                        self.render_empty_state(
                            "Keychain / Known Hosts — open via header Keys or palette (KeysPane)",
                            cx,
                        )
                    }
                    LeftNav::PortForwarding => self.render_empty_state(
                        "Port Forwarding — open via palette or header (ForwardPane)",
                        cx,
                    ),
                    LeftNav::Snippets => self.render_empty_state("Snippets — no snippets yet", cx),
                    LeftNav::Logs => self.render_empty_state("Logs — no logs yet", cx),
                }
            }
            MainTab::Sftp => self.render_sftp(cx),
            MainTab::Session(_) => self.render_session(cx),
        }
    }

    /// The Vaults/Hosts screen — Termius light: search + Connect, toolbar, Hosts header, 2-col white cards, right Host Details.
    fn render_vault(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let content_bg = cx.theme().accent; // #edf1f2 in light
        let muted = cx.theme().muted_foreground; // #798c94
        let has_selection = self.selected.is_some();
        let selected_id = self.selected.clone();

        // Top search row: Input + Connect button (disabled until a host is selected).
        let search_row = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .w_full()
            .p_2()
            .child(
                div()
                    .flex_1()
                    .child(Input::new(&self.filter).small().cleanable(true)),
            )
            .child(
                Button::new("vault-connect")
                    .small()
                    .label("Connect")
                    .when(!has_selection, |b| b.disabled(true))
                    .when(has_selection, |b| {
                        let host = selected_id
                            .as_ref()
                            .and_then(|id| self.store.inventory().get(id).cloned());
                        b.on_click(cx.listener(move |this, _, window, cx| {
                            if let Some(host) = host.clone() {
                                this.connect(host, window, cx);
                            }
                        }))
                    }),
            );

        // Toolbar row: + New host (split), Terminal, Serial; right view toggles (inert except New host).
        let toolbar = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .w_full()
            .px_2()
            .py_1()
            .child(
                Button::new("vault-new-host")
                    .small()
                    .label("+ New host")
                    .on_click(cx.listener(|this, _, window, cx| this.open_add_host(window, cx))),
            )
            .child(
                Button::new("vault-terminal")
                    .small()
                    .ghost()
                    .label("Terminal")
                    .tooltip("Terminal (coming soon)")
                    .on_click(|_, window, cx| {
                        window.push_notification(Notification::info("Terminal coming soon"), cx);
                    }),
            )
            .child(
                Button::new("vault-serial")
                    .small()
                    .ghost()
                    .label("Serial")
                    .tooltip("Serial (coming soon)")
                    .on_click(|_, window, cx| {
                        window.push_notification(Notification::info("Serial coming soon"), cx);
                    }),
            )
            .child(div().flex_1())
            .child(
                Button::new("vault-view-grid")
                    .ghost()
                    .icon(IconName::LayoutDashboard)
                    .tooltip("Grid view"),
            )
            .child(
                Button::new("vault-filter")
                    .ghost()
                    .icon(IconName::Search)
                    .tooltip("Filter"),
            )
            .child(
                Button::new("vault-calendar")
                    .ghost()
                    .icon(IconName::Inbox)
                    .tooltip("Calendar"),
            );

        // Host cards — two-column via chunking into rows (avoids flex_wrap which may not exist).
        let query = self.filter.read(cx).value().to_string();
        let filtered: Vec<Host> = self
            .store
            .inventory()
            .filtered(&query)
            .into_iter()
            .cloned()
            .collect();
        let mut rows: Vec<AnyElement> = Vec::new();
        for chunk in filtered.chunks(2) {
            let mut row_cards: Vec<AnyElement> = Vec::new();
            for host in chunk {
                row_cards.push(self.render_host_card(host, cx));
            }
            if chunk.len() == 1 {
                // Pad second column with empty flex so cards keep ~50% width.
                row_cards.push(div().flex_1().into_any_element());
            }
            rows.push(
                div()
                    .flex()
                    .flex_row()
                    .gap_3()
                    .w_full()
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
            .px_3()
            .py_2()
            .child(
                div()
                    .text_sm()
                    .text_color(muted)
                    .child(format!("Hosts ({})", filtered.len())),
            )
            .children(rows);

        let centre = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(0.))
            .bg(content_bg)
            .overflow_hidden()
            .child(search_row)
            .child(toolbar)
            .child(grid);

        let details = self.render_host_details_panel(cx);

        div()
            .flex()
            .flex_row()
            .flex_1()
            .h_full()
            .min_h(px(0.))
            .overflow_hidden()
            .bg(content_bg)
            .child(centre)
            .child(details)
            .into_any_element()
    }

    fn render_host_card(&mut self, host: &Host, cx: &mut Context<Self>) -> AnyElement {
        let is_selected = self.selected.as_ref() == Some(&host.id);
        let connect_host = host.clone();
        let id = host.id.clone();
        let muted = cx.theme().muted_foreground;
        let fg = cx.theme().foreground;
        let card_bg = rgb(0xffffff);
        let border_selected = rgb(0x2091f6);
        let border_default = rgb(0xd5dde0);
        let orange = rgb(0xd96c2b);

        div()
            .id(SharedString::from(format!("vault-card-{id}")))
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .flex_1()
            .h(px(64.))
            .px_3()
            .rounded(px(10.))
            .bg(card_bg)
            .border_1()
            .border_color(if is_selected {
                border_selected
            } else {
                border_default
            })
            .when(is_selected, |el| el.border_2())
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                this.selected = Some(id.clone());
                cx.notify();
            }))
            .on_double_click(cx.listener(move |this, _, window, cx| {
                this.connect(connect_host.clone(), window, cx);
            }))
            .child(
                div()
                    .size(px(40.))
                    .rounded(px(8.))
                    .bg(orange)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(Icon::new(IconName::Globe).text_color(rgb(0xffffff))),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .overflow_hidden()
                    .child(
                        div()
                            .text_size(px(15.))
                            .text_color(fg)
                            .child(SharedString::from(host.label.clone())),
                    )
                    .child(
                        div()
                            .text_size(px(13.))
                            .text_color(muted)
                            .child(SharedString::from(format!("ssh, {}", host.username))),
                    ),
            )
            .into_any_element()
    }

    fn render_host_details_panel(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let border = rgb(0xd5dde0);
        let muted = cx.theme().muted_foreground;
        let fg = cx.theme().foreground;
        let accent = rgb(0x2091f6);
        let card_bg = rgb(0xffffff);

        let selected_host = self
            .selected
            .as_ref()
            .and_then(|id| self.store.inventory().get(id).cloned());

        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .w_full()
            .p_3()
            .border_b_1()
            .border_color(border)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(div().text_color(fg).child("Host Details"))
                    .child(div().text_xs().text_color(muted).child("Personal vault")),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_1()
                    .child(
                        Button::new("details-overflow")
                            .ghost()
                            .icon(IconName::Search)
                            .tooltip("More"),
                    )
                    .child(
                        Button::new("details-collapse")
                            .ghost()
                            .icon(IconName::PanelLeftClose)
                            .tooltip("Collapse"),
                    ),
            );

        let body: AnyElement =
            match selected_host.clone() {
                None => div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .p_6()
                    .text_color(muted)
                    .child("Select a host to see details")
                    .into_any_element(),
                Some(host) => {
                    let host_clone = host.clone();
                    let host_for_connect = host.clone();
                    div()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .p_3()
                        .overflow_y_scrollbar()
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_2()
                                .p_3()
                                .rounded_md()
                                .bg(card_bg)
                                .border_1()
                                .border_color(border)
                                .child(div().text_sm().text_color(fg).child("General"))
                                .child(Input::new(&self.draft_label).small().cleanable(false))
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(muted)
                                        .child(format!("{} · {}", host.label, host.address)),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .gap_2()
                                        .child(
                                            div()
                                                .flex_1()
                                                .text_xs()
                                                .text_color(muted)
                                                .child("Parent Group"),
                                        )
                                        .child(div().text_xs().text_color(muted).child("Default")),
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .justify_center()
                                .p_2()
                                .text_color(accent)
                                .child("Share this host"),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_2()
                                .p_3()
                                .rounded_md()
                                .bg(card_bg)
                                .border_1()
                                .border_color(border)
                                .child(div().flex().flex_row().items_center().gap_2().child(
                                    div().text_sm().child(format!("SSH on {} port", host.port)),
                                ))
                                .child(div().text_sm().text_color(fg).child("Credentials"))
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(fg)
                                        .child(SharedString::from(host.username.clone())),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(muted)
                                        .child(SharedString::from(host.auth.label())),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(muted)
                                        .child("+ SSH ID, Key, Certificate, FIDO2"),
                                )
                                .child(div().text_xs().text_color(muted).child("Show more")),
                        )
                        .child(
                            Button::new("details-connect")
                                .primary()
                                .label("Connect")
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.connect(host_for_connect.clone(), window, cx);
                                })),
                        )
                        .into_any_element()
                }
            };

        div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .w(px(360.))
            .h_full()
            .bg(card_bg)
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
            .into_any_element()
    }

    /// The SFTP tab: the browser, attached to the focused session.
    fn render_sftp(&mut self, cx: &mut Context<Self>) -> AnyElement {
        match self.sftp_pane.clone() {
            Some(pane) => div()
                .flex()
                .flex_col()
                .flex_1()
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
        }
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

    /// The live terminal (or the empty state) for the focused session.
    fn render_session(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;

        // Bring the focused pane's status onto the tab before rendering chrome.
        if let Some(session) = self.active.and_then(|index| self.sessions.get_mut(index)) {
            session.status = session.pane.read(cx).status();
        }

        let content = match self.active_session().map(|session| session.pane.clone()) {
            Some(pane) => div()
                .flex()
                .flex_col()
                .flex_1()
                .size_full()
                .overflow_hidden()
                .child(pane)
                .into_any_element(),
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

        let secret = self.render_secret_prompt(cx);

        div()
            .flex()
            .flex_col()
            .flex_1()
            .h_full()
            .min_h(px(0.))
            .overflow_hidden()
            .when_some(secret, |el, prompt| el.child(prompt))
            .child(content)
            .into_any_element()
    }

    /// The host, auth method and connection state the removed status bar used
    /// to show, folded into the header's right cluster: the active session's
    /// endpoint and state, or the selected host when no session is open.
    fn status_line(&self) -> String {
        let host = self
            .active_session()
            .and_then(|session| self.store.inventory().get(&session.host))
            .or_else(|| {
                self.selected
                    .as_ref()
                    .and_then(|id| self.store.inventory().get(id))
            })
            .map(|host| format!("{} · {}", host.endpoint(), host.auth.label()));

        let state = match self.active_session() {
            Some(session) => session.status.state.label(),
            None => "no session".to_string(),
        };

        match host {
            Some(host) => format!("{host} · {state}"),
            None => state,
        }
    }
}

impl Render for SshDeck {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let foreground = cx.theme().foreground;

        let header = self.render_header(cx);
        let sidebar = self.render_sidebar(cx);
        let pane_tabs = self.render_pane_tabs(cx);
        let main = self.render_main(cx);
        let add_host_sheet = self.render_add_host_sheet(cx);
        let palette = self.palette.clone();

        let mut root = div()
            .flex()
            .flex_col()
            .size_full()
            // `--main-bg` is `#1d2033` (the theme's `sidebar` token); the
            // window's `background` token is the darker `--surface-lowest`.
            .bg(cx.theme().sidebar)
            .text_color(foreground)
            // `--default-font-size` is 14px; the theme's `font.size` is 13, so
            // the body size is set explicitly here.
            .text_size(px(14.))
            // The palette's navigation actions are bound in its own context. The
            // context is only active while the palette is open, so these keys are
            // never stolen from the terminal or an input.
            .when(palette.is_some(), |el| el.key_context(palette::CONTEXT))
            .on_action(cx.listener(Self::on_palette_up))
            .on_action(cx.listener(Self::on_palette_down))
            .on_action(cx.listener(Self::on_palette_cancel))
            .child(header)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .overflow_hidden()
                    .child(sidebar)
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w(px(0.))
                            .overflow_hidden()
                            .child(pane_tabs)
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
    fn closing_a_session_repairs_the_selected_tab() {
        assert_eq!(tab_after_close(MainTab::Session(2), 2), MainTab::Vaults);
        assert_eq!(tab_after_close(MainTab::Session(3), 1), MainTab::Session(2));
        assert_eq!(tab_after_close(MainTab::Session(1), 3), MainTab::Session(1));
        assert_eq!(tab_after_close(MainTab::Sftp, 0), MainTab::Sftp);
        assert_eq!(tab_after_close(MainTab::Vaults, 0), MainTab::Vaults);
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
}
