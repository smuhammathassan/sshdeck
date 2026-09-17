//! Settings surface.
//!
//! Contract for the wiring pass (do not change without updating `main.rs`):
//! `SettingsView::new(window, cx) -> Self`, plus `impl Render for SettingsView`.
//! The root view constructs it with `cx.new(|cx| SettingsView::new(window, cx))`.
