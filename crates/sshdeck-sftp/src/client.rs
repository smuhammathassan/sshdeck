//! The high-level SFTP client.
//!
//! [`SftpClient`] is an executor-agnostic handle: every method is an `async fn`
//! that only awaits `async_channel`, so it can be called from GPUI or any other
//! executor. The SFTP protocol itself runs on the connection session's runtime
//! (see [`sshdeck_core::sftp`]), driven by a single task this module spawns.

use std::path::PathBuf;
use std::sync::Arc;

use async_channel::{Receiver, Sender, TrySendError};
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::{FileAttributes, FileType, OpenFlags};
use sshdeck_core::session::Session;

use crate::listing::{sort_entries, DirEntry, FileKind, FileStat};
use crate::stream::RemoteStream;
use crate::transfer::{
    CancelToken, ProgressSink, Transfer, TransferDirection, TransferEvent, TransferExecutor,
    TransferFuture, TransferId, TransferOutcome, TransferQueue, DEFAULT_TRANSFER_CONCURRENCY,
};
use crate::SftpError;

/// Commands sent from the UI thread to the SFTP driver.
const COMMAND_CAPACITY: usize = 32;
/// Transfer progress events buffered before the producer is asked to wait.
const EVENT_CAPACITY: usize = 64;
/// Bytes moved per transfer read/write.
const TRANSFER_CHUNK: usize = 64 * 1024;

enum Command {
    List {
        path: String,
        reply: Sender<Result<Vec<DirEntry>, SftpError>>,
    },
    Stat {
        path: String,
        reply: Sender<Result<FileStat, SftpError>>,
    },
    Read {
        path: String,
        reply: Sender<Result<Vec<u8>, SftpError>>,
    },
    Write {
        path: String,
        data: Vec<u8>,
        reply: Sender<Result<(), SftpError>>,
    },
    Rename {
        from: String,
        to: String,
        reply: Sender<Result<(), SftpError>>,
    },
    Remove {
        path: String,
        reply: Sender<Result<(), SftpError>>,
    },
    CreateDir {
        path: String,
        reply: Sender<Result<(), SftpError>>,
    },
    RemoveDir {
        path: String,
        reply: Sender<Result<(), SftpError>>,
    },
    Chmod {
        path: String,
        mode: u32,
        reply: Sender<Result<(), SftpError>>,
    },
    Symlink {
        target: String,
        link_path: String,
        reply: Sender<Result<(), SftpError>>,
    },
    Canonicalize {
        path: String,
        reply: Sender<Result<String, SftpError>>,
    },
    Enqueue {
        transfer: Transfer,
        reply: Sender<Result<TransferId, SftpError>>,
    },
    Cancel {
        id: TransferId,
        reply: Sender<Result<bool, SftpError>>,
    },
}

/// A live SFTP session over an existing SSH connection.
pub struct SftpClient {
    commands: Sender<Command>,
    events: Receiver<TransferEvent>,
}

impl SftpClient {
    /// Opens SFTP on `session` and waits until the subsystem and the protocol
    /// handshake are up. Uses [`DEFAULT_TRANSFER_CONCURRENCY`] workers.
    pub async fn connect(session: &Session) -> Result<Self, SftpError> {
        Self::connect_with(session, DEFAULT_TRANSFER_CONCURRENCY).await
    }

    /// As [`Self::connect`], with an explicit transfer-concurrency cap.
    pub async fn connect_with(session: &Session, concurrency: usize) -> Result<Self, SftpError> {
        let channel = session.open_sftp()?;
        let (incoming, outgoing) = channel.split();
        let opened = channel.opened();
        let (commands, command_rx) = async_channel::bounded(COMMAND_CAPACITY);
        let (events, event_rx) = async_channel::bounded(EVENT_CAPACITY);
        let (ready, ready_rx) = async_channel::bounded(1);

        channel.spawn(run_driver(
            RemoteStream::new(incoming, outgoing),
            opened,
            command_rx,
            events,
            ready,
            concurrency.max(1),
        ))?;

        match ready_rx.recv().await {
            Ok(Ok(())) => Ok(Self {
                commands,
                events: event_rx,
            }),
            Ok(Err(err)) => Err(err),
            Err(_) => Err(SftpError::Closed),
        }
    }

    /// Progress events for enqueued transfers.
    pub fn transfers(&self) -> Receiver<TransferEvent> {
        self.events.clone()
    }

    /// Lists a remote directory, sorted directories-first then by
    /// case-insensitive name.
    pub async fn list(&self, path: &str) -> Result<Vec<DirEntry>, SftpError> {
        let path = path.to_string();
        self.request(|reply| Command::List { path, reply }).await
    }

    /// Metadata for one remote path.
    pub async fn stat(&self, path: &str) -> Result<FileStat, SftpError> {
        let path = path.to_string();
        self.request(|reply| Command::Stat { path, reply }).await
    }

    /// Reads a whole remote file.
    pub async fn read(&self, path: &str) -> Result<Vec<u8>, SftpError> {
        let path = path.to_string();
        self.request(|reply| Command::Read { path, reply }).await
    }

    /// Creates or truncates a remote file and writes `data` to it.
    pub async fn write(&self, path: &str, data: &[u8]) -> Result<(), SftpError> {
        let path = path.to_string();
        let data = data.to_vec();
        self.request(|reply| Command::Write { path, data, reply })
            .await
    }

    /// Renames or moves a remote file or directory.
    pub async fn rename(&self, from: &str, to: &str) -> Result<(), SftpError> {
        let from = from.to_string();
        let to = to.to_string();
        self.request(|reply| Command::Rename { from, to, reply })
            .await
    }

    /// Removes a remote file.
    pub async fn remove(&self, path: &str) -> Result<(), SftpError> {
        let path = path.to_string();
        self.request(|reply| Command::Remove { path, reply }).await
    }

    /// Creates a remote directory.
    pub async fn create_dir(&self, path: &str) -> Result<(), SftpError> {
        let path = path.to_string();
        self.request(|reply| Command::CreateDir { path, reply })
            .await
    }

    /// Removes an empty remote directory.
    pub async fn remove_dir(&self, path: &str) -> Result<(), SftpError> {
        let path = path.to_string();
        self.request(|reply| Command::RemoveDir { path, reply })
            .await
    }

    /// Sets a remote path's permission bits. Higher mode bits (setuid/setgid/
    /// sticky) are honoured; type bits are ignored.
    pub async fn chmod(&self, path: &str, mode: u32) -> Result<(), SftpError> {
        let path = path.to_string();
        let mode = mode & 0o7777;
        self.request(|reply| Command::Chmod { path, mode, reply })
            .await
    }

    /// Creates a symbolic link at `link_path` pointing at `target`.
    pub async fn symlink(&self, target: &str, link_path: &str) -> Result<(), SftpError> {
        let target = target.to_string();
        let link_path = link_path.to_string();
        self.request(|reply| Command::Symlink {
            target,
            link_path,
            reply,
        })
        .await
    }

    /// Resolves a remote path to its canonical absolute form.
    pub async fn canonicalize(&self, path: &str) -> Result<String, SftpError> {
        let path = path.to_string();
        self.request(|reply| Command::Canonicalize { path, reply })
            .await
    }

    /// Queues an upload, taking the total size from the local file.
    pub async fn upload(
        &self,
        local: impl Into<PathBuf>,
        remote: impl Into<String>,
    ) -> Result<TransferId, SftpError> {
        let local = local.into();
        let total = std::fs::metadata(&local).ok().map(|meta| meta.len());
        let transfer = Transfer::upload(local, remote, total);
        self.request(|reply| Command::Enqueue { transfer, reply })
            .await
    }

    /// Queues a download, asking the server for the total size first.
    pub async fn download(
        &self,
        remote: impl Into<String>,
        local: impl Into<PathBuf>,
    ) -> Result<TransferId, SftpError> {
        let remote = remote.into();
        let local = local.into();
        let total = self.stat(&remote).await.ok().map(|stat| stat.size());
        let transfer = Transfer::download(remote, local, total);
        self.request(|reply| Command::Enqueue { transfer, reply })
            .await
    }

    /// Cancels a queued or running transfer. Returns `false` if it is unknown
    /// or already finished.
    pub async fn cancel_transfer(&self, id: TransferId) -> Result<bool, SftpError> {
        self.request(|reply| Command::Cancel { id, reply }).await
    }

    async fn request<T, F>(&self, build: F) -> Result<T, SftpError>
    where
        F: FnOnce(Sender<Result<T, SftpError>>) -> Command,
    {
        let (reply, response) = async_channel::bounded(1);
        self.commands
            .try_send(build(reply))
            .map_err(|err| match err {
                TrySendError::Full(_) => SftpError::Backpressure,
                TrySendError::Closed(_) => SftpError::Closed,
            })?;

        match response.recv().await {
            Ok(result) => result,
            Err(_) => Err(SftpError::Closed),
        }
    }
}

/// The single task that owns the SFTP protocol session. Runs on the connection
/// session's runtime, where `russh-sftp` may spawn its own tasks.
async fn run_driver(
    stream: RemoteStream,
    opened: Receiver<Result<(), String>>,
    commands: Receiver<Command>,
    events: Sender<TransferEvent>,
    ready: Sender<Result<(), SftpError>>,
    concurrency: usize,
) {
    let startup = async {
        match opened.recv().await {
            Ok(Ok(())) => {}
            Ok(Err(message)) => return Err(SftpError::Transport(message)),
            Err(_) => return Err(SftpError::Closed),
        }
        let session = SftpSession::new(stream)
            .await
            .map_err(|err| SftpError::Remote(err.to_string()))?;
        Ok::<_, SftpError>(Arc::new(session))
    }
    .await;

    let sftp = match startup {
        Ok(sftp) => sftp,
        Err(err) => {
            let _ = ready.send(Err(err)).await;
            return;
        }
    };
    let _ = ready.send(Ok(())).await;

    let queue = TransferQueue::new(events);
    let executor: Arc<dyn TransferExecutor> = Arc::new(LiveExecutor { sftp: sftp.clone() });
    for _ in 0..concurrency {
        tokio::spawn(TransferQueue::run_worker(queue.clone(), executor.clone()));
    }

    while let Ok(command) = commands.recv().await {
        dispatch(&sftp, &queue, command);
    }
}

/// Runs one command in its own task so a slow operation cannot block the
/// command loop. The shared session multiplexes them.
fn dispatch(sftp: &Arc<SftpSession>, queue: &TransferQueue, command: Command) {
    let sftp = sftp.clone();
    let queue = queue.clone();
    tokio::spawn(async move {
        match command {
            Command::List { path, reply } => {
                let _ = reply.send(list(&sftp, &path).await).await;
            }
            Command::Stat { path, reply } => {
                let _ = reply.send(stat(&sftp, &path).await).await;
            }
            Command::Read { path, reply } => {
                let result = sftp.read(path).await.map_err(remote);
                let _ = reply.send(result).await;
            }
            Command::Write { path, data, reply } => {
                let result = write(&sftp, &path, &data).await;
                let _ = reply.send(result).await;
            }
            Command::Rename { from, to, reply } => {
                let result = sftp.rename(from, to).await.map_err(remote);
                let _ = reply.send(result).await;
            }
            Command::Remove { path, reply } => {
                let result = sftp.remove_file(path).await.map_err(remote);
                let _ = reply.send(result).await;
            }
            Command::CreateDir { path, reply } => {
                let result = sftp.create_dir(path).await.map_err(remote);
                let _ = reply.send(result).await;
            }
            Command::RemoveDir { path, reply } => {
                let result = sftp.remove_dir(path).await.map_err(remote);
                let _ = reply.send(result).await;
            }
            Command::Chmod { path, mode, reply } => {
                let attributes = FileAttributes {
                    permissions: Some(mode),
                    ..FileAttributes::empty()
                };
                let result = sftp.set_metadata(path, attributes).await.map_err(remote);
                let _ = reply.send(result).await;
            }
            Command::Symlink {
                target,
                link_path,
                reply,
            } => {
                let result = sftp.symlink(link_path, target).await.map_err(remote);
                let _ = reply.send(result).await;
            }
            Command::Canonicalize { path, reply } => {
                let result = sftp.canonicalize(path).await.map_err(remote);
                let _ = reply.send(result).await;
            }
            Command::Enqueue { transfer, reply } => {
                let _ = reply.send(Ok(queue.enqueue(transfer).await)).await;
            }
            Command::Cancel { id, reply } => {
                let _ = reply.send(Ok(queue.cancel(id).await)).await;
            }
        }
    });
}

async fn list(sftp: &SftpSession, path: &str) -> Result<Vec<DirEntry>, SftpError> {
    let mut entries: Vec<DirEntry> = sftp
        .read_dir(path)
        .await
        .map_err(remote)?
        .map(entry_from)
        .collect();
    sort_entries(&mut entries);
    Ok(entries)
}

async fn stat(sftp: &SftpSession, path: &str) -> Result<FileStat, SftpError> {
    let meta = sftp.metadata(path).await.map_err(remote)?;
    Ok(FileStat::new(
        meta.len(),
        kind_from(meta.file_type()),
        meta.permissions,
        meta.modified().ok(),
        meta.uid,
        meta.gid,
    ))
}

async fn write(sftp: &SftpSession, path: &str, data: &[u8]) -> Result<(), SftpError> {
    use tokio::io::AsyncWriteExt;

    let mut file = sftp
        .open_with_flags(
            path,
            OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE,
        )
        .await
        .map_err(remote)?;
    file.write_all(data).await?;
    file.close().await?;
    Ok(())
}

fn entry_from(entry: russh_sftp::client::fs::DirEntry) -> DirEntry {
    let metadata = entry.metadata();
    DirEntry::new(
        entry.file_name(),
        metadata.len(),
        kind_from(metadata.file_type()),
        metadata.permissions,
        metadata.modified().ok(),
    )
}

fn kind_from(kind: FileType) -> FileKind {
    match kind {
        FileType::Dir => FileKind::Dir,
        FileType::File => FileKind::File,
        FileType::Symlink => FileKind::Symlink,
        FileType::Other => FileKind::Other,
    }
}

fn remote(err: russh_sftp::client::error::Error) -> SftpError {
    SftpError::Remote(err.to_string())
}

/// Moves one queued transfer against the live session.
struct LiveExecutor {
    sftp: Arc<SftpSession>,
}

impl TransferExecutor for LiveExecutor {
    fn execute(
        &self,
        transfer: Transfer,
        progress: ProgressSink,
        cancel: CancelToken,
    ) -> TransferFuture {
        let sftp = self.sftp.clone();
        Box::pin(async move {
            match transfer.direction() {
                TransferDirection::Upload => upload(&sftp, &transfer, &progress, &cancel).await,
                TransferDirection::Download => download(&sftp, &transfer, &progress, &cancel).await,
            }
        })
    }
}

async fn upload(
    sftp: &SftpSession,
    transfer: &Transfer,
    progress: &ProgressSink,
    cancel: &CancelToken,
) -> Result<TransferOutcome, SftpError> {
    use std::io::Read as _;
    use tokio::io::AsyncWriteExt;

    let mut local = std::fs::File::open(transfer.local_path())?;
    let mut remote = sftp
        .open_with_flags(
            transfer.remote_path(),
            OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE,
        )
        .await
        .map_err(remote)?;

    let mut buffer = vec![0u8; TRANSFER_CHUNK];
    let mut done = 0u64;
    loop {
        if cancel.is_cancelled() {
            let _ = remote.close().await;
            return Ok(TransferOutcome::Cancelled);
        }
        let read = local.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        remote.write_all(&buffer[..read]).await?;
        done += read as u64;
        progress.report(done);
    }

    remote.close().await?;
    Ok(TransferOutcome::Complete)
}

async fn download(
    sftp: &SftpSession,
    transfer: &Transfer,
    progress: &ProgressSink,
    cancel: &CancelToken,
) -> Result<TransferOutcome, SftpError> {
    use std::io::Write as _;
    use tokio::io::AsyncReadExt;

    let mut remote = sftp.open(transfer.remote_path()).await.map_err(remote)?;
    let mut local = std::fs::File::create(transfer.local_path())?;

    let mut buffer = vec![0u8; TRANSFER_CHUNK];
    let mut done = 0u64;
    loop {
        if cancel.is_cancelled() {
            let _ = remote.close().await;
            return Ok(TransferOutcome::Cancelled);
        }
        let read = remote.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        local.write_all(&buffer[..read])?;
        done += read as u64;
        progress.report(done);
    }

    remote.close().await?;
    Ok(TransferOutcome::Complete)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::block_on;
    use sshdeck_core::session::{SessionConfig, SessionEvent};
    use sshdeck_core::{AuthMethod, Host};

    /// Live check. Skipped by default; run with
    /// `cargo test -p sshdeck-sftp -- --ignored` and `SSHDECK_TEST_HOST`,
    /// `SSHDECK_TEST_USER`, `SSHDECK_TEST_PASSWORD` (optional
    /// `SSHDECK_TEST_PORT`) pointing at a throwaway sshd with SFTP enabled.
    #[test]
    #[ignore = "needs a live sshd with the sftp subsystem"]
    fn lists_a_live_host() {
        let (Ok(address), Ok(username), Ok(password)) = (
            std::env::var("SSHDECK_TEST_HOST"),
            std::env::var("SSHDECK_TEST_USER"),
            std::env::var("SSHDECK_TEST_PASSWORD"),
        ) else {
            return;
        };

        let mut host = Host::new("live", address);
        host.username = username;
        host.port = std::env::var("SSHDECK_TEST_PORT")
            .ok()
            .and_then(|port| port.parse().ok())
            .unwrap_or(22);
        host.auth = AuthMethod::Password {
            secret_ref: "test".into(),
        };

        let session = sshdeck_core::session::Session::connect(
            SessionConfig::from_host(&host).with_password(password),
        )
        .expect("spawns the connection thread");
        let events = session.events();
        loop {
            match events.recv_blocking() {
                Ok(SessionEvent::Connected) => break,
                Ok(SessionEvent::Error(message)) => panic!("{message}"),
                Ok(_) => {}
                Err(err) => panic!("event channel closed: {err}"),
            }
        }

        let client = block_on(SftpClient::connect(&session)).expect("opens SFTP");
        let entries = block_on(client.list(".")).expect("lists the home directory");
        assert!(!entries.is_empty());
    }
}
