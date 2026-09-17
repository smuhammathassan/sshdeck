//! Command palette.
//!
//! Contract for the wiring pass (do not change without updating `main.rs`):
//! `PaletteView::new(window, cx) -> Self`, plus `impl Render for PaletteView`.
//! The root view constructs it with `cx.new(|cx| PaletteView::new(window, cx))`.
