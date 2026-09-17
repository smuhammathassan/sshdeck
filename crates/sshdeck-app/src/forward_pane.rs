//! Port forwarding pane: create, watch and stop SSH tunnels on one session.
//!
//! A self-contained gpui-kit view over [`Session::forward`]. The layout follows
//! the Termius "Port Forwarding" surface: a white toolbar carrying the
//! `New forwarding` control and a live/known count, an empty state when nothing
//! is configured, and a hairline-separated list of forwards below it. Each row
//! carries its kind (local/remote/dynamic), the bind address and port, the
//! target, a status marker for a live vs stopped forward, and one action:
//! **stop** while the forward is live, **remove** once it has stopped or failed.
//!
//! The add form (kind selector, spec field, `Add`) sits behind the toolbar's
//! `New forwarding` button rather than being always visible, the way the
//! reference reveals creation from its toolbar.
//!
//! # Wiring contract
//!
//! `ForwardPane::new` deliberately takes no transport. A forward can only exist
//! on a live SSH connection, and the host view owns that connection, so the host
//! supplies a handle with [`ForwardPane::set_session`] and clears it again with
//! `None`. The host also constructs the pane, so [`ForwardPane::new`] matches the
//! shape the other panes use:
//!
//! ```ignore
//! let pane = cx.new(|cx| ForwardPane::new(window, cx));
//! // ... later, once the session is connected:
//! pane.update(cx, |pane, cx| pane.set_session(Some(session.clone()), cx));
//! // and on disconnect:
//! pane.update(cx, |pane, cx| pane.set_session(None, cx));
//! ```
//!
//! Until a session is supplied the pane renders a "no session" state instead of
//! an empty list. [`ForwardPane::session`] and [`ForwardPane::forward_count`] are
//! the matching readers.
//!
//! # What this pane does not do
//!
//! * Dynamic forwards are SOCKS5 only: the crate speaks `CONNECT` with no
//!   authentication and does not implement `BIND` or `UDP ASSOCIATE`. The form
//!   says so next to the dynamic option (and the row says `SOCKS5`, nothing
//!   stronger), so nobody expects a transparent proxy.
//! * Specs are the OpenSSH grammar, parsed by `sshdeck_core::forward` — this
//!   pane never re-implements the parser, and a rejected spec is shown with the
//!   crate's own message (which names the broken rule) rather than a generic
//!   "invalid input".
//! * It does not persist forwards: the empty state says "add", not "save", so
//!   the copy cannot promise a store that does not exist.
//!
//! # Threading and bounds
//!
//! `Session::forward` and `Forward::stop` are documented as non-blocking
//! (`try_send` on a bounded queue); the transport work itself happens on the
//! session's own thread. Both are nevertheless driven from a spawned task so no
//! transport call runs on the render path, and one task per forward awaits its
//! event stream. State is bounded: at most [`MAX_FORWARDS`] rows, one status per
//! row, and only the latest [`ForwardEvent`] is kept — no event log.

use std::sync::Arc;

use async_channel::Receiver;
use gpui_kit::component::alert::Alert;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _};
use gpui_kit::prelude::{FluentBuilder as _, StatefulInteractiveElement as _};
use gpui_kit::{
    div, px, AnyElement, App, AppContext as _, Context, Div, Entity, FocusHandle, FontWeight, Hsla,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString, Styled as _,
    Window,
};
use sshdeck_core::forward::{Forward, ForwardConfig, ForwardError, ForwardEvent};
use sshdeck_core::session::Session;

/// Forwards are user-created and few, but the list is still capped so a runaway
/// create loop cannot grow the process without bound.
const MAX_FORWARDS: usize = 64;

/// Which OpenSSH grammar a spec is parsed with.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ForwardKind {
    Local,
    Remote,
    Dynamic,
}

impl ForwardKind {
    fn index(self) -> usize {
        match self {
            Self::Local => 0,
            Self::Remote => 1,
            Self::Dynamic => 2,
        }
    }

    fn from_index(index: usize) -> Self {
        match index {
            1 => Self::Remote,
            2 => Self::Dynamic,
            _ => Self::Local,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Local => "Local (-L)",
            Self::Remote => "Remote (-R)",
            Self::Dynamic => "Dynamic (-D)",
        }
    }

    /// The grammar and what the forward actually does, shown under the input.
    fn hint(self) -> &'static str {
        match self {
            Self::Local => {
                "Local (-L) [bind:]port:host:hostport — listens on this machine and \
                 reaches the target through the server."
            }
            Self::Remote => {
                "Remote (-R) [bind:]port:host:hostport — the server listens and forwards \
                 connections back to a target reachable from this machine."
            }
            Self::Dynamic => {
                "Dynamic (-D) [bind:]port — a SOCKS5 proxy on this machine. CONNECT only, \
                 no authentication: it is not a transparent HTTP proxy."
            }
        }
    }
}

/// What the pane last heard about one forward.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ForwardStatus {
    /// Created, waiting for the transport to report where it bound.
    Starting,
    /// Live. `note` carries the most recent per-connection failure, if any.
    Listening {
        address: String,
        port: u16,
        note: Option<String>,
    },
    /// The forward could not be started (a bind failure, a refused `-R`).
    Failed(String),
    /// The forward stopped, normally or after a failure.
    Stopped,
}

impl ForwardStatus {
    fn is_active(&self) -> bool {
        matches!(self, Self::Starting | Self::Listening { .. })
    }

    fn label(&self) -> String {
        match self {
            Self::Starting => "Starting…".to_string(),
            Self::Listening { address, port, .. } => {
                format!("Listening on {}", format_endpoint(address, *port))
            }
            Self::Failed(message) => format!("Failed — {message}"),
            Self::Stopped => "Stopped".to_string(),
        }
    }

    fn note(&self) -> Option<&str> {
        match self {
            Self::Listening { note, .. } => note.as_deref(),
            _ => None,
        }
    }

    /// The marker colour for the row: green while it is up, red after a
    /// failure, muted once it is over (or still coming up).
    fn marker(&self, cx: &App) -> Hsla {
        match self {
            Self::Listening { .. } => cx.theme().success,
            Self::Failed(_) => cx.theme().danger,
            Self::Starting | Self::Stopped => cx.theme().muted_foreground,
        }
    }
}

/// One forward the pane owns, as last observed.
struct ForwardRow {
    /// Domain-derived id: element ids and event routing must never use an index.
    id: u64,
    config: ForwardConfig,
    /// The stoppable handle. Dropped once the forward is stopped or removed.
    handle: Option<Forward>,
    status: ForwardStatus,
}

impl ForwardRow {
    /// Folds one transport event into the row.
    fn apply(&mut self, event: ForwardEvent) {
        match event {
            ForwardEvent::Listening { address, port } => {
                self.status = ForwardStatus::Listening {
                    address,
                    port,
                    note: None,
                };
            }
            ForwardEvent::ConnectionFailed { target, message } => {
                // A single refused connection is not the forward dying: it keeps
                // listening, so the listening status stays and the message is
                // attached to it.
                if let ForwardStatus::Listening { note, .. } = &mut self.status {
                    *note = Some(format!("{target}: {message}"));
                }
            }
            ForwardEvent::Failed(message) => self.status = ForwardStatus::Failed(message),
            ForwardEvent::Stopped => self.status = ForwardStatus::Stopped,
        }
    }
}

/// The port forwarding pane. Constructed by the root view.
pub struct ForwardPane {
    /// `None` until the host supplies one; the pane then shows "no session".
    session: Option<Arc<Session>>,
    forwards: Vec<ForwardRow>,
    /// Monotonic; never resets, so an id is unique for the pane's lifetime.
    next_id: u64,
    /// The pending form's kind.
    kind: ForwardKind,
    spec_input: Entity<InputState>,
    /// Whether the add form is revealed under the toolbar.
    form_open: bool,
    /// A rejected spec or a failed start, shown next to the form.
    error: Option<String>,
    /// True while one start is in flight, so "Add" cannot be double-fired.
    starting: bool,
    focus_handle: FocusHandle,
}

impl ForwardPane {
    /// Creates a pane with no session.
    ///
    /// The host must call [`Self::set_session`] once it has a live session; until
    /// then the pane renders its "no session" state.
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let spec_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("8080:db.internal:5432 or 1080"));
        let focus_handle = cx.focus_handle();
        focus_handle.focus(window, cx);
        Self {
            session: None,
            forwards: Vec::new(),
            next_id: 1,
            kind: ForwardKind::Local,
            spec_input,
            form_open: false,
            error: None,
            starting: false,
            focus_handle,
        }
    }

    /// The live session forwards are created on, if one has been supplied.
    pub fn session(&self) -> Option<Arc<Session>> {
        self.session.clone()
    }

    /// How many forwards the pane is tracking, live or not.
    pub fn forward_count(&self) -> usize {
        self.forwards.len()
    }

    /// Attaches or detaches the session forwards are created on.
    ///
    /// Passing `None` asks every live forward to stop and drops the rows, so a
    /// disconnected pane cannot show a tunnel it no longer owns. Forward handles
    /// are not RAII — dropping one does not stop the forward — which is why the
    /// stop is explicit here.
    pub fn set_session(&mut self, session: Option<Arc<Session>>, cx: &mut Context<Self>) {
        if session.is_none() {
            for row in &self.forwards {
                if let Some(handle) = &row.handle {
                    let _ = handle.stop();
                }
            }
        }
        self.session = session;
        self.forwards.clear();
        self.error = None;
        self.form_open = false;
        self.starting = false;
        cx.notify();
    }

    /// Validates the pending spec and starts the forward, off the render path.
    fn add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.starting {
            return;
        }
        let spec = self.spec_input.read(cx).value().trim().to_string();
        if spec.is_empty() {
            self.error = Some(match self.kind {
                ForwardKind::Dynamic => "Enter a port or bind address and port, e.g. 1080".into(),
                _ => "Enter [bind:]port:host:hostport, e.g. 8080:db.internal:5432".into(),
            });
            cx.notify();
            return;
        }
        // The crate's parser, reused verbatim; its error names the broken rule.
        let config = match parse_spec(self.kind, &spec) {
            Ok(config) => config,
            Err(error) => {
                self.error = Some(error.to_string());
                cx.notify();
                return;
            }
        };
        let Some(session) = self.session.clone() else {
            self.error = Some("No session to forward through".into());
            cx.notify();
            return;
        };

        self.error = None;
        self.starting = true;
        cx.notify();

        let row_config = config.clone();
        cx.spawn_in(window, async move |pane, cx| {
            // `Session::forward` validates and enqueues without blocking; the
            // task exists so no transport call is made from a click handler.
            let result = session.forward(config);
            pane.update_in(cx, |pane, _window, cx| {
                pane.starting = false;
                match result {
                    Ok(forward) => pane.insert(row_config, forward, cx),
                    Err(error) => pane.error = Some(error.to_string()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Records a started forward and begins watching its events.
    fn insert(&mut self, config: ForwardConfig, forward: Forward, cx: &mut Context<Self>) {
        if self.forwards.len() >= MAX_FORWARDS {
            // Over the cap: stop the straggler rather than track it.
            let _ = forward.stop();
            self.error = Some(format!("At most {MAX_FORWARDS} forwards can be tracked"));
            return;
        }
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        let events = forward.events();
        self.forwards.push(ForwardRow {
            id,
            config,
            handle: Some(forward),
            status: ForwardStatus::Starting,
        });
        self.watch(id, events, cx);
    }

    /// Drives one forward's event stream into its row until the stream ends.
    ///
    /// Runs on GPUI's foreground executor and yields at every `await`, so it
    /// never blocks the UI thread.
    fn watch(&mut self, id: u64, events: Receiver<ForwardEvent>, cx: &mut Context<Self>) {
        cx.spawn(async move |pane, cx| {
            // `Err` means the forward's task dropped the sender: it is over, and
            // the `Stopped` event before it has already been folded in.
            while let Ok(event) = events.recv().await {
                let tracked = pane
                    .update(cx, |pane, cx| {
                        if let Some(row) = pane.forwards.iter_mut().find(|row| row.id == id) {
                            row.apply(event);
                            cx.notify();
                            true
                        } else {
                            // The row was removed; stop watching and drop the
                            // receiver rather than accumulate a dead task.
                            false
                        }
                    })
                    .unwrap_or(false);
                if !tracked {
                    break;
                }
            }
        })
        .detach();
    }

    /// Asks one forward to stop. The status becomes `Stopped` immediately and
    /// the event stream confirms it.
    fn stop(&mut self, id: u64, cx: &mut Context<Self>) {
        cx.spawn(async move |pane, cx| {
            pane.update(cx, |pane, cx| {
                if let Some(row) = pane.forwards.iter_mut().find(|row| row.id == id) {
                    // Best-effort and non-blocking; `Err` means it has already
                    // stopped, which is the same end state.
                    if let Some(handle) = &row.handle {
                        let _ = handle.stop();
                    }
                    row.handle = None;
                    row.status = ForwardStatus::Stopped;
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The white toolbar strip: the add control on the left, the count on the
    /// right. `New forwarding` is disabled without a session, because a forward
    /// cannot exist off one.
    fn render_toolbar(&self, cx: &mut Context<Self>) -> Div {
        let muted = cx.theme().muted_foreground;
        let border = cx.theme().border;
        let connected = self.session.is_some();
        let active = self
            .forwards
            .iter()
            .filter(|row| row.status.is_active())
            .count();

        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .h(px(44.))
            .px_3()
            .flex_shrink_0()
            .bg(cx.theme().background)
            .border_b_1()
            .border_color(border)
            .child(
                Button::new("forward-new")
                    .icon(IconName::Plus)
                    .label("New forwarding")
                    .disabled(!connected)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.form_open = !this.form_open;
                        // A message about a form that just closed no longer applies.
                        if !this.form_open {
                            this.error = None;
                        }
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(muted)
                    .child(format!("{active} active of {}", self.forwards.len())),
            )
    }

    /// The add form, revealed by `New forwarding`: a white card over the content
    /// background, in the shape of the reference's add sheet.
    fn render_form(&self, cx: &mut Context<Self>) -> Div {
        let muted = cx.theme().muted_foreground;
        let addable = !self.starting && self.session.is_some();

        div().flex().flex_col().flex_shrink_0().p_3().child(
            div()
                .flex()
                .flex_col()
                .gap_2()
                .w_full()
                .p_3()
                .rounded(px(10.))
                .bg(cx.theme().background)
                .border_1()
                .border_color(cx.theme().border)
                .child(
                    div()
                        .text_size(px(14.))
                        .font_weight(FontWeight::BOLD)
                        .child("New forwarding"),
                )
                .child(self.render_kind_tabs(cx))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .child(div().flex_1().child(Input::new(&self.spec_input).small()))
                        .child(
                            Button::new("forward-add")
                                .small()
                                .primary()
                                .label("Add")
                                .disabled(!addable)
                                .on_click(cx.listener(|this, _, window, cx| this.add(window, cx))),
                        ),
                )
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(muted)
                        .child(self.kind.hint()),
                )
                .when_some(self.error.clone(), |el, error| {
                    el.child(Alert::error("forward-error", error).title("Forward not created"))
                }),
        )
    }

    fn render_kind_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        TabBar::new("forward-kind")
            .segmented()
            .selected_index(self.kind.index())
            .on_click(cx.listener(|this, index, _, cx| {
                this.kind = ForwardKind::from_index(*index);
                // A message about the old grammar no longer applies.
                this.error = None;
                cx.notify();
            }))
            .child(Tab::new().label(ForwardKind::Local.label()))
            .child(Tab::new().label(ForwardKind::Remote.label()))
            .child(Tab::new().label(ForwardKind::Dynamic.label()))
    }

    /// The list, or whichever state stands in for it.
    fn render_body(&self, cx: &mut Context<Self>) -> AnyElement {
        // No session: say so rather than show an empty tunnels list.
        if self.session.is_none() {
            return empty_state(
                cx,
                "No session",
                "Open an SSH session to forward ports through it.",
            )
            .into_any_element();
        }

        if self.forwards.is_empty() {
            return empty_state(
                cx,
                "Set up port forwarding",
                "Add a port forward to reach databases, web apps, and other services \
                 through the session.",
            )
            .into_any_element();
        }

        let body = div()
            .id("forward-list")
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scrollbar();
        let rows: Vec<AnyElement> = self
            .forwards
            .iter()
            .map(|row| self.render_row(row, cx).into_any_element())
            .collect();
        body.children(rows).into_any_element()
    }

    /// One forward: a status marker, the endpoints, the kind, its one action and
    /// the live status line beneath.
    ///
    /// Returns `impl IntoElement` rather than `Div`: `.id(..)` wraps the div in a
    /// `Stateful<Div>`, so naming the return type would leak that wrapper here.
    fn render_row(&self, row: &ForwardRow, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let border = cx.theme().border;
        let hover = cx.theme().muted;
        let id = row.id;
        let active = row.status.is_active();

        let endpoints = match row.config.target() {
            Some((host, port)) => format!(
                "{} → {}",
                row.config.bind_label(),
                format_endpoint(host, port)
            ),
            // Dynamic has no fixed target: the SOCKS client picks one per
            // connection, so say that instead of printing a fake one.
            None => format!("{} → SOCKS5", row.config.bind_label()),
        };
        let status = row.status.label();
        let marker = row.status.marker(cx);
        let note = row.status.note().map(str::to_string);
        let kind = kind_label(&row.config);

        let action = if active {
            Button::new(format!("forward-stop-{id}"))
                .ghost()
                .xsmall()
                .icon(IconName::CircleX)
                .tooltip("Stop this forward")
                .on_click(cx.listener(move |this, _, _, cx| this.stop(id, cx)))
        } else {
            Button::new(format!("forward-remove-{id}"))
                .ghost()
                .xsmall()
                .icon(IconName::Close)
                .tooltip("Remove this entry")
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.forwards.retain(|row| row.id != id);
                    cx.notify();
                }))
        };

        div()
            .id(format!("forward-row-{id}"))
            .flex()
            .flex_col()
            .gap_1()
            .min_h(px(44.))
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(border)
            .hover(move |style| style.bg(hover))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(status_marker(marker))
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .truncate()
                            .font_family("Menlo")
                            .text_size(px(14.))
                            .child(endpoints),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .px_1()
                            .rounded(px(4.))
                            .bg(cx.theme().muted)
                            .text_color(muted)
                            .child(kind),
                    )
                    .child(action),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(div().text_size(px(12.)).text_color(marker).child(status))
                    .when_some(note, |el, note| {
                        el.child(div().text_size(px(12.)).text_color(muted).child(note))
                    }),
            )
    }
}

impl Render for ForwardPane {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // `sidebar` is the light content surface (#edf1f2); the toolbar above it
        // is the white `background` (#ffffff) in light mode.
        let background = cx.theme().sidebar;
        let foreground = cx.theme().foreground;
        let connected = self.session.is_some();
        let form_open = self.form_open;

        div()
            .flex()
            .flex_col()
            .size_full()
            .overflow_hidden()
            .bg(background)
            .text_color(foreground)
            .track_focus(&self.focus_handle)
            .child(self.render_toolbar(cx))
            .when(connected && form_open, |el| el.child(self.render_form(cx)))
            .child(self.render_body(cx))
    }
}

/// The centred empty state: a rounded tile with the forward glyph, a heading and
/// one line of explanation. Used for both "no session" and "nothing yet".
fn empty_state(cx: &App, title: &str, detail: &str) -> Div {
    let muted = cx.theme().muted_foreground;
    let foreground = cx.theme().foreground;

    div()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_3()
        .flex_1()
        .min_h(px(0.))
        .p_6()
        .child(
            div()
                .size(px(72.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(16.))
                .bg(cx.theme().muted)
                .child(
                    Icon::new(IconName::ExternalLink)
                        .large()
                        .text_color(foreground),
                ),
        )
        .child(
            div()
                .text_size(px(20.))
                .font_weight(FontWeight::BOLD)
                .text_color(foreground)
                .child(SharedString::from(title.to_string())),
        )
        .child(
            div()
                .max_w(px(420.))
                .text_center()
                .text_size(px(14.))
                .text_color(muted)
                .child(SharedString::from(detail.to_string())),
        )
}

/// The 6px status dot. A dot plus the status text reads as live vs stopped at a
/// glance, without relying on colour alone (the text carries the same meaning).
fn status_marker(color: Hsla) -> Div {
    div().size(px(6.)).rounded(px(3.)).flex_shrink_0().bg(color)
}

/// A row's kind, capitalised for display. The stored kind stays the crate's
/// lower-case label.
fn kind_label(config: &ForwardConfig) -> &'static str {
    match config {
        ForwardConfig::Local { .. } => "Local",
        ForwardConfig::Remote { .. } => "Remote",
        ForwardConfig::Dynamic { .. } => "Dynamic",
    }
}

/// Parses a spec with the parser matching the chosen kind. Kept as one function
/// so the kind-to-grammar mapping is testable on its own.
fn parse_spec(kind: ForwardKind, spec: &str) -> Result<ForwardConfig, ForwardError> {
    match kind {
        ForwardKind::Local => ForwardConfig::parse_local(spec),
        ForwardKind::Remote => ForwardConfig::parse_remote(spec),
        ForwardKind::Dynamic => ForwardConfig::parse_dynamic(spec),
    }
}

/// `host:port`, bracketing an IPv6 literal so it cannot be mistaken for a host
/// and a port.
fn format_endpoint(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local() -> ForwardConfig {
        ForwardConfig::Local {
            bind_address: "localhost".into(),
            bind_port: 8080,
            target_host: "db.internal".into(),
            target_port: 5432,
        }
    }

    #[test]
    fn parse_spec_picks_the_grammar_for_the_kind() {
        assert_eq!(
            parse_spec(ForwardKind::Dynamic, "1080").expect("dynamic spec"),
            ForwardConfig::Dynamic {
                bind_address: "localhost".into(),
                bind_port: 1080,
            }
        );
        assert_eq!(
            parse_spec(ForwardKind::Remote, "8080:db.internal:5432").expect("remote spec"),
            ForwardConfig::Remote {
                bind_address: "localhost".into(),
                bind_port: 8080,
                target_host: "db.internal".into(),
                target_port: 5432,
            }
        );
        // The grammars are not interchangeable, and the crate's message is
        // propagated rather than swallowed.
        assert!(parse_spec(ForwardKind::Dynamic, "8080:db.internal:5432").is_err());
        assert!(parse_spec(ForwardKind::Local, "1080").is_err());
        let error = parse_spec(ForwardKind::Local, "notaport:host:22").expect_err("rejects");
        assert!(error.to_string().starts_with("invalid forward:"));
    }

    #[test]
    fn a_refused_connection_does_not_hide_a_listening_forward() {
        let mut row = ForwardRow {
            id: 1,
            config: local(),
            handle: None,
            status: ForwardStatus::Starting,
        };
        row.apply(ForwardEvent::Listening {
            address: "localhost".into(),
            port: 8080,
        });
        assert!(row.status.is_active());
        assert_eq!(row.status.label(), "Listening on localhost:8080");

        row.apply(ForwardEvent::ConnectionFailed {
            target: "db.internal:5432".into(),
            message: "refused".into(),
        });
        assert!(matches!(row.status, ForwardStatus::Listening { .. }));
        assert_eq!(row.status.note(), Some("db.internal:5432: refused"));

        row.apply(ForwardEvent::Failed("bind failed".into()));
        assert_eq!(row.status.label(), "Failed — bind failed");
        assert!(!row.status.is_active());

        row.apply(ForwardEvent::Stopped);
        assert_eq!(row.status, ForwardStatus::Stopped);
    }

    #[test]
    fn endpoints_bracket_ipv6_literals() {
        assert_eq!(format_endpoint("localhost", 8080), "localhost:8080");
        assert_eq!(format_endpoint("::1", 1080), "[::1]:1080");
    }

    #[test]
    fn kind_labels_cover_every_config() {
        assert_eq!(kind_label(&local()), "Local");
        assert_eq!(
            kind_label(&ForwardConfig::Remote {
                bind_address: "localhost".into(),
                bind_port: 8080,
                target_host: "db.internal".into(),
                target_port: 5432,
            }),
            "Remote"
        );
        assert_eq!(
            kind_label(&ForwardConfig::Dynamic {
                bind_address: "localhost".into(),
                bind_port: 1080,
            }),
            "Dynamic"
        );
    }
}
