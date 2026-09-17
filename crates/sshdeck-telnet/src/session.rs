//! Blocking Telnet transport.
//!
//! Telnet needs no async runtime: the socket is a plain [`std::net::TcpStream`].
//! One [`std::thread`] owns it for writing and a second drains it for reading,
//! and the only things crossing a thread boundary are plain data over bounded
//! `async_channel`s. The caller gets an opaque [`TelnetSession`] handle plus a
//! [`TelnetEvent`] stream, mirroring `sshdeck_core::session`'s shape; no tokio
//! type appears in a `pub` signature.
//!
//! `ponytail:` two threads per connection instead of SSH's runtime-driven
//! select, because a blocking read cannot also wait on a command channel with
//! only `std`. Upgrade path: one thread plus `mio`/`poll` if connection counts
//! ever justify it.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};

use async_channel::{Receiver, Sender, TrySendError};

use crate::protocol::{
    self, DefaultPolicy, Subnegotiation, TelnetCodec, TelnetPolicy, DEFAULT_TERMINAL_TYPE,
};

/// Commands from the caller to the connection thread.
enum Command {
    Write(Vec<u8>),
    Resize { cols: u16, rows: u16 },
    Close,
}

/// Everything the caller sees from a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TelnetEvent {
    /// The TCP connection is up and negotiation may begin.
    Connected,
    /// Plain payload bytes from the server, with negotiation stripped out.
    Data(Vec<u8>),
    /// The server closed the connection (or we did).
    Closed,
    /// A failure that ended the session.
    Error(String),
}

/// A connection failure.
#[derive(Debug, thiserror::Error)]
pub enum TelnetError {
    #[error("connection failed: {0}")]
    Connect(String),
    #[error("input queue is full")]
    Backpressure,
    #[error("telnet session is closed")]
    Closed,
}

/// A live (or connecting) Telnet connection.
pub struct TelnetSession {
    commands: Sender<Command>,
    events: Receiver<TelnetEvent>,
}

/// What the connection thread needs to open the socket.
struct Config {
    host: String,
    port: u16,
    policy: Box<dyn TelnetPolicy + Send>,
    terminal_type: String,
}

impl TelnetSession {
    /// Spawns a connection thread and returns immediately. Progress and failure
    /// arrive as [`TelnetEvent`]s on [`TelnetSession::events`].
    pub fn connect(host: &str, port: u16) -> Result<Self, TelnetError> {
        Self::connect_with(host, port, Box::new(DefaultPolicy), DEFAULT_TERMINAL_TYPE)
    }

    /// [`Self::connect`] with a caller-supplied negotiation policy and the
    /// terminal type reported when the server sends `SB TERMINAL_TYPE SEND`.
    pub fn connect_with(
        host: &str,
        port: u16,
        policy: Box<dyn TelnetPolicy + Send>,
        terminal_type: impl Into<String>,
    ) -> Result<Self, TelnetError> {
        if host.trim().is_empty() {
            return Err(TelnetError::Connect("host address is empty".into()));
        }
        let config = Config {
            host: host.to_string(),
            port,
            policy,
            terminal_type: terminal_type.into(),
        };

        // ponytail: bounded both ways so a fast server cannot balloon memory.
        // 64 queued writes is far more than a human generates; 256 chunks of
        // buffered output is a screenful or so. When the event channel fills,
        // the read loop parks and TCP backpressure engages. Upgrade path: raise
        // the event capacity if the UI ever lags by more than a few frames.
        let (commands, command_rx) = async_channel::bounded(64);
        let (event_tx, events) = async_channel::bounded(256);

        std::thread::Builder::new()
            .name(format!("telnet-{host}"))
            .spawn(move || run(config, command_rx, event_tx))
            .map_err(|err| {
                TelnetError::Connect(format!("could not spawn the telnet thread: {err}"))
            })?;

        Ok(Self { commands, events })
    }

    /// Sends input to the remote login. Non-blocking: returns
    /// [`TelnetError::Backpressure`] rather than stalling the UI thread.
    pub fn write(&self, data: &[u8]) -> Result<(), TelnetError> {
        self.send(Command::Write(data.to_vec()))
    }

    /// Tells the remote the terminal size, as a NAWS subnegotiation.
    ///
    /// `ponytail:` sent unconditionally rather than gated on `DO NAWS`
    /// (RFC 1073 allows it once agreed), because the size may arrive before
    /// the server negotiates and real servers ignore an early NAWS. Upgrade
    /// path: share the codec's `WINDOW_SIZE` state with the writer if a
    /// strict server is ever met.
    pub fn resize(&self, cols: u16, rows: u16) -> Result<(), TelnetError> {
        self.send(Command::Resize { cols, rows })
    }

    /// A handle to the session's event stream. `async_channel` is
    /// competing-consumer, so call this once and own the receiver.
    pub fn events(&self) -> Receiver<TelnetEvent> {
        self.events.clone()
    }

    /// Asks the connection thread to close. Best-effort and non-blocking.
    pub fn disconnect(&self) {
        let _ = self.commands.try_send(Command::Close);
    }

    fn send(&self, command: Command) -> Result<(), TelnetError> {
        self.commands.try_send(command).map_err(|err| match err {
            TrySendError::Full(_) => TelnetError::Backpressure,
            TrySendError::Closed(_) => TelnetError::Closed,
        })
    }
}

impl Drop for TelnetSession {
    fn drop(&mut self) {
        // Do not leak the connection thread when the UI discards the handle.
        self.disconnect();
    }
}

/// Owns the socket for writing: connects, spawns the reader, then serves the
/// command channel until it closes or the socket fails.
fn run(config: Config, commands: Receiver<Command>, events: Sender<TelnetEvent>) {
    let Config {
        host,
        port,
        policy,
        terminal_type,
    } = config;

    let mut stream = match TcpStream::connect((host.as_str(), port)) {
        Ok(stream) => stream,
        Err(err) => {
            let _ = events.send_blocking(TelnetEvent::Error(format!("{host}:{port}: {err}")));
            return;
        }
    };
    let _ = stream.set_nodelay(true);
    let read_stream = match stream.try_clone() {
        Ok(stream) => stream,
        Err(err) => {
            let _ = events.send_blocking(TelnetEvent::Error(err.to_string()));
            return;
        }
    };

    let reader_events = events.clone();
    let reader = std::thread::Builder::new()
        .name(format!("telnet-read-{host}"))
        .spawn(move || read_loop(read_stream, policy, terminal_type, reader_events));
    if let Err(err) = reader {
        let _ = events.send_blocking(TelnetEvent::Error(format!(
            "could not spawn the reader thread: {err}"
        )));
        return;
    }

    let _ = events.send_blocking(TelnetEvent::Connected);

    while let Ok(command) = commands.recv_blocking() {
        let written = match command {
            Command::Write(data) => stream.write_all(&data),
            Command::Resize { cols, rows } => stream.write_all(&protocol::naws(cols, rows)),
            Command::Close => break,
        };
        if written.is_err() {
            break;
        }
    }
    let _ = stream.shutdown(Shutdown::Both);
}

/// Drains the socket: decodes payload into events and answers negotiation on
/// its own clone of the socket. The writer notices the server closing through
/// the socket, and stops for good when the [`TelnetSession`] handle drops and
/// closes the command channel.
fn read_loop(
    mut stream: TcpStream,
    policy: Box<dyn TelnetPolicy + Send>,
    terminal_type: String,
    events: Sender<TelnetEvent>,
) {
    let mut codec = TelnetCodec::new(policy);
    let mut buffer = [0u8; 4096];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => {
                let _ = events.send_blocking(TelnetEvent::Closed);
                break;
            }
            Ok(read) => match codec.receive(&buffer[..read]) {
                Ok(outcome) => {
                    let mut reply = Vec::new();
                    for subnegotiation in outcome.subnegotiations() {
                        if let Subnegotiation::TerminalTypeRequest = subnegotiation {
                            reply.extend_from_slice(&protocol::terminal_type(&terminal_type));
                        }
                    }
                    reply.extend_from_slice(outcome.replies());

                    let mut failed = !outcome.data().is_empty()
                        && events
                            .send_blocking(TelnetEvent::Data(outcome.data().to_vec()))
                            .is_err();
                    // ponytail: the reader answers negotiation on its own socket
                    // clone; replies are a handful of bytes and never interleave
                    // in practice at human scale. Upgrade path: route replies
                    // through the command channel if a server ever negotiates
                    // mid-paste.
                    if !failed && !reply.is_empty() {
                        failed = stream.write_all(&reply).is_err();
                    }
                    if failed {
                        break;
                    }
                }
                Err(err) => {
                    let _ = events.send_blocking(TelnetEvent::Error(err.to_string()));
                    break;
                }
            },
            Err(err) => {
                let _ = events.send_blocking(TelnetEvent::Error(err.to_string()));
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detached() -> (TelnetSession, Sender<TelnetEvent>) {
        let (commands, _command_rx) = async_channel::bounded(1);
        let (event_tx, events) = async_channel::bounded(4);
        (TelnetSession { commands, events }, event_tx)
    }

    #[test]
    fn connect_rejects_an_empty_host() {
        let err = TelnetSession::connect("  ", 23)
            .err()
            .expect("must not spawn a thread for an empty host");
        assert!(matches!(err, TelnetError::Connect(_)), "got {err:?}");
    }

    #[test]
    fn write_and_resize_fail_closed_once_the_thread_is_gone() {
        let (commands, command_rx) = async_channel::bounded(1);
        drop(command_rx);
        let (_event_tx, events) = async_channel::bounded(1);
        let session = TelnetSession { commands, events };

        assert!(matches!(session.write(b"ls\n"), Err(TelnetError::Closed)));
        assert!(matches!(session.resize(120, 40), Err(TelnetError::Closed)));
    }

    #[test]
    fn events_flow_through_the_channel() {
        let (session, sender) = detached();
        sender.send_blocking(TelnetEvent::Connected).expect("send");
        sender
            .send_blocking(TelnetEvent::Data(b"login: ".to_vec()))
            .expect("send");

        let events = session.events();
        assert!(matches!(events.recv_blocking(), Ok(TelnetEvent::Connected)));
        assert!(matches!(
            events.recv_blocking(),
            Ok(TelnetEvent::Data(data)) if data.as_slice() == b"login: "
        ));
    }

    /// Live check. Skipped by default; run with
    /// `cargo test -p sshdeck-telnet -- --ignored` and `SSHDECK_TEST_TELNET`
    /// pointing at a throwaway telnetd as `host:port`.
    #[test]
    #[ignore = "needs a live telnetd"]
    fn connects_and_reads_a_login_banner() {
        let Ok(target) = std::env::var("SSHDECK_TEST_TELNET") else {
            return;
        };
        let Some((host, port)) = target.rsplit_once(':') else {
            return;
        };
        let Ok(port) = port.parse::<u16>() else {
            return;
        };
        let session = TelnetSession::connect(host, port).expect("spawns the connection thread");
        let events = session.events();
        loop {
            match events.recv_blocking() {
                Ok(TelnetEvent::Connected) => {
                    assert!(session.resize(120, 40).is_ok());
                }
                Ok(TelnetEvent::Error(message)) => panic!("{message}"),
                Ok(TelnetEvent::Closed) => panic!("closed before any data"),
                Ok(TelnetEvent::Data(_)) => return,
                Err(err) => panic!("event channel closed: {err}"),
            }
        }
    }
}
