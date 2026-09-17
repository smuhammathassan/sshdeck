//! Pure, synchronous Telnet option negotiation.
//!
//! No I/O, no threads, no timers. [`TelnetCodec`] takes the bytes read from the
//! socket and returns the plain payload to show the user, the negotiation
//! replies to write back, and any subnegotiation it understood. Everything here
//! is exercised by `#[test]`s without a network.
//!
//! Loop prevention follows RFC 854's rule that a negotiation for an option
//! already in the requested state is never acknowledged, so two peers cannot
//! answer each other forever; a refusal (`DONT`/`WONT`) is only sent in reply to
//! a `DO`/`WILL`, never to a `DONT`/`WONT`.
//!
//! `ponytail:` the four-state Q method of RFC 1143 (`WANTYES`/`WANTNO` pending
//! states) is collapsed to two states per side because this codec records an
//! option's state the moment it replies, so a duplicate request lands in the
//! "already in state" branch. Ceiling: a simultaneous `WILL`/`DO` pair is
//! resolved optimistically instead of queued. Upgrade path: add a per-option
//! pending flag if a real server is ever seen to mis-order the exchange.

/// Interpret As Command: escapes every Telnet control sequence.
pub const IAC: u8 = 255;
/// End of subnegotiation.
pub const SE: u8 = 240;
/// Start of subnegotiation.
pub const SB: u8 = 250;
/// "I want to enable this option on my side."
pub const WILL: u8 = 251;
/// "I refuse to enable this option on my side."
pub const WONT: u8 = 252;
/// "Please enable this option on your side."
pub const DO: u8 = 253;
/// "Please disable this option on your side."
pub const DONT: u8 = 254;

/// Echo (RFC 857).
pub const ECHO: u8 = 1;
/// Suppress Go Ahead (RFC 858).
pub const SUPPRESS_GO_AHEAD: u8 = 3;
/// Terminal Type (RFC 1091).
pub const TERMINAL_TYPE: u8 = 24;
/// Window Size / NAWS (RFC 1073).
pub const WINDOW_SIZE: u8 = 31;

/// Terminal-Type subnegotiation verb asking the peer to send its type.
pub const SEND: u8 = 1;
/// Terminal-Type subnegotiation verb carrying a type name.
pub const IS: u8 = 0;

/// The terminal type a login session reports unless the caller overrides it.
pub const DEFAULT_TERMINAL_TYPE: &str = "xterm-256color";

/// Caps a subnegotiation so a peer that never sends `IAC SE` cannot grow our
/// buffer without bound. 4 KiB is far above any real option payload.
const MAX_SUBNEGOTIATION: usize = 4096;

/// A malformed or unsupported command in the byte stream.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum ProtocolError {
    /// A byte followed `IAC` but is not a command we know.
    #[error("unknown telnet command {0}")]
    UnknownCommand(u8),
    /// `IAC SE` arrived outside a subnegotiation.
    #[error("unexpected IAC SE outside a subnegotiation")]
    UnexpectedSe,
    /// Inside `SB`, `IAC` was followed by something other than `SE` or `IAC`.
    #[error("subnegotiation for option {option} was interrupted by IAC {command}")]
    MalformedSubnegotiation { option: u8, command: u8 },
    /// A subnegotiation exceeded [`MAX_SUBNEGOTIATION`].
    #[error("subnegotiation is too large ({0} bytes)")]
    SubnegotiationTooLarge(usize),
}

/// A subnegotiation we know how to read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Subnegotiation {
    /// `SB TERMINAL_TYPE SEND`: the peer wants our terminal type, so a real
    /// login flow answers with [`terminal_type`].
    TerminalTypeRequest,
    /// `SB TERMINAL_TYPE IS <name>`: the peer reported its terminal type.
    TerminalType(String),
    /// `SB WINDOW_SIZE <cols> <rows>`: two 16-bit big-endian values.
    WindowSize { cols: u16, rows: u16 },
    /// Any other option, with the raw payload after unescaping `IAC IAC`.
    Unknown { option: u8, payload: Vec<u8> },
}

/// What one call to [`TelnetCodec::receive`] produced.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Outcome {
    data: Vec<u8>,
    replies: Vec<u8>,
    subnegotiations: Vec<Subnegotiation>,
}

impl Outcome {
    /// Plain payload bytes, in the order they arrived, with every negotiation
    /// stripped out.
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// Wire bytes to write back to the peer (`IAC DO/WILL/DONT/WONT …`).
    pub fn replies(&self) -> &[u8] {
        &self.replies
    }

    /// Subnegotiations received from the peer.
    pub fn subnegotiations(&self) -> &[Subnegotiation] {
        &self.subnegotiations
    }

    /// True when the call decoded nothing worth acting on (typical for a
    /// command split across two reads).
    pub fn is_empty(&self) -> bool {
        self.data.is_empty() && self.replies.is_empty() && self.subnegotiations.is_empty()
    }
}

/// The caller-supplied decision hook: which options this end accepts.
///
/// The codec asks before answering a negotiation, so a caller can keep the
/// default login-client behaviour or restrict it. `Send` because one codec
/// lives on the connection's read thread.
pub trait TelnetPolicy: Send {
    /// May the peer enable `option` on its side? Answered to `WILL` with `DO`
    /// when true and `DONT` when false.
    fn allow_remote(&self, option: u8) -> bool;

    /// May we enable `option` on our side? Answered to `DO` with `WILL` when
    /// true and `WONT` when false.
    fn allow_local(&self, option: u8) -> bool;
}

/// The options a plain login client wants: let the server echo, we suppress go
/// ahead, and we report terminal type and window size when asked.
#[derive(Debug, Default, Clone, Copy)]
pub struct DefaultPolicy;

impl TelnetPolicy for DefaultPolicy {
    fn allow_remote(&self, option: u8) -> bool {
        matches!(option, ECHO | SUPPRESS_GO_AHEAD)
    }

    fn allow_local(&self, option: u8) -> bool {
        matches!(option, SUPPRESS_GO_AHEAD | TERMINAL_TYPE | WINDOW_SIZE)
    }
}

/// A `WILL`/`DO`/`WONT`/`DONT` frame for one option.
pub fn frame(command: u8, option: u8) -> [u8; 3] {
    [IAC, command, option]
}

/// Wraps `payload` in `IAC SB <option> … IAC SE`, escaping any literal `IAC`.
pub fn subnegotiate(option: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 5);
    out.extend_from_slice(&[IAC, SB, option]);
    for &byte in payload {
        if byte == IAC {
            out.push(IAC);
        }
        out.push(byte);
    }
    out.extend_from_slice(&[IAC, SE]);
    out
}

/// The `SB WINDOW_SIZE` frame for a terminal size (RFC 1073, NAWS).
pub fn naws(cols: u16, rows: u16) -> Vec<u8> {
    let mut payload = [0u8; 4];
    payload[..2].copy_from_slice(&cols.to_be_bytes());
    payload[2..].copy_from_slice(&rows.to_be_bytes());
    subnegotiate(WINDOW_SIZE, &payload)
}

/// The `SB TERMINAL_TYPE IS <name>` frame (RFC 1091).
pub fn terminal_type(name: &str) -> Vec<u8> {
    let mut payload = Vec::with_capacity(name.len() + 1);
    payload.push(IS);
    payload.extend_from_slice(name.as_bytes());
    subnegotiate(TERMINAL_TYPE, &payload)
}

/// The negotiation state machine plus a byte-stream decoder.
///
/// Feed it whatever the socket returned; it buffers a command split across
/// reads and never panics on malformed input.
pub struct TelnetCodec {
    policy: Box<dyn TelnetPolicy + Send>,
    local: [bool; 256],
    remote: [bool; 256],
    pending: Vec<u8>,
}

impl TelnetCodec {
    /// Builds a codec that consults `policy` for every negotiation.
    pub fn new(policy: Box<dyn TelnetPolicy + Send>) -> Self {
        Self {
            policy,
            local: [false; 256],
            remote: [false; 256],
            pending: Vec::new(),
        }
    }

    /// Has `option` been enabled on our side (we answered `WILL`)?
    pub fn local_enabled(&self, option: u8) -> bool {
        self.local[usize::from(option)]
    }

    /// Has `option` been enabled on the peer's side (we answered `DO`)?
    pub fn remote_enabled(&self, option: u8) -> bool {
        self.remote[usize::from(option)]
    }

    /// Decodes one chunk of the peer's byte stream.
    ///
    /// A command cut off at the end of `bytes` is kept for the next call and
    /// reported as an empty [`Outcome`]; genuinely malformed input returns a
    /// [`ProtocolError`] and clears the buffer, since the stream is then
    /// desynchronised.
    pub fn receive(&mut self, bytes: &[u8]) -> Result<Outcome, ProtocolError> {
        let mut input = std::mem::take(&mut self.pending);
        input.extend_from_slice(bytes);

        let mut outcome = Outcome::default();
        let mut index = 0usize;
        while index < input.len() {
            if input[index] != IAC {
                let start = index;
                while index < input.len() && input[index] != IAC {
                    index += 1;
                }
                outcome.data.extend_from_slice(&input[start..index]);
                continue;
            }

            // `input[index] == IAC`; the command byte may not have arrived yet.
            let Some(&command) = input.get(index + 1) else {
                break;
            };
            match command {
                // IAC IAC is a literal 0xFF in the data stream.
                IAC => {
                    outcome.data.push(IAC);
                    index += 2;
                }
                DO | DONT | WILL | WONT => {
                    let Some(&option) = input.get(index + 2) else {
                        break;
                    };
                    self.negotiate(command, option, &mut outcome.replies);
                    index += 3;
                }
                SB => {
                    let Some(&option) = input.get(index + 2) else {
                        break;
                    };
                    let mut cursor = index + 3;
                    let mut payload = Vec::new();
                    let mut complete = false;
                    while cursor < input.len() {
                        if input[cursor] == IAC {
                            let Some(&next) = input.get(cursor + 1) else {
                                break;
                            };
                            if next == SE {
                                cursor += 2;
                                complete = true;
                                break;
                            }
                            if next != IAC {
                                return Err(ProtocolError::MalformedSubnegotiation {
                                    option,
                                    command: next,
                                });
                            }
                        }
                        payload.push(input[cursor]);
                        if payload.len() > MAX_SUBNEGOTIATION {
                            return Err(ProtocolError::SubnegotiationTooLarge(payload.len()));
                        }
                        cursor += if input[cursor] == IAC { 2 } else { 1 };
                    }
                    if !complete {
                        break;
                    }
                    outcome
                        .subnegotiations
                        .push(parse_subnegotiation(option, &payload));
                    index = cursor;
                }
                // SE only belongs inside a subnegotiation.
                SE => return Err(ProtocolError::UnexpectedSe),
                // The two-byte commands that carry no option byte (NOP, DM,
                // BRK, IP, AO, AYT, EC, EL, GA) are ignored.
                241..=249 => index += 2,
                other => return Err(ProtocolError::UnknownCommand(other)),
            }
        }

        // `index` only ever advances past fully-decoded input, so the tail is
        // exactly the command still waiting for more bytes.
        self.pending = input[index..].to_vec();
        Ok(outcome)
    }

    /// Applies one negotiation, appending any reply to `replies`.
    ///
    /// RFC 854 loop prevention: an option already in the requested state is not
    /// acknowledged, and `DO`/`WILL` are only ever sent to move an option from
    /// disabled to enabled. `DONT`/`WONT` are requests to disable, never
    /// answered with another `DONT`/`WONT`.
    fn negotiate(&mut self, command: u8, option: u8, replies: &mut Vec<u8>) {
        let index = usize::from(option);
        match command {
            WILL => {
                if self.remote[index] {
                    return;
                }
                if self.policy.allow_remote(option) {
                    self.remote[index] = true;
                    replies.extend_from_slice(&frame(DO, option));
                } else {
                    replies.extend_from_slice(&frame(DONT, option));
                }
            }
            DO => {
                if self.local[index] {
                    return;
                }
                if self.policy.allow_local(option) {
                    self.local[index] = true;
                    replies.extend_from_slice(&frame(WILL, option));
                } else {
                    replies.extend_from_slice(&frame(WONT, option));
                }
            }
            // A disable request needs no reply and is idempotent; RFC 854
            // never acknowledges a `DONT`/`WONT`.
            WONT => self.remote[index] = false,
            DONT => self.local[index] = false,
            _ => {}
        }
    }
}

/// Reads the options inside a completed `SB … SE` command.
fn parse_subnegotiation(option: u8, payload: &[u8]) -> Subnegotiation {
    match option {
        TERMINAL_TYPE => match payload.first() {
            Some(&SEND) => Subnegotiation::TerminalTypeRequest,
            Some(&IS) => {
                Subnegotiation::TerminalType(String::from_utf8_lossy(&payload[1..]).into_owned())
            }
            _ => Subnegotiation::Unknown {
                option,
                payload: payload.to_vec(),
            },
        },
        WINDOW_SIZE if payload.len() == 4 => Subnegotiation::WindowSize {
            cols: u16::from_be_bytes([payload[0], payload[1]]),
            rows: u16::from_be_bytes([payload[2], payload[3]]),
        },
        _ => Subnegotiation::Unknown {
            option,
            payload: payload.to_vec(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct AllowAll;

    impl TelnetPolicy for AllowAll {
        fn allow_remote(&self, _option: u8) -> bool {
            true
        }

        fn allow_local(&self, _option: u8) -> bool {
            true
        }
    }

    struct DenyAll;

    impl TelnetPolicy for DenyAll {
        fn allow_remote(&self, _option: u8) -> bool {
            false
        }

        fn allow_local(&self, _option: u8) -> bool {
            false
        }
    }

    fn codec(policy: impl TelnetPolicy + 'static) -> TelnetCodec {
        TelnetCodec::new(Box::new(policy))
    }

    #[test]
    fn iac_iac_is_a_literal_ff() {
        let mut codec = codec(AllowAll);
        let outcome = codec.receive(&[b'a', IAC, IAC, b'b']).expect("decodes");
        assert_eq!(outcome.data(), &[b'a', 0xff, b'b']);
        assert!(outcome.replies().is_empty());
    }

    #[test]
    fn repeat_negotiation_is_not_answered() {
        // DO TERMINAL_TYPE is accepted once...
        let mut codec = codec(AllowAll);
        let first = codec.receive(&frame(DO, TERMINAL_TYPE)).expect("decodes");
        assert_eq!(first.replies(), &frame(WILL, TERMINAL_TYPE));
        assert!(codec.local_enabled(TERMINAL_TYPE));

        // ...and the identical request changes no state, so it is ignored.
        let again = codec.receive(&frame(DO, TERMINAL_TYPE)).expect("decodes");
        assert!(again.replies().is_empty());
    }

    #[test]
    fn repeated_will_for_agreed_option_is_not_answered() {
        let mut codec = codec(AllowAll);
        let first = codec.receive(&frame(WILL, ECHO)).expect("decodes");
        assert_eq!(first.replies(), &frame(DO, ECHO));
        assert!(codec.remote_enabled(ECHO));

        let again = codec.receive(&frame(WILL, ECHO)).expect("decodes");
        assert!(again.replies().is_empty());
    }

    #[test]
    fn will_echo_follows_the_supplied_policy() {
        let mut accepting = codec(AllowAll);
        let outcome = accepting.receive(&frame(WILL, ECHO)).expect("decodes");
        assert_eq!(outcome.replies(), &frame(DO, ECHO));
        assert!(accepting.remote_enabled(ECHO));

        let mut refusing = codec(DenyAll);
        let outcome = refusing.receive(&frame(WILL, ECHO)).expect("decodes");
        assert_eq!(outcome.replies(), &frame(DONT, ECHO));
        assert!(!refusing.remote_enabled(ECHO));
    }

    #[test]
    fn default_policy_refuses_local_echo_but_accepts_a_server_echo() {
        let mut codec = TelnetCodec::new(Box::new(DefaultPolicy));

        let outcome = codec.receive(&frame(DO, ECHO)).expect("decodes");
        assert_eq!(outcome.replies(), &frame(WONT, ECHO));
        assert!(!codec.local_enabled(ECHO));

        let outcome = codec.receive(&frame(WILL, ECHO)).expect("decodes");
        assert_eq!(outcome.replies(), &frame(DO, ECHO));
        assert!(codec.remote_enabled(ECHO));
    }

    #[test]
    fn disable_requests_change_state_without_a_reply() {
        let mut codec = codec(AllowAll);
        codec.receive(&frame(WILL, ECHO)).expect("decodes");
        codec.receive(&frame(DO, TERMINAL_TYPE)).expect("decodes");

        let outcome = codec.receive(&frame(WONT, ECHO)).expect("decodes");
        assert!(outcome.replies().is_empty());
        assert!(!codec.remote_enabled(ECHO));

        let outcome = codec.receive(&frame(DONT, TERMINAL_TYPE)).expect("decodes");
        assert!(outcome.replies().is_empty());
        assert!(!codec.local_enabled(TERMINAL_TYPE));
    }

    #[test]
    fn terminal_type_is_parsed() {
        let mut codec = codec(AllowAll);
        let mut wire = vec![IAC, SB, TERMINAL_TYPE, IS];
        wire.extend_from_slice(b"xterm-256color");
        wire.extend_from_slice(&[IAC, SE]);

        let outcome = codec.receive(&wire).expect("decodes");
        assert_eq!(
            outcome.subnegotiations(),
            &[Subnegotiation::TerminalType("xterm-256color".into())]
        );
        assert!(outcome.data().is_empty());
        assert!(outcome.replies().is_empty());
    }

    #[test]
    fn terminal_type_request_is_recognised() {
        let mut codec = codec(AllowAll);
        let wire = [IAC, SB, TERMINAL_TYPE, SEND, IAC, SE];
        let outcome = codec.receive(&wire).expect("decodes");
        assert_eq!(
            outcome.subnegotiations(),
            &[Subnegotiation::TerminalTypeRequest]
        );
    }

    #[test]
    fn naws_parses_columns_and_rows() {
        let mut codec = codec(AllowAll);
        // cols = 80 (0x0050), rows = 24 (0x0018).
        let wire = [IAC, SB, WINDOW_SIZE, 0x00, 0x50, 0x00, 0x18, IAC, SE];
        let outcome = codec.receive(&wire).expect("decodes");
        assert_eq!(
            outcome.subnegotiations(),
            &[Subnegotiation::WindowSize { cols: 80, rows: 24 }]
        );
    }

    #[test]
    fn escaped_iac_inside_a_subnegotiation_is_a_literal() {
        let mut codec = codec(AllowAll);
        // IAC IAC twice, then IAC SE: two literal 0xFF bytes of payload.
        let wire = [IAC, SB, WINDOW_SIZE, IAC, IAC, IAC, IAC, IAC, SE];
        let outcome = codec.receive(&wire).expect("decodes");
        assert_eq!(
            outcome.subnegotiations(),
            &[Subnegotiation::Unknown {
                option: WINDOW_SIZE,
                payload: vec![IAC, IAC],
            }]
        );
    }

    #[test]
    fn truncated_subnegotiation_buffers_without_panicking() {
        let mut codec = codec(AllowAll);

        // Ends mid-subnegotiation: no IAC SE has arrived yet.
        let outcome = codec
            .receive(&[IAC, SB, TERMINAL_TYPE, IS, b'x'])
            .expect("buffers");
        assert!(outcome.is_empty());

        // This chunk stops on a bare IAC, which is also incomplete.
        let outcome = codec
            .receive(&[b't', b'e', b'r', b'm', IAC])
            .expect("buffers");
        assert!(outcome.is_empty());

        // The final byte completes the command.
        let outcome = codec.receive(&[SE]).expect("decodes");
        assert_eq!(
            outcome.subnegotiations(),
            &[Subnegotiation::TerminalType("xterm".into())]
        );
    }

    #[test]
    fn data_interleaved_with_negotiation_is_preserved() {
        let mut codec = codec(AllowAll);
        let mut wire = Vec::new();
        wire.extend_from_slice(b"login: ");
        wire.extend_from_slice(&frame(DO, TERMINAL_TYPE));
        wire.extend_from_slice(b"user");
        wire.extend_from_slice(&[IAC, IAC]);
        wire.extend_from_slice(&frame(WILL, ECHO));
        wire.extend_from_slice(&[b' ', IAC, IAC, b'\n']);

        let outcome = codec.receive(&wire).expect("decodes");
        assert_eq!(outcome.data(), b"login: user\xff \xff\n");
        assert_eq!(
            outcome.replies(),
            &[IAC, WILL, TERMINAL_TYPE, IAC, DO, ECHO]
        );
    }

    #[test]
    fn resize_emits_a_well_formed_naws_subnegotiation() {
        assert_eq!(
            naws(80, 24).as_slice(),
            &[IAC, SB, WINDOW_SIZE, 0, 80, 0, 24, IAC, SE]
        );

        // The parser reads back exactly what the builder wrote.
        let mut codec = codec(AllowAll);
        let outcome = codec.receive(&naws(132, 43)).expect("decodes");
        assert_eq!(
            outcome.subnegotiations(),
            &[Subnegotiation::WindowSize {
                cols: 132,
                rows: 43,
            }]
        );
    }

    #[test]
    fn terminal_type_frame_is_well_formed() {
        assert_eq!(
            terminal_type("xterm").as_slice(),
            &[
                IAC,
                SB,
                TERMINAL_TYPE,
                IS,
                b'x',
                b't',
                b'e',
                b'r',
                b'm',
                IAC,
                SE
            ]
        );
    }

    #[test]
    fn malformed_commands_are_errors_not_panics() {
        let mut codec = codec(AllowAll);
        assert!(codec.receive(&[IAC, 200]).is_err());
        assert!(codec.receive(&[IAC, SE]).is_err());
        assert!(codec.receive(&[IAC, SB, WINDOW_SIZE, IAC, 7]).is_err());
    }
}
