//! Raw SFTP subsystem transport.
//!
//! [`Session::open_sftp`](crate::session::Session::open_sftp) opens the `sftp`
//! subsystem on the existing SSH connection and bridges its bytes to an
//! in-process duplex, [`SftpChannel`]. Like [`crate::forward`], the handle is
//! plain data over bounded `async_channel`s: no tokio type appears in a `pub`
//! signature, so the SFTP protocol layer in `sshdeck-sftp` can drive it from
//! any executor.
//!
//! The channel itself is *not* driven from the caller's executor. The connection
//! thread owns the only tokio runtime (`session.rs`), so [`serve`] runs the byte
//! pump and every future handed to [`SftpChannel::spawn`] on that runtime. This
//! is what lets `russh-sftp` (which calls `tokio::spawn` and `tokio::time`
//! internally) work without a second runtime.
//!
//! ```text
//!   ssh channel ──pump──► incoming ──► ChannelStream (sshdeck-sftp) ──► SftpSession
//!   ssh channel ◄─pump──  outgoing ◄── ChannelStream ◄──────────────────┘
//! ```

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use async_channel::{Receiver, Sender, TrySendError};
use bytes::Bytes;
use russh::{client, ChannelMsg, ChannelStream};

use crate::session::SessionError;

/// Bytes buffered in either direction before the pump applies backpressure.
///
/// ponytail: 16 messages caps a stalled reader at ~4 MiB (32 KiB pump chunks),
/// which matters on an 8 GB box. Upgrade path: size it from the negotiated
/// channel window if throughput ever becomes the bottleneck.
const BUFFER: usize = 16;

/// Read size for the byte pump. Matches the channel window's typical chunk.
const PUMP_CHUNK: usize = 32 * 1024;

/// How long to wait for the server to accept the sftp subsystem. Without this a
/// server that never answers the subsystem request would hang the caller.
const OPEN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// A future to run on the connection's runtime, boxed so it can cross the
/// `async_channel` boundary. `std::future::Future`, not a tokio type.
pub(crate) struct SftpJob(Pin<Box<dyn Future<Output = ()> + Send + 'static>>);

/// The connection-thread side of an opened SFTP channel.
pub(crate) struct SftpSetup {
    pub(crate) incoming: Sender<Bytes>,
    pub(crate) outgoing: Receiver<Bytes>,
    pub(crate) jobs: Receiver<SftpJob>,
    pub(crate) opened: Sender<Result<(), String>>,
}

/// An opaque, executor-agnostic handle to an SFTP subsystem channel.
///
/// Bytes read from the remote side arrive on the [`SftpChannel::split`]
/// receiver; bytes written to the sender go to the remote side. Futures
/// submitted with [`SftpChannel::spawn`] run on the connection's own tokio
/// runtime.
pub struct SftpChannel {
    incoming: Receiver<Bytes>,
    outgoing: Sender<Bytes>,
    jobs: Sender<SftpJob>,
    opened: Receiver<Result<(), String>>,
}

impl SftpChannel {
    /// Builds the handle and the connection-thread setup. Crate-private; the
    /// only entry point is [`Session::open_sftp`](crate::session::Session::open_sftp).
    pub(crate) fn open() -> (Self, SftpSetup) {
        let (incoming_tx, incoming) = async_channel::bounded(BUFFER);
        let (outgoing, outgoing_rx) = async_channel::bounded(BUFFER);
        let (jobs, jobs_rx) = async_channel::bounded(BUFFER);
        let (opened_tx, opened) = async_channel::bounded(1);
        (
            Self {
                incoming,
                outgoing,
                jobs,
                opened,
            },
            SftpSetup {
                incoming: incoming_tx,
                outgoing: outgoing_rx,
                jobs: jobs_rx,
                opened: opened_tx,
            },
        )
    }

    /// The two byte streams: `(remote -> caller, caller -> remote)`.
    pub fn split(&self) -> (Receiver<Bytes>, Sender<Bytes>) {
        (self.incoming.clone(), self.outgoing.clone())
    }

    /// Resolves once the subsystem has started: `Ok(())`, or the reason it did
    /// not. Cloning is cheap; the sender is dropped after the first message.
    pub fn opened(&self) -> Receiver<Result<(), String>> {
        self.opened.clone()
    }

    /// Runs `future` on the runtime that owns the SSH connection. Non-blocking;
    /// returns [`SessionError::Backpressure`] if the connection thread is behind,
    /// or [`SessionError::Closed`] if the session is gone.
    pub fn spawn<F>(&self, future: F) -> Result<(), SessionError>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let job: Pin<Box<dyn Future<Output = ()> + Send + 'static>> = Box::pin(future);
        self.jobs.try_send(SftpJob(job)).map_err(|err| match err {
            TrySendError::Full(_) => SessionError::Backpressure,
            TrySendError::Closed(_) => SessionError::Closed,
        })
    }

    /// Ends the channel in both directions. Best-effort.
    pub fn close(&self) {
        self.outgoing.close();
        self.incoming.close();
    }
}

type SharedHandle<H> = Arc<tokio::sync::Mutex<client::Handle<H>>>;

/// Opens the SFTP subsystem and keeps its job queue alive on the connection
/// runtime. Runs as a task spawned by `session::run_session`.
pub(crate) async fn serve<H>(handle: SharedHandle<H>, setup: SftpSetup)
where
    H: client::Handler + 'static,
{
    let SftpSetup {
        incoming,
        outgoing,
        jobs,
        opened,
    } = setup;

    let opened_result = match tokio::time::timeout(OPEN_TIMEOUT, open_subsystem(&handle)).await {
        Ok(result) => result,
        Err(_) => Err("timed out waiting for the sftp subsystem".to_string()),
    };
    let stream = match opened_result {
        Ok(stream) => stream,
        Err(message) => {
            let _ = opened.send(Err(message)).await;
            return;
        }
    };
    if opened.send(Ok(())).await.is_err() {
        // The caller dropped the handle before the subsystem came up.
        return;
    }

    // ponytail: the pump and every submitted job are detached. When the caller
    // drops the handle the byte channels close, the pump exits, and jobs drain.
    // Upgrade path: keep the join handles if a hard shutdown ever matters.
    tokio::spawn(pump(stream, incoming, outgoing));
    while let Ok(job) = jobs.recv().await {
        tokio::spawn(job.0);
    }
}

/// Opens a session channel and starts the `sftp` subsystem, waiting for the
/// server's acknowledgement.
async fn open_subsystem<H>(handle: &SharedHandle<H>) -> Result<ChannelStream<client::Msg>, String>
where
    H: client::Handler + 'static,
{
    let channel = {
        let connection = handle.lock().await;
        connection
            .channel_open_session()
            .await
            .map_err(|err| format!("could not open an SFTP channel: {err}"))?
    };

    channel
        .request_subsystem(true, "sftp")
        .await
        .map_err(|err| format!("could not request the sftp subsystem: {err}"))?;

    // ponytail: no timeout. A server that accepts the request and never answers
    // leaves the caller waiting on `opened`, which the caller can abandon by
    // dropping the handle. Upgrade path: a tokio::time::timeout here.
    let mut channel = channel;
    loop {
        match channel.wait().await {
            Some(ChannelMsg::Success) => break,
            Some(ChannelMsg::Failure) | Some(ChannelMsg::OpenFailure(_)) => {
                return Err("the server refused the sftp subsystem".to_string());
            }
            Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => {
                return Err("the SFTP channel closed before the subsystem started".to_string());
            }
            // The server sends nothing until we send INIT, so a stray message
            // here is ignorable rather than data we are dropping.
            Some(_) => {}
        }
    }

    Ok(channel.into_stream())
}

/// Copies bytes between the SSH channel and the in-process duplex until either
/// side closes.
async fn pump<S>(stream: S, incoming: Sender<Bytes>, outgoing: Receiver<Bytes>)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (mut reader, mut writer) = tokio::io::split(stream);

    let to_remote = async {
        while let Ok(chunk) = outgoing.recv().await {
            if writer.write_all(&chunk).await.is_err() {
                break;
            }
        }
        let _ = writer.shutdown().await;
    };

    let from_remote = async {
        let mut buffer = vec![0u8; PUMP_CHUNK];
        loop {
            match reader.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    if incoming
                        .send(Bytes::copy_from_slice(&buffer[..read]))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
        // Wake the consumer's pending read so it sees EOF instead of parking.
        incoming.close();
    };

    tokio::join!(to_remote, from_remote);
}
