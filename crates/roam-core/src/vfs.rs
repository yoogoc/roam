use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use opendal::layers::{ConcurrentLimitLayer, RetryLayer, TimeoutLayer};
use opendal::{Capability, Operator, services};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::menu::{self, MenuItem};
use crate::profile::Profile;
use crate::transfer::{CHUNK, TaskProgress, WRITER_CONCURRENCY};
use crate::{DirEntry, Error, ObjectVersion, Result, Rt, path};

/// Suffix for a download in progress. The real filename is only created once
/// the bytes are all there.
pub const PART_SUFFIX: &str = ".roampart";

fn part_path(local: &std::path::Path) -> PathBuf {
    let mut name = local.as_os_str().to_os_string();
    name.push(PART_SUFFIX);
    PathBuf::from(name)
}

/// Sidecar holding the etag the part file was fetched against.
fn etag_path(part: &std::path::Path) -> PathBuf {
    let mut name = part.as_os_str().to_os_string();
    name.push(".etag");
    PathBuf::from(name)
}

/// An etag usable for resume validation, or `None`.
///
/// **Weak etags are rejected.** A `W/`-prefixed etag only promises semantic
/// equivalence, not identical bytes — which is precisely what appending to a
/// partial file requires. They are also unusable with `If-Match`, which RFC 7232
/// defines in terms of strong comparison: Apache's default weak etags make every
/// such request answer 412, as WebDAV demonstrated.
fn strong_etag(meta: &opendal::Metadata) -> Option<String> {
    let etag = meta.etag()?;
    (!etag.starts_with("W/") && !etag.starts_with("w/")).then(|| etag.to_string())
}

/// Local-filesystem errors, which are genuinely not OpenDAL's.
fn io_err(e: impl std::fmt::Display) -> Error {
    Error::Config(format!("本地文件操作失败: {e}"))
}

/// Errors from a byte stream.
///
/// OpenDAL's `into_bytes_stream` yields `io::Error` with the real
/// `opendal::Error` boxed inside. Unwrap it: otherwise a remote 404 is reported
/// as "本地文件操作失败", which is both wrong and loses the `ErrorKind` that
/// drives the message and the recovery action.
fn stream_err(e: std::io::Error) -> Error {
    match e.downcast::<opendal::Error>() {
        Ok(backend) => backend.into(),
        Err(local) => io_err(local),
    }
}

/// Entries per batch pushed to the UI. Large enough that a small directory
/// arrives in one go, small enough that a huge one paints incrementally.
pub const BATCH_SIZE: usize = 500;

/// Flush a partial batch once it is this old, so a slow backend still paints
/// rows instead of showing an empty pane.
pub const BATCH_INTERVAL: Duration = Duration::from_millis(100);

/// Concurrent `stat` calls when filling in metadata the listing omitted.
pub const STAT_CONCURRENCY: usize = 16;

/// Largest upload attempted against a backend that can only write in one
/// request. Such a write needs the whole body in memory, so this is a guard
/// against turning a large upload into an out-of-memory kill.
pub const ONE_SHOT_LIMIT: u64 = 512 * 1024 * 1024;

/// How many batches may sit in the channel before the producer blocks. Back
/// pressure here is deliberate: it keeps a fast backend from queueing megabytes
/// of entries the UI has not drawn yet.
const CHANNEL_DEPTH: usize = 8;

#[derive(Clone)]
pub struct Vfs {
    inner: Arc<VfsInner>,
}

impl std::fmt::Debug for Vfs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never derive this: the Operator's debug output can include
        // configuration such as endpoints, and this ends up in logs.
        f.debug_struct("Vfs")
            .field("label", &self.inner.label)
            .finish_non_exhaustive()
    }
}

struct VfsInner {
    op: Operator,
    rt: Rt,
    label: Arc<str>,
    capability: Capability,
}

impl Vfs {
    /// A local-filesystem session rooted at `root`.
    pub fn local(rt: Rt, root: impl AsRef<str>) -> Result<Self> {
        let root = root.as_ref();

        // Note the shape here: in OpenDAL 0.58 `Operator::new` already returns
        // an `Operator`, and `.layer()` returns one too — there is no
        // `.finish()` step any more.
        let op = Operator::new(services::Fs::default().root(root))?
            .layer(RetryLayer::new().with_max_times(3).with_jitter())
            .layer(TimeoutLayer::new().with_timeout(Duration::from_secs(30)))
            .layer(ConcurrentLimitLayer::new(32));

        Ok(Self::from_operator(rt, op, root))
    }

    /// Build a session from a saved profile, pulling credentials from `store`.
    pub fn from_profile(rt: Rt, profile: &Profile) -> Result<Self> {
        let options = profile.connect_options()?;

        // `from_uri` takes a single argument; options ride along as a tuple.
        let op = Operator::from_uri((profile.uri.as_str(), options))?
            .layer(RetryLayer::new().with_max_times(3).with_jitter())
            .layer(TimeoutLayer::new().with_timeout(Duration::from_secs(30)))
            .layer(ConcurrentLimitLayer::new(32));

        Ok(Self::from_operator(rt, op, &profile.name))
    }

    /// The context menu for `entry` on this backend.
    pub fn entry_menu(&self, entry: &DirEntry) -> Vec<MenuItem> {
        menu::entry_menu(&self.inner.capability, entry)
    }

    // ── mutations ─────────────────────────────────────────────────────────
    //
    // Each one re-checks the capability rather than trusting the caller. The
    // menu disables unsupported actions, but a keyboard shortcut or a future
    // call site could reach these directly, and a clear refusal beats a
    // backend-specific error from three layers down.

    fn require(&self, supported: bool, what: &str) -> Result<()> {
        if supported {
            Ok(())
        } else {
            Err(Error::Unsupported(format!(
                "{} 不支持{what}",
                self.inner.label
            )))
        }
    }

    /// Create a directory. `path` is normalized to directory form.
    pub fn create_dir(&self, path: &str) -> impl Future<Output = Result<()>> + Send + 'static {
        let op = self.inner.op.clone();
        let dir = path::as_dir(path);
        let check = self.require(self.inner.capability.create_dir, "新建目录");

        self.inner.rt.spawn(async move {
            check?;
            op.create_dir(&dir).await?;
            Ok(())
        })
    }

    pub fn rename(
        &self,
        from: &str,
        to: &str,
    ) -> impl Future<Output = Result<()>> + Send + 'static {
        let op = self.inner.op.clone();
        let (from, to) = (from.to_string(), to.to_string());

        // OpenDAL's `rename` rejects directory paths on every backend, so catch
        // it here rather than surfacing "from path is a directory" from deep
        // inside the operator. Directories go through
        // `transfer::plan_move_dir` instead, which copies and then deletes.
        let check = if from.ends_with('/') || to.ends_with('/') {
            Err(Error::Unsupported(
                "重命名目录请使用移动任务（copy + delete）".into(),
            ))
        } else {
            self.require(self.inner.capability.rename, "重命名")
        };

        self.inner.rt.spawn(async move {
            check?;
            op.rename(&from, &to).await?;
            Ok(())
        })
    }

    /// Server-side copy where the backend offers one.
    pub fn copy(&self, from: &str, to: &str) -> impl Future<Output = Result<()>> + Send + 'static {
        let op = self.inner.op.clone();
        let (from, to) = (from.to_string(), to.to_string());
        let check = self.require(self.inner.capability.copy, "复制");

        self.inner.rt.spawn(async move {
            check?;
            op.copy(&from, &to).await?;
            Ok(())
        })
    }

    /// Delete an entry. Directories go through `remove_all`, since deleting a
    /// directory means deleting what is inside it.
    pub fn delete(&self, entry: &DirEntry) -> impl Future<Output = Result<()>> + Send + 'static {
        let op = self.inner.op.clone();
        let path = entry.path.to_string();
        let is_dir = entry.is_dir();

        let check = if is_dir {
            self.require(
                self.inner.capability.delete && self.inner.capability.list,
                "删除目录",
            )
        } else {
            self.require(self.inner.capability.delete, "删除")
        };

        self.inner.rt.spawn(async move {
            check?;
            if is_dir {
                // `remove_all` is deprecated in 0.58 in favour of spelling the
                // recursion out here.
                op.delete_with(&path).recursive(true).await?;
            } else {
                op.delete(&path).await?;
            }
            Ok(())
        })
    }

    // ── byte movement ─────────────────────────────────────────────────────
    //
    // These are the transfer engine's primitives. They live here so that
    // `opendal` stays behind the Vfs boundary and `transfer.rs` deals only in
    // paths, progress, and errors.
    //
    // All three are `async fn` rather than the `rt.spawn` shape used above: the
    // engine already runs them on the tokio runtime, so spawning again would add
    // a hop and lose the cooperative-cancellation point.

    /// The engine-only methods below run on the tokio runtime already, because
    /// `TransferEngine` spawns them there. This makes a misuse say so instead of
    /// surfacing OpenDAL's opaque "there is no reactor running" panic.
    fn debug_assert_on_tokio(what: &str) {
        debug_assert!(
            tokio::runtime::Handle::try_current().is_ok(),
            "Vfs::{what} must run on the tokio runtime — call it through \
             TransferEngine, or wrap it in Rt::spawn (see roam_core::rt rule 1)"
        );
    }

    /// True when both handles wrap the same backend, so a copy between them can
    /// stay server-side.
    pub fn same_backend(&self, other: &Vfs) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    /// Stream a remote object into a local file, resuming a previous attempt
    /// when one was interrupted.
    ///
    /// Bytes land in a `.roampart` sidefile and are renamed into place only on
    /// success, so **a partial download never occupies the real filename** — a
    /// half-written file that looks complete is worse than no file at all.
    ///
    /// What happens to the part file depends on why the transfer stopped:
    ///
    /// - **cancelled** — deleted; the user said stop.
    /// - **failed** — kept, so a retry resumes instead of starting over.
    ///
    /// Resuming is only safe if the object has not changed underneath, so the
    /// first attempt records the etag next to the part file and the resumed
    /// request carries `if_match`. Without a usable etag the download restarts
    /// from zero rather than risking two versions spliced together.
    pub async fn download_to(
        &self,
        remote: &str,
        local: PathBuf,
        progress: Arc<TaskProgress>,
    ) -> Result<()> {
        Self::debug_assert_on_tokio("download_to");

        if let Some(parent) = local.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(io_err)?;
        }

        let part = part_path(&local);
        let outcome = self.download_inner(remote, &local, &part, &progress).await;

        match &outcome {
            Ok(()) => {}
            Err(err) if err.is_cancelled() => {
                let _ = tokio::fs::remove_file(&part).await;
                let _ = tokio::fs::remove_file(etag_path(&part)).await;
            }
            // Kept on purpose: this is what makes a retry a resume.
            Err(_) => {}
        }

        outcome
    }

    async fn download_inner(
        &self,
        remote: &str,
        local: &PathBuf,
        part: &PathBuf,
        progress: &Arc<TaskProgress>,
    ) -> Result<()> {
        let meta = self.inner.op.stat(remote).await?;
        let etag = strong_etag(&meta);
        let total = meta.content_length();

        let resume_from = self.resumable_offset(part, etag.as_deref(), total).await;

        if resume_from > 0 && resume_from == total {
            // Everything was already fetched; only the rename was missing.
            tokio::fs::rename(part, local).await.map_err(io_err)?;
            let _ = tokio::fs::remove_file(etag_path(part)).await;
            progress.advance(total);
            return Ok(());
        }

        if resume_from > 0 {
            match self
                .fetch_into(remote, part, resume_from, etag.as_deref(), progress)
                .await
            {
                Ok(()) => {}
                // The object moved on — or the server's etag did. Apache flips
                // between strong and weak etags for a recently modified file, so
                // an `If-Match` that was valid a moment ago answers 412. Either
                // way the prefix cannot be trusted, and refetching is the correct
                // answer rather than failing the download.
                Err(err) if err.kind() == Some(opendal::ErrorKind::ConditionNotMatch) => {
                    tracing::debug!(
                        path = remote,
                        "resume rejected by the server; refetching from the start"
                    );
                    let _ = tokio::fs::remove_file(part).await;
                    let _ = tokio::fs::remove_file(etag_path(part)).await;
                    self.fetch_into(remote, part, 0, None, progress).await?;
                }
                Err(err) => return Err(err),
            }
        } else {
            let _ = tokio::fs::remove_file(part).await;
            // No prefix to protect, so no conditional header: sending one would
            // turn any etag quirk into a failed download.
            self.fetch_into(remote, part, 0, None, progress).await?;
        }

        // Only now does the real filename appear.
        tokio::fs::rename(part, local).await.map_err(io_err)?;
        let _ = tokio::fs::remove_file(etag_path(part)).await;
        Ok(())
    }

    /// Stream `remote` from `offset` into the part file.
    ///
    /// `if_match` is passed only when resuming, where it is the guard that stops
    /// two different versions being spliced together.
    async fn fetch_into(
        &self,
        remote: &str,
        part: &PathBuf,
        offset: u64,
        if_match: Option<&str>,
        progress: &Arc<TaskProgress>,
    ) -> Result<()> {
        use futures::TryStreamExt;
        use tokio::io::AsyncWriteExt;

        let mut builder = self.inner.op.reader_with(remote);
        if let (Some(etag), true) = (if_match, self.inner.capability.read_with_if_match) {
            builder = builder.if_match(etag);
        }
        let reader = builder.await?;

        // Streamed rather than read into a Buffer: the whole point of resuming is
        // large objects, and buffering one would need as much memory as it has
        // bytes.
        let mut stream = reader.into_bytes_stream(offset..).await?;

        let mut file = if offset > 0 {
            tokio::fs::OpenOptions::new()
                .append(true)
                .open(part)
                .await
                .map_err(io_err)?
        } else {
            tokio::fs::File::create(part).await.map_err(io_err)?
        };

        // Record what the resumed bytes belong to, so a later attempt can decide
        // whether continuing is safe.
        if offset == 0 {
            match self
                .inner
                .op
                .stat(remote)
                .await
                .ok()
                .and_then(|m| strong_etag(&m))
            {
                Some(etag) => {
                    let _ = tokio::fs::write(etag_path(part), etag).await;
                }
                None => {
                    let _ = tokio::fs::remove_file(etag_path(part)).await;
                }
            }
        }
        progress.advance(offset);

        while let Some(chunk) = stream.try_next().await.map_err(stream_err)? {
            progress.check()?;
            file.write_all(&chunk).await.map_err(io_err)?;
            progress.advance(chunk.len() as u64);
        }

        file.flush().await.map_err(io_err)?;
        Ok(())
    }

    /// Bytes of `part` that may be reused, or 0 to start over.
    async fn resumable_offset(&self, part: &PathBuf, etag: Option<&str>, total: u64) -> u64 {
        let Ok(meta) = tokio::fs::metadata(part).await else {
            return 0;
        };
        let have = meta.len();
        if have == 0 || have > total {
            // Longer than the object means it is not a prefix of it.
            return 0;
        }

        // An etag mismatch — or no recorded etag — means the part cannot be
        // trusted to belong to this object.
        let recorded = tokio::fs::read_to_string(etag_path(part)).await.ok();
        match (recorded.as_deref(), etag) {
            (Some(recorded), Some(current)) if recorded == current => have,
            _ => 0,
        }
    }

    /// Stream a local file into a remote object, in parallel parts.
    ///
    /// Backends that report `write_can_multi == false` — WebDAV among them — take
    /// a one-shot path instead. Asking such a backend for a chunked write fails
    /// with "OneShotWriter doesn't support multiple write" for **any** file over
    /// one chunk, which is 8 MB: small enough that this is an everyday upload,
    /// not an edge case.
    pub async fn upload_from(
        &self,
        local: PathBuf,
        remote: &str,
        progress: Arc<TaskProgress>,
    ) -> Result<()> {
        use tokio::io::AsyncReadExt;
        Self::debug_assert_on_tokio("upload_from");

        if !self.inner.capability.write {
            return Err(Error::Unsupported(format!(
                "{} 不支持写入",
                self.inner.label
            )));
        }

        if !self.inner.capability.write_can_multi {
            return self.upload_one_shot(&local, remote, &progress).await;
        }

        let mut file = tokio::fs::File::open(&local).await.map_err(io_err)?;
        let mut writer = self
            .inner
            .op
            .writer_with(remote)
            .concurrent(WRITER_CONCURRENCY)
            .chunk(CHUNK)
            .await?;

        let mut buffer = vec![0u8; CHUNK];
        loop {
            let read = file.read(&mut buffer).await.map_err(io_err)?;
            if read == 0 {
                break;
            }

            if let Err(err) = progress.check() {
                // Tell the backend to drop the multipart upload rather than
                // leaving an orphan that still costs storage.
                let _ = writer.abort().await;
                return Err(err);
            }

            if let Err(err) = writer.write(buffer[..read].to_vec()).await {
                let _ = writer.abort().await;
                return Err(err.into());
            }
            progress.advance(read as u64);
        }

        writer.close().await?;
        Ok(())
    }

    /// Single-request upload, for backends that cannot write in parts.
    ///
    /// The body has to be in memory, because a one-shot writer takes a whole
    /// buffer — so this is bounded by [`ONE_SHOT_LIMIT`] and refuses beyond it
    /// with a message rather than trying and running the process out of memory.
    /// Progress necessarily jumps from 0 to done: there are no parts to count.
    async fn upload_one_shot(
        &self,
        local: &PathBuf,
        remote: &str,
        progress: &Arc<TaskProgress>,
    ) -> Result<()> {
        let size = tokio::fs::metadata(local).await.map_err(io_err)?.len();

        if size > ONE_SHOT_LIMIT {
            return Err(Error::Unsupported(format!(
                "{} 只支持一次性写入，{} 超过了 {} 的上限",
                self.inner.label,
                crate::fmt::size(Some(size)),
                crate::fmt::size(Some(ONE_SHOT_LIMIT)),
            )));
        }

        progress.check()?;
        let bytes = tokio::fs::read(local).await.map_err(io_err)?;
        progress.check()?;

        self.inner.op.write(remote, bytes).await?;
        progress.advance(size);
        Ok(())
    }

    /// Copy one object to another location, possibly on another backend.
    ///
    /// Within one backend that supports it this is a server-side copy: no bytes
    /// pass through this process, so it costs no egress and finishes in one
    /// request. Otherwise the object is streamed across.
    pub async fn copy_to(
        &self,
        from: &str,
        target: &Vfs,
        to: &str,
        progress: Arc<TaskProgress>,
    ) -> Result<()> {
        Self::debug_assert_on_tokio("copy_to");
        progress.check()?;

        if self.same_backend(target) && self.inner.capability.copy {
            self.inner.op.copy(from, to).await?;
            // Server-side copy is atomic, so progress goes straight to the end.
            if let Ok(meta) = self.inner.op.stat(to).await {
                progress.advance(meta.content_length());
            }
            return Ok(());
        }

        self.pump(from, target, to, progress).await
    }

    async fn pump(
        &self,
        from: &str,
        target: &Vfs,
        to: &str,
        progress: Arc<TaskProgress>,
    ) -> Result<()> {
        use futures::TryStreamExt;

        // Same one-shot constraint as `upload_from`, but for the target side.
        if !target.inner.capability.write_can_multi {
            return self.pump_one_shot(from, target, to, progress).await;
        }

        let reader = self.inner.op.reader(from).await?;
        let mut stream = reader.into_bytes_stream(..).await?;

        let mut writer = target
            .inner
            .op
            .writer_with(to)
            .concurrent(WRITER_CONCURRENCY)
            .chunk(CHUNK)
            .await?;

        // Streamed chunk by chunk on purpose: reading the object into memory
        // first would make a 10 GB copy need 10 GB of RAM.
        loop {
            let chunk = match stream.try_next().await {
                Ok(Some(chunk)) => chunk,
                Ok(None) => break,
                Err(err) => {
                    let _ = writer.abort().await;
                    return Err(stream_err(err));
                }
            };

            if let Err(err) = progress.check() {
                let _ = writer.abort().await;
                return Err(err);
            }

            let len = chunk.len() as u64;
            if let Err(err) = writer.write(chunk.to_vec()).await {
                let _ = writer.abort().await;
                return Err(err.into());
            }
            progress.advance(len);
        }

        writer.close().await?;
        Ok(())
    }

    /// Copy to a target that can only be written in one request.
    async fn pump_one_shot(
        &self,
        from: &str,
        target: &Vfs,
        to: &str,
        progress: Arc<TaskProgress>,
    ) -> Result<()> {
        // The source's size decides whether this is even attemptable, since the
        // body must be held in memory for a single write.
        let size = self.inner.op.stat(from).await?.content_length();
        if size > ONE_SHOT_LIMIT {
            return Err(Error::Unsupported(format!(
                "{} 只支持一次性写入，{} 超过了 {} 的上限",
                target.inner.label,
                crate::fmt::size(Some(size)),
                crate::fmt::size(Some(ONE_SHOT_LIMIT)),
            )));
        }

        progress.check()?;
        let bytes = self.inner.op.read(from).await?.to_vec();
        progress.check()?;

        let len = bytes.len() as u64;
        target.inner.op.write(to, bytes).await?;
        progress.advance(len);
        Ok(())
    }

    /// Every entry beneath `dir`, flattened.
    ///
    /// Used to plan a directory transfer, so the totals are known before any
    /// bytes move.
    /// Goes through `rt.spawn` like the other public methods: planning a
    /// directory transfer happens on GPUI's executor, and awaiting the lister
    /// there would panic with "there is no reactor running" (see `rt` rule 1).
    pub fn list_recursive(
        &self,
        dir: &str,
    ) -> impl Future<Output = Result<Vec<DirEntry>>> + Send + 'static {
        let op = self.inner.op.clone();
        let dir = path::as_dir(dir);

        self.inner.rt.spawn(async move {
            let mut lister = op.lister_with(&dir).recursive(true).await?;
            let mut out = Vec::new();

            while let Some(entry) = lister.next().await {
                let entry = entry?;
                if path::as_dir(entry.path()) == dir || path::basename(entry.path()).is_empty() {
                    continue;
                }
                out.push(DirEntry::from_entry(&entry));
            }

            Ok(out)
        })
    }

    /// Read at most `limit` bytes from the start of an object.
    ///
    /// Used by the preview panel, so a preview never pulls a whole
    /// multi-gigabyte object over the network.
    ///
    /// Streamed and stopped early rather than requested as `range(0..limit)`:
    /// **a range whose end exceeds the object length fails with
    /// `RangeNotSatisfied`**, so the explicit-range version broke for every file
    /// smaller than the limit — which is nearly every text file. Dropping the
    /// stream cancels the rest of the request, so the bound is still real.
    pub fn read_prefix(
        &self,
        path: &str,
        limit: u64,
    ) -> impl Future<Output = Result<Vec<u8>>> + Send + 'static {
        use futures::TryStreamExt;

        let op = self.inner.op.clone();
        let path = path.to_string();

        self.inner.rt.spawn(async move {
            let reader = op.reader(&path).await?;
            let mut stream = reader.into_bytes_stream(..).await?;

            let mut out = Vec::new();
            while let Some(chunk) = stream.try_next().await.map_err(stream_err)? {
                out.extend_from_slice(&chunk);
                if out.len() as u64 >= limit {
                    out.truncate(limit as usize);
                    break;
                }
            }

            Ok(out)
        })
    }

    // ── versions ──────────────────────────────────────────────────────────

    /// Every stored version of an object, newest first.
    ///
    /// Refused on backends without versioning rather than returning a
    /// single-entry list: showing one "version" would imply history exists.
    pub fn list_versions(
        &self,
        path: &str,
    ) -> impl Future<Output = Result<Vec<ObjectVersion>>> + Send + 'static {
        let op = self.inner.op.clone();
        let path = path.to_string();
        let check = self.require(self.inner.capability.list_with_versions, "版本历史");

        self.inner.rt.spawn(async move {
            check?;

            // `deleted(true)` includes delete markers, which are part of the
            // history: without them a deleted-then-restored object looks like it
            // was never gone.
            let mut lister = op.lister_with(&path).versions(true).deleted(true).await?;

            let mut out = Vec::new();
            while let Some(entry) = lister.next().await {
                let entry = entry?;
                // A versioned listing of a file path can still echo the prefix
                // itself; only entries at this exact path are its versions.
                if entry.path() != path {
                    continue;
                }
                out.push(ObjectVersion::from_metadata(entry.metadata()));
            }

            // Newest first, with the current version pinned to the top — that is
            // the one a plain read returns, so it belongs where the eye lands.
            out.sort_by(|a, b| {
                b.is_current
                    .cmp(&a.is_current)
                    .then(b.modified.cmp(&a.modified))
            });

            Ok(out)
        })
    }

    /// Read a specific version, bounded like [`Vfs::read_prefix`].
    pub fn read_version(
        &self,
        path: &str,
        version: &str,
        limit: u64,
    ) -> impl Future<Output = Result<Vec<u8>>> + Send + 'static {
        let op = self.inner.op.clone();
        let path = path.to_string();
        let version = version.to_string();
        let check = self.require(self.inner.capability.read_with_version, "按版本读取");

        self.inner.rt.spawn(async move {
            check?;
            let buffer = op.read_with(&path).version(&version).await?;
            let mut bytes = buffer.to_vec();
            bytes.truncate(limit as usize);
            Ok(bytes)
        })
    }

    /// Copy an old version back over the current one.
    ///
    /// Versioned stores have no "revert" operation, so restoring is a write of
    /// the old bytes — which itself becomes a new version. That is the honest
    /// model: history is append-only.
    pub fn restore_version(
        &self,
        path: &str,
        version: &str,
    ) -> impl Future<Output = Result<()>> + Send + 'static {
        let op = self.inner.op.clone();
        let path = path.to_string();
        let version = version.to_string();
        let check = self
            .require(self.inner.capability.read_with_version, "按版本读取")
            .and_then(|()| self.require(self.inner.capability.write, "写入"));

        self.inner.rt.spawn(async move {
            check?;
            let bytes = op.read_with(&path).version(&version).await?;
            op.write(&path, bytes).await?;
            Ok(())
        })
    }

    /// A time-limited public URL, or `None` if this backend cannot presign.
    pub fn presign_read(
        &self,
        path: &str,
        expire: Duration,
    ) -> impl Future<Output = Result<Option<String>>> + Send + 'static {
        let op = self.inner.op.clone();
        let can = self.inner.capability.presign && self.inner.capability.presign_read;
        let path = path.to_string();

        self.inner.rt.spawn(async move {
            if !can {
                return Ok(None);
            }
            let signed = op.presign_read(&path, expire).await?;
            Ok(Some(signed.uri().to_string()))
        })
    }

    pub fn from_operator(rt: Rt, op: Operator, label: impl AsRef<str>) -> Self {
        let capability = op.info().capability();
        Self {
            inner: Arc::new(VfsInner {
                op,
                rt,
                label: label.as_ref().into(),
                capability,
            }),
        }
    }

    pub fn label(&self) -> &str {
        &self.inner.label
    }

    /// What this backend can actually do. Menu items are enabled from this
    /// rather than assumed — S3 has no native rename, local `fs` has no
    /// presign, and offering either produces a guaranteed error.
    pub fn capability(&self) -> &Capability {
        &self.inner.capability
    }

    /// Start listing `path`, streaming batches back over a channel.
    ///
    /// Dropping the returned [`Listing`] aborts the backing task, which drops
    /// the `Lister` and cancels the in-flight request. That is how navigating
    /// away from a large directory stops paying for it.
    pub fn list(&self, path: &str) -> Listing {
        let op = self.inner.op.clone();
        let dir = path::as_dir(path);
        let (tx, rx) = mpsc::channel(CHANNEL_DEPTH);

        let task = self.inner.rt.handle().spawn(async move {
            let mut lister = match op.lister_with(&dir).await {
                Ok(lister) => lister,
                Err(e) => {
                    let _ = tx.send(Err(e.into())).await;
                    return;
                }
            };

            let mut batch: Vec<DirEntry> = Vec::with_capacity(BATCH_SIZE);
            let mut last_flush = Instant::now();

            while let Some(item) = lister.next().await {
                match item {
                    Ok(entry) => {
                        // `list` yields the listed directory itself. Compare in
                        // normalized form: at the root the self-entry comes
                        // back as "/" while `dir` is "", so a string equality
                        // check on the raw paths misses it.
                        if path::as_dir(entry.path()) == dir
                            || path::basename(entry.path()).is_empty()
                        {
                            continue;
                        }
                        batch.push(DirEntry::from_entry(&entry));
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e.into())).await;
                        return;
                    }
                }

                let due = batch.len() >= BATCH_SIZE || last_flush.elapsed() >= BATCH_INTERVAL;
                if due {
                    // A send error means the receiver was dropped, i.e. the user
                    // navigated away. Stop rather than keep listing.
                    if tx.send(Ok(std::mem::take(&mut batch))).await.is_err() {
                        return;
                    }
                    batch = Vec::with_capacity(BATCH_SIZE);
                    last_flush = Instant::now();
                }
            }

            if !batch.is_empty() {
                let _ = tx.send(Ok(batch)).await;
            }
        });

        Listing { rx, task }
    }

    /// Collect a whole directory. Convenience for tests and small directories;
    /// the UI uses [`Vfs::list`] so it can paint as batches arrive.
    pub async fn list_all(&self, path: &str) -> Result<Vec<DirEntry>> {
        let mut listing = self.list(path);
        let mut out = Vec::new();
        while let Some(batch) = listing.next_batch().await {
            out.extend(batch?);
        }
        Ok(out)
    }

    pub fn stat(
        &self,
        path: &str,
    ) -> impl Future<Output = Result<opendal::Metadata>> + Send + 'static {
        let op = self.inner.op.clone();
        let path = path.to_string();
        self.inner
            .rt
            .spawn(async move { Ok(op.stat(&path).await?) })
    }

    /// Fill in size/mtime for entries whose listing did not carry them.
    ///
    /// Call this for the visible rows only. Entries that fail to `stat` (a file
    /// deleted since the listing, say) are dropped from the result rather than
    /// failing the batch, so one vanished file cannot blank out a viewport.
    pub fn fill_metadata(
        &self,
        entries: Vec<DirEntry>,
    ) -> impl Future<Output = Result<Vec<DirEntry>>> + Send + 'static {
        let op = self.inner.op.clone();

        self.inner.rt.spawn(async move {
            let filled = futures::stream::iter(entries.into_iter().map(|entry| {
                let op = op.clone();
                async move {
                    match op.stat(&entry.path).await {
                        Ok(meta) => Some(entry.with_metadata(&meta)),
                        Err(_) => None,
                    }
                }
            }))
            .buffer_unordered(STAT_CONCURRENCY)
            .filter_map(|refreshed| async move { refreshed })
            .collect::<Vec<_>>()
            .await;

            Ok(filled)
        })
    }
}

/// A streaming directory listing. Drop it to cancel.
pub struct Listing {
    rx: mpsc::Receiver<Result<Vec<DirEntry>>>,
    task: JoinHandle<()>,
}

impl Listing {
    /// The backing task's abort handle, so a test can observe that dropping the
    /// listing really cancelled the scan rather than merely stopping the reads.
    #[doc(hidden)]
    pub fn abort_handle_for_test(&self) -> tokio::task::AbortHandle {
        self.task.abort_handle()
    }

    /// The next batch, or `None` once the directory is fully listed.
    ///
    /// Awaitable from GPUI's executor: a tokio mpsc channel needs no reactor.
    pub async fn next_batch(&mut self) -> Option<Result<Vec<DirEntry>>> {
        self.rx.recv().await
    }
}

impl Drop for Listing {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EntryKind;

    fn fixture() -> (tempfile::TempDir, Vfs) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("a.txt"), b"hello").unwrap();
        std::fs::write(dir.path().join("b.txt"), b"").unwrap();
        std::fs::write(dir.path().join("sub/c.txt"), b"nested").unwrap();

        let vfs = Vfs::local(Rt::from_current().unwrap(), dir.path().to_str().unwrap()).unwrap();

        (dir, vfs)
    }

    #[tokio::test]
    async fn lists_the_root_without_including_itself() {
        let (_guard, vfs) = fixture();

        let mut names: Vec<String> = vfs
            .list_all("")
            .await
            .unwrap()
            .iter()
            .map(|e| e.name.to_string())
            .collect();
        names.sort();

        assert_eq!(names, vec!["a.txt", "b.txt", "sub"]);
    }

    #[tokio::test]
    async fn directories_are_classified_and_keep_their_trailing_slash() {
        let (_guard, vfs) = fixture();

        let entries = vfs.list_all("").await.unwrap();
        let sub = entries.iter().find(|e| &*e.name == "sub").unwrap();

        assert_eq!(sub.kind, EntryKind::Dir);
        assert!(sub.path.ends_with('/'), "got {:?}", sub.path);
        assert!(sub.is_dir());
    }

    #[tokio::test]
    async fn lists_a_subdirectory() {
        let (_guard, vfs) = fixture();

        let entries = vfs.list_all("sub/").await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(&*entries[0].name, "c.txt");
        assert_eq!(&*entries[0].path, "sub/c.txt");
    }

    #[tokio::test]
    async fn listing_a_missing_directory_yields_empty_not_an_error() {
        let (_guard, vfs) = fixture();

        // Object-store semantics, which the fs service follows too: a prefix
        // with no children is simply empty. `list` therefore cannot be used to
        // decide whether a path exists, and the UI must not treat an empty pane
        // as proof that navigation succeeded.
        let entries = vfs.list_all("nope/").await.unwrap();
        assert!(entries.is_empty());
    }

    #[tokio::test]
    async fn stat_is_what_actually_detects_a_missing_path() {
        let (_guard, vfs) = fixture();

        let err = vfs.stat("nope/").await.unwrap_err();
        assert_eq!(err.kind(), Some(opendal::ErrorKind::NotFound));
        assert_eq!(err.user_message(), "路径已不存在");
    }

    #[tokio::test]
    async fn the_fs_backend_lists_with_full_metadata() {
        let (_guard, vfs) = fixture();

        // Worth pinning down per backend: the local fs lister stats each entry,
        // so sizes and mtimes are present on the first paint and the viewport
        // never needs to fill anything in. Object stores are not all this
        // generous, which is why `meta_complete` exists at all.
        let entries = vfs.list_all("").await.unwrap();
        let a = entries.iter().find(|e| &*e.name == "a.txt").unwrap();

        assert!(a.meta_complete);
        assert_eq!(a.size, Some(5), "a.txt holds 'hello'");
        assert!(a.modified.is_some());
    }

    #[tokio::test]
    async fn fill_metadata_completes_incomplete_entries() {
        let (_guard, vfs) = fixture();

        let entries = vfs.list_all("").await.unwrap();
        let filled = vfs.fill_metadata(entries).await.unwrap();

        let a = filled.iter().find(|e| &*e.name == "a.txt").unwrap();
        assert!(a.meta_complete);
        assert_eq!(a.size, Some(5));

        let b = filled.iter().find(|e| &*e.name == "b.txt").unwrap();
        assert_eq!(b.size, Some(0), "an empty file is 0, not unknown");
    }

    #[tokio::test]
    async fn fill_metadata_skips_entries_that_vanished() {
        let (guard, vfs) = fixture();

        let entries = vfs.list_all("").await.unwrap();
        std::fs::remove_file(guard.path().join("a.txt")).unwrap();

        let filled = vfs.fill_metadata(entries).await.unwrap();

        assert!(filled.iter().all(|e| &*e.name != "a.txt"));
        assert!(filled.iter().any(|e| &*e.name == "b.txt"));
    }

    #[tokio::test]
    async fn dropping_a_listing_cancels_it() {
        let (_guard, vfs) = fixture();

        let listing = vfs.list("");
        let task = listing.task.abort_handle();
        drop(listing);

        // The abort is observable rather than merely assumed.
        tokio::task::yield_now().await;
        assert!(task.is_finished());
    }

    #[tokio::test]
    async fn from_profile_builds_a_working_local_session() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"hi").unwrap();

        let mut profile = crate::Profile::new("local", "本机", "fs:///");
        profile
            .options
            .insert("root".into(), dir.path().to_str().unwrap().to_string());

        let vfs = Vfs::from_profile(Rt::from_current().unwrap(), &profile).unwrap();

        let entries = vfs.list_all("").await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(&*entries[0].name, "a.txt");
        assert_eq!(vfs.label(), "本机", "the pane is labelled by profile name");
    }

    #[tokio::test]
    async fn a_session_builds_without_static_credentials() {
        // Nothing but a URI. This must succeed: S3 credentials can come from an
        // IAM role or the environment, and demanding them here broke a real
        // profile that had none.
        let mut profile = crate::Profile::new("prod", "Prod S3", "s3://bucket/prefix");
        // Region is not a credential — OpenDAL's builder needs it regardless —
        // so a realistic IAM-role profile still carries one.
        profile.options.insert("region".into(), "us-east-1".into());

        Vfs::from_profile(Rt::from_current().unwrap(), &profile).unwrap();
    }

    #[tokio::test]
    async fn a_structurally_incomplete_profile_stops_the_session() {
        // The other half of the same rule: what is missing here is not a
        // credential but part of the address, so it cannot be filled in by the
        // environment and has to fail before any request is made.
        let profile = crate::Profile::new("az", "Azure", "azblob://container/");

        let err = Vfs::from_profile(Rt::from_current().unwrap(), &profile).unwrap_err();

        assert!(
            err.user_message().contains("账户名"),
            "should name the missing field: {}",
            err.user_message()
        );
        assert_eq!(err.recovery(), Some(crate::Recovery::OpenSettings));
    }

    #[tokio::test]
    async fn capabilities_really_do_differ_between_backends() {
        let rt = Rt::from_current().unwrap();

        let mut s3 = crate::Profile::new("prod", "Prod S3", "s3://bucket/prefix");
        s3.options.insert("region".into(), "us-east-1".into());
        s3.options
            .insert("access_key_id".into(), "AKIAEXAMPLE".into());
        s3.options
            .insert("secret_access_key".into(), "not-real".into());

        // Building an Operator performs no network IO, so this is a pure check
        // of what the service advertises.
        let s3 = Vfs::from_profile(rt.clone(), &s3).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let local = Vfs::local(rt, dir.path().to_str().unwrap()).unwrap();

        // This asymmetry is the entire reason menus are capability-driven.
        assert!(s3.capability().presign, "S3 can presign");
        assert!(!local.capability().presign, "local fs cannot");
        assert!(local.capability().rename, "local fs renames natively");
        assert!(!s3.capability().rename, "S3 has no native rename");
    }

    #[tokio::test]
    async fn the_menu_follows_the_backend() {
        use crate::EntryAction;

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"hi").unwrap();
        let vfs = Vfs::local(Rt::from_current().unwrap(), dir.path().to_str().unwrap()).unwrap();

        let entries = vfs.list_all("").await.unwrap();
        let items = vfs.entry_menu(&entries[0]);

        let share = items
            .iter()
            .find(|i| i.action == EntryAction::CopyShareLink)
            .unwrap();
        assert!(!share.enabled, "local fs cannot presign");
        assert!(share.disabled_reason.is_some(), "and it says why");
    }

    #[tokio::test]
    async fn presign_on_a_backend_without_it_returns_none_rather_than_failing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"hi").unwrap();
        let vfs = Vfs::local(Rt::from_current().unwrap(), dir.path().to_str().unwrap()).unwrap();

        let signed = vfs
            .presign_read("a.txt", Duration::from_secs(600))
            .await
            .unwrap();
        assert_eq!(signed, None);
    }

    #[tokio::test]
    async fn create_dir_then_list_shows_it() {
        let (_guard, vfs) = fixture();

        vfs.create_dir("new-folder").await.unwrap();

        let entries = vfs.list_all("").await.unwrap();
        let created = entries.iter().find(|e| &*e.name == "new-folder").unwrap();
        assert_eq!(created.kind, EntryKind::Dir);
    }

    #[tokio::test]
    async fn create_dir_normalizes_to_directory_form() {
        let (guard, vfs) = fixture();

        // Passed without a trailing slash; OpenDAL requires one for create_dir.
        vfs.create_dir("deep/nested").await.unwrap();

        assert!(guard.path().join("deep/nested").is_dir());
    }

    #[tokio::test]
    async fn rename_moves_a_file_within_its_directory() {
        let (guard, vfs) = fixture();

        vfs.rename("a.txt", "renamed.txt").await.unwrap();

        assert!(!guard.path().join("a.txt").exists());
        assert_eq!(
            std::fs::read_to_string(guard.path().join("renamed.txt")).unwrap(),
            "hello"
        );
    }

    #[tokio::test]
    async fn renaming_a_directory_is_refused_before_any_request() {
        let (guard, vfs) = fixture();

        // OpenDAL rejects directory paths in `rename` on every backend; the
        // guard turns that into a clear refusal instead of an IsADirectory error
        // from inside the operator.
        let err = vfs.rename("sub/", "renamed/").await.unwrap_err();

        assert!(matches!(err, Error::Unsupported(_)));
        assert_eq!(
            err.user_message(),
            "重命名目录请使用移动任务（copy + delete）"
        );
        assert!(guard.path().join("sub").is_dir(), "nothing changed");
    }

    #[tokio::test]
    async fn copy_leaves_both_entries() {
        let (guard, vfs) = fixture();

        vfs.copy("a.txt", "a 副本.txt").await.unwrap();

        assert_eq!(
            std::fs::read_to_string(guard.path().join("a.txt")).unwrap(),
            "hello"
        );
        assert_eq!(
            std::fs::read_to_string(guard.path().join("a 副本.txt")).unwrap(),
            "hello"
        );
    }

    #[tokio::test]
    async fn delete_removes_a_file() {
        let (guard, vfs) = fixture();
        let entries = vfs.list_all("").await.unwrap();
        let file = entries.iter().find(|e| &*e.name == "a.txt").unwrap();

        vfs.delete(file).await.unwrap();

        assert!(!guard.path().join("a.txt").exists());
    }

    #[tokio::test]
    async fn deleting_a_directory_takes_its_contents_with_it() {
        let (guard, vfs) = fixture();
        let entries = vfs.list_all("").await.unwrap();
        let dir = entries.iter().find(|e| &*e.name == "sub").unwrap();

        assert!(guard.path().join("sub/c.txt").exists());
        vfs.delete(dir).await.unwrap();

        assert!(!guard.path().join("sub").exists(), "recursive delete");
    }

    #[tokio::test]
    async fn an_unsupported_mutation_is_refused_without_being_attempted() {
        // A capability-less operator: the guard must fire before any request.
        let dir = tempfile::tempdir().unwrap();
        let op = Operator::new(services::Fs::default().root(dir.path().to_str().unwrap()))
            .unwrap()
            .layer(opendal::layers::CapabilityOverrideLayer::new(|_| {
                Capability::default()
            }));
        let vfs = Vfs::from_operator(Rt::from_current().unwrap(), op, "受限后端");

        let err = vfs.create_dir("nope").await.unwrap_err();

        assert!(matches!(err, crate::Error::Unsupported(_)));
        assert_eq!(err.user_message(), "受限后端 不支持新建目录");
        assert!(
            !dir.path().join("nope").exists(),
            "nothing should have been attempted"
        );
    }

    #[tokio::test]
    async fn read_prefix_stops_at_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("long.txt"), "0123456789").unwrap();
        let vfs = Vfs::local(Rt::from_current().unwrap(), dir.path().to_str().unwrap()).unwrap();

        // A ranged request, so previewing a huge object costs a few KB.
        let head = vfs.read_prefix("long.txt", 4).await.unwrap();
        assert_eq!(head, b"0123");
    }

    #[tokio::test]
    async fn read_prefix_of_a_short_file_returns_all_of_it() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("short.txt"), "hi").unwrap();
        let vfs = Vfs::local(Rt::from_current().unwrap(), dir.path().to_str().unwrap()).unwrap();

        // Asking for more than exists must not error. An explicit
        // `range(0..limit)` fails here with RangeNotSatisfied, which would have
        // broken the preview for nearly every text file.
        let all = vfs.read_prefix("short.txt", 4096).await.unwrap();
        assert_eq!(all, b"hi");
    }

    #[tokio::test]
    async fn read_prefix_of_an_empty_file_is_empty_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("empty.txt"), "").unwrap();
        let vfs = Vfs::local(Rt::from_current().unwrap(), dir.path().to_str().unwrap()).unwrap();

        assert!(vfs.read_prefix("empty.txt", 4096).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_partial_download_never_takes_the_real_filename() {
        let (_guard, vfs) = fixture();
        let out = tempfile::tempdir().unwrap();
        let local = out.path().join("a.txt");

        // Cancelled before any byte is written.
        let progress = Arc::new(TaskProgress::new());
        progress.cancel();
        let err = vfs
            .download_to("a.txt", local.clone(), progress)
            .await
            .unwrap_err();

        assert!(err.is_cancelled());
        assert!(!local.exists(), "the real name must stay unused");
        // Cancelling is the user saying stop, so nothing is kept for a resume.
        assert!(!part_path(&local).exists());
    }

    #[tokio::test]
    async fn a_download_resumes_from_a_previous_attempt() {
        let dir = tempfile::tempdir().unwrap();
        let body: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(dir.path().join("big.bin"), &body).unwrap();
        let vfs = Vfs::local(Rt::from_current().unwrap(), dir.path().to_str().unwrap()).unwrap();

        let out = tempfile::tempdir().unwrap();
        let local = out.path().join("big.bin");
        let part = part_path(&local);

        // Stand in for an interrupted attempt: the first half on disk, plus the
        // etag it was fetched against.
        std::fs::write(&part, &body[..1000]).unwrap();
        let etag = vfs
            .stat("big.bin")
            .await
            .unwrap()
            .etag()
            .map(str::to_string);
        if let Some(etag) = &etag {
            std::fs::write(etag_path(&part), etag).unwrap();
        } else {
            eprintln!("skipping: this backend reports no etag, so resuming is unsafe");
            return;
        }

        let progress = Arc::new(TaskProgress::new());
        vfs.download_to("big.bin", local.clone(), progress.clone())
            .await
            .unwrap();

        assert_eq!(std::fs::read(&local).unwrap(), body, "bytes must join up");
        assert!(!part.exists(), "the part file is consumed");
        assert!(!etag_path(&part).exists(), "and so is its sidecar");
        // The proof that it resumed rather than restarted: progress counts the
        // bytes it already had plus only the remainder.
        assert_eq!(progress.done(), body.len() as u64);
    }

    #[tokio::test]
    async fn a_server_rejecting_the_resume_falls_back_to_a_full_refetch() {
        // Reproduces the WebDAV case without a server: the recorded etag looks
        // valid to us, but the backend answers the conditional request with 412.
        // Apache does this because it flips between strong and weak etags for a
        // recently modified file. A 412 must mean "refetch", never "fail".
        let dir = tempfile::tempdir().unwrap();
        let body = b"the authoritative content";
        std::fs::write(dir.path().join("doc.txt"), body).unwrap();

        let op = Operator::new(services::Fs::default().root(dir.path().to_str().unwrap()))
            .unwrap()
            .layer(opendal::layers::CapabilityOverrideLayer::new(|mut cap| {
                // Claim if_match support so the resume path sends the header; the
                // fs service then rejects it, which is the behaviour under test.
                cap.read_with_if_match = true;
                cap
            }));
        let vfs = Vfs::from_operator(Rt::from_current().unwrap(), op, "挑剔的后端");

        let out = tempfile::tempdir().unwrap();
        let local = out.path().join("doc.txt");
        let part = part_path(&local);
        std::fs::write(&part, &body[..5]).unwrap();
        let etag = vfs
            .stat("doc.txt")
            .await
            .unwrap()
            .etag()
            .map(str::to_string);
        let Some(etag) = etag else { return };
        std::fs::write(etag_path(&part), &etag).unwrap();

        vfs.download_to("doc.txt", local.clone(), Arc::new(TaskProgress::new()))
            .await
            .unwrap();

        assert_eq!(
            std::fs::read(&local).unwrap(),
            body,
            "whichever path was taken, the bytes must be exact"
        );
        assert!(!part.exists());
    }

    #[tokio::test]
    async fn a_stale_part_file_is_discarded_rather_than_spliced() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("doc.txt"), b"the real content").unwrap();
        let vfs = Vfs::local(Rt::from_current().unwrap(), dir.path().to_str().unwrap()).unwrap();

        let out = tempfile::tempdir().unwrap();
        let local = out.path().join("doc.txt");
        let part = part_path(&local);

        // A leftover from some *other* object, with a mismatched etag. Resuming
        // onto it would splice two different files together.
        std::fs::write(&part, b"WRONG").unwrap();
        std::fs::write(etag_path(&part), "not-the-right-etag").unwrap();

        vfs.download_to("doc.txt", local.clone(), Arc::new(TaskProgress::new()))
            .await
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(&local).unwrap(),
            "the real content",
            "the stale prefix must not survive"
        );
    }

    #[test]
    fn a_weak_etag_is_not_usable_for_resuming() {
        let mut meta = opendal::Metadata::new(opendal::EntryMode::FILE);

        meta.set_etag("\"strong-value\"");
        assert_eq!(strong_etag(&meta).as_deref(), Some("\"strong-value\""));

        // Apache serves these by default. A weak etag promises only semantic
        // equivalence, so it cannot certify that a partial file is a byte-prefix
        // of the object — and `If-Match` rejects it outright.
        meta.set_etag("W/\"800800-65916599e1459\"");
        assert_eq!(strong_etag(&meta), None);
    }

    #[tokio::test]
    async fn a_part_file_without_an_etag_sidecar_starts_over() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("doc.txt"), b"fresh").unwrap();
        let vfs = Vfs::local(Rt::from_current().unwrap(), dir.path().to_str().unwrap()).unwrap();

        let out = tempfile::tempdir().unwrap();
        let local = out.path().join("doc.txt");
        std::fs::write(part_path(&local), b"XX").unwrap();

        vfs.download_to("doc.txt", local.clone(), Arc::new(TaskProgress::new()))
            .await
            .unwrap();

        // Unverifiable provenance means it cannot be trusted as a prefix.
        assert_eq!(std::fs::read_to_string(&local).unwrap(), "fresh");
    }

    #[tokio::test]
    async fn an_already_complete_part_file_is_just_published() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("done.txt"), b"complete").unwrap();
        let vfs = Vfs::local(Rt::from_current().unwrap(), dir.path().to_str().unwrap()).unwrap();

        let out = tempfile::tempdir().unwrap();
        let local = out.path().join("done.txt");
        let part = part_path(&local);
        std::fs::write(&part, b"complete").unwrap();
        let etag = vfs
            .stat("done.txt")
            .await
            .unwrap()
            .etag()
            .map(str::to_string);
        let Some(etag) = etag else { return };
        std::fs::write(etag_path(&part), &etag).unwrap();

        // An interrupted-at-the-last-moment attempt: everything is there, only
        // the rename was missing. Requesting `range(len..)` would fail with
        // RangeNotSatisfied, so this case is handled before the request.
        vfs.download_to("done.txt", local.clone(), Arc::new(TaskProgress::new()))
            .await
            .unwrap();

        assert_eq!(std::fs::read_to_string(&local).unwrap(), "complete");
        assert!(!part.exists());
    }

    #[tokio::test]
    async fn a_failed_download_keeps_its_part_file_for_a_retry() {
        let dir = tempfile::tempdir().unwrap();
        let vfs = Vfs::local(Rt::from_current().unwrap(), dir.path().to_str().unwrap()).unwrap();

        let out = tempfile::tempdir().unwrap();
        let local = out.path().join("missing.txt");
        let part = part_path(&local);
        std::fs::write(&part, b"partial").unwrap();

        // The object does not exist, so this fails — but a failure is exactly the
        // case where the part file has to survive.
        assert!(
            vfs.download_to("missing.txt", local.clone(), Arc::new(TaskProgress::new()))
                .await
                .is_err()
        );
        assert!(part.exists(), "a retry needs it");
        assert!(!local.exists());
    }

    #[tokio::test]
    async fn a_one_shot_backend_can_still_take_a_multi_chunk_file() {
        // Regression test for a real failure found against WebDAV: any backend
        // reporting `write_can_multi == false` rejected every upload over one
        // chunk (8 MB) with "OneShotWriter doesn't support multiple write".
        // Reproduced here without a server by overriding the capability.
        let dir = tempfile::tempdir().unwrap();
        let op = Operator::new(services::Fs::default().root(dir.path().to_str().unwrap()))
            .unwrap()
            .layer(opendal::layers::CapabilityOverrideLayer::new(|mut cap| {
                cap.write_can_multi = false;
                cap
            }));
        let vfs = Vfs::from_operator(Rt::from_current().unwrap(), op, "一次性写入后端");

        let size = CHUNK + 2048;
        let payload: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        let source = tempfile::tempdir().unwrap();
        let file = source.path().join("big.bin");
        std::fs::write(&file, &payload).unwrap();

        let progress = Arc::new(TaskProgress::new());
        vfs.upload_from(file, "big.bin", progress.clone())
            .await
            .unwrap();

        assert_eq!(std::fs::read(dir.path().join("big.bin")).unwrap(), payload);
        assert_eq!(progress.done(), size as u64);
    }

    #[tokio::test]
    async fn a_one_shot_backend_refuses_an_upload_past_the_memory_guard() {
        let dir = tempfile::tempdir().unwrap();
        let op = Operator::new(services::Fs::default().root(dir.path().to_str().unwrap()))
            .unwrap()
            .layer(opendal::layers::CapabilityOverrideLayer::new(|mut cap| {
                cap.write_can_multi = false;
                cap
            }));
        let vfs = Vfs::from_operator(Rt::from_current().unwrap(), op, "一次性写入后端");

        // Pretend the file is enormous by lowering nothing and raising the file:
        // instead of writing 512 MB, check the guard's message directly on a
        // sparse file of that size.
        let source = tempfile::tempdir().unwrap();
        let file = source.path().join("huge.bin");
        let handle = std::fs::File::create(&file).unwrap();
        handle.set_len(ONE_SHOT_LIMIT + 1).unwrap();
        drop(handle);

        let err = vfs
            .upload_from(file, "huge.bin", Arc::new(TaskProgress::new()))
            .await
            .unwrap_err();

        // A message beats an out-of-memory kill.
        assert!(matches!(err, Error::Unsupported(_)));
        assert!(
            err.user_message().contains("一次性写入"),
            "got {}",
            err.user_message()
        );
    }

    #[tokio::test]
    async fn local_fs_reports_no_presign_support() {
        let (_guard, vfs) = fixture();
        // The reason menus are capability-driven: this backend cannot presign,
        // so a "copy share link" item must never be offered for it.
        assert!(!vfs.capability().presign);
        assert!(vfs.capability().rename, "local fs can rename");
    }
}
