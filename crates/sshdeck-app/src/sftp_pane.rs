//! SFTP file browser pane.
//!
//! Contract for the wiring pass (do not change without updating `main.rs`):
//! `SftpPane::new(window, cx) -> Self`, plus `impl Render for SftpPane`.
//! The root view constructs it with `cx.new(|cx| SftpPane::new(window, cx))`.
