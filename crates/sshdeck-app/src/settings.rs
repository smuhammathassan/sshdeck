//! The settings surface: a grouped, searchable view over the app's tunables.
//!
//! Built on gpui-kit's `setting` module. That component owns its own keyed
//! state (search box, page selection, widget plumbing), so this view stores only
//! the values themselves and rebuilds the rows from them on every render.
//!
//! Rows are drawn with `SettingItem::render` rather than `SettingItem::new`,
//! because the Termius layout is specific: a 12px uppercase `muted_foreground`
//! section header, a 14px primary label with a 12px `muted_foreground`
//! description beneath it, the control right-aligned and vertically centred,
//! and a 1px `#8d91a51a` hairline between rows instead of a card border.
//! `SettingItem::render` keeps the component's search and page machinery while
//! giving each row that shape.
//!
//! Nothing here pretends to work: every control that is not wired to behaviour
//! yet says so in its row description, and the terminal pane still reads its
//! compiled defaults until the wiring pass connects it to the reader methods
//! below.
//!
//! Contract for the wiring pass (do not change without updating `main.rs`):
//! `SettingsView::new(window, cx) -> Self`, plus `impl Render for SettingsView`.
//! The root view constructs it with `cx.new(|cx| SettingsView::new(window, cx))`.

use gpui_kit::component::{
    button::{Button, ButtonVariants as _},
    group_box::GroupBoxVariant,
    setting::{SettingGroup, SettingItem, SettingPage, Settings},
    switch::Switch,
    ActiveTheme as _, Disableable as _, Sizable as _, Size, Theme, ThemeMode,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    div, px, rgba, App, Context, FocusHandle, IntoElement, ParentElement as _, Render, Rgba,
    SharedString, Styled as _, Window,
};
// `.track_focus(..)` on the root is an `InteractiveElement` method.
use gpui_kit::InteractiveElement as _;
// The ranges are sshdeck-config's, not this view's: the terminal pane, the
// persisted file, and this pane must offer exactly the same bounds, or the pane
// can offer a value something downstream will silently clamp. `MAX_SCROLLBACK`
// is deliberately 10 000 — `docs/BUDGET.md` treats unbounded scrollback as the
// memory cliff — so this view can never offer more.
use sshdeck_config::{
    DEFAULT_CURSOR_BLINK, DEFAULT_FONT_SIZE, DEFAULT_SCROLLBACK, MAX_FONT_SIZE, MAX_SCROLLBACK,
    MIN_FONT_SIZE, MIN_SCROLLBACK,
};
use sshdeck_core::HostStore;

/// Font-size stepper increment, in pixels.
const FONT_SIZE_STEP: f64 = 1.0;

/// Scrollback stepper increment, in lines.
const SCROLLBACK_STEP: f64 = 1_000.0;

/// Clamps a font size in pixels into the supported range.
///
/// `NaN` (a cleared number input can produce it) falls back to the default
/// rather than becoming a zero-sized font.
fn clamp_font_size(value: f64) -> f32 {
    if value.is_nan() {
        return DEFAULT_FONT_SIZE;
    }
    value.clamp(f64::from(MIN_FONT_SIZE), f64::from(MAX_FONT_SIZE)) as f32
}

/// Clamps a scrollback line count into the budgeted range.
///
/// Bounded on purpose: unbounded scrollback is the memory cliff named in
/// `docs/BUDGET.md`. `NaN` falls back to the default.
fn clamp_scrollback(value: f64) -> usize {
    if value.is_nan() {
        return DEFAULT_SCROLLBACK;
    }
    value
        .clamp(MIN_SCROLLBACK as f64, MAX_SCROLLBACK as f64)
        .round() as usize
}

/// The `--border-light` row separator, `#8d91a51a`.
///
/// The bundled theme has no token for it (`AGENTS.md` errata), and the exact
/// recovered value is in `docs/UI-PARITY.md` "Semantic tokens", so it is used
/// directly here.
fn hairline() -> Rgba {
    rgba(0x8d91a51a)
}

/// A section header: 12px, muted, uppercase, with an optional muted note.
fn section_header(
    cx: &App,
    title: &'static str,
    description: Option<SharedString>,
) -> impl IntoElement {
    let mut header = div()
        .w_full()
        .pt_4()
        .pb_2()
        .text_size(px(12.0))
        .text_color(cx.theme().muted_foreground)
        .child(SharedString::from(title.to_uppercase()));
    if let Some(description) = description {
        header = header.child(
            div()
                .pt_1()
                .text_size(px(12.0))
                .text_color(cx.theme().muted_foreground)
                .child(description),
        );
    }
    header
}

/// One row of the grouped list: a 14px primary label, a 12px muted description
/// beneath it, the control right-aligned and vertically centred, and a 1px
/// hairline underneath. Rows carry no card border of their own.
fn setting_row(
    cx: &App,
    label: &'static str,
    description: SharedString,
    control: impl IntoElement,
) -> impl IntoElement {
    div()
        .w_full()
        .min_h(px(44.0))
        .py_2()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap_4()
        .border_b_1()
        .border_color(hairline())
        .child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .flex_1()
                .child(
                    div()
                        .text_size(px(14.0))
                        .text_color(cx.theme().foreground)
                        .child(label),
                )
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(cx.theme().muted_foreground)
                        .child(description),
                ),
        )
        .child(control)
}

/// A read-only value, muted and right-aligned, ellipsised rather than wrapped.
fn read_only(cx: &App, value: SharedString) -> impl IntoElement {
    div()
        .flex_none()
        .max_w(px(280.0))
        .truncate()
        .text_size(px(14.0))
        .text_color(cx.theme().muted_foreground)
        .child(value)
}

/// A `−  value  +` stepper. Both ends disable at the bounds of the range the
/// numbers came from, so the control cannot request an out-of-range value.
fn stepper(
    decrement_id: &'static str,
    increment_id: &'static str,
    value: SharedString,
    at_min: bool,
    at_max: bool,
    size: Size,
    decrement: impl Fn(&mut Window, &mut App) + 'static,
    increment: impl Fn(&mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .child(
            Button::new(decrement_id)
                .label("−")
                .ghost()
                .with_size(size)
                .disabled(at_min)
                .on_click(move |_, window, cx| decrement(window, cx)),
        )
        .child(
            div()
                .min_w(px(40.0))
                .text_center()
                .text_size(px(14.0))
                .child(value),
        )
        .child(
            Button::new(increment_id)
                .label("+")
                .ghost()
                .with_size(size)
                .disabled(at_max)
                .on_click(move |_, window, cx| increment(window, cx)),
        )
}

/// A right-aligned switch. The callback receives the requested value; the owner
/// writes it back and notifies, so the switch is controlled, not optimistic.
fn toggle(
    id: &'static str,
    label: &'static str,
    checked: bool,
    disabled: bool,
    size: Size,
    on_change: impl Fn(bool, &mut App) + 'static,
) -> impl IntoElement {
    Switch::new(id)
        .checked(checked)
        .disabled(disabled)
        .with_size(size)
        .accessibility_label(label)
        .on_click(move |next, _window, cx| on_change(*next, cx))
}

/// A section header as a search-aware settings item.
fn section_item(title: &'static str, description: Option<SharedString>) -> SettingItem {
    SettingItem::render(move |_, _, cx: &mut App| section_header(cx, title, description.clone()))
        .keywords([title])
}

/// A read-only row as a search-aware settings item.
fn read_only_item(
    label: &'static str,
    value: SharedString,
    description: &'static str,
    keywords: &'static [&'static str],
) -> SettingItem {
    SettingItem::render(move |_, _, cx: &mut App| {
        setting_row(
            cx,
            label,
            SharedString::from(description),
            read_only(cx, value.clone()),
        )
    })
    .keywords(keywords.iter().copied())
}

/// The settings surface.
///
/// Construct with `cx.new(|cx| SettingsView::new(window, cx))`.
pub struct SettingsView {
    font_size: f32,
    scrollback_lines: usize,
    cursor_blink: bool,
    /// Renders the settings surface with the smaller control size.
    compact: bool,
    /// Reveals the not-yet-wired "Experimental" group.
    show_experimental: bool,
    /// The OS appearance this window opened with, shown as a read-only row.
    system_mode: ThemeMode,
    focus_handle: FocusHandle,
}

impl SettingsView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self {
            font_size: DEFAULT_FONT_SIZE,
            scrollback_lines: DEFAULT_SCROLLBACK,
            cursor_blink: DEFAULT_CURSOR_BLINK,
            compact: false,
            show_experimental: false,
            system_mode: ThemeMode::from(window.appearance()),
            focus_handle: cx.focus_handle(),
        }
    }

    /// The terminal font size held by this view, in pixels.
    pub fn font_size(&self) -> f32 {
        self.font_size
    }

    /// The scrollback line count held by this view.
    pub fn scrollback_lines(&self) -> usize {
        self.scrollback_lines
    }

    /// Whether the cursor is expected to blink.
    pub fn cursor_blink(&self) -> bool {
        self.cursor_blink
    }

    /// Gives the surface keyboard focus. The wiring pass can call this when it
    /// opens the view, mirroring [`crate::terminal::TerminalPane::focus`].
    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_handle.focus(window, cx);
    }

    fn system_label(&self) -> SharedString {
        SharedString::from(if self.system_mode.is_dark() {
            "Dark"
        } else {
            "Light"
        })
    }

    fn appearance_page(&self) -> SettingPage {
        let system = self.system_label();

        SettingPage::new("Appearance").default_open(true).group(
            SettingGroup::new()
                .item(section_item("Appearance", None))
                .item(
                    SettingItem::render(|_, _, cx: &mut App| {
                        let dark = Theme::global(cx).is_dark();
                        let control = div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(
                                Button::new("theme-light")
                                    .label("Light")
                                    .when(dark, |button| button.ghost())
                                    .when(!dark, |button| button.primary())
                                    .on_click(|_, window, cx| {
                                        Theme::change(ThemeMode::Light, Some(window), cx);
                                    }),
                            )
                            .child(
                                Button::new("theme-dark")
                                    .label("Dark")
                                    .when(dark, |button| button.primary())
                                    .when(!dark, |button| button.ghost())
                                    .on_click(|_, window, cx| {
                                        Theme::change(ThemeMode::Dark, Some(window), cx);
                                    }),
                            );
                        setting_row(
                            cx,
                            "App colour theme",
                            SharedString::from(
                                "Applies immediately. Starts from the system appearance.",
                            ),
                            control,
                        )
                    })
                    .keywords([
                        "App colour theme",
                        "theme",
                        "appearance",
                        "light",
                        "dark",
                    ]),
                )
                .item(
                    SettingItem::render(move |_, _, cx: &mut App| {
                        setting_row(
                            cx,
                            "System at launch",
                            SharedString::from(
                                "Read-only: the OS appearance this window opened with.",
                            ),
                            read_only(cx, system.clone()),
                        )
                    })
                    .keywords(["System at launch", "appearance", "system"]),
                ),
        )
    }

    fn terminal_page(&self, cx: &mut Context<Self>) -> SettingPage {
        let view = cx.entity().downgrade();
        let font_view_decrement = view.clone();
        let font_view_increment = view.clone();
        let scroll_view_decrement = view.clone();
        let scroll_view_increment = view.clone();
        let blink_view = view;

        let font_size = self.font_size;
        let font_at_min = font_size <= MIN_FONT_SIZE;
        let font_at_max = font_size >= MAX_FONT_SIZE;
        let scrollback = self.scrollback_lines;
        let scroll_at_min = scrollback <= MIN_SCROLLBACK;
        let scroll_at_max = scrollback >= MAX_SCROLLBACK;
        let blink = self.cursor_blink;
        let size = if self.compact {
            Size::Small
        } else {
            Size::Medium
        };

        let defaults_note = SharedString::from(format!(
            "Saved for this session. Open panes keep the compiled defaults \
             ({DEFAULT_FONT_SIZE} pt, {DEFAULT_SCROLLBACK} lines) until the \
             terminal reads this view.",
        ));

        SettingPage::new("Terminal").group(
            SettingGroup::new()
                .item(section_item("Terminal", Some(defaults_note)))
                .item(
                    SettingItem::render(move |_, _, cx: &mut App| {
                        let decrement_view = font_view_decrement.clone();
                        let increment_view = font_view_increment.clone();
                        setting_row(
                            cx,
                            "Font size",
                            SharedString::from(format!(
                                "Pixels, bounded to {MIN_FONT_SIZE}–{MAX_FONT_SIZE}.",
                            )),
                            stepper(
                                "font-size-decrement",
                                "font-size-increment",
                                SharedString::from(format!("{font_size}")),
                                font_at_min,
                                font_at_max,
                                size,
                                move |_window, cx| {
                                    decrement_view
                                        .update(cx, |this, cx| {
                                            this.font_size = clamp_font_size(
                                                f64::from(this.font_size) - FONT_SIZE_STEP,
                                            );
                                            cx.notify();
                                        })
                                        .ok();
                                },
                                move |_window, cx| {
                                    increment_view
                                        .update(cx, |this, cx| {
                                            this.font_size = clamp_font_size(
                                                f64::from(this.font_size) + FONT_SIZE_STEP,
                                            );
                                            cx.notify();
                                        })
                                        .ok();
                                },
                            ),
                        )
                    })
                    .keywords(["Font size", "font", "terminal", "pixels"]),
                )
                .item(
                    SettingItem::render(move |_, _, cx: &mut App| {
                        let decrement_view = scroll_view_decrement.clone();
                        let increment_view = scroll_view_increment.clone();
                        setting_row(
                            cx,
                            "Scrollback lines",
                            SharedString::from(format!(
                                "Bounded to {MIN_SCROLLBACK}–{MAX_SCROLLBACK} lines. There is \
                                 no unlimited option: unbounded scrollback is the memory cliff \
                                 in docs/BUDGET.md.",
                            )),
                            stepper(
                                "scrollback-decrement",
                                "scrollback-increment",
                                SharedString::from(format!("{scrollback}")),
                                scroll_at_min,
                                scroll_at_max,
                                size,
                                move |_window, cx| {
                                    decrement_view
                                        .update(cx, |this, cx| {
                                            this.scrollback_lines = clamp_scrollback(
                                                this.scrollback_lines as f64 - SCROLLBACK_STEP,
                                            );
                                            cx.notify();
                                        })
                                        .ok();
                                },
                                move |_window, cx| {
                                    increment_view
                                        .update(cx, |this, cx| {
                                            this.scrollback_lines = clamp_scrollback(
                                                this.scrollback_lines as f64 + SCROLLBACK_STEP,
                                            );
                                            cx.notify();
                                        })
                                        .ok();
                                },
                            ),
                        )
                    })
                    .keywords([
                        "Scrollback lines",
                        "scrollback",
                        "buffer",
                        "memory",
                    ]),
                )
                .item(
                    SettingItem::render(move |_, _, cx: &mut App| {
                        let blink_view = blink_view.clone();
                        setting_row(
                            cx,
                            "Cursor blink",
                            SharedString::from(
                                "Recorded here; the terminal pane still draws a steady cursor.",
                            ),
                            toggle(
                                "cursor-blink",
                                "Cursor blink",
                                blink,
                                false,
                                size,
                                move |next, cx| {
                                    blink_view
                                        .update(cx, |this, cx| {
                                            this.cursor_blink = next;
                                            cx.notify();
                                        })
                                        .ok();
                                },
                            ),
                        )
                    })
                    .keywords(["Cursor blink", "cursor", "terminal"]),
                ),
        )
    }

    fn general_page(&self, cx: &mut Context<Self>) -> SettingPage {
        let view = cx.entity().downgrade();
        let compact_view = view.clone();
        let experimental_view = view;
        let compact = self.compact;
        let experimental = self.show_experimental;
        let size = if compact { Size::Small } else { Size::Medium };

        let mut groups = vec![
            SettingGroup::new()
                .item(section_item("Interface", None))
                .item(
                    SettingItem::render(move |_, _, cx: &mut App| {
                        let compact_view = compact_view.clone();
                        setting_row(
                            cx,
                            "Compact rows",
                            SharedString::from(
                                "Draws this settings surface with smaller controls.",
                            ),
                            toggle(
                                "compact-rows",
                                "Compact rows",
                                compact,
                                false,
                                size,
                                move |next, cx| {
                                    compact_view
                                        .update(cx, |this, cx| {
                                            this.compact = next;
                                            cx.notify();
                                        })
                                        .ok();
                                },
                            ),
                        )
                    })
                    .keywords([
                        "Compact rows",
                        "compact",
                        "density",
                        "interface",
                    ]),
                )
                .item(
                    SettingItem::render(move |_, _, cx: &mut App| {
                        let experimental_view = experimental_view.clone();
                        setting_row(
                            cx,
                            "Show experimental settings",
                            SharedString::from(
                                "Reveals settings that exist but are not wired yet.",
                            ),
                            toggle(
                                "show-experimental",
                                "Show experimental settings",
                                experimental,
                                false,
                                size,
                                move |next, cx| {
                                    experimental_view
                                        .update(cx, |this, cx| {
                                            this.show_experimental = next;
                                            cx.notify();
                                        })
                                        .ok();
                                },
                            ),
                        )
                    })
                    .keywords([
                        "Show experimental settings",
                        "experimental",
                        "interface",
                    ]),
                ),
            SettingGroup::new()
                .item(section_item("Paths & versions", None))
                .item(read_only_item(
                    "Host inventory",
                    SharedString::from(HostStore::default_path().display().to_string()),
                    "Where saved hosts live today.",
                    &["Host inventory", "hosts", "path", "inventory"],
                ))
                .item(read_only_item(
                    "Settings store",
                    SharedString::from("in memory (not persisted yet)"),
                    "No settings file is written yet.",
                    &["Settings store", "settings", "file"],
                ))
                .item(read_only_item(
                    "App version",
                    SharedString::from(env!("CARGO_PKG_VERSION")),
                    "sshdeck.",
                    &["App version", "version", "about"],
                )),
        ];

        if self.show_experimental {
            groups.push(
                SettingGroup::new()
                    .item(section_item("Experimental", None))
                    .item(
                        SettingItem::render(move |_, _, cx: &mut App| {
                            setting_row(
                                cx,
                                "Autocomplete",
                                SharedString::from("Not implemented — disabled."),
                                toggle(
                                    "autocomplete",
                                    "Autocomplete",
                                    false,
                                    true,
                                    size,
                                    |_, _| {},
                                ),
                            )
                        })
                        .keywords(["Autocomplete", "experimental"])
                        .disabled(true),
                    ),
            );
        }

        SettingPage::new("General").groups(groups)
    }
}

impl Render for SettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Read the size before borrowing `self` for the page builders.
        let compact = self.compact;
        let appearance = self.appearance_page();
        let terminal = self.terminal_page(cx);
        let general = self.general_page(cx);

        div().size_full().track_focus(&self.focus_handle).child(
            Settings::new("sshdeck-settings")
                .with_group_variant(GroupBoxVariant::Normal)
                .with_size(if compact { Size::Small } else { Size::Medium })
                .pages(vec![appearance, terminal, general]),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn font_size_clamps_into_range() {
        assert_eq!(clamp_font_size(4.0), MIN_FONT_SIZE);
        assert_eq!(clamp_font_size(999.0), MAX_FONT_SIZE);
        assert_eq!(clamp_font_size(13.0), DEFAULT_FONT_SIZE);
        assert_eq!(clamp_font_size(f64::NAN), DEFAULT_FONT_SIZE);
    }

    #[test]
    fn scrollback_is_bounded_and_never_unlimited() {
        assert_eq!(clamp_scrollback(0.0), MIN_SCROLLBACK);
        assert_eq!(clamp_scrollback(-5.0), MIN_SCROLLBACK);
        assert_eq!(clamp_scrollback(500_000.0), MAX_SCROLLBACK);
        assert_eq!(clamp_scrollback(f64::INFINITY), MAX_SCROLLBACK);
        assert_eq!(clamp_scrollback(f64::NAN), DEFAULT_SCROLLBACK);
        assert_eq!(clamp_scrollback(10_000.0), DEFAULT_SCROLLBACK);
    }

    /// The pane offers exactly what `sshdeck-config` enforces — the terminal
    /// clamp and the persisted file both read these constants, so a local
    /// re-declaration with a different range is caught here. The scrollback cap
    /// is the `docs/BUDGET.md` memory guard and must never exceed the config's.
    #[test]
    fn offered_ranges_match_sshdeck_config() {
        assert_eq!(MIN_FONT_SIZE, sshdeck_config::MIN_FONT_SIZE);
        assert_eq!(MAX_FONT_SIZE, sshdeck_config::MAX_FONT_SIZE);
        assert_eq!(DEFAULT_FONT_SIZE, sshdeck_config::DEFAULT_FONT_SIZE);
        assert_eq!(MIN_SCROLLBACK, sshdeck_config::MIN_SCROLLBACK);
        assert_eq!(MAX_SCROLLBACK, sshdeck_config::MAX_SCROLLBACK);
        assert_eq!(DEFAULT_SCROLLBACK, sshdeck_config::DEFAULT_SCROLLBACK);
        assert_eq!(clamp_font_size(0.0), sshdeck_config::MIN_FONT_SIZE);
        assert_eq!(clamp_font_size(1_000.0), sshdeck_config::MAX_FONT_SIZE);
        assert_eq!(clamp_scrollback(0.0), sshdeck_config::MIN_SCROLLBACK);
        assert_eq!(
            clamp_scrollback(1_000_000.0),
            sshdeck_config::MAX_SCROLLBACK
        );
    }
}
