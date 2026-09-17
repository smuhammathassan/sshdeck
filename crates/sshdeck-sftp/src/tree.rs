//! Recursive directory transfer.
//!
//! The walker is transport-agnostic: it drives a [`TreeFs`], which lists a
//! directory, ensures a directory exists, and moves one file. The live client
//! implements that trait over the SFTP session (download) and over the local
//! filesystem (upload); tests implement it in memory.
//!
//! Two properties are load-bearing:
//!
//! - **Symlinks are never followed.** A listing entry that is a symlink is
//!   counted and skipped. The live download harness uses `lstat`, so a symlink
//!   loop cannot walk forever, and a recursive symlink is not copied.
//! - **Depth is bounded.** A tree deeper than [`MAX_TREE_DEPTH`] is refused with
//!   [`SftpError::TreeTooDeep`] rather than recursed, so a hostile or accidental
//!   cycle cannot exhaust the stack.
//!
//! The walk is depth-first and one branch at a time, so the pending stack is
//! only as deep as the tree (bounded by the limit), never as wide as it.

use std::future::Future;
use std::pin::Pin;

use crate::listing::FileKind;
use crate::transfer::{CancelToken, ProgressSink};
use crate::SftpError;

/// Hard ceiling on how deep a recursive walk descends.
pub const MAX_TREE_DEPTH: usize = 64;

/// The boxed future a [`TreeFs`] method returns.
pub type TreeFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, SftpError>> + Send + 'a>>;

/// One entry the walker sees. `size` is the source size when known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    name: String,
    kind: FileKind,
    size: u64,
}

impl TreeEntry {
    pub fn new(name: impl Into<String>, kind: FileKind, size: u64) -> Self {
        Self {
            name: name.into(),
            kind,
            size,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn kind(&self) -> FileKind {
        self.kind
    }

    pub fn size(&self) -> u64 {
        self.size
    }
}

/// What one recursive transfer did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TreeReport {
    dirs: u64,
    files: u64,
    bytes: u64,
    symlinks_skipped: u64,
    others_skipped: u64,
    unsafe_names_skipped: u64,
}

impl TreeReport {
    pub fn dirs(&self) -> u64 {
        self.dirs
    }

    pub fn files(&self) -> u64 {
        self.files
    }

    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    pub fn symlinks_skipped(&self) -> u64 {
        self.symlinks_skipped
    }

    pub fn others_skipped(&self) -> u64 {
        self.others_skipped
    }

    /// Entries whose name was not a single safe path component.
    pub fn unsafe_names_skipped(&self) -> u64 {
        self.unsafe_names_skipped
    }

    fn merge(&mut self, other: &TreeReport) {
        self.dirs += other.dirs;
        self.files += other.files;
        self.bytes += other.bytes;
        self.symlinks_skipped += other.symlinks_skipped;
        self.others_skipped += other.others_skipped;
        self.unsafe_names_skipped += other.unsafe_names_skipped;
    }
}

/// The filesystem a recursive walk drives.
///
/// Implementations report their own progress through the [`ProgressSink`] they
/// hold and read cancellation from the [`CancelToken`] they hold; the walker
/// only sequences the calls.
pub trait TreeFs: Send + Sync {
    /// Lists `dir` without following symlinks (`lstat`, not `stat`).
    fn list<'a>(&'a self, dir: &'a str) -> TreeFuture<'a, Vec<TreeEntry>>;

    /// Creates `dir`, tolerating an existing directory. Parents are the
    /// walker's job (it descends top-down), not this call's.
    fn ensure_dir<'a>(&'a self, dir: &'a str) -> TreeFuture<'a, ()>;

    /// Moves one regular file from `source` to `dest`, reporting progress as
    /// `base + bytes moved so far`. Returns the bytes this file contributed
    /// (including a resumed prefix). On early stop it marks the
    /// [`PartialDisposition`](crate::PartialDisposition) on the progress sink.
    fn transfer_file<'a>(
        &'a self,
        source: &'a str,
        dest: &'a str,
        size: u64,
        base: u64,
    ) -> TreeFuture<'a, u64>;
}

/// Depth-first copy of the tree at `source_root` to `dest_root`.
///
/// Returns the completed [`TreeReport`] and the total bytes now present at the
/// destination. A cancellation stops at the next entry and leaves whatever was
/// already copied in place; the partial file (if any) carries the disposition
/// its `transfer_file` recorded.
pub async fn transfer_tree(
    fs: &dyn TreeFs,
    source_root: &str,
    dest_root: &str,
    cancel: &CancelToken,
) -> Result<(TreeReport, u64), SftpError> {
    walk(fs, source_root, dest_root, 0, cancel, 0).await
}

/// Whether a listing name is exactly one safe path component.
///
/// Remote listings are untrusted: a server could return a name containing a
/// separator or `.`/`..`, which would let an entry escape the destination root.
fn is_plain_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains('/')
}

fn child(parent: &str, name: &str) -> String {
    if parent.ends_with('/') {
        format!("{parent}{name}")
    } else {
        format!("{parent}/{name}")
    }
}

fn walk<'a>(
    fs: &'a dyn TreeFs,
    source: &'a str,
    dest: &'a str,
    depth: usize,
    cancel: &'a CancelToken,
    base: u64,
) -> TreeFuture<'a, (TreeReport, u64)> {
    Box::pin(async move {
        if depth > MAX_TREE_DEPTH {
            return Err(SftpError::TreeTooDeep {
                limit: MAX_TREE_DEPTH,
            });
        }

        let mut report = TreeReport::default();
        let mut done = base;
        fs.ensure_dir(dest).await?;
        report.dirs += 1;

        for entry in fs.list(source).await? {
            if cancel.is_cancelled() {
                break;
            }
            if !is_plain_name(entry.name()) {
                report.unsafe_names_skipped += 1;
                continue;
            }
            let src = child(source, entry.name());
            let dst = child(dest, entry.name());
            match entry.kind() {
                FileKind::Dir => {
                    let (sub, sub_done) = walk(fs, &src, &dst, depth + 1, cancel, done).await?;
                    report.merge(&sub);
                    done = sub_done;
                }
                FileKind::File => {
                    let moved = fs.transfer_file(&src, &dst, entry.size(), done).await?;
                    done += moved;
                    report.files += 1;
                    report.bytes += moved;
                }
                // Never descend or copy a symlink: that is the loop guard.
                FileKind::Symlink => report.symlinks_skipped += 1,
                FileKind::Other => report.others_skipped += 1,
            }
        }

        Ok((report, done))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::block_on;
    use crate::transfer::{
        ProgressSink, Transfer, TransferEvent, TransferExecutor, TransferFuture, TransferOutcome,
        TransferQueue, TransferState,
    };
    use async_channel::Receiver;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    fn entry(name: &str, kind: FileKind, size: u64) -> TreeEntry {
        TreeEntry::new(name, kind, size)
    }

    /// An in-memory tree. `list` returns the pre-registered children; symlinks
    /// are entries with no children of their own, so following one would be
    /// visible as extra transfers.
    struct FakeTree {
        dirs: HashMap<String, Vec<TreeEntry>>,
        created: Mutex<Vec<String>>,
        transferred: Mutex<Vec<(String, String)>>,
        progress: ProgressSink,
        cancel: CancelToken,
        files_seen: AtomicUsize,
        cancel_after: Option<usize>,
    }

    impl FakeTree {
        fn new(entries: HashMap<String, Vec<TreeEntry>>, cancel: CancelToken) -> Self {
            Self {
                dirs: entries,
                created: Mutex::new(Vec::new()),
                transferred: Mutex::new(Vec::new()),
                progress: ProgressSink::new(|_| {}),
                cancel,
                files_seen: AtomicUsize::new(0),
                cancel_after: None,
            }
        }

        fn with_progress(mut self, progress: ProgressSink) -> Self {
            self.progress = progress;
            self
        }

        fn cancelling_after(mut self, files: usize) -> Self {
            self.cancel_after = Some(files);
            self
        }
    }

    impl TreeFs for FakeTree {
        fn list<'a>(&'a self, dir: &'a str) -> TreeFuture<'a, Vec<TreeEntry>> {
            let entries = self.dirs.get(dir).cloned().unwrap_or_default();
            Box::pin(async move { Ok(entries) })
        }

        fn ensure_dir<'a>(&'a self, dir: &'a str) -> TreeFuture<'a, ()> {
            self.created.lock().expect("lock").push(dir.to_string());
            Box::pin(async { Ok(()) })
        }

        fn transfer_file<'a>(
            &'a self,
            source: &'a str,
            dest: &'a str,
            size: u64,
            base: u64,
        ) -> TreeFuture<'a, u64> {
            let seen = self.files_seen.fetch_add(1, Ordering::Relaxed) + 1;
            let cancel = self.cancel.clone();
            let progress = self.progress.clone();
            let source = source.to_string();
            let dest = dest.to_string();
            let cancel_after = self.cancel_after;
            let half = size / 2;
            let transferred = &self.transferred;
            Box::pin(async move {
                progress.report(base + half);
                if cancel_after.is_some_and(|n| seen >= n) {
                    cancel.cancel();
                    progress.mark_partial(crate::PartialDisposition::KeptResumable { done: half });
                    return Ok(half);
                }
                progress.report(base + size);
                transferred.lock().expect("lock").push((source, dest));
                Ok(size)
            })
        }
    }

    fn sample() -> HashMap<String, Vec<TreeEntry>> {
        let mut dirs = HashMap::new();
        dirs.insert(
            "/src".to_string(),
            vec![
                entry("a.bin", FileKind::File, 3),
                entry("sub", FileKind::Dir, 0),
                entry("loop", FileKind::Symlink, 0),
            ],
        );
        dirs.insert(
            "/src/sub".to_string(),
            vec![entry("b.bin", FileKind::File, 5)],
        );
        // If a symlink were followed this would be walked.
        dirs.insert(
            "/src/loop".to_string(),
            vec![entry("never.bin", FileKind::File, 999)],
        );
        dirs
    }

    #[test]
    fn descends_directories_but_never_follows_a_symlink() {
        let cancel = CancelToken::default();
        let fake = FakeTree::new(sample(), cancel.clone());
        let (report, done) =
            block_on(transfer_tree(&fake, "/src", "/dst", &cancel)).expect("walks");

        assert_eq!(report.files(), 2);
        assert_eq!(report.bytes(), 8);
        assert_eq!(report.symlinks_skipped(), 1);
        assert_eq!(done, 8);

        let transferred = fake.transferred.lock().expect("lock").clone();
        assert_eq!(
            transferred,
            vec![
                ("/src/a.bin".to_string(), "/dst/a.bin".to_string()),
                ("/src/sub/b.bin".to_string(), "/dst/sub/b.bin".to_string()),
            ]
        );
        assert!(transferred.iter().all(|(src, _)| !src.contains("/loop")));

        let created = fake.created.lock().expect("lock").clone();
        assert!(created.contains(&"/dst".to_string()));
        assert!(created.contains(&"/dst/sub".to_string()));
        assert!(!created.contains(&"/dst/loop".to_string()));
    }

    #[test]
    fn refuses_a_tree_deeper_than_the_limit() {
        let cancel = CancelToken::default();
        let mut dirs = HashMap::new();
        // A directory chain one level deeper than the walker may enter. The
        // walker names each child `sub`, so the chain is `/deep/sub/sub/...`.
        let mut here = "/deep".to_string();
        for depth in 0..=MAX_TREE_DEPTH + 1 {
            let mut children = Vec::new();
            if depth <= MAX_TREE_DEPTH {
                children.push(entry("sub", FileKind::Dir, 0));
            }
            dirs.insert(here.clone(), children);
            here = format!("{here}/sub");
        }
        let fake = FakeTree::new(dirs, cancel.clone());

        let err =
            block_on(transfer_tree(&fake, "/deep", "/out", &cancel)).expect_err("must refuse");
        assert!(matches!(err, SftpError::TreeTooDeep { limit } if limit == MAX_TREE_DEPTH));
    }

    /// Runs `transfer_tree` through the real queue so cancellation, progress
    /// and the partial disposition are observed exactly as the app sees them.
    struct TreeExecutor {
        dirs: HashMap<String, Vec<TreeEntry>>,
        cancel_after: usize,
    }

    impl TransferExecutor for TreeExecutor {
        fn execute(
            &self,
            _transfer: Transfer,
            progress: ProgressSink,
            cancel: CancelToken,
        ) -> TransferFuture {
            let dirs = self.dirs.clone();
            let cancel_after = self.cancel_after;
            Box::pin(async move {
                let fake = FakeTree::new(dirs, cancel.clone())
                    .with_progress(progress)
                    .cancelling_after(cancel_after);
                transfer_tree(&fake, "/src", "/dst", &cancel).await?;
                if cancel.is_cancelled() {
                    Ok(TransferOutcome::Cancelled)
                } else {
                    Ok(TransferOutcome::Complete)
                }
            })
        }
    }

    #[test]
    fn cancellation_mid_tree_reports_progress_and_leaves_a_resumable_partial() {
        let (events_tx, events_rx) = async_channel::bounded(64);
        let queue = TransferQueue::new(events_tx);
        let executor: Arc<dyn TransferExecutor> = Arc::new(TreeExecutor {
            dirs: sample(),
            cancel_after: 1,
        });

        let _ = block_on(queue.enqueue(Transfer::download_tree("/src", "/dst")));
        queue.close();
        block_on(TransferQueue::run_worker(queue, executor));

        let events = drain_events(&events_rx);
        // Partial progress got out before the terminal event.
        let moved: Vec<u64> = events
            .iter()
            .filter_map(|event| match event.state() {
                TransferState::Running { done, .. } => Some(*done),
                _ => None,
            })
            .collect();
        assert!(moved.iter().any(|done| *done > 0), "got {moved:?}");
        assert_eq!(
            events.last().map(|event| event.state().clone()),
            Some(TransferState::Cancelled)
        );
        // The in-flight file is marked resumable, not silently left behind.
        let mut dispositions = events.iter().filter_map(TransferEvent::partial);
        assert_eq!(
            dispositions.next(),
            Some(crate::PartialDisposition::KeptResumable { done: 1 })
        );
        assert_eq!(dispositions.next(), None);
    }

    fn drain_events(events: &Receiver<TransferEvent>) -> Vec<TransferEvent> {
        let mut out = Vec::new();
        while let Ok(event) = events.try_recv() {
            out.push(event);
        }
        out
    }
}
