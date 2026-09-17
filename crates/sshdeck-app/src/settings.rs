//! The settings surface: a grouped, searchable view over the app's tunables.
//!
//! Built on gpui-kit's `setting` module. That component owns its own keyed
//! state (search box, page selection, field widgets), so this view stores only
//! the values themselves and rebuilds the fields from them on every render.
//! A field's setter runs on `&mut App`, not on this view's context, so each
//! setter captures a weak handle to the view and notifies it explicitly.
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
    setting::{NumberFieldOptions, SettingField, SettingGroup, SettingItem, SettingPage, Settings},
    ActiveTheme as _, Sizable as _, Size, Theme, ThemeMode,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::InteractiveElement as _;
use gpui_kit::{
    div, App, Context, FocusHandle, IntoElement, ParentElement as _, Render, SharedString,
    Styled as _, Window,
};
use sshdeck_core::HostStore;

/// Terminal font size bounds, in pixels. The floor keeps text legible; the
/// ceiling keeps a single line from swallowing the pane.
const MIN_FONT_SIZE: f64 = 8.0;
const MAX_FONT_SIZE: f64 = 32.0;
/// Default font size. Matches `terminal::FONT_SIZE`; keep the two in step when
/// the pane starts reading this view.
const DEFAULT_FONT_SIZE: f32 = 13.0;

/// Scrollback bounds. `docs/BUDGET.md` caps scrollback as the memory guard, so
/// there is deliberately no "unlimited" value: a huge or non-finite request
/// ends at [`MAX_SCROLLBACK`].
const MIN_SCROLLBACK: usize = 100;
const MAX_SCROLLBACK: usize = 100_000;
/// Default scrollback. Matches `terminal::SCROLLBACK_LINES`.
const DEFAULT_SCROLLBACK: usize = 10_000;

/// Cursor blink is the one accepted timer in the terminal (`docs/BUDGET.md`);
/// on by default, suppressed while the window is unfocused.
const DEFAULT_CURSOR_BLINK: bool = true;

/// Clamps a font size in pixels into the supported range.
///
/// `NaN` (a cleared number input can produce it) falls back to the default
/// rather than becoming a zero-sized font.
fn clamp_font_size(value: f64) -> f32 {
    if value.is_nan() {
        return DEFAULT_FONT_SIZE;
    }
    value.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE) as f32
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

/// A read-only value rendered on the right of a setting row.
fn read_only_field(value: SharedString) -> SettingField<SharedString> {
    SettingField::render(move |_, _, cx: &mut App| {
        div()
            .text_sm()
            .text_color(cx.theme().muted_foreground)
            .child(value.clone())
    })
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
        SettingPage::new("Appearance").default_open(true).group(
            SettingGroup::new()
                .title("Theme")
                .item(
                    SettingItem::new(
                        "App colour theme",
                        SettingField::render(|_, _, cx: &mut App| {
                            let dark = Theme::global(cx).is_dark();
                            div()
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
                                )
                        }),
                    )
                    .description("Applies immediately. Starts from the system appearance."),
                )
                .item(
                    SettingItem::new("System at launch", read_only_field(self.system_label()))
                        .description("Read-only: the OS appearance this window opened with."),
                ),
        )
    }

    fn terminal_page(&self, cx: &mut Context<Self>) -> SettingPage {
        let view = cx.entity().downgrade();
        let font_view = view.clone();
        let scrollback_view = view.clone();
        let blink_view = view;
        let font_size = f64::from(self.font_size);
        let scrollback = self.scrollback_lines as f64;
        let blink = self.cursor_blink;

        SettingPage::new("Terminal").group(
            SettingGroup::new()
                .title("Defaults")
                .description(
                    "Saved for this session. Open panes keep the compiled defaults \
                     (13 pt, 10 000 lines) until the terminal reads this view.",
                )
                .items(vec![
                    SettingItem::new(
                        "Font size",
                        SettingField::number_input(
                            NumberFieldOptions {
                                min: MIN_FONT_SIZE,
                                max: MAX_FONT_SIZE,
                                step: 1.0,
                            },
                            move |_cx: &App| font_size,
                            move |value: f64, cx: &mut App| {
                                font_view
                                    .update(cx, |this, cx| {
                                        this.font_size = clamp_font_size(value);
                                        cx.notify();
                                    })
                                    .ok();
                            },
                        )
                        .default_value(f64::from(DEFAULT_FONT_SIZE)),
                    )
                    .description("Pixels, bounded to 8–32."),
                    SettingItem::new(
                        "Scrollback lines",
                        SettingField::number_input(
                            NumberFieldOptions {
                                min: MIN_SCROLLBACK as f64,
                                max: MAX_SCROLLBACK as f64,
                                step: 1_000.0,
                            },
                            move |_cx: &App| scrollback,
                            move |value: f64, cx: &mut App| {
                                scrollback_view
                                    .update(cx, |this, cx| {
                                        this.scrollback_lines = clamp_scrollback(value);
                                        cx.notify();
                                    })
                                    .ok();
                            },
                        )
                        .default_value(DEFAULT_SCROLLBACK as f64),
                    )
                    .description(
                        "Bounded to 100–100 000 lines. There is no unlimited option: \
                         unbounded scrollback is the memory cliff in docs/BUDGET.md.",
                    ),
                    SettingItem::new(
                        "Cursor blink",
                        SettingField::switch(
                            move |_cx: &App| blink,
                            move |value: bool, cx: &mut App| {
                                blink_view
                                    .update(cx, |this, cx| {
                                        this.cursor_blink = value;
                                        cx.notify();
                                    })
                                    .ok();
                            },
                        )
                        .default_value(DEFAULT_CURSOR_BLINK),
                    )
                    .description("Recorded here; the terminal pane still draws a steady cursor."),
                ]),
        )
    }

    fn general_page(&self, cx: &mut Context<Self>) -> SettingPage {
        let view = cx.entity().downgrade();
        let compact_view = view.clone();
        let experimental_view = view;
        let compact = self.compact;
        let experimental = self.show_experimental;

        let mut groups = vec![
            SettingGroup::new().title("Interface").items(vec![
                SettingItem::new(
                    "Compact rows",
                    SettingField::switch(
                        move |_cx: &App| compact,
                        move |value: bool, cx: &mut App| {
                            compact_view
                                .update(cx, |this, cx| {
                                    this.compact = value;
                                    cx.notify();
                                })
                                .ok();
                        },
                    )
                    .default_value(false),
                )
                .description("Draws this settings surface with smaller controls."),
                SettingItem::new(
                    "Show experimental settings",
                    SettingField::switch(
                        move |_cx: &App| experimental,
                        move |value: bool, cx: &mut App| {
                            experimental_view
                                .update(cx, |this, cx| {
                                    this.show_experimental = value;
                                    cx.notify();
                                })
                                .ok();
                        },
                    )
                    .default_value(false),
                )
                .description("Reveals settings that exist but are not wired yet."),
            ]),
            SettingGroup::new().title("Paths & versions").items(vec![
                SettingItem::new(
                    "Host inventory",
                    read_only_field(SharedString::from(
                        HostStore::default_path().display().to_string(),
                    )),
                )
                .description("Where saved hosts live today."),
                SettingItem::new(
                    "Settings store",
                    read_only_field(SharedString::from("in memory (not persisted yet)")),
                )
                .description("No settings file is written yet."),
                SettingItem::new(
                    "App version",
                    read_only_field(SharedString::from(env!("CARGO_PKG_VERSION"))),
                )
                .description("sshdeck."),
            ]),
        ];

        if self.show_experimental {
            groups.push(
                SettingGroup::new().title("Experimental").item(
                    SettingItem::new(
                        "Autocomplete",
                        SettingField::switch(|_cx: &App| false, |_: bool, _: &mut App| {}),
                    )
                    .description("Not implemented — disabled.")
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
                .with_group_variant(GroupBoxVariant::Outline)
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
        assert_eq!(clamp_font_size(4.0), 8.0);
        assert_eq!(clamp_font_size(999.0), 32.0);
        assert_eq!(clamp_font_size(13.0), 13.0);
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
}
