//! A bounded-concurrency transfer queue with progress events.
//!
//! The queue is transport-agnostic: it owns the bookkeeping (queued → running →
//! done/cancelled), the progress clock and the cancellation tokens, and asks a
//! [`TransferExecutor`] to move the bytes. The live executor talks to the SFTP
//! session; tests supply a fake.
//!
//! Concurrency is capped by spawning exactly [`DEFAULT_TRANSFER_CONCURRENCY`]
//! worker tasks (see [`TransferQueue::run_worker`]). Workers are
//! competing-consumers over one job channel, so the number of workers *is* the
//! cap — no unbounded task or channel growth.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use async_channel::{Receiver, Sender};

use crate::SftpError;

/// Worker tasks the live client starts by default. Small on purpose: this app
/// targets an 8 GB machine and a transfer moves through one multiplexed SFTP
/// channel, not one channel per transfer.
pub const DEFAULT_TRANSFER_CONCURRENCY: usize = 3;

/// Which way a transfer moves bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferDirection {
    Upload,
    Download,
}

/// One queued transfer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transfer {
    direction: TransferDirection,
    local_path: PathBuf,
    remote_path: String,
    total: Option<u64>,
}

impl Transfer {
    /// Local file → remote path.
    pub fn upload(
        local_path: impl Into<PathBuf>,
        remote_path: impl Into<String>,
        total: Option<u64>,
    ) -> Self {
        Self {
            direction: TransferDirection::Upload,
            local_path: local_path.into(),
            remote_path: remote_path.into(),
            total,
        }
    }

    /// Remote path → local file.
    pub fn download(
        remote_path: impl Into<String>,
        local_path: impl Into<PathBuf>,
        total: Option<u64>,
    ) -> Self {
        Self {
            direction: TransferDirection::Download,
            local_path: local_path.into(),
            remote_path: remote_path.into(),
            total,
        }
    }

    pub fn direction(&self) -> TransferDirection {
        self.direction
    }

    pub fn local_path(&self) -> &Path {
        &self.local_path
    }

    pub fn remote_path(&self) -> &str {
        &self.remote_path
    }

    /// Total bytes when known, used to scale progress.
    pub fn total(&self) -> Option<u64> {
        self.total
    }
}

/// Identifies one transfer for its lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TransferId(u64);

impl TransferId {
    pub fn get(self) -> u64 {
        self.0
    }
}

/// Where a transfer is in its lifecycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferState {
    Queued,
    Running { done: u64, total: Option<u64> },
    Complete,
    Failed { message: String },
    Cancelled,
}

/// A state change for one transfer, emitted as it progresses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferEvent {
    id: TransferId,
    state: TransferState,
}

impl TransferEvent {
    pub fn id(&self) -> TransferId {
        self.id
    }

    pub fn state(&self) -> &TransferState {
        &self.state
    }
}

/// How a completed [`TransferExecutor::execute`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferOutcome {
    Complete,
    Cancelled,
}

/// Cooperative cancellation flag handed to an executor.
#[derive(Clone, Default, Debug)]
pub struct CancelToken {
    flag: Arc<AtomicBool>,
}

impl CancelToken {
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }

    pub fn cancel(&self) {
        self.flag.store(true, Ordering::Relaxed);
    }
}

/// Progress sink handed to an executor. Reporting the same or a smaller total
/// than before is ignored, so progress is monotonic.
#[derive(Clone)]
pub struct ProgressSink {
    report: Arc<dyn Fn(u64) + Send + Sync>,
}

impl ProgressSink {
    pub fn new<F: Fn(u64) + Send + Sync + 'static>(report: F) -> Self {
        Self {
            report: Arc::new(report),
        }
    }

    /// Reports `done` bytes moved so far.
    pub fn report(&self, done: u64) {
        (self.report)(done);
    }
}

/// The boxed future an executor returns. `std::future::Future` only — no tokio
/// type is part of this crate's public API.
pub type TransferFuture =
    Pin<Box<dyn Future<Output = Result<TransferOutcome, SftpError>> + Send + 'static>>;

/// Moves one transfer's bytes, reporting progress and honouring cancellation.
pub trait TransferExecutor: Send + Sync + 'static {
    fn execute(
        &self,
        transfer: Transfer,
        progress: ProgressSink,
        cancel: CancelToken,
    ) -> TransferFuture;
}

struct Entry {
    transfer: Transfer,
    state: TransferState,
    done: u64,
    cancel: CancelToken,
}

struct QueueState {
    next_id: u64,
    entries: HashMap<TransferId, Entry>,
}

struct Shared {
    state: Mutex<QueueState>,
    events: Sender<TransferEvent>,
    jobs: Sender<TransferId>,
    jobs_rx: Receiver<TransferId>,
}

/// A handle to the transfer queue. Cloning is cheap and shares the same state.
#[derive(Clone)]
pub struct TransferQueue {
    shared: Arc<Shared>,
}

impl TransferQueue {
    /// Creates a queue that emits events on `events`. Pair it with
    /// [`TransferQueue::run_worker`] for each unit of concurrency.
    pub fn new(events: Sender<TransferEvent>) -> Self {
        let (jobs, jobs_rx) = async_channel::unbounded();
        Self {
            shared: Arc::new(Shared {
                state: Mutex::new(QueueState {
                    next_id: 1,
                    entries: HashMap::new(),
                }),
                events,
                jobs,
                jobs_rx,
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, QueueState> {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Queues a transfer and returns its id. Emits [`TransferState::Queued`].
    pub async fn enqueue(&self, transfer: Transfer) -> TransferId {
        let id = {
            let mut state = self.lock();
            let id = TransferId(state.next_id);
            state.next_id += 1;
            state.entries.insert(
                id,
                Entry {
                    transfer,
                    state: TransferState::Queued,
                    done: 0,
                    cancel: CancelToken::default(),
                },
            );
            id
        };
        self.emit(TransferEvent {
            id,
            state: TransferState::Queued,
        })
        .await;
        // Unbounded: enqueueing never blocks a UI thread.
        let _ = self.shared.jobs.send(id).await;
        id
    }

    /// Requests cancellation. Returns `false` if the transfer is unknown or
    /// already finished. A queued transfer is cancelled immediately; a running
    /// one is asked to stop and reports [`TransferState::Cancelled`] when its
    /// executor notices.
    pub async fn cancel(&self, id: TransferId) -> bool {
        let (changed, event) = {
            let mut state = self.lock();
            let Some(entry) = state.entries.get_mut(&id) else {
                return false;
            };
            match &entry.state {
                TransferState::Queued => {
                    entry.state = TransferState::Cancelled;
                    entry.cancel.cancel();
                    (
                        true,
                        Some(TransferEvent {
                            id,
                            state: TransferState::Cancelled,
                        }),
                    )
                }
                TransferState::Running { .. } => {
                    entry.cancel.cancel();
                    (true, None)
                }
                _ => (false, None),
            }
        };
        if let Some(event) = event {
            self.emit(event).await;
        }
        changed
    }

    /// Closes the job channel so worker tasks drain and exit.
    pub fn close(&self) {
        self.shared.jobs.close();
        self.shared.jobs_rx.close();
    }

    /// Runs `executor` until the job channel closes. Start
    /// [`DEFAULT_TRANSFER_CONCURRENCY`] of these for the live client's cap.
    pub async fn run_worker(queue: TransferQueue, executor: Arc<dyn TransferExecutor>) {
        let jobs = queue.shared.jobs_rx.clone();
        while let Ok(id) = jobs.recv().await {
            queue.run_one(id, executor.clone()).await;
        }
    }

    async fn run_one(&self, id: TransferId, executor: Arc<dyn TransferExecutor>) {
        let Some((transfer, cancel)) = self.start(id).await else {
            return;
        };
        let queue = self.clone();
        let progress = ProgressSink::new(move |done| queue.progress(id, done));

        let state = match executor.execute(transfer, progress, cancel).await {
            Ok(TransferOutcome::Complete) => TransferState::Complete,
            Ok(TransferOutcome::Cancelled) => TransferState::Cancelled,
            Err(err) => TransferState::Failed {
                message: err.to_string(),
            },
        };
        self.finish(id, state).await;
    }

    /// Moves a transfer from queued to running. `None` if it was cancelled (or
    /// removed) before a worker picked it up.
    async fn start(&self, id: TransferId) -> Option<(Transfer, CancelToken)> {
        let (transfer, cancel, total) = {
            let mut state = self.lock();
            let entry = state.entries.get_mut(&id)?;
            if matches!(entry.state, TransferState::Cancelled) {
                return None;
            }
            entry.state = TransferState::Running {
                done: 0,
                total: entry.transfer.total(),
            };
            (
                entry.transfer.clone(),
                entry.cancel.clone(),
                entry.transfer.total(),
            )
        };
        self.emit(TransferEvent {
            id,
            state: TransferState::Running { done: 0, total },
        })
        .await;
        Some((transfer, cancel))
    }

    /// Records progress. Out-of-order or duplicate totals are ignored so a
    /// consumer can render a monotonic bar. Progress is dropped rather than
    /// blocking when the event channel is full — the completion event still
    /// arrives from [`Self::finish`].
    fn progress(&self, id: TransferId, done: u64) {
        let event = {
            let mut state = self.lock();
            let Some(entry) = state.entries.get_mut(&id) else {
                return;
            };
            if !matches!(entry.state, TransferState::Running { .. }) || done <= entry.done {
                return;
            }
            entry.done = done;
            TransferEvent {
                id,
                state: TransferState::Running {
                    done,
                    total: entry.transfer.total(),
                },
            }
        };
        let _ = self.shared.events.try_send(event);
    }

    async fn finish(&self, id: TransferId, state: TransferState) {
        let final_state = {
            let mut state_map = self.lock();
            let Some(entry) = state_map.entries.remove(&id) else {
                return;
            };
            // Cancellation wins over any executor outcome.
            if entry.cancel.is_cancelled() {
                TransferState::Cancelled
            } else {
                state
            }
        };
        self.emit(TransferEvent {
            id,
            state: final_state,
        })
        .await;
    }

    async fn emit(&self, event: TransferEvent) {
        let _ = self.shared.events.send(event).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::block_on;

    /// Reports a fixed list of chunk sizes, then completes.
    struct ChunkedExecutor {
        chunks: Vec<u64>,
        fail: bool,
    }

    impl TransferExecutor for ChunkedExecutor {
        fn execute(
            &self,
            _transfer: Transfer,
            progress: ProgressSink,
            cancel: CancelToken,
        ) -> TransferFuture {
            let chunks = self.chunks.clone();
            let fail = self.fail;
            Box::pin(async move {
                let mut done = 0;
                for chunk in chunks {
                    if cancel.is_cancelled() {
                        return Ok(TransferOutcome::Cancelled);
                    }
                    done += chunk;
                    progress.report(done);
                }
                if fail {
                    return Err(SftpError::Remote("boom".into()));
                }
                Ok(TransferOutcome::Complete)
            })
        }
    }

    /// Reports each value verbatim, so a regression can be simulated.
    struct RawExecutor {
        reports: Vec<u64>,
    }

    impl TransferExecutor for RawExecutor {
        fn execute(
            &self,
            _transfer: Transfer,
            progress: ProgressSink,
            _cancel: CancelToken,
        ) -> TransferFuture {
            let reports = self.reports.clone();
            Box::pin(async move {
                for report in reports {
                    progress.report(report);
                }
                Ok(TransferOutcome::Complete)
            })
        }
    }

    fn drain_events(events: &Receiver<TransferEvent>) -> Vec<TransferState> {
        let mut states = Vec::new();
        while let Ok(event) = events.try_recv() {
            states.push(event.state().clone());
        }
        states
    }

    fn upload() -> Transfer {
        Transfer::upload("/tmp/local.bin", "/remote/local.bin", Some(300))
    }

    #[test]
    fn queue_walks_enqueued_through_running_to_complete() {
        let (events_tx, events_rx) = async_channel::bounded(64);
        let queue = TransferQueue::new(events_tx);
        let executor: Arc<dyn TransferExecutor> = Arc::new(ChunkedExecutor {
            chunks: vec![100, 100, 100],
            fail: false,
        });

        let id = block_on(queue.enqueue(upload()));
        queue.close();
        block_on(TransferQueue::run_worker(queue.clone(), executor));

        let states = drain_events(&events_rx);
        assert_eq!(
            states,
            vec![
                TransferState::Queued,
                TransferState::Running {
                    done: 0,
                    total: Some(300)
                },
                TransferState::Running {
                    done: 100,
                    total: Some(300)
                },
                TransferState::Running {
                    done: 200,
                    total: Some(300)
                },
                TransferState::Running {
                    done: 300,
                    total: Some(300)
                },
                TransferState::Complete,
            ]
        );
        assert_ne!(id.get(), 0);
    }

    #[test]
    fn progress_is_monotonic_even_when_reports_go_backwards() {
        let (events_tx, events_rx) = async_channel::bounded(64);
        let queue = TransferQueue::new(events_tx);
        let executor: Arc<dyn TransferExecutor> = Arc::new(RawExecutor {
            // 30 goes backwards from 50 and must be ignored.
            reports: vec![10, 50, 30, 80],
        });

        block_on(queue.enqueue(upload()));
        queue.close();
        block_on(TransferQueue::run_worker(queue, executor));

        let done: Vec<u64> = drain_events(&events_rx)
            .into_iter()
            .filter_map(|state| match state {
                TransferState::Running { done, .. } => Some(done),
                _ => None,
            })
            .collect();
        assert_eq!(done, vec![0, 10, 50, 80]);
    }

    #[test]
    fn chunked_transfer_reports_the_expected_number_of_chunks() {
        let (events_tx, events_rx) = async_channel::bounded(1024);
        let queue = TransferQueue::new(events_tx);
        let executor: Arc<dyn TransferExecutor> = Arc::new(ChunkedExecutor {
            chunks: vec![1; 1000],
            fail: false,
        });

        block_on(queue.enqueue(upload()));
        queue.close();
        block_on(TransferQueue::run_worker(queue, executor));

        let states = drain_events(&events_rx);
        // 1000 progress updates plus the queued/running/complete bookends.
        assert_eq!(states.len(), 1003);
        assert_eq!(states.last(), Some(&TransferState::Complete));
    }

    #[test]
    fn cancelling_a_queued_transfer_never_runs_it() {
        let (events_tx, events_rx) = async_channel::bounded(64);
        let queue = TransferQueue::new(events_tx);
        let executor: Arc<dyn TransferExecutor> = Arc::new(ChunkedExecutor {
            chunks: vec![100],
            fail: false,
        });

        let id = block_on(queue.enqueue(upload()));
        assert!(block_on(queue.cancel(id)));
        queue.close();
        block_on(TransferQueue::run_worker(queue, executor));

        assert_eq!(
            drain_events(&events_rx),
            vec![TransferState::Queued, TransferState::Cancelled]
        );
        // Cancelling again is a no-op.
        assert!(!block_on(queue.cancel(id)));
    }

    /// Blocks inside `execute` until the test releases it, so cancellation can
    /// be delivered to a *running* transfer.
    struct BlockingExecutor {
        started: Sender<bool>,
        resume: Receiver<bool>,
    }

    impl TransferExecutor for BlockingExecutor {
        fn execute(
            &self,
            _transfer: Transfer,
            _progress: ProgressSink,
            cancel: CancelToken,
        ) -> TransferFuture {
            let started = self.started.clone();
            let resume = self.resume.clone();
            Box::pin(async move {
                let _ = started.send(true).await;
                for _ in 0..100 {
                    if resume.recv().await.is_err() {
                        break;
                    }
                    if cancel.is_cancelled() {
                        return Ok(TransferOutcome::Cancelled);
                    }
                }
                Ok(TransferOutcome::Cancelled)
            })
        }
    }

    #[test]
    fn cancelling_a_running_transfer_is_observed_by_the_executor() {
        let (events_tx, events_rx) = async_channel::bounded(64);
        let queue = TransferQueue::new(events_tx);
        let (started_tx, started_rx) = async_channel::bounded(1);
        let (resume_tx, resume_rx) = async_channel::bounded(1);
        let executor: Arc<dyn TransferExecutor> = Arc::new(BlockingExecutor {
            started: started_tx,
            resume: resume_rx,
        });

        let id = block_on(queue.enqueue(upload()));
        let worker_queue = queue.clone();
        let worker = std::thread::spawn(move || {
            block_on(TransferQueue::run_worker(worker_queue, executor));
        });

        // Wait until it is running, cancel, then let it re-check the token.
        started_rx.recv().expect("worker started");
        assert!(block_on(queue.cancel(id)));
        resume_tx.send(true).expect("resume");

        queue.close();
        worker.join().expect("worker finishes");

        let states = drain_events(&events_rx);
        assert_eq!(states.first(), Some(&TransferState::Queued));
        assert!(matches!(
            states.get(1),
            Some(TransferState::Running { done: 0, .. })
        ));
        assert_eq!(states.last(), Some(&TransferState::Cancelled));
    }

    #[test]
    fn executor_failure_is_reported_as_failed() {
        let (events_tx, events_rx) = async_channel::bounded(64);
        let queue = TransferQueue::new(events_tx);
        let executor: Arc<dyn TransferExecutor> = Arc::new(ChunkedExecutor {
            chunks: vec![10],
            fail: true,
        });

        block_on(queue.enqueue(upload()));
        queue.close();
        block_on(TransferQueue::run_worker(queue, executor));

        let states = drain_events(&events_rx);
        assert!(matches!(
            states.last(),
            Some(TransferState::Failed { message }) if message == "sftp: boom"
        ));
    }
}
