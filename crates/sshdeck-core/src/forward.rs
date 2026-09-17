//! Port forwarding over an existing SSH connection.
//!
//! Local (`-L`), remote (`-R`) and dynamic/SOCKS5 (`-D`) forwarding, built on
//! russh's channel APIs (`channel_open_direct_tcpip`, `tcpip_forward`,
//! `cancel_tcpip_forward`, and the `server_channel_open_forwarded_tcpip`
//! handler hook — see `docs/re/RUSSH-GAP.md`).
//!
//! Like [`crate::session`], the public surface is executor-agnostic: the caller
//! gets a [`Forward`] handle plus an `async_channel` of [`ForwardEvent`]s. No
//! tokio type appears in a `pub` signature.
//!
//! `russh::client::Handle` is `Send` but not `Sync` (it owns the reply
//! receiver), so forwarding shares it behind a tokio mutex; the individual
//! calls are short, and channel opens are naturally serialised anyway.
//!
//! `ponytail:` the listener is a raw TCP listener, one task per accepted
//! connection, no pooling. Upgrade path: a shared accept loop if a host ever
//! carries hundreds of tunnels.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use async_channel::{Receiver, Sender};
use russh::client;
use russh::Channel;
use tokio::io::{copy_bidirectional, AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// A forwarding rule.
///
/// `Local` listens on this machine and reaches the target through the server
/// (OpenSSH `-L`). `Remote` asks the server to listen and forwards connections
/// back to a target reachable from this machine (`-R`). `Dynamic` listens
/// locally and speaks SOCKS5 (`-D`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForwardConfig {
    Local {
        bind_address: String,
        bind_port: u16,
        target_host: String,
        target_port: u16,
    },
    Remote {
        bind_address: String,
        bind_port: u16,
        target_host: String,
        target_port: u16,
    },
    Dynamic {
        bind_address: String,
        bind_port: u16,
    },
}

/// A forwarding configuration or runtime failure.
#[derive(Debug, thiserror::Error)]
pub enum ForwardError {
    #[error("invalid forward: {0}")]
    Invalid(String),
    #[error("forward transport: {0}")]
    Transport(String),
    #[error("forward is no longer running")]
    Stopped,
    #[error("forward I/O: {0}")]
    Io(#[from] std::io::Error),
}

/// Everything the UI sees from a running forward.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForwardEvent {
    /// The forward is up. `port` is the actual port when one was chosen by the
    /// OS (`Local`/`Dynamic`) or by the server (`Remote`).
    Listening { address: String, port: u16 },
    /// One connection could not be established. The forward keeps listening.
    ConnectionFailed { target: String, message: String },
    /// The forward could not be started.
    Failed(String),
    /// The forward stopped (normally or after a failure).
    Stopped,
}

/// A running forward. Dropping this handle does not stop the forward; call
/// [`Forward::stop`]. (The connection thread stops it when the session closes.)
pub struct Forward {
    commands: Sender<ForwardCommand>,
    events: Receiver<ForwardEvent>,
}

impl Forward {
    /// A handle to the forward's event stream. `async_channel` is
    /// competing-consumer, so call this once and own the receiver.
    pub fn events(&self) -> Receiver<ForwardEvent> {
        self.events.clone()
    }

    /// Asks the forward task to stop. Best-effort and non-blocking.
    pub fn stop(&self) -> Result<(), ForwardError> {
        self.commands
            .try_send(ForwardCommand::Stop)
            .map_err(|_| ForwardError::Stopped)
    }

    /// Builds the UI handle and the setup the connection thread needs. Kept
    /// crate-private so `session` can hand the setup to its thread.
    pub(crate) fn channel(config: ForwardConfig) -> (Self, ForwardSetup) {
        let (commands, command_rx) = async_channel::bounded(1);
        let (event_tx, events) = async_channel::bounded(16);
        let handle = Self { commands, events };
        let setup = ForwardSetup {
            config,
            commands: command_rx,
            events: event_tx,
        };
        (handle, setup)
    }
}

/// The connection-thread side of a forward.
pub(crate) struct ForwardSetup {
    pub(crate) config: ForwardConfig,
    pub(crate) commands: Receiver<ForwardCommand>,
    pub(crate) events: Sender<ForwardEvent>,
}

/// Commands from [`Forward`] to its task.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ForwardCommand {
    Stop,
}

impl ForwardConfig {
    /// Parses an OpenSSH-style `-L`/`-R` argument:
    /// `[bind_address:]port:host:hostport`, with IPv6 addresses in brackets.
    pub fn parse_local(spec: &str) -> Result<Self, ForwardError> {
        let (bind_address, bind_port, target_host, target_port) = parse_forward_spec(spec)?;
        Ok(Self::Local {
            bind_address,
            bind_port,
            target_host,
            target_port,
        })
    }

    /// Parses an OpenSSH-style `-R` argument (same grammar as `-L`).
    pub fn parse_remote(spec: &str) -> Result<Self, ForwardError> {
        let (bind_address, bind_port, target_host, target_port) = parse_forward_spec(spec)?;
        Ok(Self::Remote {
            bind_address,
            bind_port,
            target_host,
            target_port,
        })
    }

    /// Parses an OpenSSH-style `-D` argument: `[bind_address:]port`.
    pub fn parse_dynamic(spec: &str) -> Result<Self, ForwardError> {
        let parts = split_outside_brackets(spec)?;
        let (bind_address, bind_port) = match parts.as_slice() {
            [port] => ("localhost".to_string(), parse_port(port)?),
            [bind, port] => (normalise_bind(bind), parse_port(port)?),
            _ => {
                return Err(ForwardError::Invalid(format!(
                    "dynamic forward spec {spec:?} must be [bind_address:]port"
                )))
            }
        };
        Ok(Self::Dynamic {
            bind_address,
            bind_port,
        })
    }

    /// The address this forward listens on.
    pub fn bind(&self) -> (&str, u16) {
        match self {
            Self::Local {
                bind_address,
                bind_port,
                ..
            }
            | Self::Remote {
                bind_address,
                bind_port,
                ..
            }
            | Self::Dynamic {
                bind_address,
                bind_port,
            } => (bind_address, *bind_port),
        }
    }

    /// The target of a `Local`/`Remote` forward, absent for `Dynamic` (the
    /// SOCKS client picks it per connection).
    pub fn target(&self) -> Option<(&str, u16)> {
        match self {
            Self::Local {
                target_host,
                target_port,
                ..
            }
            | Self::Remote {
                target_host,
                target_port,
                ..
            } => Some((target_host, *target_port)),
            Self::Dynamic { .. } => None,
        }
    }

    /// `"host:port"` / `"[v6]:port"` for logs and status lines.
    pub fn bind_label(&self) -> String {
        let (address, port) = self.bind();
        format_bind(address, port)
    }

    /// Short kind label.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Local { .. } => "local",
            Self::Remote { .. } => "remote",
            Self::Dynamic { .. } => "dynamic",
        }
    }

    /// Rejects configs we cannot serve, before a task is spawned.
    pub fn validate(&self) -> Result<(), ForwardError> {
        let (bind_address, _) = self.bind();
        if bind_address.trim().is_empty() {
            return Err(ForwardError::Invalid("bind address is empty".into()));
        }
        if let Some((host, port)) = self.target() {
            if host.trim().is_empty() {
                return Err(ForwardError::Invalid("target host is empty".into()));
            }
            if port == 0 {
                return Err(ForwardError::Invalid("target port is 0".into()));
            }
        }
        Ok(())
    }
}

fn format_bind(address: &str, port: u16) -> String {
    if address.contains(':') {
        format!("[{address}]:{port}")
    } else {
        format!("{address}:{port}")
    }
}

impl std::fmt::Display for ForwardConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.kind())?;
        write!(f, " {}", self.bind_label())?;
        if let Some((host, port)) = self.target() {
            write!(f, " -> {}", format_bind(host, port))?;
        }
        Ok(())
    }
}

fn split_outside_brackets(spec: &str) -> Result<Vec<&str>, ForwardError> {
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut depth = 0usize;
    for (index, ch) in spec.char_indices() {
        match ch {
            '[' => {
                if depth != 0 {
                    return Err(ForwardError::Invalid(format!(
                        "unbalanced brackets in {spec:?}"
                    )));
                }
                depth = 1;
            }
            ']' => {
                if depth != 1 {
                    return Err(ForwardError::Invalid(format!(
                        "unbalanced brackets in {spec:?}"
                    )));
                }
                depth = 0;
            }
            ':' if depth == 0 => {
                parts.push(&spec[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    if depth != 0 {
        return Err(ForwardError::Invalid(format!(
            "unbalanced brackets in {spec:?}"
        )));
    }
    parts.push(&spec[start..]);
    Ok(parts)
}

fn unbracket(part: &str) -> &str {
    part.strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(part)
}

fn normalise_bind(part: &str) -> String {
    let address = unbracket(part);
    if address.is_empty() {
        "localhost".to_string()
    } else {
        address.to_string()
    }
}

fn parse_port(part: &str) -> Result<u16, ForwardError> {
    part.parse::<u16>()
        .map_err(|_| ForwardError::Invalid(format!("{part:?} is not a port")))
}

fn parse_forward_spec(spec: &str) -> Result<(String, u16, String, u16), ForwardError> {
    let parts = split_outside_brackets(spec)?;
    let (bind_address, bind_port, host_index) = match parts.as_slice() {
        [port, _, _] => ("localhost".to_string(), parse_port(port)?, 1usize),
        [bind, port, _, _] => (normalise_bind(bind), parse_port(port)?, 2usize),
        _ => {
            return Err(ForwardError::Invalid(format!(
                "forward spec {spec:?} must be [bind_address:]port:host:hostport"
            )))
        }
    };
    let target_host = unbracket(parts[host_index]).to_string();
    let target_port = parse_port(parts[host_index + 1])?;
    Ok((bind_address, bind_port, target_host, target_port))
}

// ---------------------------------------------------------------------------
// Engine
// ---------------------------------------------------------------------------

/// One server-initiated (`-R`) route, keyed by the address/port the server
/// reports it was connected on.
#[derive(Clone)]
pub(crate) struct RemoteRoute {
    pub(crate) target_host: String,
    pub(crate) target_port: u16,
    pub(crate) events: Sender<ForwardEvent>,
}

/// Routes registered by running remote forwards, read by the session's handler
/// when the server opens a forwarded channel.
pub(crate) type Routes = Arc<Mutex<HashMap<(String, u32), RemoteRoute>>>;

type SharedHandle<H> = Arc<tokio::sync::Mutex<client::Handle<H>>>;

/// Runs one forward until stopped or the session ends.
pub(crate) async fn serve<H>(handle: SharedHandle<H>, setup: ForwardSetup, routes: Routes)
where
    H: client::Handler + 'static,
{
    let ForwardSetup {
        config,
        commands,
        events,
    } = setup;
    match &config {
        ForwardConfig::Local { .. } => serve_local(handle, &config, &commands, &events).await,
        ForwardConfig::Dynamic { .. } => serve_dynamic(handle, &config, &commands, &events).await,
        ForwardConfig::Remote { .. } => {
            serve_remote(handle, &config, routes, &commands, &events).await
        }
    }
    let _ = events.send(ForwardEvent::Stopped).await;
}

async fn serve_local<H>(
    handle: SharedHandle<H>,
    config: &ForwardConfig,
    commands: &Receiver<ForwardCommand>,
    events: &Sender<ForwardEvent>,
) where
    H: client::Handler + 'static,
{
    let (bind_address, bind_port) = config.bind();
    let Some((target_host, target_port)) = config.target() else {
        return;
    };
    let listener = match TcpListener::bind((bind_address, bind_port)).await {
        Ok(listener) => listener,
        Err(err) => {
            let _ = events
                .send(ForwardEvent::Failed(format!(
                    "could not bind {bind_address}:{bind_port}: {err}"
                )))
                .await;
            return;
        }
    };
    let port = match listener.local_addr() {
        Ok(address) => address.port(),
        Err(_) => bind_port,
    };
    let _ = events
        .send(ForwardEvent::Listening {
            address: bind_address.to_string(),
            port,
        })
        .await;

    loop {
        tokio::select! {
            _ = commands.recv() => break,
            accepted = listener.accept() => {
                let Ok((socket, peer)) = accepted else { break };
                let handle = handle.clone();
                let events = events.clone();
                let target_host = target_host.to_string();
                tokio::spawn(async move {
                    if let Err(err) =
                        open_and_bridge(&handle, socket, peer, &target_host, target_port).await
                    {
                        let _ = events
                            .send(ForwardEvent::ConnectionFailed {
                                target: format_bind(&target_host, target_port),
                                message: err.to_string(),
                            })
                            .await;
                    }
                });
            }
        }
    }
}

async fn serve_dynamic<H>(
    handle: SharedHandle<H>,
    config: &ForwardConfig,
    commands: &Receiver<ForwardCommand>,
    events: &Sender<ForwardEvent>,
) where
    H: client::Handler + 'static,
{
    let (bind_address, bind_port) = config.bind();
    let listener = match TcpListener::bind((bind_address, bind_port)).await {
        Ok(listener) => listener,
        Err(err) => {
            let _ = events
                .send(ForwardEvent::Failed(format!(
                    "could not bind {bind_address}:{bind_port}: {err}"
                )))
                .await;
            return;
        }
    };
    let port = match listener.local_addr() {
        Ok(address) => address.port(),
        Err(_) => bind_port,
    };
    let _ = events
        .send(ForwardEvent::Listening {
            address: bind_address.to_string(),
            port,
        })
        .await;

    loop {
        tokio::select! {
            _ = commands.recv() => break,
            accepted = listener.accept() => {
                let Ok((socket, peer)) = accepted else { break };
                let handle = handle.clone();
                let events = events.clone();
                tokio::spawn(serve_socks5_connection(handle, socket, peer, events));
            }
        }
    }
}

async fn serve_remote<H>(
    handle: SharedHandle<H>,
    config: &ForwardConfig,
    routes: Routes,
    commands: &Receiver<ForwardCommand>,
    events: &Sender<ForwardEvent>,
) where
    H: client::Handler + 'static,
{
    let (bind_address, bind_port) = config.bind();
    let Some((target_host, target_port)) = config.target() else {
        return;
    };
    let requested = u32::from(bind_port);
    let bound_port = {
        let connection = handle.lock().await;
        match connection
            .tcpip_forward(bind_address.to_string(), requested)
            .await
        {
            Ok(port) => port,
            Err(err) => {
                let _ = events
                    .send(ForwardEvent::Failed(format!(
                        "remote forward {bind_address}:{bind_port} was refused: {err}"
                    )))
                    .await;
                return;
            }
        }
    };

    let route = RemoteRoute {
        target_host: target_host.to_string(),
        target_port,
        events: events.clone(),
    };
    insert_route(&routes, bind_address, bound_port, route);
    let _ = events
        .send(ForwardEvent::Listening {
            address: bind_address.to_string(),
            port: u16::try_from(bound_port).unwrap_or(bind_port),
        })
        .await;

    let _ = commands.recv().await;

    {
        let connection = handle.lock().await;
        let _ = connection
            .cancel_tcpip_forward(bind_address.to_string(), bound_port)
            .await;
    }
    remove_route(&routes, bind_address, bound_port);
}

/// Opens a `direct-tcpip` channel and pipes it to `socket` until either closes.
async fn open_and_bridge<H>(
    handle: &SharedHandle<H>,
    socket: TcpStream,
    peer: SocketAddr,
    host: &str,
    port: u16,
) -> Result<(), ForwardError>
where
    H: client::Handler + 'static,
{
    let channel = open_direct(handle, host, port, peer).await?;
    bridge(channel, socket).await;
    Ok(())
}

async fn open_direct<H>(
    handle: &SharedHandle<H>,
    host: &str,
    port: u16,
    peer: SocketAddr,
) -> Result<Channel<client::Msg>, ForwardError>
where
    H: client::Handler + 'static,
{
    let connection = handle.lock().await;
    connection
        .channel_open_direct_tcpip(
            host.to_string(),
            u32::from(port),
            peer.ip().to_string(),
            u32::from(peer.port()),
        )
        .await
        .map_err(|err| ForwardError::Transport(err.to_string()))
}

async fn serve_socks5_connection<H>(
    handle: SharedHandle<H>,
    mut socket: TcpStream,
    peer: SocketAddr,
    events: Sender<ForwardEvent>,
) where
    H: client::Handler + 'static,
{
    let (host, port) = match socks5::negotiate(&mut socket).await {
        Ok(target) => target,
        // Negotiation failed; the greeting reply (if any) already told the
        // client, and dropping the socket ends it.
        Err(_) => return,
    };
    match open_direct(&handle, &host, port, peer).await {
        Ok(channel) => {
            if socket
                .write_all(&socks5::connect_reply(socks5::REPLY_SUCCEEDED))
                .await
                .is_err()
            {
                return;
            }
            bridge(channel, socket).await;
        }
        Err(err) => {
            let _ = socket
                .write_all(&socks5::connect_reply(socks5::REPLY_GENERAL_FAILURE))
                .await;
            let _ = events
                .send(ForwardEvent::ConnectionFailed {
                    target: format_bind(&host, port),
                    message: err.to_string(),
                })
                .await;
        }
    }
}

/// Pipes an SSH channel to any byte stream until either side closes.
async fn bridge<S>(channel: Channel<client::Msg>, mut socket: S)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut stream = channel.into_stream();
    let _ = copy_bidirectional(&mut stream, &mut socket).await;
}

/// Bridges a server-initiated (`-R`) channel to the route's local target.
pub(crate) async fn bridge_forwarded(
    channel: Channel<client::Msg>,
    route: RemoteRoute,
    originator: String,
) {
    match TcpStream::connect((route.target_host.as_str(), route.target_port)).await {
        Ok(socket) => bridge(channel, socket).await,
        Err(err) => {
            let _ = route
                .events
                .send(ForwardEvent::ConnectionFailed {
                    target: format_bind(&route.target_host, route.target_port),
                    message: format!("{originator}: {err}"),
                })
                .await;
        }
    }
}

/// Serves one server-initiated agent-forwarding channel by piping it to the
/// local `SSH_AUTH_SOCK`.
#[cfg(unix)]
pub(crate) async fn bridge_agent(channel: Channel<client::Msg>) {
    let agent = match std::env::var("SSH_AUTH_SOCK") {
        Ok(path) => tokio::net::UnixStream::connect(path).await.ok(),
        Err(_) => None,
    };
    match agent {
        Some(socket) => bridge(channel, socket).await,
        None => {
            let _ = channel.close().await;
        }
    }
}

/// On non-unix there is no local agent socket, so just decline the channel.
#[cfg(not(unix))]
pub(crate) async fn bridge_agent(channel: Channel<client::Msg>) {
    let _ = channel.close().await;
}

fn insert_route(routes: &Routes, address: &str, port: u32, route: RemoteRoute) {
    if let Ok(mut map) = routes.lock() {
        map.insert((address.to_string(), port), route);
    }
}

fn remove_route(routes: &Routes, address: &str, port: u32) {
    if let Ok(mut map) = routes.lock() {
        map.remove(&(address.to_string(), port));
    }
}

/// Finds the remote route a server-initiated channel belongs to. The server may
/// report a canonicalised bind address, so an unambiguous port is accepted as a
/// fallback.
pub(crate) fn resolve(routes: &Routes, address: &str, port: u32) -> Option<RemoteRoute> {
    let map = routes.lock().ok()?;
    if let Some(route) = map.get(&(address.to_string(), port)) {
        return Some(route.clone());
    }
    let mut matches = map
        .iter()
        .filter(|((_, bound_port), _)| *bound_port == port)
        .map(|(_, route)| route);
    let first = matches.next()?.clone();
    if matches.next().is_some() {
        return None;
    }
    Some(first)
}

// ---------------------------------------------------------------------------
// SOCKS5 (CONNECT only, no authentication)
// ---------------------------------------------------------------------------

/// Minimal SOCKS5 server: version 5, `CONNECT` only, "no authentication" only.
/// `BIND`/`UDP ASSOCIATE` and username/password auth are deliberately not
/// implemented.
mod socks5 {
    use super::ForwardError;
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

    pub(super) const REPLY_SUCCEEDED: u8 = 0x00;
    pub(super) const REPLY_GENERAL_FAILURE: u8 = 0x05;

    const VERSION: u8 = 0x05;
    const METHOD_NO_AUTH: u8 = 0x00;
    const METHOD_NONE_ACCEPTABLE: u8 = 0xff;
    const COMMAND_CONNECT: u8 = 0x01;
    const ATYP_IPV4: u8 = 0x01;
    const ATYP_DOMAIN: u8 = 0x03;
    const ATYP_IPV6: u8 = 0x04;
    const MAX_GREETING: usize = 512;
    const MAX_REQUEST: usize = 4096;

    /// A parsed `CONNECT` request.
    pub(super) struct Connect {
        pub(super) host: String,
        pub(super) port: u16,
    }

    /// Decodes the method-selection greeting. `Ok(None)` means more bytes are
    /// needed; `Ok(Some((method, consumed)))` is complete.
    pub(super) fn decode_greeting(buf: &[u8]) -> Result<Option<(u8, usize)>, ForwardError> {
        let (Some(&version), Some(&count)) = (buf.first(), buf.get(1)) else {
            return Ok(None);
        };
        if version != VERSION {
            return Err(ForwardError::Invalid(format!(
                "unsupported SOCKS version {version}"
            )));
        }
        let count = usize::from(count);
        let Some(methods) = buf.get(2..2 + count) else {
            return Ok(None);
        };
        let method = if methods.contains(&METHOD_NO_AUTH) {
            METHOD_NO_AUTH
        } else {
            METHOD_NONE_ACCEPTABLE
        };
        Ok(Some((method, 2 + count)))
    }

    /// Decodes a `CONNECT` request. `Ok(None)` means more bytes are needed.
    pub(super) fn decode_connect(buf: &[u8]) -> Result<Option<Connect>, ForwardError> {
        let (Some(&version), Some(&command), Some(&reserved), Some(&address_type)) =
            (buf.first(), buf.get(1), buf.get(2), buf.get(3))
        else {
            return Ok(None);
        };
        if version != VERSION {
            return Err(ForwardError::Invalid(format!(
                "unsupported SOCKS version {version}"
            )));
        }
        if command != COMMAND_CONNECT {
            return Err(ForwardError::Invalid(format!(
                "unsupported SOCKS command {command} (CONNECT only)"
            )));
        }
        if reserved != 0 {
            return Err(ForwardError::Invalid("SOCKS reserved byte is not 0".into()));
        }

        let mut offset = 4usize;
        let host = match address_type {
            ATYP_IPV4 => {
                let Some(octets) = buf.get(offset..offset + 4) else {
                    return Ok(None);
                };
                offset += 4;
                format!("{}.{}.{}.{}", octets[0], octets[1], octets[2], octets[3])
            }
            ATYP_IPV6 => {
                let Some(octets) = buf.get(offset..offset + 16) else {
                    return Ok(None);
                };
                offset += 16;
                let mut address = [0u8; 16];
                address.copy_from_slice(octets);
                std::net::Ipv6Addr::from(address).to_string()
            }
            ATYP_DOMAIN => {
                let Some(&length) = buf.get(offset) else {
                    return Ok(None);
                };
                offset += 1;
                let length = usize::from(length);
                let Some(name) = buf.get(offset..offset + length) else {
                    return Ok(None);
                };
                offset += length;
                std::str::from_utf8(name)
                    .map_err(|_| ForwardError::Invalid("SOCKS domain is not UTF-8".into()))?
                    .to_string()
            }
            other => {
                return Err(ForwardError::Invalid(format!(
                    "unsupported SOCKS address type {other}"
                )))
            }
        };
        let Some(port_bytes) = buf.get(offset..offset + 2) else {
            return Ok(None);
        };
        let port = u16::from_be_bytes([port_bytes[0], port_bytes[1]]);
        Ok(Some(Connect { host, port }))
    }

    /// `VER`, chosen method.
    pub(super) fn greeting_reply(method: u8) -> [u8; 2] {
        [VERSION, method]
    }

    /// `VER`, status, reserved, `ATYP=IPv4`, `BND.ADDR=0.0.0.0`, `BND.PORT=0`.
    pub(super) fn connect_reply(status: u8) -> [u8; 10] {
        [VERSION, status, 0, ATYP_IPV4, 0, 0, 0, 0, 0, 0]
    }

    /// Reads the greeting and a `CONNECT` request, replying to the greeting.
    /// Returns the requested target; the caller replies to `CONNECT` once the
    /// SSH channel is open (or failed).
    pub(super) async fn negotiate<S>(stream: &mut S) -> Result<(String, u16), ForwardError>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let mut chunk = [0u8; 128];
        let mut greeting = Vec::new();
        loop {
            if let Some((method, consumed)) = decode_greeting(&greeting)? {
                stream.write_all(&greeting_reply(method)).await?;
                if method != METHOD_NO_AUTH {
                    return Err(ForwardError::Invalid(
                        "SOCKS client offered no supported auth method".into(),
                    ));
                }
                // Keep any bytes that arrived after the greeting.
                greeting.drain(..consumed);
                break;
            }
            if greeting.len() > MAX_GREETING {
                return Err(ForwardError::Invalid("SOCKS greeting too large".into()));
            }
            let read = stream.read(&mut chunk).await?;
            if read == 0 {
                return Err(ForwardError::Invalid(
                    "SOCKS client closed during greeting".into(),
                ));
            }
            greeting.extend_from_slice(&chunk[..read]);
        }

        let mut request = greeting;
        loop {
            if let Some(connect) = decode_connect(&request)? {
                return Ok((connect.host, connect.port));
            }
            if request.len() > MAX_REQUEST {
                return Err(ForwardError::Invalid("SOCKS request too large".into()));
            }
            let read = stream.read(&mut chunk).await?;
            if read == 0 {
                return Err(ForwardError::Invalid(
                    "SOCKS client closed during CONNECT".into(),
                ));
            }
            request.extend_from_slice(&chunk[..read]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(host: &str, port: u16) -> RemoteRoute {
        let (events, _events_rx) = async_channel::bounded(1);
        RemoteRoute {
            target_host: host.to_string(),
            target_port: port,
            events,
        }
    }

    #[test]
    fn parses_local_specs() {
        assert_eq!(
            ForwardConfig::parse_local("8080:db.internal:5432").expect("parses"),
            ForwardConfig::Local {
                bind_address: "localhost".into(),
                bind_port: 8080,
                target_host: "db.internal".into(),
                target_port: 5432,
            }
        );
        assert_eq!(
            ForwardConfig::parse_local("0.0.0.0:8080:db.internal:5432").expect("parses"),
            ForwardConfig::Local {
                bind_address: "0.0.0.0".into(),
                bind_port: 8080,
                target_host: "db.internal".into(),
                target_port: 5432,
            }
        );
        assert_eq!(
            ForwardConfig::parse_remote("[::1]:8080:db.internal:5432").expect("parses"),
            ForwardConfig::Remote {
                bind_address: "::1".into(),
                bind_port: 8080,
                target_host: "db.internal".into(),
                target_port: 5432,
            }
        );
        assert_eq!(
            ForwardConfig::parse_local("[2001:db8::1]:8080:[2001:db8::2]:5432").expect("parses"),
            ForwardConfig::Local {
                bind_address: "2001:db8::1".into(),
                bind_port: 8080,
                target_host: "2001:db8::2".into(),
                target_port: 5432,
            }
        );
    }

    #[test]
    fn parses_dynamic_specs_and_rejects_junk() {
        assert_eq!(
            ForwardConfig::parse_dynamic("1080").expect("parses"),
            ForwardConfig::Dynamic {
                bind_address: "localhost".into(),
                bind_port: 1080,
            }
        );
        assert_eq!(
            ForwardConfig::parse_dynamic("127.0.0.1:1080").expect("parses"),
            ForwardConfig::Dynamic {
                bind_address: "127.0.0.1".into(),
                bind_port: 1080,
            }
        );
        assert!(ForwardConfig::parse_dynamic("localhost:1080:extra").is_err());
        assert!(ForwardConfig::parse_local("8080:host").is_err());
        assert!(ForwardConfig::parse_local("[::1:8080:host:22").is_err());
        assert!(ForwardConfig::parse_local("notaport:host:22").is_err());
    }

    #[test]
    fn bind_labels_bracket_ipv6() {
        let local = ForwardConfig::parse_local("8080:db:5432").expect("parses");
        assert_eq!(local.bind_label(), "localhost:8080");
        assert_eq!(local.to_string(), "local localhost:8080 -> db:5432");

        let v6 = ForwardConfig::parse_local("[::1]:8080:db:5432").expect("parses");
        assert_eq!(v6.bind_label(), "[::1]:8080");
    }

    #[test]
    fn validate_rejects_empty_targets() {
        let mut config = ForwardConfig::parse_local("8080:db:5432").expect("parses");
        assert!(config.validate().is_ok());
        if let ForwardConfig::Local { target_host, .. } = &mut config {
            target_host.clear();
        }
        assert!(config.validate().is_err());
    }

    #[test]
    fn socks5_greeting_selects_no_auth() {
        // VER=5, NMETHODS=1, NO_AUTH
        assert_eq!(
            socks5::decode_greeting(&[5, 1, 0]).expect("decodes"),
            Some((0, 3))
        );
        // No acceptable method -> 0xff, still consumed.
        assert_eq!(
            socks5::decode_greeting(&[5, 1, 2]).expect("decodes"),
            Some((0xff, 3))
        );
        // Incomplete.
        assert_eq!(socks5::decode_greeting(&[5]).expect("decodes"), None);
        assert_eq!(socks5::greeting_reply(0), [5, 0]);
    }

    #[test]
    fn socks5_canned_connect_request_decodes_and_replies() {
        // VER=5, CMD=CONNECT, RSV=0, ATYP=DOMAIN, len=11, "example.com", port=443
        let mut request = vec![5, 1, 0, 3, 11];
        request.extend_from_slice(b"example.com");
        request.extend_from_slice(&443u16.to_be_bytes());
        let connect = socks5::decode_connect(&request)
            .expect("decodes")
            .expect("complete");
        assert_eq!(connect.host, "example.com");
        assert_eq!(connect.port, 443);
        assert_eq!(socks5::connect_reply(0), [5, 0, 0, 1, 0, 0, 0, 0, 0, 0]);
        assert_eq!(socks5::connect_reply(5), [5, 5, 0, 1, 0, 0, 0, 0, 0, 0]);

        // Truncated request asks for more bytes.
        assert!(socks5::decode_connect(&request[..request.len() - 1])
            .expect("decodes")
            .is_none());

        // IPv4 literal.
        let ipv4 = [5, 1, 0, 1, 127, 0, 0, 1, 0x1f, 0x90];
        let connect = socks5::decode_connect(&ipv4)
            .expect("decodes")
            .expect("complete");
        assert_eq!(connect.host, "127.0.0.1");
        assert_eq!(connect.port, 8080);

        // Unsupported command is rejected.
        let bad = [5, 2, 0, 1, 127, 0, 0, 1, 0, 80];
        assert!(socks5::decode_connect(&bad).is_err());
    }

    #[test]
    fn resolve_matches_exact_then_unambiguous_port() {
        let routes: Routes = Routes::default();
        insert_route(&routes, "127.0.0.1", 9000, route("db.internal", 5432));

        let exact = resolve(&routes, "127.0.0.1", 9000).expect("exact match");
        assert_eq!(exact.target_host, "db.internal");
        // Server reported a canonicalised bind address for the same port.
        assert!(resolve(&routes, "0.0.0.0", 9000).is_some());
        // Unknown port.
        assert!(resolve(&routes, "127.0.0.1", 9001).is_none());

        // Two forwards on the same port: the port fallback is now ambiguous.
        insert_route(&routes, "::1", 9000, route("other.internal", 22));
        assert!(resolve(&routes, "127.0.0.1", 9000).is_some());
        assert!(resolve(&routes, "0.0.0.0", 9000).is_none());
    }
}
