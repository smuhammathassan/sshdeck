//! SFTP for sshdeck: remote filesystem operations and transfers.
//!
//! No UI dependency, and no tokio type appears in a `pub` signature, so the
//! operations and the transfer queue are callable from any executor and are
//! testable on a headless runner.
//!
//! [`SftpClient::connect`] takes an existing [`sshdeck_core::session::Session`]
//! and drives the SFTP protocol on that session's own runtime (see
//! [`sshdeck_core::sftp`]); this crate never starts a runtime of its own.

mod client;
pub mod listing;
pub mod path;
mod stream;
pub mod transfer;

pub use client::SftpClient;
pub use listing::{format_permissions, sort_entries, DirEntry, FileKind, FileStat};
pub use path::{join, normalize, PathError};
pub use transfer::{
    CancelToken, ProgressSink, Transfer, TransferDirection, TransferEvent, TransferExecutor,
    TransferFuture, TransferId, TransferOutcome, TransferQueue, TransferState,
    DEFAULT_TRANSFER_CONCURRENCY,
};

/// A failure from an SFTP operation or a transfer.
#[derive(Debug, thiserror::Error)]
pub enum SftpError {
    /// The SFTP subsystem could not be started or was closed.
    #[error("sftp transport: {0}")]
    Transport(String),
    /// The underlying SSH session failed.
    #[error(transparent)]
    Session(#[from] sshdeck_core::session::SessionError),
    /// The server returned an error status or spoke an unexpected packet.
    #[error("sftp: {0}")]
    Remote(String),
    /// Local filesystem I/O failed.
    #[error("local I/O: {0}")]
    Local(#[from] std::io::Error),
    /// A remote path escaped its base directory.
    #[error(transparent)]
    Path(#[from] path::PathError),
    /// The command queue is full; retry when the session catches up.
    #[error("sftp input queue is full")]
    Backpressure,
    /// The SFTP session is gone.
    #[error("the SFTP session is closed")]
    Closed,
    /// A transfer was cancelled.
    #[error("transfer {0:?} was cancelled")]
    Cancelled(TransferId),
}

#[cfg(test)]
pub(crate) mod test_support;
