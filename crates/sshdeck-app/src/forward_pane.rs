//! Port forwarding UI.
//!
//! Contract for the wiring pass (do not change without updating `main.rs`):
//! `ForwardPane::new(window, cx) -> Self`, plus `impl Render for ForwardPane`.
//! The root view constructs it with `cx.new(|cx| ForwardPane::new(window, cx))`.
