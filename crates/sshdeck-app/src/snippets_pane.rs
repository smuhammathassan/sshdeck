//! Snippets pane.
//!
//! Contract for the wiring pass (do not change without updating `main.rs`):
//! `SnippetsPane::new(window, cx) -> Self`, plus `impl Render for SnippetsPane`.
//! The root view constructs it with `cx.new(|cx| SnippetsPane::new(window, cx))`.
