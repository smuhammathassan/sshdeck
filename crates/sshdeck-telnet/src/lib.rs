//! Telnet protocol support for sshdeck.
//!
//! The option-negotiation state machine is pure logic and is unit tested; the
//! socket layer stays out of `pub` signatures so this crate is testable
//! headlessly.
