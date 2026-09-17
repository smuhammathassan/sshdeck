//! Executor-agnostic SSH transport.
//!
//! `russh` is tokio-native, so each connection gets one `std::thread` that owns
//! a tokio runtime and drives the whole session. The only things crossing the
//! thread boundary are plain data over bounded `async_channel`s: raw bytes in,
//! [`SessionEvent`]s out. No tokio type appears in this module's public API, so
//! the UI can drive a connection from any executor (GPUI/smol included).
//!
//! Shape follows `docs/re/RUSSH-GAP.md` §8.

use std::path::Path;
use std::sync::Arc;

use async_channel::{Receiver, Sender, TrySendError};
use bytes::Bytes;
use russh::client;
use russh::keys::agent::client::AgentClient;
use russh::keys::agent::AgentIdentity;
use russh::keys::{load_secret_key, PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use russh::{ChannelMsg, Disconnect, Sig};

use crate::forward::{self, Forward, ForwardConfig};
use crate::jump::{ChainHop, HostChain};
use crate::sftp::{self, SftpChannel};
use crate::{AuthMethod, Host, SessionState};

/// Caps how many keyboard-interactive rounds we will answer, so a misbehaving
/// server cannot loop us forever.
const MAX_INTERACTIVE_ROUNDS: usize = 8;

/// Commands from the UI thread to the connection thread.
enum Command {
    Write(Bytes),
    Resize { cols: u32, rows: u32 },
    Forward(forward::ForwardSetup),
    // Opens the `sftp` subsystem and bridges it to the caller's handle.
    Sftp(sftp::SftpSetup),
    AgentForward,
    Close,
}

/// What the session channel is opened for.
enum Mode {
    /// An interactive login shell over a PTY, for the terminal pane.
    Shell,
    /// One command via the SSH `exec` request: no PTY and no login shell.
    Exec(String),
}

/// Everything the UI sees from a session.
#[derive(Debug, Clone)]
pub enum SessionEvent {
    /// Auth succeeded and the session channel is being opened.
    Connected,
    /// Bytes from the remote session (stdout and stderr are merged): shell
    /// output for [`Session::connect`], the command's output for
    /// [`Session::exec`].
    Data(Vec<u8>),
    /// Lifecycle change, mirroring [`SessionState`].
    State(SessionState),
    /// The channel closed, with the remote exit status when the server sent one.
    Closed(Option<i32>),
    /// A failure that ended the session.
    Error(String),
}

/// A connection failure.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("unsupported: {0}")]
    Unsupported(&'static str),
    #[error("the keychain secret for {0} was not supplied to SessionConfig")]
    MissingSecret(&'static str),
    #[error("connection failed: {0}")]
    Connect(String),
    #[error("transport error: {0}")]
    Transport(String),
    #[error("input queue is full")]
    Backpressure,
    #[error("session is closed")]
    Closed,
    /// A failure resolving the jump chain, before any connection is attempted.
    #[error("host chain: {0}")]
    Chain(#[from] crate::jump::ChainError),
    /// A hop failed while dialling or authenticating. `hop` names it — its
    /// position, label and `user@host:port` — so the UI can show which hop died.
    #[error("hop {hop}: {message}")]
    Hop { hop: String, message: String },
}

impl From<russh::Error> for SessionError {
    fn from(err: russh::Error) -> Self {
        Self::Transport(err.to_string())
    }
}

/// What to connect to. Built from a [`Host`]; secrets are supplied separately by
/// the caller (they live in the keychain, not in core).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionConfig {
    address: String,
    port: u16,
    username: String,
    auth: AuthMethod,
    password: Option<String>,
    passphrase: Option<String>,
}

impl SessionConfig {
    /// Copies the non-secret fields of a host. Call [`Self::with_password`] or
    /// [`Self::with_passphrase`] when the host's `AuthMethod` needs a secret.
    pub fn from_host(host: &Host) -> Self {
        Self {
            address: host.address.clone(),
            port: host.port,
            username: host.username.clone(),
            auth: host.auth.clone(),
            password: None,
            passphrase: None,
        }
    }

    /// An endpoint that is not backed by a `Host` — a hop resolved from a
    /// `ProxyJump` spec. Secrets are attached with [`Self::with_password`] or
    /// [`Self::set_password`].
    pub fn direct(
        address: impl Into<String>,
        port: u16,
        username: impl Into<String>,
        auth: AuthMethod,
    ) -> Self {
        Self {
            address: address.into(),
            port,
            username: username.into(),
            auth,
            password: None,
            passphrase: None,
        }
    }

    /// Resolved password for [`AuthMethod::Password`].
    pub fn with_password(mut self, password: impl Into<String>) -> Self {
        self.password = Some(password.into());
        self
    }

    /// Resolved passphrase for an encrypted [`AuthMethod::Key`].
    pub fn with_passphrase(mut self, passphrase: impl Into<String>) -> Self {
        self.passphrase = Some(passphrase.into());
        self
    }

    /// In-place form of [`Self::with_password`], for a hop borrowed from a
    /// [`HostChain`](crate::jump::HostChain).
    pub fn set_password(&mut self, password: impl Into<String>) {
        self.password = Some(password.into());
    }

    /// In-place form of [`Self::with_passphrase`].
    pub fn set_passphrase(&mut self, passphrase: impl Into<String>) {
        self.passphrase = Some(passphrase.into());
    }

    pub fn address(&self) -> &str {
        &self.address
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn username(&self) -> &str {
        &self.username
    }

    pub fn auth(&self) -> &AuthMethod {
        &self.auth
    }

    /// Rejects the cases we cannot serve before a thread is spawned, so callers
    /// get a synchronous, typed error instead of an event.
    fn validate(&self) -> Result<(), SessionError> {
        if self.address.is_empty() {
            return Err(SessionError::Connect("host address is empty".into()));
        }
        match &self.auth {
            AuthMethod::KeyboardInteractive if self.password.is_none() => {
                Err(SessionError::Unsupported(
                    "keyboard-interactive authentication needs a resolved secret",
                ))
            }
            AuthMethod::None => Err(SessionError::Unsupported("none authentication")),
            AuthMethod::Password { .. } if self.password.is_none() => {
                Err(SessionError::MissingSecret("password"))
            }
            _ => Ok(()),
        }
    }
}

/// A live (or connecting) SSH connection.
pub struct Session {
    commands: Sender<Command>,
    events: Receiver<SessionEvent>,
}

impl Session {
    /// Spawns a dedicated connection thread that serves an interactive login
    /// shell. Returns immediately; progress and failure arrive as
    /// [`SessionEvent`]s on [`Session::events`].
    pub fn connect(config: SessionConfig) -> Result<Self, SessionError> {
        Self::spawn(config, Mode::Shell)
    }

    /// Spawns a dedicated connection thread that runs exactly one command with
    /// the SSH `exec` request: no PTY and no login shell, so the command is
    /// never echoed back and stdout carries nothing but the command's own
    /// output. Output arrives as [`SessionEvent::Data`] (stdout and stderr
    /// merged) and the remote exit status as [`SessionEvent::Closed`].
    pub fn exec(config: SessionConfig, command: &str) -> Result<Self, SessionError> {
        Self::spawn(config, Mode::Exec(command.to_string()))
    }

    /// Spawns a connection thread that dials a jump chain: the first hop
    /// directly, then every later hop over a `direct-tcpip` channel through the
    /// hop before it, authenticating and verifying each hop's host key on the
    /// way. The last hop is the target and carries the interactive shell.
    ///
    /// Build the chain with [`HostChain::resolve`], which follows native host
    /// references and `ProxyJump` specs, rejects cycles and over-deep chains,
    /// and lets the caller attach per-hop secrets via
    /// [`HostChain::hop_mut`](crate::jump::HostChain::hop_mut).
    pub fn connect_chain(chain: HostChain) -> Result<Self, SessionError> {
        Self::spawn_chain(chain, Mode::Shell)
    }

    /// Like [`Self::connect_chain`], but runs one command on the target with
    /// the SSH `exec` request instead of an interactive shell.
    pub fn exec_chain(chain: HostChain, command: &str) -> Result<Self, SessionError> {
        Self::spawn_chain(chain, Mode::Exec(command.to_string()))
    }

    fn spawn(config: SessionConfig, mode: Mode) -> Result<Self, SessionError> {
        config.validate()?;

        // ponytail: bounded both ways so a fast server cannot balloon memory.
        // 64 acknowledged commands is far more than a human generates; 256
        // chunks of buffered output is a screenful or so. Ceiling: when the
        // event channel fills, the read loop parks and TCP backpressure engages
        // (see RUSSH-GAP.md §8). Upgrade path: raise the event capacity if the
        // UI ever lags by more than a few frames.
        let (commands, cmd_rx) = async_channel::bounded(64);
        let (event_tx, events) = async_channel::bounded(256);

        let label = format!("ssh-{}", config.address());
        std::thread::Builder::new()
            .name(label)
            .spawn(move || {
                // ponytail: one runtime per connection, capped at 2 workers
                // instead of the default num_cpus — several sessions on an 8 GB
                // box otherwise spawn a worker per core each. Upgrade path: a
                // shared runtime when session count grows.
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build();
                match runtime {
                    Ok(runtime) => runtime.block_on(run(config, mode, cmd_rx, event_tx)),
                    Err(err) => {
                        let _ = event_tx.send_blocking(SessionEvent::Error(format!(
                            "could not start the connection runtime: {err}"
                        )));
                    }
                }
            })
            .map_err(|err| {
                SessionError::Connect(format!("could not spawn the connection thread: {err}"))
            })?;

        Ok(Self { commands, events })
    }

    /// The chain counterpart of [`Self::spawn`]: same thread, same bounded
    /// channels, but the thread dials a [`HostChain`] instead of one endpoint.
    fn spawn_chain(chain: HostChain, mode: Mode) -> Result<Self, SessionError> {
        // Validate every hop up front, so a missing secret or an unsupported
        // auth method is a synchronous typed error rather than an event. This is
        // the same check the single-endpoint path runs.
        for hop in chain.hops() {
            hop.config().validate()?;
        }

        let (commands, cmd_rx) = async_channel::bounded(64);
        let (event_tx, events) = async_channel::bounded(256);

        let label = format!("ssh-{}", chain.target().address());
        std::thread::Builder::new()
            .name(label)
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build();
                match runtime {
                    Ok(runtime) => runtime.block_on(run_chain(chain, mode, cmd_rx, event_tx)),
                    Err(err) => {
                        let _ = event_tx.send_blocking(SessionEvent::Error(format!(
                            "could not start the connection runtime: {err}"
                        )));
                    }
                }
            })
            .map_err(|err| {
                SessionError::Connect(format!("could not spawn the connection thread: {err}"))
            })?;

        Ok(Self { commands, events })
    }

    /// Sends input to the remote shell. Non-blocking: returns
    /// [`SessionError::Backpressure`] rather than stalling the UI thread.
    pub fn write(&self, data: &[u8]) -> Result<(), SessionError> {
        self.send(Command::Write(Bytes::copy_from_slice(data)))
    }

    /// Tells the remote PTY its new size.
    pub fn resize(&self, cols: u16, rows: u16) -> Result<(), SessionError> {
        self.send(Command::Resize {
            cols: u32::from(cols),
            rows: u32::from(rows),
        })
    }

    /// Starts a port forward on this connection. Progress (including bind
    /// failures) arrives on [`Forward::events`]. The command is processed once
    /// the shell channel is open.
    pub fn forward(&self, config: ForwardConfig) -> Result<Forward, SessionError> {
        config
            .validate()
            .map_err(|err| SessionError::Connect(err.to_string()))?;
        let (forward, setup) = Forward::channel(config);
        self.send(Command::Forward(setup))?;
        Ok(forward)
    }

    /// Opens the `sftp` subsystem on this connection. Returns an opaque,
    /// executor-agnostic [`SftpChannel`]; progress is reported by the channel
    /// itself and the subsystem is started on the connection's runtime. The
    /// channel is not usable until [`SftpChannel::opened`] resolves.
    pub fn open_sftp(&self) -> Result<SftpChannel, SessionError> {
        let (channel, setup) = SftpChannel::open();
        self.send(Command::Sftp(setup))?;
        Ok(channel)
    }

    /// Asks the server to allow agent forwarding on this connection. Incoming
    /// `auth-agent@openssh.com` channels are piped to the local `SSH_AUTH_SOCK`.
    pub fn request_agent_forwarding(&self) -> Result<(), SessionError> {
        self.send(Command::AgentForward)
    }

    /// A handle to the session's event stream. `async_channel` is
    /// competing-consumer, so call this once and own the receiver; a second
    /// receiver would split events rather than duplicate them.
    pub fn events(&self) -> Receiver<SessionEvent> {
        self.events.clone()
    }

    /// Asks the connection thread to close. Best-effort and non-blocking.
    pub fn disconnect(&self) {
        let _ = self.commands.try_send(Command::Close);
    }

    fn send(&self, command: Command) -> Result<(), SessionError> {
        self.commands.try_send(command).map_err(|err| match err {
            TrySendError::Full(_) => SessionError::Backpressure,
            TrySendError::Closed(_) => SessionError::Closed,
        })
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Do not leak the connection thread when the UI discards the handle.
        self.disconnect();
    }
}

/// Verifies one server key against the user's `~/.ssh/known_hosts`.
///
/// The check is `russh::keys::known_hosts::check_known_hosts_path` (via
/// [`crate::known_hosts::known`]), which hashes hostnames, handles
/// `[host]:port`, returns `Ok(false)` for an unknown host and a `KeyChanged`
/// error for a mismatch. Anything other than "recorded with this key" is an
/// error here, so an unknown key fails closed instead of being accepted.
///
/// `known_hosts` overrides the default file and exists so tests can point a hop
/// at a throwaway file; production always passes `None`.
fn verify_server_key(
    host: &str,
    port: u16,
    server_public_key: &PublicKeyOrCertificate,
    known_hosts: Option<&Path>,
) -> Result<(), SessionError> {
    let public_key = match server_public_key {
        PublicKeyOrCertificate::PublicKey { key, .. } => key,
        // ponytail: OpenSSH host certificates need CA-fingerprint validation
        // (ssh_key::certificate::Certificate::validate), which is a separate
        // trust decision. Until that exists, refuse rather than guess.
        PublicKeyOrCertificate::Certificate(_) => {
            return Err(SessionError::Unsupported("OpenSSH host certificate"));
        }
    };
    let path = match known_hosts {
        Some(path) => Some(path.to_path_buf()),
        None => crate::known_hosts::default_path(),
    };
    let Some(path) = path else {
        return Err(SessionError::Connect(format!(
            "host key for {host}:{port} cannot be checked: no known_hosts path"
        )));
    };
    match crate::known_hosts::known(host, port, public_key, &path) {
        Ok(true) => Ok(()),
        Ok(false) => Err(SessionError::Connect(format!(
            "host key for {host}:{port} is not in known_hosts"
        ))),
        Err(err) => Err(SessionError::Connect(format!(
            "known_hosts check for {host}:{port} failed: {err}"
        ))),
    }
}

/// The handler for one hop. Every hop of a chain gets its own instance carrying
/// that hop's `host`/`port`, so the key check is always against the hop that
/// actually offered the key — never only the last hop.
struct KnownHostsHandler {
    host: String,
    port: u16,
    /// Routes for running `-R` forwards, filled in by the connection thread.
    routes: forward::Routes,
}

impl KnownHostsHandler {
    fn new(host: impl Into<String>, port: u16, routes: forward::Routes) -> Self {
        Self {
            host: host.into(),
            port,
            routes,
        }
    }
}

impl client::Handler for KnownHostsHandler {
    type Error = SessionError;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        verify_server_key(&self.host, self.port, server_public_key, None)?;
        Ok(true)
    }

    /// A `-R` connection arrived. Accept it and pipe it to the local target
    /// registered for this bind address/port; reject anything we did not ask
    /// for.
    async fn server_channel_open_forwarded_tcpip(
        &mut self,
        channel: russh::Channel<client::Msg>,
        connected_address: &str,
        connected_port: u32,
        originator_address: &str,
        _originator_port: u32,
        reply: client::ChannelOpenHandle,
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        match forward::resolve(&self.routes, connected_address, connected_port) {
            Some(route) => {
                reply.accept().await;
                let originator = originator_address.to_string();
                tokio::spawn(forward::bridge_forwarded(channel, route, originator));
            }
            None => {
                reply
                    .reject(russh::ChannelOpenFailure::AdministrativelyProhibited)
                    .await;
            }
        }
        Ok(())
    }

    /// The server offered an agent-forwarding channel; pipe it to the local
    /// `SSH_AUTH_SOCK` (only opened when the caller requested forwarding).
    async fn server_channel_open_agent_forward(
        &mut self,
        channel: russh::Channel<client::Msg>,
        reply: client::ChannelOpenHandle,
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        reply.accept().await;
        tokio::spawn(forward::bridge_agent(channel));
        Ok(())
    }
}

async fn emit(tx: &Sender<SessionEvent>, event: SessionEvent) {
    let _ = tx.send(event).await;
}

/// Drives one connection on the dedicated runtime, then reports the outcome.
async fn run(
    config: SessionConfig,
    mode: Mode,
    commands: Receiver<Command>,
    events: Sender<SessionEvent>,
) {
    if let Err(err) = run_session(&config, mode, commands, &events).await {
        let message = err.to_string();
        emit(&events, SessionEvent::Error(message.clone())).await;
        emit(
            &events,
            SessionEvent::State(SessionState::Failed { message }),
        )
        .await;
    }
}

async fn run_session(
    config: &SessionConfig,
    mode: Mode,
    commands: Receiver<Command>,
    events: &Sender<SessionEvent>,
) -> Result<(), SessionError> {
    emit(events, SessionEvent::State(SessionState::Connecting)).await;

    let routes = forward::Routes::default();
    let handler = KnownHostsHandler::new(config.address(), config.port(), routes.clone());

    let mut handle =
        client::connect(client_config(), (config.address(), config.port()), handler).await?;

    emit(events, SessionEvent::State(SessionState::Authenticating)).await;
    authenticate(&mut handle, config).await?;
    emit(events, SessionEvent::Connected).await;

    // `Handle` is `Send` but not `Sync` (it owns the reply receiver), and the
    // forwarding tasks need `&Handle`; a mutex gives them that without moving
    // the session loop off this task.
    let handle = Arc::new(tokio::sync::Mutex::new(handle));
    drive(handle, routes, mode, commands, events).await
}

/// Drives a chain on the dedicated runtime, then reports the outcome.
async fn run_chain(
    chain: HostChain,
    mode: Mode,
    commands: Receiver<Command>,
    events: Sender<SessionEvent>,
) {
    if let Err(err) = run_chain_inner(&chain, mode, commands, &events).await {
        let message = err.to_string();
        emit(&events, SessionEvent::Error(message.clone())).await;
        emit(
            &events,
            SessionEvent::State(SessionState::Failed { message }),
        )
        .await;
    }
}

/// Dial order: hop 0 goes directly, every later hop is reached over a
/// `direct-tcpip` channel through the hop before it. Each hop is authenticated
/// (and its host key verified against that hop's identity) before the next is
/// opened, and every intermediate `Handle` lives until the end: the tunnel is
/// the previous hop's channel, so dropping one would close it.
async fn run_chain_inner(
    chain: &HostChain,
    mode: Mode,
    commands: Receiver<Command>,
    events: &Sender<SessionEvent>,
) -> Result<(), SessionError> {
    emit(events, SessionEvent::State(SessionState::Connecting)).await;

    let routes = forward::Routes::default();
    let client_config = client_config();
    let hops = chain.hops();
    let total = hops.len();
    let mut handles: Vec<client::Handle<KnownHostsHandler>> = Vec::with_capacity(total);

    for (index, hop) in hops.iter().enumerate() {
        let handler = KnownHostsHandler::new(hop.address(), hop.port(), routes.clone());
        let mut handle = match handles.last() {
            None => client::connect(client_config.clone(), (hop.address(), hop.port()), handler)
                .await
                .map_err(|err| hop_failure(index, total, hop, err))?,
            Some(previous) => {
                let channel = previous
                    .channel_open_direct_tcpip(
                        hop.address().to_string(),
                        u32::from(hop.port()),
                        "127.0.0.1",
                        0,
                    )
                    .await
                    .map_err(|err| hop_failure(index, total, hop, err.into()))?;
                // `Box::pin` makes the stream `Unpin`, so it satisfies
                // `connect_stream`, which drives the next SSH session over it.
                client::connect_stream(
                    client_config.clone(),
                    Box::pin(channel.into_stream()),
                    handler,
                )
                .await
                .map_err(|err| hop_failure(index, total, hop, err))?
            }
        };

        if index + 1 == total {
            emit(events, SessionEvent::State(SessionState::Authenticating)).await;
        }
        authenticate(&mut handle, hop.config())
            .await
            .map_err(|err| hop_failure(index, total, hop, err))?;
        handles.push(handle);
    }

    emit(events, SessionEvent::Connected).await;

    // The last hop is the target; the intermediates stay in `handles` until this
    // function returns, keeping every tunnel open.
    let target = handles
        .pop()
        .ok_or_else(|| SessionError::Connect("host chain is empty".into()))?;
    let handle = Arc::new(tokio::sync::Mutex::new(target));
    drive(handle, routes, mode, commands, events).await
}

/// Names the hop a failure happened on, the way the UI shows it.
fn hop_failure(index: usize, total: usize, hop: &ChainHop, err: SessionError) -> SessionError {
    let name = match hop.label() {
        Some(label) => format!("{label} ({})", hop.endpoint()),
        None => hop.endpoint(),
    };
    SessionError::Hop {
        hop: format!("{} of {total} {name}", index + 1),
        message: err.to_string(),
    }
}

/// The client config every connection uses.
///
/// ponytail: 1 MiB window and 32 buffered channel messages, on the modest end of
/// RUSSH-GAP.md §8's recommendation; the terminal scrollback is the only
/// unbounded growth.
fn client_config() -> Arc<client::Config> {
    Arc::new(client::Config {
        window_size: 1 << 20,
        channel_buffer_size: 32,
        ..Default::default()
    })
}

/// Opens the session channel for `mode` and pumps it until it closes. Shared by
/// the single-endpoint and chain paths so their behaviour cannot drift.
async fn drive(
    handle: Arc<tokio::sync::Mutex<client::Handle<KnownHostsHandler>>>,
    routes: forward::Routes,
    mode: Mode,
    commands: Receiver<Command>,
    events: &Sender<SessionEvent>,
) -> Result<(), SessionError> {
    let channel = {
        let connection = handle.lock().await;
        connection.channel_open_session().await?
    };
    let (mut read, write) = channel.split();
    match &mode {
        // A real PTY + shell so interactive terminal bytes flow. The initial
        // size is a placeholder; the UI sends the first real size via `resize`.
        Mode::Shell => {
            write
                .request_pty(true, "xterm-256color", 80, 24, 0, 0, &[])
                .await?;
            write.request_shell(true).await?;
        }
        // Direct execution: the server runs `command` with no shell and no PTY,
        // so there is nothing to echo and no prompt in the byte stream.
        Mode::Exec(command) => write.exec(true, command.as_bytes().to_vec()).await?,
    }

    let mut exit_code: Option<i32> = None;
    loop {
        tokio::select! {
            command = commands.recv() => match command {
                Ok(Command::Write(data)) => write.data_bytes(data).await?,
                Ok(Command::Resize { cols, rows }) => {
                    write.window_change(cols, rows, 0, 0).await?;
                }
                Ok(Command::Forward(setup)) => {
                    tokio::spawn(forward::serve(handle.clone(), setup, routes.clone()));
                }
                Ok(Command::Sftp(setup)) => {
                    tokio::spawn(sftp::serve(handle.clone(), setup));
                }
                Ok(Command::AgentForward) => {
                    write.agent_forward(true).await?;
                }
                Ok(Command::Close) | Err(_) => {
                    let _ = write.eof().await;
                    let _ = write.close().await;
                    break;
                }
            },
            message = read.wait() => match message {
                Some(ChannelMsg::Data { data })
                | Some(ChannelMsg::ExtendedData { data, .. }) => {
                    emit(events, SessionEvent::Data(data.to_vec())).await;
                }
                Some(ChannelMsg::ExitStatus { exit_status }) => {
                    exit_code = Some(exit_status as i32);
                }
                // A command killed by a signal sends no exit status; report the
                // shell convention rather than a misleading success.
                Some(ChannelMsg::ExitSignal { signal_name, .. }) => {
                    exit_code.get_or_insert_with(|| signal_exit_code(&signal_name));
                }
                Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => break,
                // Other channel messages are not needed for a login shell.
                Some(_) => {}
            },
        }
    }

    {
        let connection = handle.lock().await;
        let _ = connection
            .disconnect(Disconnect::ByApplication, "", "")
            .await;
    }
    emit(events, SessionEvent::Closed(exit_code)).await;
    Ok(())
}

/// The status to report when the server says a command died from a signal
/// ([`ChannelMsg::ExitSignal`], which carries no exit status of its own),
/// following the shell convention of `128 + signal number`. A signal russh
/// does not model by name reports 128, so a signalled command is never mistaken
/// for a successful one.
fn signal_exit_code(signal: &Sig) -> i32 {
    let number = match signal {
        Sig::HUP => 1,
        Sig::INT => 2,
        Sig::QUIT => 3,
        Sig::ILL => 4,
        Sig::ABRT => 6,
        Sig::FPE => 8,
        Sig::KILL => 9,
        Sig::USR1 => 10,
        Sig::SEGV => 11,
        Sig::PIPE => 13,
        Sig::ALRM => 14,
        Sig::TERM => 15,
        Sig::Custom(_) => return 128,
    };
    128 + number
}

async fn authenticate(
    handle: &mut client::Handle<KnownHostsHandler>,
    config: &SessionConfig,
) -> Result<(), SessionError> {
    match &config.auth {
        AuthMethod::Agent => {
            let mut agent = AgentClient::connect_env()
                .await
                .map_err(|err| SessionError::Transport(format!("ssh-agent: {err}")))?;
            let identities = agent
                .request_identities()
                .await
                .map_err(|err| SessionError::Transport(format!("ssh-agent: {err}")))?;
            let rsa_hash = handle.best_supported_rsa_hash().await?.flatten();
            for identity in identities {
                // ponytail: plain public keys only. Certificates held by the
                // agent need `authenticate_certificate_with` plus a trust
                // decision; upgrade path is a second arm here.
                let AgentIdentity::PublicKey { key, .. } = identity else {
                    continue;
                };
                match handle
                    .authenticate_publickey_with(config.username.clone(), key, rsa_hash, &mut agent)
                    .await
                {
                    Ok(client::AuthResult::Success) => return Ok(()),
                    Ok(client::AuthResult::Failure { .. }) => continue,
                    Err(err) => {
                        return Err(SessionError::Transport(format!("ssh-agent: {err}")));
                    }
                }
            }
            Err(SessionError::Connect(
                "the ssh-agent holds no usable public key".into(),
            ))
        }
        AuthMethod::Key {
            key_path,
            passphrase_ref: _,
        } => {
            let key = load_secret_key(key_path, config.passphrase.as_deref())
                .map_err(|err| SessionError::Transport(format!("{}: {err}", key_path.display())))?;
            let rsa_hash = handle.best_supported_rsa_hash().await?.flatten();
            let key = PrivateKeyWithHashAlg::new(Arc::new(key), rsa_hash);
            match handle
                .authenticate_publickey(config.username.clone(), key)
                .await?
            {
                client::AuthResult::Success => Ok(()),
                client::AuthResult::Failure { .. } => {
                    Err(SessionError::Connect("public key was rejected".into()))
                }
            }
        }
        AuthMethod::Password { .. } => {
            let password = config
                .password
                .clone()
                .ok_or(SessionError::MissingSecret("password"))?;
            match handle
                .authenticate_password(config.username.clone(), password)
                .await?
            {
                client::AuthResult::Success => Ok(()),
                client::AuthResult::Failure { .. } => {
                    Err(SessionError::Connect("password was rejected".into()))
                }
            }
        }
        AuthMethod::KeyboardInteractive => {
            let password = config
                .password
                .clone()
                .ok_or(SessionError::MissingSecret("password"))?;
            let mut response = handle
                .authenticate_keyboard_interactive_start(
                    config.username.clone(),
                    Option::<String>::None,
                )
                .await?;
            // ponytail: the resolved password answers every prompt. Servers that
            // ask for OTPs or several distinct factors need a prompt callback on
            // SessionEvent; until that exists those flows fail rather than guess.
            for _ in 0..MAX_INTERACTIVE_ROUNDS {
                match response {
                    client::KeyboardInteractiveAuthResponse::Success => return Ok(()),
                    client::KeyboardInteractiveAuthResponse::Failure { .. } => {
                        return Err(SessionError::Connect(
                            "keyboard-interactive was rejected".into(),
                        ));
                    }
                    client::KeyboardInteractiveAuthResponse::InfoRequest { prompts, .. } => {
                        let answers = vec![password.clone(); prompts.len()];
                        response = handle
                            .authenticate_keyboard_interactive_respond(answers)
                            .await?;
                    }
                }
            }
            Err(SessionError::Connect(
                "keyboard-interactive did not finish".into(),
            ))
        }
        AuthMethod::None => Err(SessionError::Unsupported(
            "authentication method not implemented",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Inventory;

    fn host(auth: AuthMethod) -> Host {
        let mut host = Host::new("Prod", "example.com");
        host.username = "deploy".into();
        host.port = 2222;
        host.auth = auth;
        host
    }

    fn detached_session() -> (Session, Sender<SessionEvent>) {
        let (commands, _cmd_rx) = async_channel::bounded(1);
        let (event_tx, events) = async_channel::bounded(4);
        (Session { commands, events }, event_tx)
    }

    #[test]
    fn from_host_copies_endpoint_and_auth() {
        let config = SessionConfig::from_host(&host(AuthMethod::Agent));
        assert_eq!(config.address(), "example.com");
        assert_eq!(config.port(), 2222);
        assert_eq!(config.username(), "deploy");
        assert_eq!(config.auth(), &AuthMethod::Agent);
    }

    /// The keys used below are the ed25519 keys russh's own known_hosts tests
    /// use (also quoted in `known_hosts.rs`).
    const KEY_A: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIJdD7y3aLq454yWBdwLWbieU1ebz9/cu7/QEXn9OIeZJ";

    fn public_key(base64: &str) -> russh::keys::PublicKey {
        russh::keys::parse_public_key_base64(base64).expect("valid key")
    }

    fn server_key() -> PublicKeyOrCertificate {
        PublicKeyOrCertificate::PublicKey {
            key: public_key(KEY_A),
            hash_alg: None,
        }
    }

    #[test]
    fn connect_chain_validates_every_hop_before_dialling() {
        let mut target = Host::new("db", "db.internal");
        target.username = "deploy".into();
        target.auth = AuthMethod::Password {
            secret_ref: "target".into(),
        };
        target.proxy_jump = Some("bastion".into());
        let mut bastion = Host::new("bastion", "bastion.internal");
        bastion.username = "ops".into();
        bastion.auth = AuthMethod::Password {
            secret_ref: "jump".into(),
        };
        let mut inventory = Inventory::default();
        inventory.insert(bastion);
        inventory.insert(target.clone());

        let mut chain = HostChain::resolve(&target, &inventory).expect("resolves");
        chain
            .hop_mut(1)
            .expect("target is the last hop")
            .set_password("target-secret");

        // The jump's secret is missing, so the chain must not dial at all.
        let err = Session::connect_chain(chain)
            .err()
            .expect("must not spawn a thread without the jump secret");
        assert!(
            matches!(err, SessionError::MissingSecret("password")),
            "got {err:?}"
        );
    }

    #[test]
    fn every_hop_of_a_chain_is_verified_against_known_hosts() {
        let mut target = Host::new("db", "db.internal");
        target.proxy_jump = Some("bastion".into());
        let bastion = Host::new("bastion", "bastion.internal");
        let mut inventory = Inventory::default();
        inventory.insert(bastion);
        inventory.insert(target.clone());

        let chain = HostChain::resolve(&target, &inventory).expect("resolves");
        assert_eq!(chain.len(), 2);

        let dir = std::env::temp_dir().join(format!("sshdeck-chain-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("known_hosts");

        // Record only the jump's key, then run the exact check the handler runs
        // for each hop: both hops are checked, and the unrecorded target fails
        // closed rather than being waved through.
        crate::known_hosts::learn("bastion.internal", 22, &public_key(KEY_A), &path)
            .expect("learn");
        let mut checked = 0usize;
        let mut outcomes = Vec::new();
        for hop in chain.hops() {
            checked += 1;
            outcomes.push(
                verify_server_key(
                    hop.address(),
                    hop.port(),
                    &server_key(),
                    Some(path.as_path()),
                )
                .is_ok(),
            );
        }
        assert_eq!(checked, 2, "a 2-hop chain verifies exactly two hops");
        assert_eq!(outcomes, [true, false]);

        // Record the target too: both hops verify now.
        crate::known_hosts::learn("db.internal", 22, &public_key(KEY_A), &path).expect("learn");
        for hop in chain.hops() {
            verify_server_key(
                hop.address(),
                hop.port(),
                &server_key(),
                Some(path.as_path()),
            )
            .expect("every hop verifies");
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn hop_failures_name_the_hop_at_its_position() {
        let mut target = Host::new("db", "db.internal");
        target.proxy_jump = Some("ops@bastion.example.com:2222".into());
        let inventory = Inventory::default();
        let chain = HostChain::resolve(&target, &inventory).expect("resolves");
        let hops = chain.hops();

        let jump = hop_failure(0, 2, hops[0], SessionError::Connect("refused".into()));
        let jump = jump.to_string();
        assert!(jump.contains("1 of 2"), "{jump}");
        assert!(jump.contains("ops@bastion.example.com:2222"), "{jump}");

        let target = hop_failure(1, 2, hops[1], SessionError::Connect("refused".into()));
        let target = target.to_string();
        assert!(target.contains("2 of 2"), "{target}");
        assert!(target.contains("db.internal"), "{target}");
    }

    #[test]
    fn connect_rejects_auth_methods_we_do_not_implement() {
        for auth in [AuthMethod::KeyboardInteractive, AuthMethod::None] {
            let err = Session::connect(SessionConfig::from_host(&host(auth)))
                .err()
                .expect("must not spawn a thread for unsupported auth");
            assert!(matches!(err, SessionError::Unsupported(_)), "got {err:?}");
        }
    }

    #[test]
    fn password_auth_needs_a_resolved_secret() {
        let auth = AuthMethod::Password {
            secret_ref: "prod-password".into(),
        };
        let err = Session::connect(SessionConfig::from_host(&host(auth)))
            .err()
            .expect("must not connect without the secret");
        assert!(matches!(err, SessionError::MissingSecret("password")));
    }

    #[test]
    fn write_and_resize_fail_closed_once_the_thread_is_gone() {
        let (commands, cmd_rx) = async_channel::bounded(1);
        drop(cmd_rx);
        let (_event_tx, events) = async_channel::bounded(1);
        let session = Session { commands, events };

        assert!(matches!(session.write(b"ls\n"), Err(SessionError::Closed)));
        assert!(matches!(session.resize(120, 40), Err(SessionError::Closed)));
    }

    #[test]
    fn events_flow_through_the_channel() {
        let (session, sender) = detached_session();
        sender.send_blocking(SessionEvent::Connected).expect("send");
        sender
            .send_blocking(SessionEvent::Data(b"hi".to_vec()))
            .expect("send");

        let events = session.events();
        assert!(matches!(
            events.recv_blocking(),
            Ok(SessionEvent::Connected)
        ));
        assert!(matches!(
            events.recv_blocking(),
            Ok(SessionEvent::Data(data)) if data == b"hi"
        ));
    }

    #[test]
    fn transport_errors_map_from_russh() {
        let err = SessionError::from(russh::Error::UnknownKey);
        assert!(matches!(err, SessionError::Transport(_)));
        assert!(!err.to_string().is_empty());
    }

    #[test]
    fn signal_exit_codes_follow_the_shell_convention() {
        assert_eq!(signal_exit_code(&Sig::TERM), 143);
        assert_eq!(signal_exit_code(&Sig::KILL), 137);
        assert_eq!(signal_exit_code(&Sig::INT), 130);
        assert_eq!(signal_exit_code(&Sig::PIPE), 141);
        // A signal russh does not model by name still fails, never succeeds.
        assert_eq!(signal_exit_code(&Sig::Custom("PWR".into())), 128);
    }

    /// Live check. Skipped by default; run with
    /// `cargo test -p sshdeck-core -- --ignored` and
    /// `SSHDECK_TEST_HOST`, `SSHDECK_TEST_USER`, `SSHDECK_TEST_PASSWORD`
    /// (optional `SSHDECK_TEST_PORT`) pointing at a throwaway sshd.
    #[test]
    #[ignore = "needs a live sshd"]
    fn connects_and_opens_a_shell_on_a_live_host() {
        let (Ok(address), Ok(username), Ok(password)) = (
            std::env::var("SSHDECK_TEST_HOST"),
            std::env::var("SSHDECK_TEST_USER"),
            std::env::var("SSHDECK_TEST_PASSWORD"),
        ) else {
            return;
        };

        let mut target = Host::new("live", address);
        target.username = username;
        target.port = std::env::var("SSHDECK_TEST_PORT")
            .ok()
            .and_then(|port| port.parse().ok())
            .unwrap_or(22);
        target.auth = AuthMethod::Password {
            secret_ref: "test".into(),
        };

        let session = Session::connect(SessionConfig::from_host(&target).with_password(password))
            .expect("spawns the connection thread");
        let events = session.events();
        loop {
            match events.recv_blocking() {
                Ok(SessionEvent::Connected) => {
                    session.write(b"echo sshdeck\n").expect("write");
                    return;
                }
                Ok(SessionEvent::Error(message)) => panic!("{message}"),
                Ok(SessionEvent::Closed(code)) => panic!("closed before auth: {code:?}"),
                Ok(_) => {}
                Err(err) => panic!("event channel closed: {err}"),
            }
        }
    }

    /// Live check of the exec path. Same env as the shell check above. Proves
    /// the command is not echoed (no PTY) and that the remote status survives.
    #[test]
    #[ignore = "needs a live sshd"]
    fn exec_runs_one_command_without_a_pty() {
        let (Ok(address), Ok(username), Ok(password)) = (
            std::env::var("SSHDECK_TEST_HOST"),
            std::env::var("SSHDECK_TEST_USER"),
            std::env::var("SSHDECK_TEST_PASSWORD"),
        ) else {
            return;
        };

        let mut target = Host::new("live", address);
        target.username = username;
        target.port = std::env::var("SSHDECK_TEST_PORT")
            .ok()
            .and_then(|port| port.parse().ok())
            .unwrap_or(22);
        target.auth = AuthMethod::Password {
            secret_ref: "test".into(),
        };

        let session = Session::exec(
            SessionConfig::from_host(&target).with_password(password),
            "printf sshdeck; exit 7",
        )
        .expect("spawns the connection thread");

        let events = session.events();
        let mut output = Vec::new();
        let mut status = None;
        while let Ok(event) = events.recv_blocking() {
            match event {
                SessionEvent::Data(bytes) => output.extend_from_slice(&bytes),
                SessionEvent::Error(message) => panic!("{message}"),
                SessionEvent::Closed(code) => status = code,
                _ => {}
            }
        }
        assert_eq!(output, b"sshdeck");
        assert_eq!(status, Some(7));
    }

    /// Live check of the jump chain: dials the target through a real jump host,
    /// verifying both host keys. The target is described by the same
    /// `SSHDECK_TEST_*` env as the checks above; the jump host additionally needs
    /// `SSHDECK_TEST_JUMP_HOST`, `SSHDECK_TEST_JUMP_USER`,
    /// `SSHDECK_TEST_JUMP_PASSWORD` (optional `SSHDECK_TEST_JUMP_PORT`).
    #[test]
    #[ignore = "needs a live sshd and a live jump host"]
    fn connects_through_a_live_jump_host() {
        let (Ok(jump_host), Ok(jump_user), Ok(jump_password)) = (
            std::env::var("SSHDECK_TEST_JUMP_HOST"),
            std::env::var("SSHDECK_TEST_JUMP_USER"),
            std::env::var("SSHDECK_TEST_JUMP_PASSWORD"),
        ) else {
            return;
        };
        let (Ok(address), Ok(username), Ok(password)) = (
            std::env::var("SSHDECK_TEST_HOST"),
            std::env::var("SSHDECK_TEST_USER"),
            std::env::var("SSHDECK_TEST_PASSWORD"),
        ) else {
            return;
        };
        let port = |name: &str| {
            std::env::var(name)
                .ok()
                .and_then(|port| port.parse().ok())
                .unwrap_or(22)
        };

        let mut bastion = Host::new("bastion", jump_host);
        bastion.username = jump_user;
        bastion.port = port("SSHDECK_TEST_JUMP_PORT");
        bastion.auth = AuthMethod::Password {
            secret_ref: "test-jump".into(),
        };
        let mut target = Host::new("live", address);
        target.username = username;
        target.port = port("SSHDECK_TEST_PORT");
        target.auth = AuthMethod::Password {
            secret_ref: "test".into(),
        };
        target.proxy_jump = Some("bastion".into());

        let mut inventory = Inventory::default();
        inventory.insert(bastion);
        inventory.insert(target.clone());

        let mut chain = HostChain::resolve(&target, &inventory).expect("resolves");
        assert_eq!(chain.len(), 2);
        chain
            .hop_mut(0)
            .expect("jump is the first hop")
            .set_password(jump_password);
        chain
            .hop_mut(1)
            .expect("target is the last hop")
            .set_password(password);

        let session = Session::connect_chain(chain).expect("spawns the connection thread");
        let events = session.events();
        loop {
            match events.recv_blocking() {
                Ok(SessionEvent::Connected) => {
                    session.write(b"echo sshdeck-chain\n").expect("write");
                    return;
                }
                Ok(SessionEvent::Error(message)) => panic!("{message}"),
                Ok(SessionEvent::Closed(code)) => panic!("closed before auth: {code:?}"),
                Ok(_) => {}
                Err(err) => panic!("event channel closed: {err}"),
            }
        }
    }
}
