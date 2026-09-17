//! sshdeck — a GPUI desktop SSH client.
//!
//! Milestone 1 is the shell: host inventory, filtering, selection, persistence,
//! theming. Transport (russh) replaces the placeholder pane next.

use gpui_kit::component::{
    button::{Button, ButtonVariants as _},
    input::{Input, InputEvent, InputState},
    notification::Notification,
    scroll::ScrollableElement as _,
    ActiveTheme as _, Disableable as _, Icon, IconName, Root, Sizable as _, Theme, ThemeMode,
    WindowExt,
};
use gpui_kit::prelude::{FluentBuilder as _, StatefulInteractiveElement as _};
use gpui_kit::{
    div, px, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, Styled as _, Subscription, Window, WindowOptions,
};
use sshdeck_core::{Host, HostId, HostStore, SessionState};

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

/// One open session, mirroring an entry in the tab strip.
struct Session {
    host: HostId,
    state: SessionState,
}

struct SshDeck {
    store: HostStore,
    selected: Option<HostId>,
    sessions: Vec<Session>,
    filter: Entity<InputState>,
    draft_label: Entity<InputState>,
    draft_address: Entity<InputState>,
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
            sessions: Vec::new(),
            filter,
            draft_label,
            draft_address,
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

    fn connected_count(&self) -> usize {
        self.sessions.iter().filter(|s| s.state.is_active()).count()
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
            let remove_id = id.clone();

            div()
                .id(SharedString::from(format!("host-{}", id)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.selected = Some(id.clone());
                    cx.notify();
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

    fn render_main(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let border = cx.theme().border;
        let muted = cx.theme().muted_foreground;

        let selected = self
            .selected
            .as_ref()
            .and_then(|id| self.store.inventory().get(id))
            .cloned();

        let content = match selected {
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
                .size_full()
                .child(
                    // Terminal surface placeholder. The real grid lands with the
                    // russh transport; this reserves the layout and type.
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .p_3()
                        .gap_1()
                        .font_family("Menlo")
                        .text_sm()
                        .child(
                            div()
                                .text_color(muted)
                                .child(SharedString::from(format!("$ ssh {}", host.endpoint()))),
                        )
                        .child(
                            div()
                                .text_color(muted)
                                .child("transport not wired up yet — see docs/re/"),
                        ),
                )
                .into_any_element(),
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
                    .child(match &self.selected {
                        Some(id) => SharedString::from(format!("session · {id}")),
                        None => SharedString::from("no session"),
                    })
                    .child(
                        Button::new("connect")
                            .small()
                            .label("Connect")
                            .disabled(true)
                            .tooltip("Transport lands next milestone"),
                    ),
            )
            .child(content)
    }

    fn render_status_bar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let border = cx.theme().border;
        let muted = cx.theme().muted_foreground;
        let active = self.connected_count();

        let host_label = self
            .selected
            .as_ref()
            .and_then(|id| self.store.inventory().get(id))
            .map(|host| format!("{} · {}", host.endpoint(), host.auth.label()))
            .unwrap_or_else(|| "no host selected".to_string());

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
