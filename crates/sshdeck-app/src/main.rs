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
    Sizable as _, Theme, ThemeMode, WindowExt,
};
use gpui_kit::prelude::{FluentBuilder as _, StatefulInteractiveElement as _};
use gpui_kit::{
    div, px, AnyElement, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, Styled as _, Subscription, Window, WindowOptions,
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
            cx.spawn(async move |cx| {
                cx.open_window(WindowOptions::default(), |window, cx| {
                    let view = cx.new(|cx| SshDeck::new(window, cx));
                    cx.new(|cx| Root::new(view, window, cx))
                })
                .expect("failed to open window");
            })
            .detach();
        });
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

/// Runs one command over SSH with no window, no gpui, and no async runtime.
///
/// This is the same transport the terminal pane uses: `Session::connect` owns
/// the connection thread and, exactly as in the GUI, requests the PTY and login
/// shell, so the bytes are what a pane would have fed to its grid. `events()` is
/// an `async_channel::Receiver`; `recv_blocking` (verified on the pinned
/// async-channel 2.5.0) lets that stream be drained on the main thread.
///
/// Returns the process exit code: 0 on a clean close, 1 on a connect failure or
/// a [`SessionEvent::Error`].
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

    let session = match SshSession::connect(config) {
        Ok(session) => session,
        Err(error) => {
            eprintln!("sshdeck: {error}");
            return 1;
        }
    };
    // The command is queued; the connection thread only reads it once the shell
    // is open, so it cannot race the PTY setup. The newline runs it.
    if let Err(error) = session
        .write(command.as_bytes())
        .and_then(|()| session.write(b"\n"))
    {
        eprintln!("sshdeck: could not send the command: {error}");
        return 1;
    }

    // One receiver for the whole session: `events()` clones, and the channel is
    // competing-consumer, so calling it per iteration would split the stream.
    let events = session.events();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut failed = false;
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
            // Lifecycle and close events carry no bytes; the channel closing is
            // what ends the loop.
            _ => {}
        }
    }

    if failed {
        1
    } else {
        0
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
    /// The in-flight secret prompt, when a host needs a password we do not have.
    secret: Option<(HostId, Entity<InputState>)>,
    /// The command palette while it is open, if at all.
    palette: Option<Entity<PaletteView>>,
    /// The settings, keys or SFTP pane showing over the session, if at all.
    overlay: Option<Overlay>,
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
            secret: None,
            palette: None,
            overlay: None,
            sftp_attached: None,
            sftp_generation: 0,
            _subscriptions: subscriptions,
        };

        // Development and test affordance, not a user feature: with
        // `SSHDECK_AUTOCONNECT` set to a host id or label, connect once as soon
        // as the view exists so a live session can be measured without a human.
        // A missing host is a notification, never a startup failure.
        if let Ok(target) = std::env::var("SSHDECK_AUTOCONNECT") {
            cx.defer_in(window, move |this, window, cx| {
                this.auto_connect(&target, window, cx);
            });
        }
        view
    }

    /// Connects to the `SSHDECK_AUTOCONNECT` host by id or label, if it exists.
    ///
    /// Reuses the normal [`Self::connect`] path, so password handling and the
    /// tab/pane setup are identical to a human double-click. A target with no
    /// host is a notification; startup continues regardless.
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
    fn add_draft_host(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let label = self.draft_label.read(cx).value().trim().to_string();
        let address = self.draft_address.read(cx).value().trim().to_string();

        if label.is_empty() || address.is_empty() {
            window.push_notification(
                Notification::warning("A label and an address are both required"),
                cx,
            );
            return;
        }

        let id = self
            .store
            .inventory_mut()
            .insert(Host::new(&label, &address));
        self.selected = Some(id);

        if let Err(error) = self.store.save() {
            window.push_notification(Notification::error(format!("Could not save: {error}")), cx);
        }

        self.draft_label
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.draft_address
            .update(cx, |state, cx| state.set_value("", window, cx));
        cx.notify();
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
            palette::CommandId::AddHost => self.add_draft_host(window, cx),
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

    fn render_top_bar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let border = cx.theme().border;

        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .flex_shrink_0()
            .h(px(44.))
            .px_3()
            .border_b_1()
            .border_color(border)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(Icon::new(IconName::Frame).text_color(cx.theme().primary))
                    .child("sshdeck"),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
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
                        Button::new("settings")
                            .ghost()
                            .icon(IconName::Settings2)
                            .tooltip("Settings")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_settings(window, cx);
                            })),
                    ),
            )
    }

    fn render_sidebar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let border = cx.theme().border;
        let surface = cx.theme().sidebar;
        let muted = cx.theme().muted_foreground;

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
                .py_1()
                .rounded_sm()
                .cursor_pointer()
                .bg(if is_selected {
                    cx.theme().muted
                } else {
                    surface
                })
                .child(Icon::new(IconName::Globe).small().text_color(muted))
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
            .w(px(280.))
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
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p_2()
                    .border_t_1()
                    .border_color(border)
                    .child(Input::new(&self.draft_label).small())
                    .child(Input::new(&self.draft_address).small())
                    .child(
                        Button::new("add-host")
                            .small()
                            .primary()
                            .label("Add host")
                            .icon(IconName::Plus)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.add_draft_host(window, cx);
                            })),
                    ),
            )
    }

    /// The tab strip: one tab per open session, with its state.
    fn render_tabs(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let border = cx.theme().border;
        let muted = cx.theme().muted_foreground;
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
                .h_full()
                .cursor_pointer()
                .border_r_1()
                .border_color(border)
                .when(is_active, |el| el.bg(cx.theme().muted))
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
            .flex()
            .flex_row()
            .items_center()
            .flex_shrink_0()
            .h(px(32.))
            .border_b_1()
            .border_color(border)
            .child(
                div()
                    .id("tab-strip")
                    .flex()
                    .flex_row()
                    .items_center()
                    .h_full()
                    .flex_1()
                    .overflow_x_scrollbar()
                    .children(tabs),
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

    fn render_status_bar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let border = cx.theme().border;
        let muted = cx.theme().muted_foreground;
        let active = self.connected_count();

        let host_label = self
            .active_session()
            .and_then(|session| self.store.inventory().get(&session.host))
            .map(|host| format!("{} · {}", host.endpoint(), host.auth.label()))
            .or_else(|| {
                self.selected
                    .as_ref()
                    .and_then(|id| self.store.inventory().get(id))
                    .map(|host| format!("{} · {}", host.endpoint(), host.auth.label()))
            })
            .unwrap_or_else(|| "no host selected".to_string());

        let state_label = match self.active_session() {
            Some(session) => session.status.state.label(),
            None => "no session".to_string(),
        };

        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .flex_shrink_0()
            .h(px(26.))
            .px_3()
            .border_t_1()
            .border_color(border)
            .text_xs()
            .text_color(muted)
            .child(host_label)
            .child(SharedString::from(state_label))
            .child(format!("{active} connected"))
    }
}

impl Render for SshDeck {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let background = cx.theme().background;
        let foreground = cx.theme().foreground;

        let top_bar = self.render_top_bar(cx);
        let sidebar = self.render_sidebar(cx);
        let main = self.render_main(cx);
        let status = self.render_status_bar(cx);
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
            .child(top_bar)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .overflow_hidden()
                    .child(sidebar)
                    .child(main),
            )
            .child(status)
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
}
