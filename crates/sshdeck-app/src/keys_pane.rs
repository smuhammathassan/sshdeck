//! SSH keys and known-hosts management UI.
//!
//! Contract for the wiring pass (do not change without updating `main.rs`):
//! `KeysPane::new(window, cx) -> Self`, plus `impl Render for KeysPane`.
//! The root view constructs it with `cx.new(|cx| KeysPane::new(window, cx))`.
