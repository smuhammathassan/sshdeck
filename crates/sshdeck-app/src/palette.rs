//! Command palette.
//!
//! Contract for the wiring pass (do not change without updating `main.rs`):
//! `PaletteView::new(window, cx) -> Self`, plus `impl Render for PaletteView`.
//! The root view constructs it with `cx.new(|cx| PaletteView::new(window, cx))`.
//!
//! Dispatch choice: the palette is self-contained and hands the chosen command
//! back through [`PaletteView::set_on_select`], a boxed
//! `Fn(CommandId, &mut Window, &mut App)`. A callback is used rather than a GPUI
//! event because the host needs `window` and `cx` to actually perform a command
//! (open a session, edit the host store) and an event payload cannot carry
//! either. Commands the palette can perform by itself (today only theme
//! toggling) run in place; commands that belong to the host stay disabled until
//! `set_on_select` has been installed. [`PaletteView::set_on_cancel`] reports
//! Escape so the host can hide the view it owns.
//!
//! Commands are a short, honest registry: only [`Builtin::ToggleTheme`] is
//! performed here, the host-dispatched entries map to operations `SshDeck`
//! already has (`add_draft_host`, `connect`, `remove_host`), and the rest are
//! explicitly marked unavailable with the reason shown on the row.
//!
//! Visual reference: Termius's quick launcher
//! (`termius-ui/Screenshot 2026-09-17 at 3.09.49 PM.png`, the "New Tab" search
//! overlay) — a 720px panel anchored near the top and centred, a filled search
//! row across the full panel width, 44px rows of icon + label + right-aligned
//! secondary text or shortcut chip, and a 10px panel radius. The reference
//! surface is flat (no drop shadow) and sits on the normal page, not on a
//! dimming scrim, so this module draws neither.

use gpui_kit::component::{
    input::{Input, InputEvent, InputState},
    kbd::Kbd,
    scroll::ScrollableElement as _,
    ActiveTheme as _, Icon, IconName, Sizable as _, Theme, ThemeMode,
};
use gpui_kit::prelude::{FluentBuilder as _, StatefulInteractiveElement as _};
use gpui_kit::{
    actions, div, px, App, AppContext as _, Context, Entity, Focusable as _, Hsla,
    InteractiveElement as _, IntoElement, KeyBinding, Keystroke, ParentElement as _, Render, Role,
    SharedString, Styled as _, Subscription, Window,
};

actions!(palette, [PaletteUp, PaletteDown, PaletteCancel]);

/// Key context for the palette's navigation bindings.
///
/// The query field is a real focused [`Input`], but a single-line GPUI input
/// registers no `up`/`down` handlers and propagates `escape`, so these bindings
/// are reached on the ancestor context — the same mechanism the gpui-kit
/// `Command` component relies on. Enter is handled through the input's
/// `PressEnter` event instead and is deliberately not bound here: binding it as
/// well would confirm the same command twice.
pub const CONTEXT: &str = "SshDeckPalette";

/// Stable identity for a palette command; the host dispatches on this.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CommandId {
    ToggleTheme,
    AddHost,
    ConnectSelectedHost,
    RemoveSelectedHost,
    OpenSftp,
    ManageKeys,
    OpenSettings,
}

impl CommandId {
    /// The stable, lowercase id used for element identity and host dispatch.
    pub fn as_str(self) -> &'static str {
        match self {
            CommandId::ToggleTheme => "toggle-theme",
            CommandId::AddHost => "add-host",
            CommandId::ConnectSelectedHost => "connect-selected-host",
            CommandId::RemoveSelectedHost => "remove-selected-host",
            CommandId::OpenSftp => "open-sftp",
            CommandId::ManageKeys => "manage-keys",
            CommandId::OpenSettings => "open-settings",
        }
    }
}

/// The palette section a command belongs to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Category {
    Appearance,
    Hosts,
    Panes,
}

impl Category {
    fn label(self) -> &'static str {
        match self {
            Category::Appearance => "appearance",
            Category::Hosts => "hosts",
            Category::Panes => "panes",
        }
    }

    fn icon(self) -> IconName {
        match self {
            Category::Appearance => IconName::Moon,
            Category::Hosts => IconName::Globe,
            Category::Panes => IconName::Folder,
        }
    }
}

/// What happens when a command is confirmed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    /// The palette performs the action itself.
    Builtin(Builtin),
    /// The host performs the action through `set_on_select`.
    Host,
    /// The app cannot do this yet; the reason is shown on the row.
    Unavailable(&'static str),
}

/// Actions the palette can run without the host.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Builtin {
    ToggleTheme,
}

/// One entry in the command registry. Fields are private; read them through
/// the accessors below.
#[derive(Clone, Copy)]
pub struct Command {
    id: CommandId,
    category: Category,
    label: &'static str,
    keywords: &'static [&'static str],
    shortcut: Option<&'static str>,
    kind: Kind,
}

impl Command {
    const fn new(
        id: CommandId,
        category: Category,
        label: &'static str,
        keywords: &'static [&'static str],
    ) -> Self {
        Self {
            id,
            category,
            label,
            keywords,
            shortcut: None,
            kind: Kind::Unavailable("Not implemented"),
        }
    }

    const fn with_shortcut(mut self, shortcut: &'static str) -> Self {
        self.shortcut = Some(shortcut);
        self
    }

    const fn kind(mut self, kind: Kind) -> Self {
        self.kind = kind;
        self
    }

    /// Stable identity, for host dispatch.
    pub fn id(&self) -> CommandId {
        self.id
    }

    /// Section this command is filed under.
    pub fn category(&self) -> Category {
        self.category
    }

    /// Human-readable label, also the primary search text.
    pub fn label(&self) -> &'static str {
        self.label
    }

    /// Extra terms that should match this command.
    pub fn keywords(&self) -> &'static [&'static str] {
        self.keywords
    }

    /// The observed shortcut, as a GPUI keystroke string, if it has one.
    pub fn shortcut(&self) -> Option<&'static str> {
        self.shortcut
    }

    /// Why the command cannot run, when it is explicitly unavailable.
    pub fn unavailable_reason(&self) -> Option<&'static str> {
        match self.kind {
            Kind::Unavailable(reason) => Some(reason),
            _ => None,
        }
    }

    /// Whether the host must perform this command through `set_on_select`.
    pub fn needs_host(&self) -> bool {
        matches!(self.kind, Kind::Host)
    }

    /// Whether the palette performs this command on its own.
    pub fn is_builtin(&self) -> bool {
        matches!(self.kind, Kind::Builtin(_))
    }
}

/// The command registry, in display order.
fn registry() -> Vec<Command> {
    vec![
        Command::new(
            CommandId::ToggleTheme,
            Category::Appearance,
            "Toggle Light / Dark Theme",
            &["theme", "dark", "light", "appearance", "mode"],
        )
        .with_shortcut("cmd-shift-t")
        .kind(Kind::Builtin(Builtin::ToggleTheme)),
        Command::new(
            CommandId::AddHost,
            Category::Hosts,
            "Add Host",
            &["new", "create", "server", "host"],
        )
        .kind(Kind::Host),
        Command::new(
            CommandId::ConnectSelectedHost,
            Category::Hosts,
            "Connect to Selected Host",
            &["ssh", "open", "session", "connect"],
        )
        .kind(Kind::Host),
        Command::new(
            CommandId::RemoveSelectedHost,
            Category::Hosts,
            "Remove Selected Host",
            &["delete", "remove", "host"],
        )
        .kind(Kind::Host),
        Command::new(
            CommandId::OpenSftp,
            Category::Panes,
            "Open SFTP Browser",
            &["files", "sftp", "browse", "transfer"],
        )
        .kind(Kind::Unavailable(
            "SFTP pane is not wired into the shell yet",
        )),
        Command::new(
            CommandId::ManageKeys,
            Category::Panes,
            "Manage SSH Keys",
            &["keys", "identity", "known hosts", "security"],
        )
        .kind(Kind::Unavailable(
            "Keys pane is not wired into the shell yet",
        )),
        Command::new(
            CommandId::OpenSettings,
            Category::Panes,
            "Open Settings",
            &["preferences", "config"],
        )
        .with_shortcut("cmd-,")
        .kind(Kind::Unavailable(
            "Settings screen is not wired into the shell yet",
        )),
    ]
}

/// Rank `commands` against `query`, best match first.
///
/// An empty query returns every index in registry order. Matching is
/// case-insensitive and orders matches as exact, prefix, word-prefix, then
/// plain substring, taking the best of the label and the keywords.
fn filter_commands(commands: &[Command], query: &str) -> Vec<usize> {
    let query = query.trim().to_ascii_lowercase();
    if query.is_empty() {
        return (0..commands.len()).collect();
    }

    let mut scored = commands
        .iter()
        .enumerate()
        .filter_map(|(index, command)| match_score(command, &query).map(|score| (score, index)))
        .collect::<Vec<_>>();
    scored.sort_by_key(|&(score, index)| (score, index));
    scored.into_iter().map(|(_, index)| index).collect()
}

fn match_score(command: &Command, query: &str) -> Option<u8> {
    let mut best = field_score(&command.label.to_ascii_lowercase(), query);
    for keyword in command.keywords {
        if let Some(score) = field_score(&keyword.to_ascii_lowercase(), query) {
            best = Some(best.map_or(score, |current| current.min(score)));
        }
    }
    best
}

fn field_score(field: &str, query: &str) -> Option<u8> {
    if field == query {
        Some(0)
    } else if field.starts_with(query) {
        Some(1)
    } else if field.split_whitespace().any(|word| word.starts_with(query)) {
        Some(2)
    } else if field.contains(query) {
        Some(3)
    } else {
        None
    }
}

fn is_enabled(command: &Command, host_wired: bool) -> bool {
    match command.kind {
        Kind::Builtin(_) => true,
        Kind::Host => host_wired,
        Kind::Unavailable(_) => false,
    }
}

fn disabled_reason(command: &Command, host_wired: bool) -> Option<&'static str> {
    match command.kind {
        Kind::Unavailable(reason) => Some(reason),
        Kind::Host if !host_wired => Some("Waiting for host wiring"),
        _ => None,
    }
}

fn kbd(stroke: &str) -> Option<Kbd> {
    Keystroke::parse(stroke).ok().map(Kbd::new)
}

fn toggle_theme(window: &mut Window, cx: &mut App) {
    let next = if Theme::global(cx).is_dark() {
        ThemeMode::Light
    } else {
        ThemeMode::Dark
    };
    Theme::change(next, Some(window), cx);
}

/// A keyboard-first command palette. Construct it with [`PaletteView::new`].
pub struct PaletteView {
    query: Entity<InputState>,
    commands: Vec<Command>,
    /// Indices into `commands`, best match first.
    filtered: Vec<usize>,
    /// Position within `filtered`, not within `commands`.
    selected: Option<usize>,
    on_select: Option<Box<dyn Fn(CommandId, &mut Window, &mut App)>>,
    on_cancel: Option<Box<dyn Fn(&mut Window, &mut App)>>,
    /// Subscription handles must outlive construction, so they are owned here.
    _subscriptions: Vec<Subscription>,
}

impl PaletteView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let query = cx.new(|cx| InputState::new(window, cx).placeholder("Type a command…"));

        let subscription =
            cx.subscribe_in(
                &query,
                window,
                |this, state, event, window, cx| match event {
                    InputEvent::Change => {
                        let value = state.read(cx).value().to_string();
                        this.refilter(&value);
                        cx.notify();
                    }
                    InputEvent::PressEnter { .. } => this.confirm(window, cx),
                    InputEvent::Focus | InputEvent::Blur => {}
                },
            );

        // Bind navigation in this module's key context. A second palette would
        // re-bind the same keys, which is harmless.
        cx.bind_keys([
            KeyBinding::new("up", PaletteUp, Some(CONTEXT)),
            KeyBinding::new("down", PaletteDown, Some(CONTEXT)),
            KeyBinding::new("escape", PaletteCancel, Some(CONTEXT)),
        ]);

        let mut this = Self {
            query,
            commands: registry(),
            filtered: Vec::new(),
            selected: None,
            on_select: None,
            on_cancel: None,
            _subscriptions: vec![subscription],
        };
        this.refilter("");
        this.focus(window, cx);
        this
    }

    /// The command registry, in display order.
    pub fn commands(&self) -> &[Command] {
        &self.commands
    }

    /// The current query text.
    pub fn query(&self, cx: &App) -> SharedString {
        self.query.read(cx).value()
    }

    /// How many commands match the current query.
    pub fn matched_count(&self) -> usize {
        self.filtered.len()
    }

    /// The highlighted command, if any.
    pub fn selected(&self) -> Option<CommandId> {
        self.selected_command().map(|command| command.id())
    }

    /// Install the host dispatch callback.
    ///
    /// Until this is set, commands that need the host render disabled and are
    /// skipped by keyboard navigation.
    pub fn set_on_select(&mut self, handler: impl Fn(CommandId, &mut Window, &mut App) + 'static) {
        self.on_select = Some(Box::new(handler));
        self.selected = self.first_enabled();
    }

    /// Install the callback invoked when Escape is pressed.
    pub fn set_on_cancel(&mut self, handler: impl Fn(&mut Window, &mut App) + 'static) {
        self.on_cancel = Some(Box::new(handler));
    }

    /// Move keyboard focus to the query field.
    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        // `focus_handle` takes `cx`; take the owned handle first so the read
        // borrow of `cx` ends before `focus` needs it mutably.
        let handle = self.query.read(cx).focus_handle(cx);
        handle.focus(window, cx);
    }

    /// Moves the highlighted command by `delta` (`-1` up, `+1` down).
    ///
    /// The host calls this when it carries [`CONTEXT`] and receives the palette's
    /// navigation actions while focus is outside the view.
    pub fn nudge(&mut self, delta: isize, cx: &mut Context<Self>) {
        self.move_selection(delta, cx);
    }

    /// Runs the Escape path: invokes the installed cancel callback.
    pub fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.cancel(window, cx);
    }

    fn refilter(&mut self, query: &str) {
        self.filtered = filter_commands(&self.commands, query);
        self.selected = self.first_enabled();
    }

    fn first_enabled(&self) -> Option<usize> {
        let host_wired = self.on_select.is_some();
        self.filtered
            .iter()
            .position(|&index| self.enabled_at(index, host_wired))
    }

    fn enabled_at(&self, index: usize, host_wired: bool) -> bool {
        self.commands
            .get(index)
            .is_some_and(|command| is_enabled(command, host_wired))
    }

    fn selected_command(&self) -> Option<Command> {
        self.selected
            .and_then(|position| self.filtered.get(position))
            .and_then(|&index| self.commands.get(index))
            .copied()
    }

    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.filtered.is_empty() {
            self.selected = None;
            cx.notify();
            return;
        }

        let len = self.filtered.len() as isize;
        let start = match self.selected {
            Some(position) => position as isize,
            None if delta > 0 => -1,
            None => 0,
        };
        let host_wired = self.on_select.is_some();

        for step in 1..=len {
            let candidate = (start + delta * step).rem_euclid(len) as usize;
            if let Some(&index) = self.filtered.get(candidate) {
                if self.enabled_at(index, host_wired) {
                    self.selected = Some(candidate);
                    cx.notify();
                    return;
                }
            }
        }
    }

    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(command) = self.selected_command() else {
            return;
        };
        if !is_enabled(&command, self.on_select.is_some()) {
            return;
        }

        match command.kind {
            Kind::Builtin(Builtin::ToggleTheme) => toggle_theme(window, cx),
            Kind::Host => {
                if let Some(handler) = self.on_select.as_ref() {
                    handler(command.id, window, cx);
                }
            }
            Kind::Unavailable(_) => {}
        }
    }

    fn select(&mut self, position: usize, cx: &mut Context<Self>) {
        self.selected = Some(position);
        cx.notify();
    }

    fn activate(&mut self, position: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = Some(position);
        cx.notify();
        self.confirm(window, cx);
    }

    fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(handler) = self.on_cancel.as_ref() {
            handler(window, cx);
        }
    }

    fn on_up(&mut self, _: &PaletteUp, _window: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(-1, cx);
    }

    fn on_down(&mut self, _: &PaletteDown, _window: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(1, cx);
    }

    fn on_palette_cancel(
        &mut self,
        _: &PaletteCancel,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cancel(window, cx);
    }
}

impl Render for PaletteView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Every colour here is a theme token: `background` is the white panel,
        // `muted` the filled search row and the selected row, `muted_foreground`
        // the secondary text, `border` the hairline. No literals, no shadow.
        let panel = cx.theme().background;
        let foreground = cx.theme().foreground;
        let border = cx.theme().border;
        let muted = cx.theme().muted_foreground;
        let highlight = cx.theme().muted;
        let danger = cx.theme().danger;

        let host_wired = self.on_select.is_some();
        let commands = self.commands.clone();
        let filtered = self.filtered.clone();
        let selected = self.selected;

        let rows = filtered
            .iter()
            .enumerate()
            .map(|(position, &command_index)| {
                let command = commands[command_index];
                let enabled = is_enabled(&command, host_wired);
                let is_selected = selected == Some(position);
                let reason = disabled_reason(&command, host_wired);
                let shortcut = if enabled {
                    command.shortcut.and_then(kbd)
                } else {
                    None
                };
                let has_shortcut = shortcut.is_some();

                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_3()
                    .w_full()
                    .h(px(44.))
                    .px_3()
                    .rounded_md()
                    .when(is_selected, |el| el.bg(highlight))
                    .when(!enabled, |el| el.text_color(muted))
                    .child(
                        Icon::new(command.category.icon())
                            .size_5()
                            .text_color(if is_selected { foreground } else { muted }),
                    )
                    .child(div().flex_1().overflow_hidden().child(command.label()))
                    .when_some(shortcut, |el, shortcut| el.child(shortcut))
                    .when(reason.is_none() && !has_shortcut, |el| {
                        el.child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .child(command.category.label()),
                        )
                    })
                    .when_some(reason, |el, reason| {
                        el.child(div().text_xs().text_color(danger).child(reason))
                    })
                    .id(SharedString::from(format!(
                        "sshdeck-palette-{}",
                        command.id.as_str()
                    )))
                    .role(Role::ListBoxOption)
                    .aria_selected(is_selected)
                    .when(enabled, |el| {
                        el.cursor_pointer()
                            .on_hover(cx.listener(move |this, hovered: &bool, _window, cx| {
                                if *hovered {
                                    this.select(position, cx);
                                }
                            }))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.activate(position, window, cx);
                            }))
                    })
            })
            .collect::<Vec<_>>();

        let empty = rows.is_empty();

        // `main.rs` centres the palette horizontally and anchors it near the
        // top; this root adds the window gutter and caps the panel at the
        // reference width so a narrow window still has breathing room.
        div()
            .flex()
            .flex_col()
            .items_center()
            .w_full()
            .px_4()
            .key_context(CONTEXT)
            .on_action(cx.listener(Self::on_up))
            .on_action(cx.listener(Self::on_down))
            .on_action(cx.listener(Self::on_palette_cancel))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .w_full()
                    .max_w(px(720.))
                    .max_h(px(430.))
                    .overflow_hidden()
                    .rounded_lg()
                    .border_1()
                    .border_color(border)
                    .bg(panel)
                    .text_color(foreground)
                    .child(
                        // The search row spans the panel and is a filled,
                        // borderless field; the focused ring comes from `Input`.
                        div().flex_none().p_3().child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .w_full()
                                .bg(highlight)
                                .rounded_md()
                                .px_3()
                                .child(
                                    Input::new(&self.query)
                                        .appearance(false)
                                        .small()
                                        .cleanable(true)
                                        .aria_label(SharedString::from("Search commands"))
                                        .prefix(
                                            Icon::new(IconName::Search).size_4().text_color(muted),
                                        ),
                                ),
                        ),
                    )
                    .child(
                        div()
                            .id("sshdeck-palette-list")
                            .flex()
                            .flex_col()
                            .gap_1()
                            .flex_1()
                            .p_2()
                            .overflow_y_scrollbar()
                            .when(empty, |el| {
                                el.child(
                                    div()
                                        .flex()
                                        .flex_1()
                                        .flex_col()
                                        .items_center()
                                        .justify_center()
                                        .gap_2()
                                        .py_8()
                                        .child(
                                            Icon::new(IconName::Search).size_5().text_color(muted),
                                        )
                                        .child(
                                            div()
                                                .text_sm()
                                                .text_color(muted)
                                                .child("No commands match that search"),
                                        ),
                                )
                            })
                            .children(rows),
                    )
                    .child(render_hints(muted, border)),
            )
    }
}

fn render_hints(muted: Hsla, border: Hsla) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_3()
        .flex_none()
        .px_3()
        .py_2()
        .border_t_1()
        .border_color(border)
        .text_xs()
        .text_color(muted)
        .child(hint(&["up", "down"], "navigate"))
        .child(hint(&["enter"], "select"))
        .child(hint(&["escape"], "close"))
}

fn hint(keys: &[&'static str], label: &'static str) -> impl IntoElement {
    let mut row = div().flex().flex_row().items_center().gap_1();
    for &key in keys {
        if let Some(kbd) = kbd(key) {
            row = row.child(kbd);
        }
    }
    row.child(label)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn two_command_registry() -> Vec<Command> {
        vec![
            Command::new(
                CommandId::ToggleTheme,
                Category::Appearance,
                "javascript",
                &[],
            )
            .kind(Kind::Unavailable("test")),
            Command::new(CommandId::AddHost, Category::Hosts, "script", &[])
                .kind(Kind::Unavailable("test")),
        ]
    }

    #[test]
    fn empty_query_lists_every_command_in_registry_order() {
        let commands = registry();
        assert_eq!(
            filter_commands(&commands, ""),
            (0..commands.len()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn prefix_outranks_substring() {
        // "scr" is a prefix of "script" but only a substring of "javascript".
        assert_eq!(filter_commands(&two_command_registry(), "scr"), vec![1, 0]);
    }

    #[test]
    fn no_match_yields_empty() {
        assert!(filter_commands(&registry(), "zzzz").is_empty());
    }

    #[test]
    fn keywords_match_case_insensitively() {
        let commands = registry();
        let matches = filter_commands(&commands, "THEME");
        assert_eq!(matches.len(), 1);
        assert_eq!(commands[matches[0]].id(), CommandId::ToggleTheme);
    }

    #[test]
    fn registry_is_honest_about_what_it_can_do() {
        let commands = registry();
        let mut ids = commands.iter().map(|c| c.id.as_str()).collect::<Vec<_>>();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), commands.len(), "command ids must be unique");
        for command in &commands {
            assert!(
                command.is_builtin()
                    || command.needs_host()
                    || command.unavailable_reason().is_some(),
                "{} must either run or say why it cannot",
                command.id.as_str()
            );
        }
    }
}
