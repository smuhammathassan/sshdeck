//! Mosh support for sshdeck.
//!
//! There is no pure-Rust mosh implementation: the protocol, the SSP handshake
//! and the datagram layer all live in the upstream `mosh-client` /
//! `mosh-server` pair. So this crate is a **supervisor**, not a
//! reimplementation — it finds the system `mosh`, builds the invocation, and
//! drives the process. Nothing here speaks the mosh protocol.
//!
//! Three pieces:
//!
//! * [`MoshBinary`] locates the executable (an explicit path, else `PATH`) and
//!   reads `mosh --version` defensively.
//! * [`MoshInvocation`] builds the argument vector — a `Vec<String>` handed
//!   straight to `exec`, never a shell string — so a host label containing `;`
//!   or a space stays one inert element. [`MoshInvocation::argv`] is public so
//!   a UI can show the command before running it.
//! * [`MoshSession`] supervises the process over a PTY and moves bytes both
//!   ways over bounded `async_channel`s, in the same shape as
//!   `sshdeck_telnet::session` and `sshdeck_core::forward`. No tokio type
//!   appears in a `pub` signature.
//!
//! # Why a PTY
//!
//! `mosh` is a full-screen terminal program, not a filter: upstream
//! `src/frontend/stmclient.cc` opens with `tcgetattr( STDIN_FILENO )` and
//! `exit(1)` if that fails, so pipes cannot work. The PTY is allocated by
//! `portable-pty` (see `Cargo.toml`) because making it the child's controlling
//! terminal — required for `SIGWINCH` on resize — needs `setsid`/`TIOCSCTTY`,
//! which is `unsafe` in `std`.
//!
//! # Deliberately not implemented
//!
//! The mosh state synchronisation, the SSP crypto, and the UDP datagram layer.
//! This crate also ships no SSH bridge: `--ssh=` is caller-supplied (default
//! `ssh`), so the application can point mosh at whatever ssh-compatible command
//! it has, including one that reuses sshdeck's own SSH connection.

mod binary;
mod invocation;
mod session;

pub use binary::{MoshBinary, MoshVersion};
pub use invocation::MoshInvocation;
pub use session::{MoshEvent, MoshExitStatus, MoshSession, MoshTerminalSize};

/// A mosh availability, invocation, or supervision failure.
#[derive(Debug, thiserror::Error)]
pub enum MoshError {
    /// No usable `mosh` binary. The message is written to be shown verbatim.
    #[error("mosh was not found: {0}")]
    NotFound(String),
    /// The invocation would be unsafe or meaningless to run.
    #[error("invalid mosh invocation: {0}")]
    Invalid(String),
    /// The pty, the child process, or a supervisor thread could not be created.
    #[error("could not start mosh: {0}")]
    Spawn(String),
    /// The write queue is full; retry once the caller has drained events.
    #[error("the mosh input queue is full")]
    Backpressure,
    /// The supervisor has stopped.
    #[error("the mosh session is closed")]
    Closed,
}
