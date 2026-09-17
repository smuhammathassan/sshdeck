//! Executor-agnostic SSH transport.
//!
//! `russh` is tokio-native, so each connection gets one `std::thread` that owns
//! a tokio runtime and drives the whole session. The only things crossing the
//! thread boundary are plain data over bounded `async_channel`s: raw bytes in,
//! [`SessionEvent`]s out. No tokio type appears in this module's public API, so
//! the UI can drive a connection from any executor (GPUI/smol included).
//!
//! Shape follows `docs/re/RUSSH-GAP.md` §8.

use std::sync::Arc;

use async_channel::{Receiver, Sender, TrySendError};
use bytes::Bytes;
use russh::client;
use russh::keys::agent::client::AgentClient;
use russh::keys::agent::AgentIdentity;
use russh::keys::{
    check_known_hosts, load_secret_key, PrivateKeyWithHashAlg, PublicKeyOrCertificate,
};
use russh::{ChannelMsg, Disconnect};

use crate::forward::{self, Forward, ForwardConfig};
use crate::{AuthMethod, Host, SessionState};

/// Caps how many keyboard-interactive rounds we will answer, so a misbehaving
/// server cannot loop us forever.
const MAX_INTERACTIVE_ROUNDS: usize = 8;

/// Commands from the UI thread to the connection thread.
enum Command {
    Write(Bytes),
    Resize { cols: u32, rows: u32 },
    Forward(forward::ForwardSetup),
    AgentForward,
    Close,
}

/// Everything the UI sees from a session.
#[derive(Debug, Clone)]
pub enum SessionEvent {
    /// Auth succeeded and the shell channel is open.
    Connected,
    /// Bytes from the remote shell (stdout and stderr are merged).
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
}

impl From<russh::Error> for SessionError {
    fn from(err: russh::Error) -> Self {
        Self::Transport(err.to_string())
    }
}

/// What to connect to. Built from a [`Host`]; secrets are supplied separately by
/// the caller (they live in the keychain, not in core).
#[derive(Debug, Clone)]
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
    /// Spawns a dedicated connection thread and returns immediately. Progress
    /// and failure arrive as [`SessionEvent`]s on [`Session::events`].
    pub fn connect(config: SessionConfig) -> Result<Self, SessionError> {
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
                    Ok(runtime) => runtime.block_on(run(config, cmd_rx, event_tx)),
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

/// Verifies the server key against the user's `~/.ssh/known_hosts`.
///
/// The check is `russh::keys::check_known_hosts` (`keys/known_hosts.rs`), which
/// hashes hostnames, handles `[host]:port`, returns `Ok(false)` for an unknown
/// host and `Err(KeyChanged)` for a mismatch. Anything other than `Ok(true)` is
/// an error here, so an unknown key fails closed instead of being accepted.
struct KnownHostsHandler {
    host: String,
    port: u16,
    /// Routes for running `-R` forwards, filled in by the connection thread.
    routes: forward::Routes,
}

impl client::Handler for KnownHostsHandler {
    type Error = SessionError;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let public_key = match server_public_key {
            PublicKeyOrCertificate::PublicKey { key, .. } => key,
            // ponytail: OpenSSH host certificates need CA-fingerprint validation
            // (ssh_key::certificate::Certificate::validate), which is a separate
            // trust decision. Until that exists, refuse rather than guess.
            PublicKeyOrCertificate::Certificate(_) => {
                return Err(SessionError::Unsupported("OpenSSH host certificate"));
            }
        };
        match check_known_hosts(&self.host, self.port, public_key) {
            Ok(true) => Ok(true),
            Ok(false) => Err(SessionError::Connect(format!(
                "host key for {}:{} is not in known_hosts",
                self.host, self.port
            ))),
            Err(err) => Err(SessionError::Connect(format!(
                "known_hosts check for {}:{} failed: {err}",
                self.host, self.port
            ))),
        }
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
async fn run(config: SessionConfig, commands: Receiver<Command>, events: Sender<SessionEvent>) {
    if let Err(err) = run_session(&config, commands, &events).await {
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
    commands: Receiver<Command>,
    events: &Sender<SessionEvent>,
) -> Result<(), SessionError> {
    emit(events, SessionEvent::State(SessionState::Connecting)).await;

    let routes = forward::Routes::default();
    let handler = KnownHostsHandler {
        host: config.address().to_string(),
        port: config.port(),
        routes: routes.clone(),
    };
    // ponytail: 1 MiB window and 32 buffered channel messages, on the modest end
    // of RUSSH-GAP.md §8's recommendation; the terminal scrollback is the only
    // unbounded growth.
    let client_config = Arc::new(client::Config {
        window_size: 1 << 20,
        channel_buffer_size: 32,
        ..Default::default()
    });

    let mut handle =
        client::connect(client_config, (config.address(), config.port()), handler).await?;

    emit(events, SessionEvent::State(SessionState::Authenticating)).await;
    authenticate(&mut handle, config).await?;
    emit(events, SessionEvent::Connected).await;

    // `Handle` is `Send` but not `Sync` (it owns the reply receiver), and the
    // forwarding tasks need `&Handle`; a mutex gives them that without moving
    // the session loop off this task.
    let handle = Arc::new(tokio::sync::Mutex::new(handle));

    let channel = {
        let connection = handle.lock().await;
        connection.channel_open_session().await?
    };
    let (mut read, write) = channel.split();
    // A real PTY + shell so interactive terminal bytes flow. The initial size is
    // a placeholder; the UI sends the first real size via `resize`.
    write
        .request_pty(true, "xterm-256color", 80, 24, 0, 0, &[])
        .await?;
    write.request_shell(true).await?;

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
}
