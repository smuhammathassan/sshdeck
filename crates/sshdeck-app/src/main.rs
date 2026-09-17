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
    ActiveTheme as _, Disableable as _, Icon, IconName, InteractiveElementExt as _, Root,
    Sizable as _, Theme, ThemeMode, ThemeRegistry, WindowExt,
};
use gpui_kit::prelude::{FluentBuilder as _, StatefulInteractiveElement as _};
use gpui_kit::{
    div, px, rgb, rgba, AnyElement, App, AppContext as _, Context, Entity, Focusable as _, Hsla,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, Rgba, SharedString,
    Styled as _, Subscription, TitlebarOptions, Window, WindowOptions,
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
/// corner-radius field, so that exact radius cannot be expressed and the native
/// macOS window radius stands. `appears_transparent` is the part GPUI does
/// express: it hides the system title bar and draws the content under it, while
/// the window is still created with the closable / minimizable / resizable
/// style masks (because `titlebar` is `Some`), so the traffic lights and the
/// close button stay. There is no titlebar-height option either, so the lights
/// keep their native position near the top; [`render_header`] reserves the left
/// inset for them. `traffic_light_position: None` means "GPUI's default", not
/// "hide them".
fn window_options() -> WindowOptions {
    WindowOptions {
        titlebar: Some(TitlebarOptions {
            title: None,
            appears_transparent: true,
            traffic_light_position: None,
        }),
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

/// The name of the dark theme in the bundled theme set, and the one the app
/// starts in.
const DEFAULT_THEME: &str = "sshdeck Dark";

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

    // Dark is the default, regardless of the system appearance.
    Theme::change(ThemeMode::Dark, None, cx);
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

/// A full-region pane that shows over the session content while it is open.
///
/// Each variant is created on demand and dropped when it closes, so a pane that
/// holds a transport (SFTP) never outlives its own visibility.
enum Overlay {
    Settings(Entity<SettingsView>),
    Keys(Entity<KeysPane>),
    Sftp(Entity<SftpPane>),
}

struct SshDeck {
    store: HostStore,
    selected: Option<HostId>,
    /// The tab the terminal pane is showing.
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
    /// The settings, keys or SFTP pane showing over the session, if at all.
    overlay: Option<Overlay>,
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
        self.active = Some(self.sessions.len() - 1);
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

    /// Replaces the session area with `overlay` (or restores it with `None`).
    ///
    /// A pane that is being replaced is dropped, closing any transport it held;
    /// the SFTP bookkeeping is cleared and any in-flight connect is invalidated.
    fn show_overlay(&mut self, overlay: Option<Overlay>, cx: &mut Context<Self>) {
        self.sftp_attached = None;
        self.sftp_generation = self.sftp_generation.wrapping_add(1);
        self.overlay = overlay;
        cx.notify();
    }

    fn close_overlay(&mut self, cx: &mut Context<Self>) {
        self.show_overlay(None, cx);
    }

    fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.overlay, Some(Overlay::Settings(_))) {
            self.close_overlay(cx);
            return;
        }
        let view = cx.new(|cx| SettingsView::new(window, cx));
        view.update(cx, |view, cx| view.focus(window, cx));
        self.show_overlay(Some(Overlay::Settings(view)), cx);
    }

    fn open_keys(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.overlay, Some(Overlay::Keys(_))) {
            self.close_overlay(cx);
            return;
        }
        let view = cx.new(|cx| KeysPane::new(window, cx));
        self.show_overlay(Some(Overlay::Keys(view)), cx);
    }

    fn open_sftp(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.overlay, Some(Overlay::Sftp(_))) {
            self.close_overlay(cx);
            return;
        }
        let pane = cx.new(|cx| SftpPane::new(window, cx));
        self.show_overlay(Some(Overlay::Sftp(pane)), cx);
        // `show_overlay` cleared the bookkeeping, so this is a fresh attach.
        self.reconcile_sftp(window, cx);
    }

    /// Attaches the SFTP pane to the active session's transport, or detaches it.
    ///
    /// Called whenever the tab selection or a session's state changes. At most
    /// one SFTP session is live: a client is opened only while a session is
    /// authenticated, and dropped as soon as that session ends or another tab
    /// becomes active (docs/BUDGET.md: no connection per tab).
    fn reconcile_sftp(&mut self, window: &Window, cx: &mut Context<Self>) {
        // Only a visible SFTP pane can hold a client. When it is closed its
        // entity, and the transport with it, is already gone.
        let pane = match &self.overlay {
            Some(Overlay::Sftp(pane)) => pane.clone(),
            _ => return,
        };

        let target = self.active.filter(|&index| {
            self.sessions
                .get(index)
                .is_some_and(|session| matches!(session.status.state, SessionState::Connected))
        });
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
    fn render_header(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let active = self.connected_count();
        let collapsed = self.sidebar_collapsed;

        let actions = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
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
                Button::new("keys")
                    .ghost()
                    .icon(IconName::HardDrive)
                    .tooltip("SSH keys & known hosts")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_keys(window, cx);
                    })),
            )
            .child(
                Button::new("sftp")
                    .ghost()
                    .icon(IconName::Folder)
                    .tooltip("SFTP browser")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_sftp(window, cx);
                    })),
            )
            .child(
                Button::new("theme")
                    .ghost()
                    .icon(IconName::Moon)
                    .tooltip("Toggle light/dark")
                    .on_click(|_, window, cx| {
                        let next = if Theme::global(cx).is_dark() {
                            ThemeMode::Light
                        } else {
                            ThemeMode::Dark
                        };
                        Theme::change(next, Some(window), cx);
                    }),
            )
            .child(
                Button::new("settings")
                    .ghost()
                    .icon(IconName::Settings2)
                    .tooltip("Settings")
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
            // `--header-height` from the recovered stylesheet.
            .h(px(56.))
            // Traffic-light inset: the native window buttons overlay the top
            // left of the content now that the title bar is transparent, so the
            // sidebar toggle starts clear of them. Termius keeps the lights
            // level with the tab row, which this 56px header contains.
            .pl(px(80.))
            .pr_3()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                Button::new("sidebar-toggle")
                    .ghost()
                    .icon(IconName::Frame)
                    .tooltip(if collapsed {
                        "Show sidebar"
                    } else {
                        "Hide sidebar"
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.sidebar_collapsed = !this.sidebar_collapsed;
                        cx.notify();
                    })),
            )
            .child(self.render_tabs(cx))
            .child(
                Button::new("add-host")
                    .ghost()
                    .icon(IconName::Plus)
                    .tooltip("Add host")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_add_host(window, cx);
                    })),
            )
            .child(div().text_xs().text_color(muted).child(self.status_line()))
            .when(active > 0, |el| {
                el.child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child(format!("{active} connected")),
                )
            })
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

    fn render_sidebar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let border = cx.theme().border;
        let surface = cx.theme().sidebar;
        let muted = cx.theme().muted_foreground;
        // Host cards: `--entity-item-background` default, `--list-hover` on
        // hover, `--list-select` when selected. Only the default maps to a
        // gpui-kit token (`muted.background`); the other two are the recovered
        // hex values, since the theme exposes no token for them.
        let card: Hsla = cx.theme().muted;
        let card_selected: Hsla = rgb(0x32364a).into();
        let card_hover: Hsla = rgb(0x3e4257).into();

        let query = self.filter.read(cx).value().to_string();
        let visible: Vec<Host> = self
            .store
            .inventory()
            .filtered(&query)
            .into_iter()
            .cloned()
            .collect();
        let total = self.store.inventory().len();

        let rows = visible.into_iter().map(|host| {
            let id = host.id.clone();
            let is_selected = self.selected.as_ref() == Some(&id);
            let is_open = self.sessions.iter().any(|s| s.host == id);
            let remove_id = id.clone();
            let connect_host = host.clone();
            let tint = host_os_tint(&host, muted);

            div()
                .id(SharedString::from(format!("host-{}", id)))
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
                .rounded_sm()
                .cursor_pointer()
                .bg(if is_selected { card_selected } else { card })
                // `--list-hover`: the card highlight, via the hover style
                // refinement (`StatefulInteractiveElement::hover`).
                .hover(|style| style.bg(card_hover))
                .child(Icon::new(IconName::Globe).small().text_color(tint))
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
                    el.child(
                        Icon::new(IconName::Check)
                            .small()
                            .text_color(cx.theme().success),
                    )
                })
                .child(
                    Button::new(SharedString::from(format!("remove-{}", remove_id)))
                        .ghost()
                        .xsmall()
                        .icon(IconName::Close)
                        .tooltip("Remove host")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.remove_host(&remove_id, window, cx);
                        })),
                )
        });

        div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            // Collapsed rail vs expanded panel. Termius: 180px / 240px.
            .w(if self.sidebar_collapsed {
                px(180.)
            } else {
                px(240.)
            })
            .h_full()
            .border_r_1()
            .border_color(border)
            .bg(surface)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_2()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(muted)
                                    .child(format!("HOSTS ({total})")),
                            ),
                    )
                    .child(Input::new(&self.filter).small().cleanable(true)),
            )
            .child(
                div()
                    .id("host-list")
                    .flex()
                    .flex_col()
                    .gap_1()
                    .flex_1()
                    .px_2()
                    .overflow_y_scrollbar()
                    .children(rows),
            )
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

    /// The session tabs, drawn as 6px cards inside the 56px header.
    ///
    /// `--horizontal-tabs-height` is 51px; the selected tab uses the
    /// `--surface-high` accent surface (`#282b3d`, the theme's `muted`
    /// background) with primary text, while unselected tabs are transparent
    /// with the secondary `#8d91a5` text. The tab carries the session state
    /// that the removed status bar used to show.
    fn render_tabs(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let selected_bg = cx.theme().muted;
        let selected_fg = cx.theme().foreground;
        let active = self.active;

        let tabs = self.sessions.iter().enumerate().map(|(index, session)| {
            let label = session.status.title.clone().unwrap_or_else(|| {
                self.store
                    .inventory()
                    .get(&session.host)
                    .map(|host| host.label.clone())
                    .unwrap_or_else(|| session.host.to_string())
            });
            let state = session.status.state.label();
            let is_active = active == Some(index);

            div()
                .id(SharedString::from(format!("tab-{}", session.host)))
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .px_3()
                .h(px(36.))
                .rounded_sm()
                .cursor_pointer()
                .text_color(if is_active { selected_fg } else { muted })
                .when(is_active, |el| el.bg(selected_bg))
                .child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child(SharedString::from(state)),
                )
                .child(SharedString::from(label))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.active = Some(index);
                    // The SFTP pane follows the active tab.
                    this.reconcile_sftp(window, cx);
                    cx.notify();
                }))
        });

        div()
            .id("tab-strip")
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .h(px(51.))
            .flex_1()
            .overflow_x_scrollbar()
            .children(tabs)
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
            self.render_overlay(cx)
        } else {
            self.render_session(cx)
        }
    }

    /// The settings, keys or SFTP pane, with a header that closes it.
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
            Some(Overlay::Sftp(view)) => ("SFTP", view.clone().into_any_element()),
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
                            .on_click(cx.listener(|this, _, _, cx| this.close_overlay(cx))),
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

    /// The tab strip, session header and live terminal.
    fn render_session(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let border = cx.theme().border;
        let muted = cx.theme().muted_foreground;

        // Bring the focused pane's status onto the tab before rendering chrome.
        if let Some(session) = self.active.and_then(|index| self.sessions.get_mut(index)) {
            session.status = session.pane.read(cx).status();
        }

        let connect_label = if self.active_session().is_some() {
            "Reconnect"
        } else {
            "Connect"
        };
        // A tab being open means reconnect it; otherwise connect what is selected.
        let connect_host = self
            .active_session()
            .and_then(|session| self.store.inventory().get(&session.host).cloned())
            .or_else(|| {
                self.selected
                    .as_ref()
                    .and_then(|id| self.store.inventory().get(id).cloned())
            });

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

        let header = match self.active_session() {
            Some(session) => {
                let label = self
                    .store
                    .inventory()
                    .get(&session.host)
                    .map(|host| format!("{} · {}", host.label, host.endpoint()))
                    .unwrap_or_else(|| session.host.to_string());
                format!("{} · {}", label, session.status.state.label())
            }
            None => "no session".to_string(),
        };

        let tabs = self.render_tabs(cx);
        let secret = self.render_secret_prompt(cx);

        div()
            .flex()
            .flex_col()
            .flex_1()
            .h_full()
            .overflow_hidden()
            .child(tabs)
            .when_some(secret, |el, prompt| el.child(prompt))
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
                    .child(SharedString::from(header))
                    .child(
                        Button::new("connect")
                            .small()
                            .primary()
                            .label(connect_label)
                            .disabled(connect_host.is_none())
                            .tooltip("Open a session to the selected host")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                if let Some(host) = connect_host.clone() {
                                    this.connect(host, window, cx);
                                }
                            })),
                    ),
            )
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
        let background = cx.theme().background;
        let foreground = cx.theme().foreground;

        let header = self.render_header(cx);
        let sidebar = self.render_sidebar(cx);
        let main = self.render_main(cx);
        let add_host_sheet = self.render_add_host_sheet(cx);
        let palette = self.palette.clone();

        let mut root = div()
            .flex()
            .flex_col()
            .size_full()
            .bg(background)
            .text_color(foreground)
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
                    .child(main),
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
    fn os_brand_colours_are_known_or_absent() {
        assert_eq!(os_brand_color("ubuntu"), Some(rgb(0xe95420)));
        assert_eq!(os_brand_color(" Ubuntu "), Some(rgb(0xe95420)));
        assert_eq!(os_brand_color("raspbian"), os_brand_color("pi"));
        // No recovered value: the tint must stay unknown, not invented.
        assert_eq!(os_brand_color("alpine"), None);
        assert_eq!(os_brand_color("plan9"), None);

        let fallback: Hsla = rgb(0x8d91a5).into();
        let mut host = Host::new("box", "10.0.0.1");
        assert_eq!(Rgba::from(host_os_tint(&host, fallback)), rgb(0x8d91a5));

        host.tags.push("debian".into());
        assert_eq!(Rgba::from(host_os_tint(&host, fallback)), rgb(0xce0056));
    }
}
