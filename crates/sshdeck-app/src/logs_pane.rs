//! Logs pane.
//!
//! Contract for the wiring pass (do not change without updating `main.rs`):
//! `LogsPane::new(window, cx) -> Self`, plus `impl Render for LogsPane`.
//! The root view constructs it with `cx.new(|cx| LogsPane::new(window, cx))`.
