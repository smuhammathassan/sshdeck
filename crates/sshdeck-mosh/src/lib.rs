//! Mosh support for sshdeck.
//!
//! There is no pure-Rust mosh implementation, so this crate drives the system
//! `mosh` client: it constructs the invocation, supervises the process, and
//! moves bytes to and from the terminal grid. Argument construction and the
//! protocol message types are pure and unit tested.
