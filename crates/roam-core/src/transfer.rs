//! The transfer engine: uploads, downloads, and copies.
//!
//! Two levels of concurrency, as designed:
//!
//! - a global semaphore caps how many tasks run at once, so queueing 500 files
//!   does not open 500 connections;
//! - inside one task, a large object is written through OpenDAL's concurrent
//!   writer, so a single big file still saturates the link.
//!
//! Cancellation is **cooperative**, not an abort: each chunk checks a flag and
//! returns [`Error::Cancelled`], which lets the pump delete its half-written
//! output on the way out. Aborting the task instead would drop the future
//! mid-write and leave partial files behind with no chance to clean up.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio::sync::Semaphore;

use crate::{DirEntry, Error, Result, Rt, Vfs, path};

/// Tasks allowed to run at once.
pub const DEFAULT_CONCURRENCY: usize = 4;

/// Chunk size for streaming, and the size of each part in a concurrent write.
pub const CHUNK: usize = 8 * 1024 * 1024;

/// Parts uploaded in parallel within a single task.
pub const WRITER_CONCURRENCY: usize = 8;

pub type TaskId = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    Download,
    Upload,
    Copy,
    Move,
}

impl Operation {
    pub fn label(self) -> &'static str {
        match self {
            Self::Download => "下载",
            Self::Upload => "上传",
            Self::Copy => "复制",
            Self::Move => "移动",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskState {
    Queued,
    Running,
    Done,
    Cancelled,
    Failed(String),
}

impl TaskState {
    pub fn is_finished(&self) -> bool {
        matches!(self, Self::Done | Self::Cancelled | Self::Failed(_))
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Queued => "排队中",
            Self::Running => "进行中",
            Self::Done => "完成",
            Self::Cancelled => "已取消",
            Self::Failed(_) => "失败",
        }
    }
}

/// What a single task moves.
#[derive(Clone)]
pub enum Transfer {
    Download {
        vfs: Vfs,
        remote: Arc<str>,
        local: PathBuf,
        size: Option<u64>,
    },
    Upload {
        vfs: Vfs,
        local: PathBuf,
        remote: Arc<str>,
    },
    /// Backend-to-backend. When both sides are the same backend and it supports
    /// server-side copy, no bytes travel through this process at all.
    Copy {
        from: Vfs,
        from_path: Arc<str>,
        to: Vfs,
        to_path: Arc<str>,
        size: Option<u64>,
    },
    /// Move a directory: copy every file to the new prefix, then delete the old
    /// one.
    ///
    /// Deliberately **one task** rather than a batch of copies plus a delete: the
    /// engine has no dependencies between tasks, so a separately queued delete
    /// could run before the copies finished. Keeping it together also means the
    /// source is only removed once every copy has succeeded.
    MoveDir {
        vfs: Vfs,
        from: Arc<str>,
        to: Arc<str>,
        /// Pre-flattened file list, so the byte total is known up front.
        files: Arc<Vec<DirEntry>>,
    },
}

impl Transfer {
    pub fn operation(&self) -> Operation {
        match self {
            Self::Download { .. } => Operation::Download,
            Self::Upload { .. } => Operation::Upload,
            Self::Copy { .. } => Operation::Copy,
            Self::MoveDir { .. } => Operation::Move,
        }
    }

    /// Short label for the transfer list.
    pub fn label(&self) -> Arc<str> {
        match self {
            Self::Download { remote, .. } => path::basename(remote).into(),
            Self::Upload { local, .. } => local
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default()
                .into(),
            Self::Copy { to_path, .. } => path::basename(to_path).into(),
            Self::MoveDir { to, .. } => path::basename(to).into(),
        }
    }

    fn known_size(&self) -> Option<u64> {
        match self {
            Self::Download { size, .. } | Self::Copy { size, .. } => *size,
            Self::Upload { local, .. } => std::fs::metadata(local).ok().map(|m| m.len()),
            // Summed from the flattened list, so the progress bar is honest from
            // the first byte. Unknown if any file's size is unknown.
            Self::MoveDir { files, .. } => files
                .iter()
                .map(|f| f.size)
                .try_fold(0u64, |acc, size| size.map(|s| acc + s)),
        }
    }
}

/// Live progress for one task, shared with whichever pump is running it.
pub struct TaskProgress {
    done: AtomicU64,
    cancelled: AtomicBool,
}

impl Default for TaskProgress {
    fn default() -> Self {
        Self::new()
    }
}

impl TaskProgress {
    /// Public so integration tests can drive the byte-movement primitives
    /// directly, without standing up a whole engine.
    pub fn new() -> Self {
        Self {
            done: AtomicU64::new(0),
            cancelled: AtomicBool::new(false),
        }
    }

    pub fn advance(&self, bytes: u64) {
        self.done.fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn done(&self) -> u64 {
        self.done.load(Ordering::Relaxed)
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    /// Ask the running pump to stop at its next chunk boundary.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    /// Call between chunks; `Err` unwinds the pump so it can clean up.
    pub fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    }
}

struct TaskRecord {
    id: TaskId,
    label: Arc<str>,
    operation: Operation,
    total: Option<u64>,
    /// Kept so a failed task can be run again. A retried download resumes from
    /// its part file; a retried upload starts over, because OpenDAL does not
    /// expose the multipart upload id needed to continue one.
    transfer: Transfer,
    progress: Mutex<Arc<TaskProgress>>,
    state: Mutex<TaskState>,
    started: Mutex<Option<Instant>>,
}

/// A point-in-time view of a task, for rendering.
#[derive(Debug, Clone)]
pub struct TaskSnapshot {
    pub id: TaskId,
    pub label: Arc<str>,
    pub operation: Operation,
    pub state: TaskState,
    pub done: u64,
    pub total: Option<u64>,
    /// Average since the task started — steady enough to read, unlike an
    /// instantaneous rate that swings with every chunk.
    pub bytes_per_sec: Option<u64>,
    /// Whether [`TransferEngine::retry`] would do anything.
    pub retryable: bool,
}

impl TaskSnapshot {
    /// 0.0–1.0, or `None` when the total is unknown.
    pub fn fraction(&self) -> Option<f32> {
        match self.total {
            Some(0) => Some(1.0),
            Some(total) => Some((self.done as f32 / total as f32).clamp(0.0, 1.0)),
            None => None,
        }
    }
}

#[derive(Clone)]
pub struct TransferEngine {
    inner: Arc<EngineInner>,
}

struct EngineInner {
    rt: Rt,
    permits: Arc<Semaphore>,
    next_id: AtomicU64,
    tasks: Mutex<Vec<Arc<TaskRecord>>>,
}

impl TransferEngine {
    pub fn new(rt: Rt, concurrency: usize) -> Self {
        Self {
            inner: Arc::new(EngineInner {
                rt,
                permits: Arc::new(Semaphore::new(concurrency.max(1))),
                next_id: AtomicU64::new(1),
                tasks: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Queue one transfer. Returns immediately; the work runs on the tokio
    /// runtime behind the global semaphore.
    pub fn enqueue(&self, transfer: Transfer) -> TaskId {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);

        let record = Arc::new(TaskRecord {
            id,
            label: transfer.label(),
            operation: transfer.operation(),
            total: transfer.known_size(),
            transfer,
            progress: Mutex::new(Arc::new(TaskProgress::new())),
            state: Mutex::new(TaskState::Queued),
            started: Mutex::new(None),
        });

        self.inner.tasks.lock().unwrap().push(record.clone());
        self.spawn_task(record);

        id
    }

    /// Run a task's work, from queued through to a terminal state.
    fn spawn_task(&self, record: Arc<TaskRecord>) {
        let permits = self.inner.permits.clone();
        self.inner.rt.handle().spawn(async move {
            // Held for the duration of the task; this is the global cap.
            let _permit = match permits.acquire().await {
                Ok(permit) => permit,
                Err(_) => return,
            };

            let progress = record.progress.lock().unwrap().clone();

            // A task cancelled while it was still queued must not start.
            if progress.is_cancelled() {
                *record.state.lock().unwrap() = TaskState::Cancelled;
                return;
            }

            *record.state.lock().unwrap() = TaskState::Running;
            *record.started.lock().unwrap() = Some(Instant::now());

            let outcome = run(&record.transfer, &progress).await;

            let mut state = record.state.lock().unwrap();
            *state = match outcome {
                Ok(()) => TaskState::Done,
                Err(err) if err.is_cancelled() => TaskState::Cancelled,
                Err(err) => TaskState::Failed(err.user_message()),
            };
        });
    }

    /// Run a failed task again.
    ///
    /// Returns false when the task is not in a state worth retrying, so a UI can
    /// disable the button rather than offering a no-op.
    pub fn retry(&self, id: TaskId) -> bool {
        let tasks = self.inner.tasks.lock().unwrap();
        let Some(record) = tasks.iter().find(|t| t.id == id).cloned() else {
            return false;
        };
        if !matches!(*record.state.lock().unwrap(), TaskState::Failed(_)) {
            return false;
        }

        // A fresh progress handle: the old one may carry a cancellation flag, and
        // the byte count restarts from whatever the pump can resume at.
        *record.progress.lock().unwrap() = Arc::new(TaskProgress::new());
        *record.state.lock().unwrap() = TaskState::Queued;
        *record.started.lock().unwrap() = None;
        drop(tasks);

        self.spawn_task(record);
        true
    }

    /// Retry every failed task.
    pub fn retry_all_failed(&self) -> usize {
        let ids: Vec<TaskId> = self
            .inner
            .tasks
            .lock()
            .unwrap()
            .iter()
            .filter(|t| matches!(*t.state.lock().unwrap(), TaskState::Failed(_)))
            .map(|t| t.id)
            .collect();

        ids.into_iter().filter(|id| self.retry(*id)).count()
    }

    pub fn enqueue_all(&self, transfers: impl IntoIterator<Item = Transfer>) -> Vec<TaskId> {
        transfers.into_iter().map(|t| self.enqueue(t)).collect()
    }

    /// Ask a task to stop. It finishes as `Cancelled` after its current chunk.
    pub fn cancel(&self, id: TaskId) {
        let tasks = self.inner.tasks.lock().unwrap();
        if let Some(record) = tasks.iter().find(|t| t.id == id) {
            record.progress.lock().unwrap().cancel();
        }
    }

    pub fn cancel_all(&self) {
        for record in self.inner.tasks.lock().unwrap().iter() {
            if !record.state.lock().unwrap().is_finished() {
                record.progress.lock().unwrap().cancel();
            }
        }
    }

    /// Drop finished tasks from the list.
    pub fn clear_finished(&self) {
        self.inner
            .tasks
            .lock()
            .unwrap()
            .retain(|record| !record.state.lock().unwrap().is_finished());
    }

    pub fn snapshot(&self) -> Vec<TaskSnapshot> {
        self.inner
            .tasks
            .lock()
            .unwrap()
            .iter()
            .map(|record| {
                let done = record.progress.lock().unwrap().done();
                let state = record.state.lock().unwrap().clone();

                let bytes_per_sec = record
                    .started
                    .lock()
                    .unwrap()
                    .and_then(|started| {
                        let secs = started.elapsed().as_secs_f64();
                        (secs > 0.05).then(|| (done as f64 / secs) as u64)
                    })
                    .filter(|_| matches!(state, TaskState::Running));

                TaskSnapshot {
                    id: record.id,
                    retryable: matches!(state, TaskState::Failed(_)),
                    label: record.label.clone(),
                    operation: record.operation,
                    state,
                    done,
                    total: record.total,
                    bytes_per_sec,
                }
            })
            .collect()
    }

    /// True while any task is queued or running — the UI polls progress only
    /// while this holds.
    pub fn is_active(&self) -> bool {
        self.inner
            .tasks
            .lock()
            .unwrap()
            .iter()
            .any(|record| !record.state.lock().unwrap().is_finished())
    }

    pub fn len(&self) -> usize {
        self.inner.tasks.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

async fn run(transfer: &Transfer, progress: &Arc<TaskProgress>) -> Result<()> {
    match transfer {
        Transfer::Download {
            vfs, remote, local, ..
        } => {
            vfs.download_to(remote, local.clone(), progress.clone())
                .await
        }
        Transfer::Upload { vfs, local, remote } => {
            vfs.upload_from(local.clone(), remote, progress.clone())
                .await
        }
        Transfer::Copy {
            from,
            from_path,
            to,
            to_path,
            ..
        } => from.copy_to(from_path, to, to_path, progress.clone()).await,
        Transfer::MoveDir {
            vfs,
            from,
            to,
            files,
        } => move_dir(vfs, from, to, files, progress).await,
    }
}

/// Copy every file to the new prefix, then remove the old one.
///
/// The delete happens **only after every copy succeeded**. A failure or a cancel
/// therefore leaves the source untouched, with some copies already at the
/// destination — nothing is lost, and re-running overwrites those copies.
async fn move_dir(
    vfs: &Vfs,
    from: &str,
    to: &str,
    files: &Arc<Vec<DirEntry>>,
    progress: &Arc<TaskProgress>,
) -> Result<()> {
    let from_root = path::as_dir(from);
    let to_root = path::as_dir(to);

    for file in files.iter() {
        progress.check()?;

        let relative = file.path.strip_prefix(&from_root).unwrap_or(&file.path);
        let target = format!("{to_root}{relative}");

        // Same backend, so this is a server-side copy wherever one is offered.
        vfs.copy_to(&file.path, vfs, &target, progress.clone())
            .await?;
    }

    progress.check()?;

    // Every byte is in its new place; only now is the old prefix removed.
    let source = DirEntry {
        name: path::basename(&from_root).into(),
        path: from_root.as_str().into(),
        kind: crate::EntryKind::Dir,
        size: None,
        modified: None,
        etag: None,
        meta_complete: true,
    };
    vfs.delete(&source).await
}

// ── planning ──────────────────────────────────────────────────────────────

/// Expand a remote entry into one download task per file.
///
/// Directories are flattened up front rather than discovered while running, so
/// the task count and byte totals are right from the start — a progress bar that
/// keeps growing its own total is worse than a brief pause before it appears.
pub async fn plan_download(
    vfs: &Vfs,
    entry: &DirEntry,
    dest_dir: &std::path::Path,
) -> Result<Vec<Transfer>> {
    if !entry.is_dir() {
        return Ok(vec![Transfer::Download {
            vfs: vfs.clone(),
            remote: entry.path.clone(),
            local: dest_dir.join(&*entry.name),
            size: entry.size,
        }]);
    }

    let root = path::as_dir(&entry.path);
    let base = dest_dir.join(&*entry.name);

    let files = vfs.list_recursive(&root).await?;
    Ok(files
        .into_iter()
        .filter(|e| !e.is_dir())
        .map(|file| {
            // Path relative to the directory being downloaded, so the local
            // tree mirrors the remote one.
            let relative = file.path.strip_prefix(&root).unwrap_or(&file.path);
            Transfer::Download {
                vfs: vfs.clone(),
                remote: file.path.clone(),
                local: base.join(relative),
                size: file.size,
            }
        })
        .collect())
}

/// Expand local paths into one upload task per file, walking directories so a
/// dropped folder arrives with its structure intact.
///
/// Unreadable directories are skipped rather than failing the whole drop: one
/// permission-denied subfolder should not cancel the other twenty files.
pub fn plan_upload(vfs: &Vfs, local: &[PathBuf], dest_dir: &str) -> Vec<Transfer> {
    fn walk(out: &mut Vec<(PathBuf, String)>, source: &std::path::Path, prefix: &str) {
        let Some(name) = source.file_name().map(|n| n.to_string_lossy().to_string()) else {
            return;
        };
        if name.is_empty() {
            return;
        }

        if source.is_dir() {
            let nested = path::join_dir(prefix, &name);
            let Ok(entries) = std::fs::read_dir(source) else {
                tracing::warn!(path = %source.display(), "skipping unreadable directory");
                return;
            };
            for entry in entries.flatten() {
                walk(out, &entry.path(), &nested);
            }
        } else {
            out.push((source.to_path_buf(), path::join_file(prefix, &name)));
        }
    }

    let mut pairs = Vec::new();
    for source in local {
        walk(&mut pairs, source, dest_dir);
    }

    pairs
        .into_iter()
        .map(|(local, remote)| Transfer::Upload {
            vfs: vfs.clone(),
            local,
            remote: Arc::from(remote.as_str()),
        })
        .collect()
}

/// Build the single task that renames a directory.
///
/// Flattened up front so the byte total is known before anything moves, and so
/// the task can report real progress rather than a spinner.
pub async fn plan_move_dir(vfs: &Vfs, from: &str, to: &str) -> Result<Transfer> {
    let from_root = path::as_dir(from);

    let files: Vec<DirEntry> = vfs
        .list_recursive(&from_root)
        .await?
        .into_iter()
        .filter(|entry| !entry.is_dir())
        .collect();

    Ok(Transfer::MoveDir {
        vfs: vfs.clone(),
        from: from_root.as_str().into(),
        to: path::as_dir(to).as_str().into(),
        files: Arc::new(files),
    })
}

/// Expand a remote directory into one server-side copy per file.
///
/// This is what makes "duplicate" work for a directory: OpenDAL's `copy` only
/// handles single objects, so the directory has to be flattened first.
pub async fn plan_duplicate_dir(
    vfs: &Vfs,
    entry: &DirEntry,
    dest_dir: &str,
) -> Result<Vec<Transfer>> {
    let root = path::as_dir(&entry.path);
    let target_root = path::as_dir(dest_dir);

    let files = vfs.list_recursive(&root).await?;
    Ok(files
        .into_iter()
        .filter(|e| !e.is_dir())
        .map(|file| {
            let relative = file.path.strip_prefix(&root).unwrap_or(&file.path);
            Transfer::Copy {
                from: vfs.clone(),
                from_path: file.path.clone(),
                to: vfs.clone(),
                to_path: Arc::from(format!("{target_root}{relative}").as_str()),
                size: file.size,
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_labels_are_stable() {
        assert_eq!(Operation::Download.label(), "下载");
        assert_eq!(Operation::Upload.label(), "上传");
        assert_eq!(Operation::Copy.label(), "复制");
    }

    #[test]
    fn finished_states_are_recognised() {
        assert!(TaskState::Done.is_finished());
        assert!(TaskState::Cancelled.is_finished());
        assert!(TaskState::Failed("boom".into()).is_finished());
        assert!(!TaskState::Queued.is_finished());
        assert!(!TaskState::Running.is_finished());
    }

    #[test]
    fn fraction_handles_unknown_and_empty_totals() {
        let snapshot = |done, total| TaskSnapshot {
            id: 1,
            label: "x".into(),
            operation: Operation::Download,
            state: TaskState::Running,
            done,
            total,
            bytes_per_sec: None,
            retryable: false,
        };

        assert_eq!(snapshot(0, None).fraction(), None, "unknown stays unknown");
        assert_eq!(snapshot(0, Some(0)).fraction(), Some(1.0), "empty is done");
        assert_eq!(snapshot(50, Some(100)).fraction(), Some(0.5));
        // A backend that under-reports its size must not produce a bar past the
        // end of its track.
        assert_eq!(snapshot(150, Some(100)).fraction(), Some(1.0));
    }

    #[test]
    fn progress_accumulates_and_cancels() {
        let progress = TaskProgress::new();

        progress.advance(10);
        progress.advance(5);
        assert_eq!(progress.done(), 15);
        assert!(progress.check().is_ok());

        progress.cancel();
        assert!(progress.check().unwrap_err().is_cancelled());
    }

    // ── engine integration ────────────────────────────────────────────────

    use crate::Vfs;
    use std::time::Duration;

    fn fixture() -> (tempfile::TempDir, Vfs) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("tree/nested")).unwrap();
        std::fs::write(dir.path().join("a.txt"), b"hello").unwrap();
        std::fs::write(dir.path().join("tree/b.txt"), b"bee").unwrap();
        std::fs::write(dir.path().join("tree/nested/c.txt"), b"sea").unwrap();

        let vfs = Vfs::local(Rt::from_current().unwrap(), dir.path().to_str().unwrap()).unwrap();
        (dir, vfs)
    }

    /// The engine runs on real tokio threads, so poll until it settles.
    async fn settle(engine: &TransferEngine) {
        for _ in 0..600 {
            if !engine.is_active() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("transfers never settled: {:?}", engine.snapshot());
    }

    #[tokio::test]
    async fn downloads_a_file_to_disk() {
        let (_guard, vfs) = fixture();
        let out = tempfile::tempdir().unwrap();
        let engine = TransferEngine::new(Rt::from_current().unwrap(), DEFAULT_CONCURRENCY);

        engine.enqueue(Transfer::Download {
            vfs,
            remote: "a.txt".into(),
            local: out.path().join("a.txt"),
            size: Some(5),
        });
        settle(&engine).await;

        let snapshot = &engine.snapshot()[0];
        assert_eq!(snapshot.state, TaskState::Done);
        assert_eq!(snapshot.done, 5, "progress reached the total");
        assert_eq!(
            std::fs::read_to_string(out.path().join("a.txt")).unwrap(),
            "hello"
        );
    }

    #[tokio::test]
    async fn uploads_a_file() {
        let (guard, vfs) = fixture();
        let source = tempfile::tempdir().unwrap();
        std::fs::write(source.path().join("up.txt"), b"uploaded").unwrap();

        let engine = TransferEngine::new(Rt::from_current().unwrap(), DEFAULT_CONCURRENCY);
        engine.enqueue(Transfer::Upload {
            vfs,
            local: source.path().join("up.txt"),
            remote: "up.txt".into(),
        });
        settle(&engine).await;

        assert_eq!(engine.snapshot()[0].state, TaskState::Done);
        assert_eq!(
            std::fs::read_to_string(guard.path().join("up.txt")).unwrap(),
            "uploaded"
        );
    }

    #[tokio::test]
    async fn a_file_larger_than_one_chunk_survives_the_round_trip() {
        // Exercises the multi-chunk loop and the progress accumulation, which a
        // small file would skip entirely.
        let (guard, vfs) = fixture();
        let source = tempfile::tempdir().unwrap();
        let big = source.path().join("big.bin");

        let size = CHUNK + 1024;
        let payload: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        std::fs::write(&big, &payload).unwrap();

        let engine = TransferEngine::new(Rt::from_current().unwrap(), DEFAULT_CONCURRENCY);
        engine.enqueue(Transfer::Upload {
            vfs: vfs.clone(),
            local: big,
            remote: "big.bin".into(),
        });
        settle(&engine).await;

        assert_eq!(engine.snapshot()[0].state, TaskState::Done);
        assert_eq!(engine.snapshot()[0].done, size as u64);
        assert_eq!(
            std::fs::read(guard.path().join("big.bin")).unwrap(),
            payload,
            "bytes must survive chunking exactly"
        );

        // And back down again.
        let out = tempfile::tempdir().unwrap();
        engine.clear_finished();
        engine.enqueue(Transfer::Download {
            vfs,
            remote: "big.bin".into(),
            local: out.path().join("big.bin"),
            size: Some(size as u64),
        });
        settle(&engine).await;

        assert_eq!(std::fs::read(out.path().join("big.bin")).unwrap(), payload);
    }

    #[tokio::test]
    async fn a_cancelled_download_leaves_no_partial_file() {
        let (_guard, vfs) = fixture();
        let out = tempfile::tempdir().unwrap();
        let local = out.path().join("partial.txt");

        // Cancelled before the first chunk is written, which is the same code
        // path a mid-transfer cancel takes.
        let progress = Arc::new(TaskProgress::new());
        progress.cancel();

        let err = vfs
            .download_to("a.txt", local.clone(), progress)
            .await
            .unwrap_err();

        assert!(err.is_cancelled());
        assert!(
            !local.exists(),
            "a half-written file that looks complete is worse than none"
        );
    }

    #[tokio::test]
    async fn cancelling_a_queued_task_never_starts_it() {
        let (_guard, vfs) = fixture();
        let out = tempfile::tempdir().unwrap();

        // One permit, so the second task cannot start until the first ends.
        let engine = TransferEngine::new(Rt::from_current().unwrap(), 1);
        let first = engine.enqueue(Transfer::Download {
            vfs: vfs.clone(),
            remote: "a.txt".into(),
            local: out.path().join("first.txt"),
            size: Some(5),
        });
        let second = engine.enqueue(Transfer::Download {
            vfs,
            remote: "tree/b.txt".into(),
            local: out.path().join("second.txt"),
            size: Some(3),
        });

        engine.cancel(second);
        settle(&engine).await;

        let snapshot = engine.snapshot();
        let state = |id| {
            snapshot
                .iter()
                .find(|t| t.id == id)
                .map(|t| t.state.clone())
                .unwrap()
        };

        assert_eq!(state(first), TaskState::Done);
        assert_eq!(state(second), TaskState::Cancelled);
        assert!(!out.path().join("second.txt").exists());
    }

    #[tokio::test]
    async fn a_failed_task_records_a_readable_reason() {
        let (_guard, vfs) = fixture();
        let out = tempfile::tempdir().unwrap();
        let engine = TransferEngine::new(Rt::from_current().unwrap(), DEFAULT_CONCURRENCY);

        engine.enqueue(Transfer::Download {
            vfs,
            remote: "does-not-exist.txt".into(),
            local: out.path().join("nope.txt"),
            size: None,
        });
        settle(&engine).await;

        match &engine.snapshot()[0].state {
            TaskState::Failed(reason) => assert_eq!(reason, "路径已不存在"),
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_failed_task_can_be_retried_and_succeed() {
        let (guard, vfs) = fixture();
        let out = tempfile::tempdir().unwrap();
        let engine = TransferEngine::new(Rt::from_current().unwrap(), DEFAULT_CONCURRENCY);

        // Fails because the object does not exist yet.
        let id = engine.enqueue(Transfer::Download {
            vfs: vfs.clone(),
            remote: "late.txt".into(),
            local: out.path().join("late.txt"),
            size: None,
        });
        settle(&engine).await;
        assert!(matches!(engine.snapshot()[0].state, TaskState::Failed(_)));
        assert!(engine.snapshot()[0].retryable);

        // Now it exists; the same task should be able to run again.
        std::fs::write(guard.path().join("late.txt"), b"arrived").unwrap();
        assert!(engine.retry(id));
        settle(&engine).await;

        assert_eq!(engine.snapshot()[0].state, TaskState::Done);
        assert_eq!(
            std::fs::read_to_string(out.path().join("late.txt")).unwrap(),
            "arrived"
        );
        assert_eq!(engine.len(), 1, "a retry reuses the task, not a new row");
    }

    #[tokio::test]
    async fn only_failed_tasks_are_retryable() {
        let (_guard, vfs) = fixture();
        let out = tempfile::tempdir().unwrap();
        let engine = TransferEngine::new(Rt::from_current().unwrap(), DEFAULT_CONCURRENCY);

        let id = engine.enqueue(Transfer::Download {
            vfs,
            remote: "a.txt".into(),
            local: out.path().join("a.txt"),
            size: Some(5),
        });
        settle(&engine).await;

        assert_eq!(engine.snapshot()[0].state, TaskState::Done);
        assert!(!engine.snapshot()[0].retryable);
        assert!(!engine.retry(id), "re-running a success would redo work");
        assert!(!engine.retry(9999), "and an unknown id is not retryable");
    }

    #[tokio::test]
    async fn retry_all_failed_reports_how_many_it_restarted() {
        let (guard, vfs) = fixture();
        let out = tempfile::tempdir().unwrap();
        let engine = TransferEngine::new(Rt::from_current().unwrap(), DEFAULT_CONCURRENCY);

        for name in ["one.txt", "two.txt"] {
            engine.enqueue(Transfer::Download {
                vfs: vfs.clone(),
                remote: name.into(),
                local: out.path().join(name),
                size: None,
            });
        }
        engine.enqueue(Transfer::Download {
            vfs,
            remote: "a.txt".into(),
            local: out.path().join("a.txt"),
            size: Some(5),
        });
        settle(&engine).await;

        std::fs::write(guard.path().join("one.txt"), b"1").unwrap();
        std::fs::write(guard.path().join("two.txt"), b"2").unwrap();

        // Only the two failures restart; the successful one is left alone.
        assert_eq!(engine.retry_all_failed(), 2);
        settle(&engine).await;
        assert!(engine.snapshot().iter().all(|t| t.state == TaskState::Done));
    }

    #[tokio::test]
    async fn clear_finished_keeps_only_live_tasks() {
        let (_guard, vfs) = fixture();
        let out = tempfile::tempdir().unwrap();
        let engine = TransferEngine::new(Rt::from_current().unwrap(), DEFAULT_CONCURRENCY);

        engine.enqueue(Transfer::Download {
            vfs,
            remote: "a.txt".into(),
            local: out.path().join("a.txt"),
            size: Some(5),
        });
        settle(&engine).await;

        assert_eq!(engine.len(), 1);
        engine.clear_finished();
        assert!(engine.is_empty());
    }

    #[tokio::test]
    async fn a_same_backend_copy_stays_server_side() {
        let (guard, vfs) = fixture();
        let engine = TransferEngine::new(Rt::from_current().unwrap(), DEFAULT_CONCURRENCY);

        assert!(vfs.same_backend(&vfs.clone()), "clones share the backend");

        engine.enqueue(Transfer::Copy {
            from: vfs.clone(),
            from_path: "a.txt".into(),
            to: vfs,
            to_path: "a-copy.txt".into(),
            size: Some(5),
        });
        settle(&engine).await;

        assert_eq!(engine.snapshot()[0].state, TaskState::Done);
        assert_eq!(
            std::fs::read_to_string(guard.path().join("a-copy.txt")).unwrap(),
            "hello"
        );
    }

    #[tokio::test]
    async fn separate_backends_are_not_the_same_backend() {
        let (_g1, first) = fixture();
        let (_g2, second) = fixture();

        // Two operators over two roots: a copy between them has to stream.
        assert!(!first.same_backend(&second));
    }

    #[tokio::test]
    async fn a_cross_backend_copy_streams_the_bytes() {
        let (_g1, source) = fixture();
        let (target_guard, target) = fixture();
        let engine = TransferEngine::new(Rt::from_current().unwrap(), DEFAULT_CONCURRENCY);

        engine.enqueue(Transfer::Copy {
            from: source,
            from_path: "a.txt".into(),
            to: target,
            to_path: "arrived.txt".into(),
            size: Some(5),
        });
        settle(&engine).await;

        assert_eq!(engine.snapshot()[0].state, TaskState::Done);
        assert_eq!(
            std::fs::read_to_string(target_guard.path().join("arrived.txt")).unwrap(),
            "hello"
        );
    }

    #[tokio::test]
    async fn plan_download_flattens_a_directory_and_mirrors_it_locally() {
        let (_guard, vfs) = fixture();
        let out = tempfile::tempdir().unwrap();

        let entries = vfs.list_all("").await.unwrap();
        let tree = entries.iter().find(|e| &*e.name == "tree").unwrap();

        let plan = plan_download(&vfs, tree, out.path()).await.unwrap();
        assert_eq!(plan.len(), 2, "one task per file, directories excluded");

        let engine = TransferEngine::new(Rt::from_current().unwrap(), DEFAULT_CONCURRENCY);
        engine.enqueue_all(plan);
        settle(&engine).await;

        assert_eq!(
            std::fs::read_to_string(out.path().join("tree/b.txt")).unwrap(),
            "bee"
        );
        assert_eq!(
            std::fs::read_to_string(out.path().join("tree/nested/c.txt")).unwrap(),
            "sea",
            "the nested structure is preserved"
        );
    }

    #[tokio::test]
    async fn plan_upload_walks_a_local_directory() {
        let (guard, vfs) = fixture();
        let source = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(source.path().join("payload/inner")).unwrap();
        std::fs::write(source.path().join("payload/one.txt"), b"1").unwrap();
        std::fs::write(source.path().join("payload/inner/two.txt"), b"2").unwrap();

        let plan = plan_upload(&vfs, &[source.path().join("payload")], "dest/");
        assert_eq!(plan.len(), 2);

        let engine = TransferEngine::new(Rt::from_current().unwrap(), DEFAULT_CONCURRENCY);
        engine.enqueue_all(plan);
        settle(&engine).await;

        assert_eq!(
            std::fs::read_to_string(guard.path().join("dest/payload/one.txt")).unwrap(),
            "1"
        );
        assert_eq!(
            std::fs::read_to_string(guard.path().join("dest/payload/inner/two.txt")).unwrap(),
            "2"
        );
    }

    #[tokio::test]
    async fn moving_a_directory_relocates_its_whole_tree() {
        let (guard, vfs) = fixture();
        let engine = TransferEngine::new(Rt::from_current().unwrap(), DEFAULT_CONCURRENCY);

        let plan = plan_move_dir(&vfs, "tree/", "moved/").await.unwrap();
        engine.enqueue(plan);
        settle(&engine).await;

        assert_eq!(engine.snapshot()[0].state, TaskState::Done);
        assert_eq!(
            std::fs::read_to_string(guard.path().join("moved/b.txt")).unwrap(),
            "bee"
        );
        assert_eq!(
            std::fs::read_to_string(guard.path().join("moved/nested/c.txt")).unwrap(),
            "sea",
            "nesting is preserved"
        );
        assert!(
            !guard.path().join("tree").exists(),
            "the source is removed once every copy landed"
        );
    }

    #[tokio::test]
    async fn a_move_reports_the_total_size_up_front() {
        let (_guard, vfs) = fixture();

        // "bee" + "sea" = 6 bytes, known before anything is copied.
        let plan = plan_move_dir(&vfs, "tree/", "moved/").await.unwrap();
        let engine = TransferEngine::new(Rt::from_current().unwrap(), DEFAULT_CONCURRENCY);
        engine.enqueue(plan);

        assert_eq!(engine.snapshot()[0].total, Some(6));
        assert_eq!(engine.snapshot()[0].operation, Operation::Move);
        settle(&engine).await;
    }

    #[tokio::test]
    async fn a_cancelled_move_leaves_the_source_intact() {
        let (guard, vfs) = fixture();
        let engine = TransferEngine::new(Rt::from_current().unwrap(), DEFAULT_CONCURRENCY);

        let plan = plan_move_dir(&vfs, "tree/", "moved/").await.unwrap();
        let id = engine.enqueue(plan);
        engine.cancel(id);
        settle(&engine).await;

        assert_eq!(engine.snapshot()[0].state, TaskState::Cancelled);
        // Nothing is lost by cancelling: the delete only ever runs after every
        // copy has succeeded.
        assert!(guard.path().join("tree/b.txt").exists());
        assert!(guard.path().join("tree/nested/c.txt").exists());
    }

    #[tokio::test]
    async fn moving_an_empty_directory_still_removes_it() {
        let (guard, vfs) = fixture();
        std::fs::create_dir(guard.path().join("hollow")).unwrap();
        let engine = TransferEngine::new(Rt::from_current().unwrap(), DEFAULT_CONCURRENCY);

        let plan = plan_move_dir(&vfs, "hollow/", "hollow-renamed/")
            .await
            .unwrap();
        engine.enqueue(plan);
        settle(&engine).await;

        assert_eq!(engine.snapshot()[0].state, TaskState::Done);
        assert!(!guard.path().join("hollow").exists());
    }

    #[tokio::test]
    async fn plan_duplicate_dir_mirrors_the_tree() {
        let (guard, vfs) = fixture();

        let entries = vfs.list_all("").await.unwrap();
        let tree = entries.iter().find(|e| &*e.name == "tree").unwrap();

        let plan = plan_duplicate_dir(&vfs, tree, "tree 副本/").await.unwrap();
        let engine = TransferEngine::new(Rt::from_current().unwrap(), DEFAULT_CONCURRENCY);
        engine.enqueue_all(plan);
        settle(&engine).await;

        for task in engine.snapshot() {
            assert_eq!(task.state, TaskState::Done, "{task:?}");
        }
        assert_eq!(
            std::fs::read_to_string(guard.path().join("tree 副本/b.txt")).unwrap(),
            "bee"
        );
        assert_eq!(
            std::fs::read_to_string(guard.path().join("tree 副本/nested/c.txt")).unwrap(),
            "sea"
        );
    }

    #[tokio::test]
    async fn list_recursive_returns_every_descendant() {
        let (_guard, vfs) = fixture();

        let all = vfs.list_recursive("").await.unwrap();
        let mut files: Vec<String> = all
            .iter()
            .filter(|e| !e.is_dir())
            .map(|e| e.path.to_string())
            .collect();
        files.sort();

        assert_eq!(files, vec!["a.txt", "tree/b.txt", "tree/nested/c.txt"]);
    }
}
