//! sshdeck — a GPUI desktop SSH client.
//!
//! The shell (host inventory, filtering, selection, persistence, theming) is
//! milestone 1. This is milestone 2: the pane next to it is a real terminal
//! backed by the `sshdeck-core` transport and the `sshdeck-terminal` grid.

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
    div, px, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, Styled as _, Subscription, Window, WindowOptions,
};
use sshdeck_core::session::SessionConfig;
use sshdeck_core::{Host, HostId, HostStore, SessionState};
use terminal::{PaneStatus, TerminalPane};

fn main() {
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

/// One open tab: the host it points at and the pane rendering it.
struct Session {
    host: HostId,
    pane: Entity<TerminalPane>,
    /// Mirrors the pane's state so the tab strip and status bar can render it
    /// without reaching into the pane every frame.
    status: PaneStatus,
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

        Self {
            store,
            selected: None,
            active: None,
            sessions: Vec::new(),
            filter,
            draft_label,
            draft_address,
            secret: None,
            _subscriptions: subscriptions,
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
            sshdeck_core::AuthMethod::Password { .. } => {
                self.prompt_for_secret(&host, window, cx);
                return;
            }
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
        let pane = TerminalPane::new(config, window, cx);
        // Re-render the chrome whenever the pane's status changes. The pane is
        // the only thing that reads the event channel; the shell just mirrors.
        let subscription = cx.observe_in(&pane, window, |this, pane, _window, cx| {
            let status = pane.read(cx).status();
            // Match by handle rather than trusting the active index: the pane can
            // notify before the tab is registered.
            if let Some(session) = this.sessions.iter_mut().find(|s| s.pane == pane) {
                session.status = status;
            }
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
                        Button::new("settings")
                            .ghost()
                            .icon(IconName::Settings2)
                            .tooltip("Settings"),
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
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.active = Some(index);
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

    fn render_main(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
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

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(background)
            .text_color(foreground)
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
            .children(Root::render_notification_layer(window, cx))
    }
}
