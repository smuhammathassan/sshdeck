//! Terminal emulation for sshdeck.
//!
//! Owns the character grid: bytes from a channel go in, a renderable screen
//! comes out. No UI and no transport dependency, so it can be tested without a
//! window or a network.
