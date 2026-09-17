//! The high-level SFTP client.
//!
//! [`SftpClient`] is an executor-agnostic handle: every method is an `async fn`
//! that only awaits `async_channel`, so it can be called from GPUI or any other
//! executor. The SFTP protocol itself runs on the connection session's runtime
//! (see [`sshdeck_core::sftp`]), driven by a single task this module spawns.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_channel::{Receiver, Sender, TrySendError};
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::{FileAttributes, FileType, OpenFlags};
use sshdeck_core::session::Session;

use crate::listing::{sort_entries, DirEntry, FileKind, FileStat};
use crate::partial::{self, ResumeDecision};
use crate::stream::RemoteStream;
use crate::transfer::{
    CancelToken, PartialDisposition, ProgressSink, Transfer, TransferDirection, TransferEvent,
    TransferExecutor, TransferFuture, TransferId, TransferOutcome, TransferQueue,
    DEFAULT_TRANSFER_CONCURRENCY,
};
use crate::tree::{self, TreeEntry, TreeFs, TreeFuture};
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

    /// Creates `path` under `base`, creating missing parents, like `mkdir -p`.
    ///
    /// `path` may be relative to `base` or absolute; it may not escape `base`.
    /// The escape check is [`crate::path::ancestors`], which goes through
    /// [`crate::path::join`], so nothing outside the base is ever sent to the
    /// server. An existing directory is not an error.
    pub async fn create_dir_all(&self, base: &str, path: &str) -> Result<(), SftpError> {
        for dir in crate::path::ancestors(base, path)? {
            match self.create_dir(&dir).await {
                Ok(()) => {}
                Err(err) => match self.stat(&dir).await {
                    // A concurrent creator (or a pre-existing directory) is fine.
                    Ok(stat) if stat.is_dir() => {}
                    _ => return Err(err),
                },
            }
        }
        Ok(())
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

    /// Queues a recursive upload of the local directory at `local` to `remote`.
    ///
    /// Symlinks inside the tree are skipped, never followed, and the walk
    /// refuses a tree deeper than [`crate::MAX_TREE_DEPTH`]. Directories are
    /// created as the walk descends; `remote` itself is created, but its
    /// parents are the caller's responsibility.
    pub async fn upload_dir(
        &self,
        local: impl Into<PathBuf>,
        remote: impl Into<String>,
    ) -> Result<TransferId, SftpError> {
        let transfer = Transfer::upload_tree(local, remote);
        self.request(|reply| Command::Enqueue { transfer, reply })
            .await
    }

    /// Queues a recursive download of the remote directory at `remote` to the
    /// local path `local`. See [`Self::upload_dir`] for the symlink and depth
    /// rules.
    pub async fn download_dir(
        &self,
        remote: impl Into<String>,
        local: impl Into<PathBuf>,
    ) -> Result<TransferId, SftpError> {
        let transfer = Transfer::download_tree(remote, local);
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
            if transfer.is_recursive() {
                let direction = match transfer.direction() {
                    TransferDirection::Upload => TreeDirection::Upload,
                    TransferDirection::Download => TreeDirection::Download,
                };
                return run_tree(sftp, &transfer, &progress, &cancel, direction).await;
            }
            match transfer.direction() {
                TransferDirection::Upload => upload(&sftp, &transfer, &progress, &cancel).await,
                TransferDirection::Download => download(&sftp, &transfer, &progress, &cancel).await,
            }
        })
    }
}

/// Why a byte-moving loop stopped.
enum Stop {
    Complete,
    Cancelled,
    Failed(SftpError),
}

async fn upload(
    sftp: &SftpSession,
    transfer: &Transfer,
    progress: &ProgressSink,
    cancel: &CancelToken,
) -> Result<TransferOutcome, SftpError> {
    upload_file(
        sftp,
        transfer.local_path(),
        transfer.remote_path(),
        progress,
        cancel,
        0,
    )
    .await?;
    Ok(outcome(cancel))
}

async fn download(
    sftp: &SftpSession,
    transfer: &Transfer,
    progress: &ProgressSink,
    cancel: &CancelToken,
) -> Result<TransferOutcome, SftpError> {
    download_file(
        sftp,
        transfer.remote_path(),
        transfer.local_path(),
        progress,
        cancel,
        0,
    )
    .await?;
    Ok(outcome(cancel))
}

fn outcome(cancel: &CancelToken) -> TransferOutcome {
    if cancel.is_cancelled() {
        TransferOutcome::Cancelled
    } else {
        TransferOutcome::Complete
    }
}

#[derive(Clone, Copy)]
enum TreeDirection {
    Upload,
    Download,
}

/// Drives the recursive walker for one tree transfer. The source/destination
/// order is `(source, dest)`, which for upload is `(local, remote)` and for
/// download is `(remote, local)`.
async fn run_tree(
    sftp: Arc<SftpSession>,
    transfer: &Transfer,
    progress: &ProgressSink,
    cancel: &CancelToken,
    direction: TreeDirection,
) -> Result<TransferOutcome, SftpError> {
    match direction {
        TreeDirection::Upload => {
            let fs = UploadTree {
                sftp,
                progress: progress.clone(),
                cancel: cancel.clone(),
            };
            let source = transfer.local_path().to_string_lossy().into_owned();
            let dest = transfer.remote_path().to_string();
            tree::transfer_tree(&fs, &source, &dest, cancel).await?;
        }
        TreeDirection::Download => {
            let fs = DownloadTree {
                sftp,
                progress: progress.clone(),
                cancel: cancel.clone(),
            };
            let source = transfer.remote_path().to_string();
            let dest = transfer.local_path().to_string_lossy().into_owned();
            tree::transfer_tree(&fs, &source, &dest, cancel).await?;
        }
    }
    Ok(outcome(cancel))
}

/// Streams one local file to `dest` over SFTP.
///
/// On cancellation or failure the remote partial is removed (there is no
/// upload resume) and the sink is told with [`PartialDisposition::Removed`].
async fn upload_file(
    sftp: &SftpSession,
    source: &Path,
    dest: &str,
    progress: &ProgressSink,
    cancel: &CancelToken,
    base: u64,
) -> Result<u64, SftpError> {
    use std::io::Read as _;
    use tokio::io::AsyncWriteExt;

    let mut local = std::fs::File::open(source)?;
    let mut remote = sftp
        .open_with_flags(
            dest,
            OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE,
        )
        .await
        .map_err(remote)?;

    let mut buffer = vec![0u8; TRANSFER_CHUNK];
    let mut done = 0u64;
    let stop = loop {
        if cancel.is_cancelled() {
            break Stop::Cancelled;
        }
        let read = match local.read(&mut buffer) {
            Ok(0) => break Stop::Complete,
            Ok(read) => read,
            Err(err) => break Stop::Failed(err.into()),
        };
        if let Err(err) = remote.write_all(&buffer[..read]).await {
            break Stop::Failed(err.into());
        }
        done += read as u64;
        progress.report(base + done);
    };

    match stop {
        Stop::Complete => {
            remote.close().await?;
            Ok(done)
        }
        other => {
            let failed = match other {
                Stop::Failed(err) => Some(err),
                _ => None,
            };
            let _ = remote.close().await;
            // No upload resume exists, so drop the partial rather than leave a
            // file that looks complete on the server.
            if sftp.remove_file(dest).await.is_ok() {
                progress.mark_partial(PartialDisposition::Removed);
            }
            match failed {
                Some(err) => Err(err),
                None => Ok(done),
            }
        }
    }
}

/// Streams one remote file into `local`, resuming a marked partial.
///
/// On cancellation or failure the local partial is kept and a sidecar records
/// the remote path and size, so a later download can resume safely; the sink is
/// told with [`PartialDisposition::KeptResumable`]. If the sidecar cannot be
/// written the partial is removed instead, because an unmarked partial is the
/// bug this closes.
async fn download_file(
    sftp: &SftpSession,
    remote_path: &str,
    local: &Path,
    progress: &ProgressSink,
    cancel: &CancelToken,
    base: u64,
) -> Result<u64, SftpError> {
    use std::io::{Seek as _, Write as _};
    use tokio::io::{AsyncReadExt, AsyncSeekExt};

    let mut source = sftp.open(remote_path).await.map_err(remote)?;
    let remote_size = source.metadata().await.ok().map(|meta| meta.len());

    let offset = match partial::resume_decision(local, remote_path, remote_size) {
        ResumeDecision::Resume { offset } => offset,
        // A refused resume restarts from 0; the stale sidecar is cleared.
        ResumeDecision::Fresh | ResumeDecision::Refused(_) => {
            partial::clear_sidecar(local);
            0
        }
    };

    let mut sink = if offset == 0 {
        std::fs::File::create(local)?
    } else {
        let mut file = std::fs::OpenOptions::new().write(true).open(local)?;
        file.seek(std::io::SeekFrom::Start(offset))?;
        file
    };
    if offset > 0 {
        source.seek(std::io::SeekFrom::Start(offset)).await?;
    }

    let mut buffer = vec![0u8; TRANSFER_CHUNK];
    let mut done = offset;
    let stop = loop {
        if cancel.is_cancelled() {
            break Stop::Cancelled;
        }
        let read = match source.read(&mut buffer).await {
            Ok(0) => break Stop::Complete,
            Ok(read) => read,
            Err(err) => break Stop::Failed(remote(err)),
        };
        if let Err(err) = sink.write_all(&buffer[..read]) {
            break Stop::Failed(err.into());
        }
        done += read as u64;
        progress.report(base + done);
    };
    let flushed = sink.flush();
    let _ = source.close().await;

    match stop {
        Stop::Complete => {
            flushed?;
            partial::clear_sidecar(local);
            Ok(done)
        }
        other => {
            let failed = match other {
                Stop::Failed(err) => Some(err),
                _ => None,
            };
            let _ = flushed;
            let marked = done > 0
                && remote_size
                    .is_some_and(|size| partial::write_sidecar(local, remote_path, size).is_ok());
            if marked {
                progress.mark_partial(PartialDisposition::KeptResumable { done });
            } else {
                // An unmarked partial is exactly the state this closes: remove
                // it when it cannot be resumed safely.
                let _ = std::fs::remove_file(local);
                partial::clear_sidecar(local);
                progress.mark_partial(PartialDisposition::Removed);
            }
            match failed {
                Some(err) => Err(err),
                None => Ok(done),
            }
        }
    }
}

/// Recursive download: `list` is remote and never follows a symlink (`lstat`).
struct DownloadTree {
    sftp: Arc<SftpSession>,
    progress: ProgressSink,
    cancel: CancelToken,
}

impl TreeFs for DownloadTree {
    fn list<'a>(&'a self, dir: &'a str) -> TreeFuture<'a, Vec<TreeEntry>> {
        let sftp = self.sftp.clone();
        let dir = dir.to_string();
        Box::pin(async move {
            let mut entries = Vec::new();
            for entry in sftp.read_dir(dir.as_str()).await.map_err(remote)? {
                let path = entry.path();
                // lstat, not stat: a symlink is reported as a symlink and never
                // resolved, so a loop cannot be walked.
                let meta = sftp.symlink_metadata(path.as_str()).await.map_err(remote)?;
                entries.push(TreeEntry::new(
                    entry.file_name(),
                    kind_from(meta.file_type()),
                    meta.len(),
                ));
            }
            Ok(entries)
        })
    }

    fn ensure_dir<'a>(&'a self, dir: &'a str) -> TreeFuture<'a, ()> {
        let dir = dir.to_string();
        Box::pin(async move { create_local_dir(&dir) })
    }

    fn transfer_file<'a>(
        &'a self,
        source: &'a str,
        dest: &'a str,
        _size: u64,
        base: u64,
    ) -> TreeFuture<'a, u64> {
        let sftp = self.sftp.clone();
        let progress = self.progress.clone();
        let cancel = self.cancel.clone();
        let source = source.to_string();
        let dest = PathBuf::from(dest);
        Box::pin(
            async move { download_file(&sftp, &source, &dest, &progress, &cancel, base).await },
        )
    }
}

/// Recursive upload: `list` is the local filesystem, also lstat-based.
struct UploadTree {
    sftp: Arc<SftpSession>,
    progress: ProgressSink,
    cancel: CancelToken,
}

impl TreeFs for UploadTree {
    fn list<'a>(&'a self, dir: &'a str) -> TreeFuture<'a, Vec<TreeEntry>> {
        let dir = dir.to_string();
        Box::pin(async move { list_local_dir(&dir) })
    }

    fn ensure_dir<'a>(&'a self, dir: &'a str) -> TreeFuture<'a, ()> {
        let sftp = self.sftp.clone();
        let dir = dir.to_string();
        Box::pin(async move { create_remote_dir(&sftp, &dir).await })
    }

    fn transfer_file<'a>(
        &'a self,
        source: &'a str,
        dest: &'a str,
        _size: u64,
        base: u64,
    ) -> TreeFuture<'a, u64> {
        let sftp = self.sftp.clone();
        let progress = self.progress.clone();
        let cancel = self.cancel.clone();
        let source = PathBuf::from(source);
        let dest = dest.to_string();
        Box::pin(async move { upload_file(&sftp, &source, &dest, &progress, &cancel, base).await })
    }
}

fn list_local_dir(dir: &str) -> Result<Vec<TreeEntry>, SftpError> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        // symlink_metadata: never follow a symlink while walking.
        let meta = std::fs::symlink_metadata(entry.path())?;
        entries.push(TreeEntry::new(
            entry.file_name().to_string_lossy().into_owned(),
            local_kind(&meta.file_type()),
            meta.len(),
        ));
    }
    Ok(entries)
}

fn local_kind(kind: &std::fs::FileType) -> FileKind {
    if kind.is_dir() {
        FileKind::Dir
    } else if kind.is_symlink() {
        FileKind::Symlink
    } else if kind.is_file() {
        FileKind::File
    } else {
        FileKind::Other
    }
}

fn create_local_dir(dir: &str) -> Result<(), SftpError> {
    match std::fs::create_dir(dir) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            match std::fs::metadata(dir) {
                Ok(meta) if meta.is_dir() => Ok(()),
                _ => Err(err.into()),
            }
        }
        Err(err) => Err(err.into()),
    }
}

async fn create_remote_dir(sftp: &SftpSession, dir: &str) -> Result<(), SftpError> {
    match sftp.create_dir(dir).await {
        Ok(()) => Ok(()),
        Err(err) => match sftp.metadata(dir).await {
            Ok(meta) if meta.is_dir() => Ok(()),
            _ => Err(remote(err)),
        },
    }
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
