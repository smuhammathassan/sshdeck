//! Telnet protocol support for sshdeck.
//!
//! Two layers, deliberately separate:
//!
//! * [`protocol`] is the pure, synchronous option-negotiation state machine. It
//!   parses the `IAC` command stream, strips negotiation out of the data,
//!   tracks per-option local/remote state, and enforces the RFC 854 loop
//!   prevention rules. All the tests live there — no sockets involved.
//! * [`session`] wraps that machine around a blocking `std::net::TcpStream`
//!   driven by `std::thread`s and bounded `async_channel`s, so the socket layer
//!   stays out of `pub` signatures and needs no async runtime.
//!
//! A login client is the target use: accept the server's `ECHO`, reply `WILL`
//! for `SUPPRESS_GO_AHEAD`, `TERMINAL_TYPE` and `WINDOW_SIZE`, and answer
//! `SB TERMINAL_TYPE SEND` with the caller's terminal type.

pub mod protocol;
pub mod session;

pub use protocol::{
    DefaultPolicy, Outcome, ProtocolError, Subnegotiation, TelnetCodec, TelnetPolicy,
};
pub use session::{TelnetError, TelnetEvent, TelnetSession};
