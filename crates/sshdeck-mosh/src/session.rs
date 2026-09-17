//! Process supervision for the system `mosh` client.
//!
//! `mosh` is a full-screen terminal program, not a pipe-friendly filter:
//! `mosh-client` opens with `tcgetattr( STDIN_FILENO )` and `exit(1)` when that
//! fails (upstream `src/frontend/stmclient.cc`), so the child gets a PTY on
//! stdin/stdout/stderr rather than pipes. The PTY is also its controlling
//! terminal, which is what makes a resize deliver `SIGWINCH` — the only resize
//! channel `mosh-client` has.
//!
//! The shape mirrors `sshdeck_telnet::session`: one supervisor thread owns the
//! child and the command queue, a second drains the PTY, and plain data crosses
//! between them over bounded `async_channel`s. No tokio type appears in a `pub`
//! signature.
//!
//! `ponytail:` the child's environment is inherited plus whatever the caller
//! sets; there is no terminal emulation here, so the bytes are handed to the
//! caller untouched. Upgrade path: a `vt100` grid (see `sshdeck-terminal`) if a
//! caller ever wants scrollback rather than raw output.

use std::io::{Read, Write};
use std::path::Path;
use std::thread::JoinHandle;

use async_channel::{Receiver, Sender, TrySendError};
use portable_pty::{native_pty_system, Child, ChildKiller, CommandBuilder, MasterPty, PtySize};

use crate::{MoshBinary, MoshError, MoshInvocation};

/// Commands from the caller to the supervisor thread.
enum Command {
    Write(Vec<u8>),
    Resize {
        cols: u16,
        rows: u16,
    },
    /// The PTY reader hit end of stream; reap the child.
    ChildEof,
    Shutdown,
}

/// The PTY size, in cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MoshTerminalSize {
    pub cols: u16,
    pub rows: u16,
}

impl MoshTerminalSize {
    /// A size in cells.
    pub fn new(cols: u16, rows: u16) -> Self {
        Self { cols, rows }
    }
}

impl Default for MoshTerminalSize {
    /// 80x24, the conventional fallback when the UI has no measurement yet.
    fn default() -> Self {
        Self { cols: 80, rows: 24 }
    }
}

/// How the `mosh` process ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoshExitStatus {
    code: u32,
    signal: Option<String>,
}

impl MoshExitStatus {
    /// The exit code, or `1` when the process was signalled (`std`-style).
    pub fn code(&self) -> u32 {
        self.code
    }

    /// The signal that ended the process, if one did.
    pub fn signal(&self) -> Option<&str> {
        self.signal.as_deref()
    }

    /// Whether the process ended by itself and successfully.
    pub fn success(&self) -> bool {
        self.signal.is_none() && self.code == 0
    }
}

impl std::fmt::Display for MoshExitStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.signal {
            Some(signal) => write!(f, "terminated by {signal}"),
            None => write!(f, "exited with code {}", self.code),
        }
    }
}

impl From<portable_pty::ExitStatus> for MoshExitStatus {
    fn from(status: portable_pty::ExitStatus) -> Self {
        Self {
            code: status.exit_code(),
            signal: status.signal().map(str::to_string),
        }
    }
}

/// Everything the caller sees from a mosh session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MoshEvent {
    /// The process is running. `pid` is `None` only if the platform reports no
    /// process id.
    Started { pid: Option<u32> },
    /// Bytes from the PTY, exactly as mosh wrote them.
    Data(Vec<u8>),
    /// The process ended. Always the last event: the PTY reader is joined
    /// before it is sent.
    Exited(MoshExitStatus),
    /// A failure the session survived, or that ended it.
    Error(String),
}

/// A running mosh process. Dropping the handle asks the supervisor to stop.
pub struct MoshSession {
    commands: Sender<Command>,
    events: Receiver<MoshEvent>,
}

impl MoshSession {
    /// Starts `mosh` against `invocation` in a fresh PTY. The child's
    /// environment is inherited plus [`MoshInvocation::env`]; a GUI caller must
    /// set `TERM` and a UTF-8 locale there, because `mosh-client` needs both.
    pub fn spawn(
        binary: &MoshBinary,
        invocation: &MoshInvocation,
        size: MoshTerminalSize,
    ) -> Result<Self, MoshError> {
        invocation.validate()?;
        let argv = invocation.argv();
        let env: Vec<(&str, &str)> = invocation.environment().collect();
        Self::spawn_argv(binary.path(), &argv, &env, size)
    }

    /// [`Self::spawn`] for an arbitrary program and vector, kept private so the
    /// supervisor is testable with `/bin/echo` and `/bin/cat` (no mosh on CI).
    fn spawn_argv(
        path: &Path,
        argv: &[String],
        env: &[(&str, &str)],
        size: MoshTerminalSize,
    ) -> Result<Self, MoshError> {
        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize {
                rows: size.rows,
                cols: size.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|err| MoshError::Spawn(format!("could not allocate a pty: {err}")))?;

        let mut builder = CommandBuilder::new(path);
        builder.args(argv.iter().map(String::as_str));
        for &(key, value) in env {
            builder.env(key, value);
        }

        let child = pair.slave.spawn_command(builder).map_err(|err| {
            MoshError::Spawn(format!("could not start {}: {err}", path.display()))
        })?;
        // The parent must not keep the slave open: the master only reports end
        // of stream once every slave descriptor is closed.
        drop(pair.slave);

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|err| MoshError::Spawn(format!("could not read the pty: {err}")))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|err| MoshError::Spawn(format!("could not write to the pty: {err}")))?;

        // ponytail: bounded both ways, like the telnet session. 64 queued
        // writes is more than a human or a paste produces before the UI
        // drains; 256 output chunks is a few screenfuls, after which the pty
        // backpressures mosh. Upgrade path: raise the event capacity if a UI
        // ever lags by more than a few frames.
        let (commands, command_rx) = async_channel::bounded(64);
        let (event_tx, events) = async_channel::bounded(256);

        // Enqueued before the supervisor thread exists, so `Started` is
        // deterministically the first event. The channel is empty and the
        // receiver is alive, so this cannot fail.
        let _ = event_tx.try_send(MoshEvent::Started {
            pid: child.process_id(),
        });

        // The reader needs a Sender to wake the supervisor when the pty reaches
        // end of stream; the supervisor keeps the Receiver.
        let command_tx = commands.clone();
        let master = pair.master;
        std::thread::Builder::new()
            .name("mosh".to_string())
            .spawn(move || {
                supervise(
                    child, master, reader, writer, command_rx, command_tx, event_tx,
                )
            })
            .map_err(|err| MoshError::Spawn(format!("could not spawn the supervisor: {err}")))?;

        Ok(Self { commands, events })
    }

    /// Sends input to mosh. Non-blocking: returns
    /// [`MoshError::Backpressure`] rather than stalling a UI thread.
    pub fn write(&self, data: &[u8]) -> Result<(), MoshError> {
        self.send(Command::Write(data.to_vec()))
    }

    /// Tells the PTY (and through `SIGWINCH`, mosh) that the window changed.
    pub fn resize(&self, cols: u16, rows: u16) -> Result<(), MoshError> {
        self.send(Command::Resize { cols, rows })
    }

    /// A handle to the session's event stream. `async_channel` is
    /// competing-consumer, so call this once and own the receiver.
    pub fn events(&self) -> Receiver<MoshEvent> {
        self.events.clone()
    }

    /// Asks the supervisor to end the process. Best-effort and non-blocking.
    pub fn shutdown(&self) -> Result<(), MoshError> {
        self.send(Command::Shutdown)
    }

    fn send(&self, command: Command) -> Result<(), MoshError> {
        self.commands.try_send(command).map_err(|err| match err {
            TrySendError::Full(_) => MoshError::Backpressure,
            TrySendError::Closed(_) => MoshError::Closed,
        })
    }
}

impl Drop for MoshSession {
    fn drop(&mut self) {
        // Do not leak the process when the UI discards the handle. `try_send`
        // can fail on a full queue; `close` still ends the supervisor once it
        // has drained what was already queued.
        let _ = self.commands.try_send(Command::Shutdown);
        let _ = self.commands.close();
    }
}

/// Owns the child and the command queue until the child is gone.
fn supervise(
    mut child: Box<dyn Child + Send + Sync>,
    master: Box<dyn MasterPty + Send>,
    reader: Box<dyn Read + Send>,
    mut writer: Box<dyn Write + Send>,
    commands: Receiver<Command>,
    command_tx: Sender<Command>,
    events: Sender<MoshEvent>,
) {
    let reader_handle: Option<JoinHandle<()>> = match std::thread::Builder::new()
        .name("mosh-read".to_string())
        .spawn({
            let events = events.clone();
            move || read_loop(reader, events, command_tx)
        }) {
        Ok(handle) => Some(handle),
        Err(err) => {
            // Without a reader there is no output and no end-of-stream nudge,
            // but the session can still be shut down.
            let _ = events.send_blocking(MoshEvent::Error(format!(
                "could not spawn the mosh reader thread: {err}"
            )));
            None
        }
    };

    let mut failure: Option<String> = None;
    let status: Option<MoshExitStatus> = loop {
        match commands.recv_blocking() {
            Ok(Command::Write(data)) => {
                if let Err(err) = writer.write_all(&data) {
                    failure = Some(format!("could not write to mosh: {err}"));
                    break reap(&mut *child);
                }
            }
            Ok(Command::Resize { cols, rows }) => {
                let _ = master.resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                });
            }
            // The pty reports end of stream only when the child's descriptors
            // are gone, so the child is exiting. `ponytail:` a child that
            // closed its pty but kept running would park this thread; mosh
            // never does.
            Ok(Command::ChildEof) => break wait_for_exit(&mut *child),
            Ok(Command::Shutdown) | Err(_) => break reap(&mut *child),
        }
    };

    if let Some(message) = failure {
        let _ = events.send_blocking(MoshEvent::Error(message));
    }
    // The reader is finished or about to be; joining keeps `Exited` last.
    // If the caller holds the handle without draining events, the reader parks
    // on backpressure until the handle (and so the receiver) is dropped —
    // the same contract as the write path.
    if let Some(handle) = reader_handle {
        let _ = handle.join();
    }
    match status {
        Some(status) => {
            let _ = events.send_blocking(MoshEvent::Exited(status));
        }
        None => {
            let _ = events.send_blocking(MoshEvent::Error(
                "could not reap the mosh process; its exit status is unknown".to_string(),
            ));
            // `1` mirrors portable-pty's own stand-in for a missing status.
            let _ = events.send_blocking(MoshEvent::Exited(MoshExitStatus {
                code: 1,
                signal: None,
            }));
        }
    }
}

/// Drains the PTY master into `Data` events, then asks the supervisor to reap.
fn read_loop(
    mut reader: Box<dyn Read + Send>,
    events: Sender<MoshEvent>,
    commands: Sender<Command>,
) {
    let mut buffer = [0u8; 8192];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                if events
                    .send_blocking(MoshEvent::Data(buffer[..read].to_vec()))
                    .is_err()
                {
                    // Every receiver is gone; nothing can observe the rest.
                    return;
                }
            }
            // A pty master reports EIO once the child has exited, so a read
            // error is end of stream, not a session failure. mosh's own
            // diagnostics arrive as `Data` before this.
            Err(_) => break,
        }
    }
    // Best-effort: if the queue is saturated the supervisor is already busy
    // with commands, and a write to the dead pty will end it.
    let _ = commands.try_send(Command::ChildEof);
}

/// Ends the child and reports how it went. `portable-pty` sends `SIGHUP` on
/// unix, which `mosh-client` does not trap, so the process dies.
///
/// `ponytail:` a signal rather than typing mosh's `Esc .` escape, which needs a
/// terminal emulator, not a supervisor. Upgrade path: `write(b"\x1e.")` first
/// and signal only if it does not exit.
fn reap(child: &mut (dyn Child + Send + Sync)) -> Option<MoshExitStatus> {
    let _ = ChildKiller::kill(child);
    wait_for_exit(child)
}

/// Waits for a child that has already ended (the pty reported end of stream).
fn wait_for_exit(child: &mut (dyn Child + Send + Sync)) -> Option<MoshExitStatus> {
    child.wait().ok().map(MoshExitStatus::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env() -> Vec<(&'static str, &'static str)> {
        Vec::new()
    }

    /// Collects events until the process exits, asserting no error arrives.
    fn collect_until_exit(events: &Receiver<MoshEvent>) -> (bool, Vec<u8>, MoshExitStatus) {
        let mut started = false;
        let mut output = Vec::new();
        loop {
            match events.recv_blocking() {
                Ok(MoshEvent::Started { .. }) => started = true,
                Ok(MoshEvent::Data(chunk)) => output.extend_from_slice(&chunk),
                Ok(MoshEvent::Exited(status)) => return (started, output, status),
                Ok(MoshEvent::Error(message)) => panic!("unexpected error: {message}"),
                Err(err) => panic!("the event channel closed before the process exited: {err}"),
            }
        }
    }

    #[test]
    fn a_one_shot_process_reports_started_data_and_exit() {
        let session = MoshSession::spawn_argv(
            Path::new("/bin/echo"),
            &["hello from the pty".to_string()],
            &no_env(),
            MoshTerminalSize::default(),
        )
        .expect("spawns /bin/echo in a pty");
        let events = session.events();

        let (started, output, status) = collect_until_exit(&events);
        assert!(started, "Started must be the first event");
        assert!(status.success(), "echo reported {status}");
        let text = String::from_utf8_lossy(&output);
        assert!(text.contains("hello from the pty"), "got {text:?}");
    }

    #[test]
    fn bytes_flow_both_ways_and_shutdown_reports_an_exit() {
        let session = MoshSession::spawn_argv(
            Path::new("/bin/cat"),
            &[],
            &no_env(),
            MoshTerminalSize::new(100, 30),
        )
        .expect("spawns /bin/cat in a pty");
        let events = session.events();

        // Deterministic: spawn_argv enqueues this before the thread exists.
        assert!(matches!(
            events.recv_blocking(),
            Ok(MoshEvent::Started { .. })
        ));

        session.write(b"ping-pong\n").expect("queues the write");
        let mut output = Vec::new();
        while !String::from_utf8_lossy(&output).contains("ping-pong") {
            match events.recv_blocking() {
                Ok(MoshEvent::Data(chunk)) => output.extend_from_slice(&chunk),
                Ok(other) => panic!("unexpected event before the echo: {other:?}"),
                Err(err) => panic!("the event channel closed: {err}"),
            }
        }

        assert!(session.resize(120, 40).is_ok());
        session.shutdown().expect("queues the shutdown");
        let status = loop {
            match events.recv_blocking() {
                Ok(MoshEvent::Exited(status)) => break status,
                Ok(_) => {}
                Err(err) => panic!("the event channel closed: {err}"),
            }
        };
        // portable-pty signals on unix; assert only that it was a signal, not
        // its localised name.
        assert!(status.signal().is_some(), "cat reported {status}");
        assert!(!status.success());
    }

    #[test]
    fn write_and_resize_fail_closed_once_the_supervisor_is_gone() {
        let (commands, command_rx) = async_channel::bounded(1);
        drop(command_rx);
        let (_event_tx, events) = async_channel::bounded(1);
        let session = MoshSession { commands, events };

        assert!(matches!(session.write(b"ls\n"), Err(MoshError::Closed)));
        assert!(matches!(session.resize(120, 40), Err(MoshError::Closed)));
        assert!(matches!(session.shutdown(), Err(MoshError::Closed)));
    }

    #[test]
    fn a_program_that_cannot_be_executed_is_a_spawn_error() {
        let err = MoshSession::spawn_argv(
            Path::new("/nonexistent/definitely-not-mosh"),
            &[],
            &no_env(),
            MoshTerminalSize::default(),
        )
        .err()
        .expect("must not spawn a missing program");
        assert!(matches!(err, MoshError::Spawn(_)), "got {err:?}");
    }

    #[test]
    fn spawn_rejects_an_invalid_invocation_before_any_pty_is_open() {
        let binary = MoshBinary::detect(Some(Path::new("/bin/echo"))).expect("echo is executable");
        let invocation = MoshInvocation::new("   ");
        let err = MoshSession::spawn(&binary, &invocation, MoshTerminalSize::default())
            .err()
            .expect("an empty host must not be spawned");
        assert!(matches!(err, MoshError::Invalid(_)), "got {err:?}");
    }

    /// Live check. Skipped by default; needs no network — `mosh --version`
    /// exits immediately, and the run exercises the real PTY plumbing:
    /// `SSHDECK_TEST_MOSH=$(command -v mosh) cargo test -p sshdeck-mosh -- --ignored`.
    #[test]
    #[ignore = "needs the real mosh binary; set SSHDECK_TEST_MOSH"]
    fn supervises_the_real_mosh_version_command() {
        let Ok(path) = std::env::var("SSHDECK_TEST_MOSH") else {
            return;
        };
        let binary =
            MoshBinary::detect(Some(Path::new(&path))).expect("the given path is executable");
        let session = MoshSession::spawn_argv(
            binary.path(),
            &["--version".to_string()],
            &no_env(),
            MoshTerminalSize::default(),
        )
        .expect("spawns mosh");
        let events = session.events();

        let (started, output, status) = collect_until_exit(&events);
        assert!(started);
        assert!(status.success(), "mosh --version reported {status}");
        let text = String::from_utf8_lossy(&output);
        assert!(text.contains("mosh"), "got {text:?}");
    }
}
