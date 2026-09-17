//! Resumable-download bookkeeping.
//!
//! A download that stops early (cancel or I/O failure) leaves the partial file
//! in place next to a tiny sidecar naming the remote path it came from and the
//! size that file had when the transfer began. Resuming is only allowed when
//! both match the remote file as it is now — otherwise the bytes on disk could
//! belong to a different file and the result would be silently corrupt.
//!
//! The sidecar is a few bytes, never a copy of the file, so this stays inside
//! the crate's bounded-state rule.

use std::io::Write as _;
use std::path::{Path, PathBuf};

/// Why an existing partial download may not be resumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResumeMismatch {
    /// The recorded remote path is not the one being downloaded.
    RemotePath,
    /// The remote file's size now differs from the recorded size.
    RemoteSize,
    /// The partial file is larger than the remote file.
    Oversized,
}

/// What to do with an existing partial download.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResumeDecision {
    /// No usable partial: start at byte 0 and truncate.
    Fresh,
    /// Resume at `offset` bytes (the partial file's length).
    Resume { offset: u64 },
    /// A partial exists but its guards failed; the caller must restart.
    Refused(ResumeMismatch),
}

/// The sidecar path for a local file: `<name>.sshdeck-partial` beside it.
fn sidecar_path(local: &Path) -> PathBuf {
    let mut name = local.file_name().map(ToOwned::to_owned).unwrap_or_default();
    name.push(".sshdeck-partial");
    local.with_file_name(name)
}

/// Reads the sidecar. `None` when it is missing or malformed.
fn read_record(local: &Path) -> Option<(String, u64)> {
    let text = std::fs::read_to_string(sidecar_path(local)).ok()?;
    let mut lines = text.lines();
    let remote = lines.next()?.to_string();
    let size = lines.next()?.parse().ok()?;
    if remote.is_empty() {
        return None;
    }
    Some((remote, size))
}

/// Decides how to start a download of `remote` into `local`.
///
/// `remote_size` is what the server reports now; the offset is always the
/// partial file's actual length, so a torn write cannot claim bytes it does not
/// have.
pub(crate) fn resume_decision(
    local: &Path,
    remote: &str,
    remote_size: Option<u64>,
) -> ResumeDecision {
    let Some((recorded_remote, recorded_size)) = read_record(local) else {
        return ResumeDecision::Fresh;
    };
    if recorded_remote != remote {
        return ResumeDecision::Refused(ResumeMismatch::RemotePath);
    }
    let Some(remote_size) = remote_size else {
        return ResumeDecision::Refused(ResumeMismatch::RemoteSize);
    };
    if recorded_size != remote_size {
        return ResumeDecision::Refused(ResumeMismatch::RemoteSize);
    }
    let Ok(meta) = std::fs::metadata(local) else {
        return ResumeDecision::Fresh;
    };
    if !meta.is_file() {
        return ResumeDecision::Fresh;
    }
    if meta.len() > recorded_size {
        return ResumeDecision::Refused(ResumeMismatch::Oversized);
    }
    ResumeDecision::Resume { offset: meta.len() }
}

/// Records that `local` holds a resumable prefix of `remote`.
pub(crate) fn write_sidecar(local: &Path, remote: &str, size: u64) -> std::io::Result<()> {
    let mut file = std::fs::File::create(sidecar_path(local))?;
    writeln!(file, "{remote}")?;
    writeln!(file, "{size}")?;
    file.flush()
}

/// Removes the sidecar, if any. Best-effort by design.
pub(crate) fn clear_sidecar(local: &Path) {
    let _ = std::fs::remove_file(sidecar_path(local));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::temp_path;

    fn partial_with(bytes: &[u8]) -> PathBuf {
        let path = temp_path("partial");
        std::fs::write(&path, bytes).expect("writes the partial file");
        path
    }

    #[test]
    fn resume_offset_is_the_partial_files_length() {
        let local = partial_with(&[0u8; 7]);
        write_sidecar(&local, "/remote/a.bin", 10).expect("writes the sidecar");

        assert_eq!(
            resume_decision(&local, "/remote/a.bin", Some(10)),
            ResumeDecision::Resume { offset: 7 }
        );

        clear_sidecar(&local);
        let _ = std::fs::remove_file(&local);
    }

    #[test]
    fn resume_is_refused_when_the_recorded_remote_or_size_differs() {
        let local = partial_with(&[0u8; 7]);

        write_sidecar(&local, "/remote/other.bin", 10).expect("writes the sidecar");
        assert_eq!(
            resume_decision(&local, "/remote/a.bin", Some(10)),
            ResumeDecision::Refused(ResumeMismatch::RemotePath)
        );

        // Right path, but the remote file changed size since the partial began.
        write_sidecar(&local, "/remote/a.bin", 10).expect("writes the sidecar");
        assert_eq!(
            resume_decision(&local, "/remote/a.bin", Some(11)),
            ResumeDecision::Refused(ResumeMismatch::RemoteSize)
        );

        // A partial longer than the remote file is corrupt.
        let big = partial_with(&[0u8; 12]);
        write_sidecar(&big, "/remote/a.bin", 10).expect("writes the sidecar");
        assert_eq!(
            resume_decision(&big, "/remote/a.bin", Some(10)),
            ResumeDecision::Refused(ResumeMismatch::Oversized)
        );

        clear_sidecar(&local);
        clear_sidecar(&big);
        let _ = std::fs::remove_file(&local);
        let _ = std::fs::remove_file(&big);
    }

    #[test]
    fn no_sidecar_means_a_fresh_start() {
        let local = partial_with(b"leftover");
        assert_eq!(
            resume_decision(&local, "/remote/a.bin", Some(10)),
            ResumeDecision::Fresh
        );
        let _ = std::fs::remove_file(&local);
    }
}
