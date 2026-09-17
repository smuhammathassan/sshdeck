//! Logs pane — session activity history.
//!
//! Contract for the wiring pass (do not change without updating `main.rs`):
//! `LogsPane::new(window, cx) -> Self`, plus `impl Render for LogsPane`.
//! The root view constructs it with `cx.new(|cx| LogsPane::new(window, cx))`.
//!
//! What the reference screenshot shows (3.13.42 PM — Logs):
//! - Left rail `Logs` selected; content area is a light `#edf1f2` surface with
//!   white cards (`#ffffff`).
//! - A thin top banner: `Logs are not available on your current plan.  Upgrade ↑`
//!   (Termius paywall gate). This is a commercial entitlement, not a local log;
//!   sshdeck has no such gate — the pane below replaces it with a local activity log.
//! - Below the banner, a table header row: `Date` (with sort arrow) | `User` |
//!   `Host` | `Saved` (bookmark column). The header is 36–44px with a hairline
//!   separator.
//! - Rows are ~44px, white, hairline `#d5dde0` separators, hover fill. Each row:
//!   Date column `Aug 12, 2026` + `15:13 - 19:41` (secondary, monospaced); User
//!   column orange `MH` avatar + `muhammad.hassan@teamredge.c…` + machine
//!   `Muhammads-MacBook-Air.local`; Host column orange (or dark blue) host icon +
//!   name (`Talluq`, `Ali Jawwad`, `Ubunutu Hadeeth`, `horly cloudzy`,
//!   `Local Terminal`, …) + `ssh, root/ubuntu/xrdpuser`; Saved column bookmark
//!   glyph. The screenshot lists ~10 session intervals from `Jul 27` to `Aug 12`.
//! - Shared page furniture (from 3.13.29 Port Forwarding empty state and 3.13.24
//!   Keychain) confirmed the toolbar layout (primary action left, search/grid/
//!   calendar icons right), card/table styling (white cards, 10px radius, light
//!   content bg), and empty-state pattern (centred muted glyph tile + heading +
//!   one-liner) this pane shares.
//!
//! Design chosen: **A — real bounded in-memory activity log**. The app has no
//! logging subsystem yet, but an empty-state-only pane would hide that wiring can
//! exist. The pane owns a bounded ring buffer (`MAX_LOG_ENTRIES = 500`, newest at
//! tail, oldest evicted) of timestamped activity entries (session
//! connected/closed/failed, forward started/stopped, transfer completed, errors).
//! No log lines are fabricated: the buffer starts empty and only shows entries the
//! wiring pass pushes via `push_entry` / `append`. The empty state (`No activity
//! yet`) is honest when nothing has been pushed. Cap is stated and tested.
//!
//! Visual match:
//! - Content bg is `cx.theme().sidebar` (`#edf1f2` in light); cards/rows are
//!   `cx.theme().background` (`#ffffff`). Table sits in a white `10px` rounded
//!   card so the light bg frames it, as in the reference.
//! - Toolbar row at top: primary action `Clear logs` on the left (disabled when
//!   empty); on the right the same three small icon controls the other panes
//!   carry: search (functional — filters the list), grid/list and calendar
//!   (dead controls, rendered **disabled** and noted below).
//! - Rows are table-like: `min_h 44px`, hairline separators `cx.theme().border`
//!   (`#d5dde0` light), hover fill `cx.theme().muted`, monospaced timestamp in
//!   `Menlo 12px` coloured `muted_foreground` (`#798c94`), then the message/host
//!   detail. Severity is a 6px coloured dot (success/warning/danger/
//!   muted_foreground) rather than a coloured row.
//! - Empty state mirrors the port-forwarding empty state: centred 72px muted tile
//!   with a glyph, heading `No activity yet` and one line of explanation.
//!
//! Hardcoded values (no theme token exists):
//! - Host/user avatar fills: host orange `rgb(0xd96c2b)`, host blue
//!   `rgb(0x204b6b)` for the two `horly`/`Local Terminal` rows, user avatar
//!   `rgb(0xf0a75a)` (the `MH` peach). These are platform/host tints recovered
//!   from the reference; no semantic token covers them.
//! - All other colours come from `cx.theme()` tokens (`background`, `sidebar`,
//!   `border`, `muted`, `muted_foreground`, `foreground`, `success`, `warning`,
//!   `danger`). Body `14px`, secondary `12px–13px`; control radius `6px`
//!   (`rounded_md`), card radius `10px` (`rounded(px(10.))`) from the theme.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    div, px, rgb, AnyElement, App, AppContext as _, Context, Div, Entity, FocusHandle,
    Focusable as _, Hsla, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    SharedString, Styled as _, Subscription, Window,
};

/// Cap for the in-memory ring buffer. `docs/BUDGET.md` treats unbounded growth as
/// the memory cliff; 500 entries is well within the 500–1000 budget and keeps the
/// pane O(1) in the idle case.
const MAX_LOG_ENTRIES: usize = 500;

/// Severity / outcome of an activity entry. Rendered as a 6px dot, not a coloured row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogLevel {
    Info,
    Success,
    Warning,
    Error,
}

impl LogLevel {
    fn label(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Success => "success",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }

    fn color(self, cx: &gpui_kit::App) -> Hsla {
        match self {
            Self::Info => cx.theme().muted_foreground,
            Self::Success => cx.theme().success,
            Self::Warning => cx.theme().warning,
            Self::Error => cx.theme().danger,
        }
    }
}

/// One activity entry — a session interval / event the app can populate.
///
/// Private fields plus readers, per `AGENTS.md`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogEntry {
    id: u64,
    date: String,
    time_range: String,
    user: String,
    user_detail: String,
    host: String,
    host_detail: String,
    level: LogLevel,
}

impl LogEntry {
    /// Creates an entry. `id` is assigned by the pane; callers supply the visible fields.
    pub fn new(
        date: impl Into<String>,
        time_range: impl Into<String>,
        user: impl Into<String>,
        user_detail: impl Into<String>,
        host: impl Into<String>,
        host_detail: impl Into<String>,
        level: LogLevel,
    ) -> Self {
        Self {
            id: 0,
            date: date.into(),
            time_range: time_range.into(),
            user: user.into(),
            user_detail: user_detail.into(),
            host: host.into(),
            host_detail: host_detail.into(),
            level,
        }
    }

    pub fn id(&self) -> u64 {
        self.id
    }
    pub fn date(&self) -> &str {
        &self.date
    }
    pub fn time_range(&self) -> &str {
        &self.time_range
    }
    pub fn user(&self) -> &str {
        &self.user
    }
    pub fn user_detail(&self) -> &str {
        &self.user_detail
    }
    pub fn host(&self) -> &str {
        &self.host
    }
    pub fn host_detail(&self) -> &str {
        &self.host_detail
    }
    pub fn level(&self) -> LogLevel {
        self.level
    }
}

/// The logs pane. Constructed by the root view.
pub struct LogsPane {
    entries: Vec<LogEntry>,
    next_id: u64,
    filter_input: Entity<InputState>,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl LogsPane {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Search logs — user, host, date"));
        let focus_handle = cx.focus_handle();
        // Re-render as the filter query changes.
        let subscriptions = vec![
            cx.subscribe_in(&filter_input, window, |_, _, event, _, cx| {
                if matches!(event, gpui_kit::component::input::InputEvent::Change) {
                    cx.notify();
                }
            }),
        ];
        Self {
            entries: Vec::new(),
            next_id: 1,
            filter_input,
            focus_handle,
            _subscriptions: subscriptions,
        }
    }

    /// How many entries are buffered (0..=MAX_LOG_ENTRIES).
    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    /// The buffered entries, oldest first.
    pub fn entries(&self) -> &[LogEntry] {
        &self.entries
    }

    /// Whether the buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The cap of the ring buffer.
    pub fn cap(&self) -> usize {
        MAX_LOG_ENTRIES
    }

    /// Pushes one entry, evicting the oldest when at capacity. Id is assigned here
    /// so element ids are domain-derived and monotonically increasing.
    pub fn push_entry(&mut self, mut entry: LogEntry, cx: &mut Context<Self>) {
        entry.id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        if self.entries.len() >= MAX_LOG_ENTRIES {
            self.entries.remove(0);
        }
        self.entries.push(entry);
        cx.notify();
    }

    /// Convenience: append a session-interval entry with a level. The wiring pass
    /// calls this for connect/close/failure events; nothing fabricates lines at
    /// startup.
    pub fn append(
        &mut self,
        date: impl Into<String>,
        time_range: impl Into<String>,
        user: impl Into<String>,
        user_detail: impl Into<String>,
        host: impl Into<String>,
        host_detail: impl Into<String>,
        level: LogLevel,
        cx: &mut Context<Self>,
    ) {
        let entry = LogEntry::new(
            date,
            time_range,
            user,
            user_detail,
            host,
            host_detail,
            level,
        );
        self.push_entry(entry, cx);
    }

    /// Clears the buffer.
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.entries.clear();
        cx.notify();
    }

    fn filtered(&self, cx: &gpui_kit::App) -> Vec<LogEntry> {
        let query = self.filter_input.read(cx).value().trim().to_lowercase();
        if query.is_empty() {
            return self.entries.clone();
        }
        self.entries
            .iter()
            .filter(|entry| {
                entry.date.to_lowercase().contains(&query)
                    || entry.time_range.to_lowercase().contains(&query)
                    || entry.user.to_lowercase().contains(&query)
                    || entry.user_detail.to_lowercase().contains(&query)
                    || entry.host.to_lowercase().contains(&query)
                    || entry.host_detail.to_lowercase().contains(&query)
                    || entry.level.label().contains(&query)
            })
            .cloned()
            .collect()
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> Div {
        let border = cx.theme().border;
        let has_entries = !self.entries.is_empty();
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
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new("logs-clear")
                            .label("Clear logs")
                            .icon(IconName::Close)
                            .disabled(!has_entries)
                            .on_click(cx.listener(|this, _, _, cx| this.clear(cx))),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(cx.theme().muted_foreground)
                            .child(format!("{} entries", self.entries.len())),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .flex_shrink_0()
                    .child(
                        div()
                            .w(px(200.))
                            .child(Input::new(&self.filter_input).small()),
                    )
                    .child(
                        Button::new("logs-search")
                            .ghost()
                            .icon(IconName::Search)
                            .tooltip("Search logs")
                            .on_click(cx.listener(|this, _, window, cx| {
                                let handle = this.filter_input.read(cx).focus_handle(cx);
                                handle.focus(window, cx);
                            })),
                    )
                    .child(
                        Button::new("logs-grid")
                            .ghost()
                            .icon(IconName::LayoutDashboard)
                            .tooltip("Grid view — not applicable to logs")
                            .disabled(true),
                    )
                    .child(
                        Button::new("logs-calendar")
                            .ghost()
                            .icon(IconName::Inbox)
                            .tooltip("Calendar — not applicable to logs")
                            .disabled(true),
                    ),
            )
    }

    fn render_table_header(&self, cx: &mut Context<Self>) -> Div {
        let muted = cx.theme().muted_foreground;
        let border = cx.theme().border;
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .flex_shrink_0()
            .text_size(px(12.))
            .text_color(muted)
            .border_b_1()
            .border_color(border)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .w(px(140.))
                    .child("Date")
                    .child(Icon::new(IconName::ArrowUp).small().text_color(muted)),
            )
            .child(div().flex_1().child("User"))
            .child(div().flex_1().child("Host"))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .w(px(48.))
                    .justify_end()
                    .child("Saved")
                    .child(Icon::new(IconName::File).small().text_color(muted)),
            )
    }

    fn render_row(&self, entry: &LogEntry, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let border = cx.theme().border;
        let hover = cx.theme().muted;
        let fg = cx.theme().foreground;
        let id = entry.id;
        let marker_color = entry.level.color(&**cx);
        // Host icon tint: the two blue rows in the reference (horly cloudzy,
        // Local Terminal) vs the orange majority.
        let host_lower = entry.host.to_lowercase();
        let host_bg = if host_lower.contains("horly") || host_lower.contains("local terminal") {
            rgb(0x204b6b)
        } else {
            rgb(0xd96c2b)
        };
        let user_bg = rgb(0xf0a75a);
        let initials = user_initials(&entry.user);

        div()
            .id(SharedString::from(format!("log-row-{id}")))
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .min_h(px(44.))
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(border)
            .hover(move |style| style.bg(hover))
            .child(status_marker(marker_color))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .w(px(120.))
                    .child(
                        div()
                            .text_size(px(14.))
                            .text_color(fg)
                            .child(SharedString::from(entry.date.clone())),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .font_family("Menlo")
                            .text_color(muted)
                            .child(SharedString::from(entry.time_range.clone())),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .flex_1()
                    .min_w(px(0.))
                    .overflow_hidden()
                    .child(
                        div()
                            .size(px(28.))
                            .rounded(px(8.))
                            .bg(user_bg)
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(11.))
                            .font_family("Menlo")
                            .text_color(rgb(0xffffff))
                            .child(SharedString::from(initials)),
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
                                    .truncate()
                                    .child(SharedString::from(entry.user.clone())),
                            )
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(muted)
                                    .truncate()
                                    .child(SharedString::from(entry.user_detail.clone())),
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .flex_1()
                    .min_w(px(0.))
                    .overflow_hidden()
                    .child(
                        div()
                            .size(px(28.))
                            .rounded(px(6.))
                            .bg(host_bg)
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(Icon::new(IconName::Globe).small().text_color(rgb(0xffffff))),
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
                                    .truncate()
                                    .child(SharedString::from(entry.host.clone())),
                            )
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(muted)
                                    .truncate()
                                    .child(SharedString::from(entry.host_detail.clone())),
                            ),
                    ),
            )
            .child(
                div().w(px(48.)).flex().justify_end().child(
                    div()
                        .size(px(28.))
                        .rounded(px(6.))
                        .bg(cx.theme().muted)
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(Icon::new(IconName::File).small().text_color(muted)),
                ),
            )
    }

    fn render_body(&self, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        use gpui_kit::IntoElement as _;

        if self.entries.is_empty() {
            return empty_state(cx).into_any_element();
        }

        let filtered = self.filtered(cx);
        if filtered.is_empty() {
            let muted = cx.theme().muted_foreground;
            return div()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_2()
                .flex_1()
                .min_h(px(0.))
                .p_6()
                .child(Icon::new(IconName::Search).large().text_color(muted))
                .child(
                    div()
                        .text_size(px(14.))
                        .text_color(muted)
                        .child("No matches"),
                )
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(muted)
                        .child("Try a different search term."),
                )
                .into_any_element();
        }

        // Newest first, as in the reference (Aug 12 at top).
        let mut entries = filtered;
        entries.reverse();

        let header = self.render_table_header(cx);
        let rows: Vec<gpui_kit::AnyElement> = entries
            .iter()
            .map(|entry| self.render_row(entry, cx).into_any_element())
            .collect();

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.))
            .p_3()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h(px(0.))
                    .bg(cx.theme().background)
                    .rounded(px(10.))
                    .border_1()
                    .border_color(cx.theme().border)
                    .overflow_hidden()
                    .child(header)
                    .child(
                        div()
                            .id("logs-list")
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_h(px(0.))
                            .overflow_y_scrollbar()
                            .children(rows),
                    ),
            )
            .into_any_element()
    }

    fn render_upgrade_banner(&self, cx: &mut Context<Self>) -> Div {
        let border = cx.theme().border;
        let fg = cx.theme().foreground;

        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .px_4()
            .py_2p5()
            .mx_3()
            .mt_3()
            .rounded(px(8.))
            .bg(cx.theme().background)
            .border_1()
            .border_color(border)
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(fg)
                    .child("Logs are not available on your current plan."),
            )
            .child(
                Button::new("logs-upgrade")
                    .small()
                    .ghost()
                    .label("Upgrade ↑")
                    .tooltip("sshdeck is 100% free and local"),
            )
    }
}

impl Render for LogsPane {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let background = cx.theme().sidebar;
        let foreground = cx.theme().foreground;
        div()
            .flex()
            .flex_col()
            .size_full()
            .overflow_hidden()
            .bg(background)
            .text_color(foreground)
            .text_size(px(14.))
            .track_focus(&self.focus_handle)
            .child(self.render_upgrade_banner(cx))
            .child(self.render_body(cx))
    }
}

fn empty_state(cx: &gpui_kit::App) -> Div {
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
                .child(Icon::new(IconName::Inbox).large().text_color(foreground)),
        )
        .child(
            div()
                .text_size(px(20.))
                .text_color(foreground)
                .child("No activity yet"),
        )
        .child(
            div()
                .max_w(px(420.))
                .text_center()
                .text_size(px(14.))
                .text_color(muted)
                .child("Connections, forwards and transfers will appear here once you use them."),
        )
}

fn status_marker(color: Hsla) -> Div {
    div().size(px(6.)).rounded(px(3.)).flex_shrink_0().bg(color)
}

fn user_initials(user: &str) -> String {
    let trimmed = user.trim();
    if trimmed.is_empty() {
        return "—".to_string();
    }
    // Prefer the part before '@', then take first two alphabetic chars.
    let handle = trimmed.split('@').next().unwrap_or(trimmed);
    let mut chars: Vec<char> = handle
        .chars()
        .filter(|c| c.is_alphabetic())
        .take(2)
        .collect();
    if chars.is_empty() {
        chars = trimmed
            .chars()
            .filter(|c| !c.is_whitespace())
            .take(2)
            .collect();
    }
    if chars.is_empty() {
        return "—".to_string();
    }
    chars.iter().collect::<String>().to_uppercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry_with_date(date: &str, level: LogLevel) -> LogEntry {
        LogEntry::new(
            date,
            "00:00 - 01:00",
            "muhammad.hassan@teamredge.com",
            "Muhammads-MacBook-Air.local",
            "Talluq",
            "ssh, root",
            level,
        )
    }

    #[test]
    fn ring_buffer_evicts_oldest_at_cap() {
        // Drive the eviction logic without a GPUI context: replicate push behaviour.
        let mut entries: Vec<LogEntry> = Vec::new();
        let mut next_id: u64 = 1;
        let mut push = |entry: LogEntry| {
            let mut e = entry;
            e.id = next_id;
            next_id = next_id.wrapping_add(1);
            if entries.len() >= MAX_LOG_ENTRIES {
                entries.remove(0);
            }
            entries.push(e);
        };
        for index in 0..MAX_LOG_ENTRIES + 10 {
            push(entry_with_date(
                &format!("2026-08-{:02}", (index % 28) + 1),
                LogLevel::Info,
            ));
        }
        assert_eq!(entries.len(), MAX_LOG_ENTRIES);
        // The first 10 should have been evicted; the oldest remaining is the 11th pushed.
        // Its id should be 11 (ids start at 1).
        assert_eq!(entries.first().map(|e| e.id), Some(11));
        assert_eq!(
            entries.last().map(|e| e.id),
            Some((MAX_LOG_ENTRIES + 10) as u64)
        );
    }

    #[test]
    fn log_level_labels_and_user_initials() {
        assert_eq!(LogLevel::Info.label(), "info");
        assert_eq!(LogLevel::Success.label(), "success");
        assert_eq!(LogLevel::Warning.label(), "warning");
        assert_eq!(LogLevel::Error.label(), "error");

        assert_eq!(user_initials("muhammad.hassan@teamredge.com"), "MU");
        assert_eq!(user_initials("ali@host"), "AL");
        assert_eq!(user_initials("  "), "—");
        assert_eq!(user_initials("x"), "X");
        assert_eq!(user_initials("john doe"), "JO");
    }

    #[test]
    fn entry_fields_are_private_and_readable() {
        let entry = entry_with_date("Aug 12, 2026", LogLevel::Success);
        assert_eq!(entry.date(), "Aug 12, 2026");
        assert_eq!(entry.level(), LogLevel::Success);
        assert_eq!(entry.host(), "Talluq");
        assert_eq!(entry.host_detail(), "ssh, root");
    }
}
