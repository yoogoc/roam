//! Integration tests against a real S3 server over HTTP.
//!
//! Everything else in this workspace is tested against the local `fs` service,
//! which means the whole network path — request signing, XML parsing, listing
//! pagination, multipart upload, presigning — had never actually run. These
//! tests exercise it.
//!
//! They are skipped unless `ROAM_S3_ENDPOINT` is set, so `cargo test` stays
//! green on a machine with no server. To run them:
//!
//! ```text
//! docker run -d --name roam-minio -p 19000:9000 \
//!   -e MINIO_ROOT_USER=roamtest -e MINIO_ROOT_PASSWORD=roamtest-secret \
//!   -v /some/dir:/data minio/minio server /data
//! mkdir /some/dir/roam-test          # MinIO turns a directory into a bucket
//!
//! ROAM_S3_ENDPOINT=http://127.0.0.1:19000 \
//! ROAM_S3_BUCKET=roam-test \
//! ROAM_S3_KEY=roamtest \
//! ROAM_S3_SECRET=roamtest-secret \
//! cargo test -p roam-core --test s3 -- --test-threads=1
//! ```

use std::sync::Arc;

use roam_core::transfer::{
    CHUNK, DEFAULT_CONCURRENCY, TaskProgress, TaskState, Transfer, TransferEngine,
};
use roam_core::{EntryKind, Profile, Rt, Vfs, service};

struct Server {
    endpoint: String,
    bucket: String,
    key: String,
    secret: String,
}

fn server() -> Option<Server> {
    Some(Server {
        endpoint: std::env::var("ROAM_S3_ENDPOINT").ok()?,
        bucket: std::env::var("ROAM_S3_BUCKET").unwrap_or_else(|_| "roam-test".into()),
        key: std::env::var("ROAM_S3_KEY").unwrap_or_else(|_| "roamtest".into()),
        secret: std::env::var("ROAM_S3_SECRET").unwrap_or_else(|_| "roamtest-secret".into()),
    })
}

/// A session rooted at a unique prefix, so tests cannot see each other's
/// objects even when run in parallel.
fn session(prefix: &str) -> Option<Vfs> {
    let server = server()?;

    let mut profile = Profile::new(
        "s3-test",
        "MinIO",
        format!("s3://{}/{prefix}", server.bucket),
    );
    profile
        .options
        .insert("endpoint".into(), server.endpoint.clone());
    profile.options.insert("region".into(), "us-east-1".into());
    // MinIO serves path-style addressing, which is also what most
    // S3-compatible servers do.
    profile
        .options
        .insert("enable_virtual_host_style".into(), "false".into());
    profile
        .options
        .insert("access_key_id".into(), server.key.clone());
    profile
        .options
        .insert("secret_access_key".into(), server.secret.clone());

    Some(Vfs::from_profile(Rt::from_current().unwrap(), &profile).unwrap())
}

/// A prefix unique to this process run.
///
/// Version history is append-only and survives deletion, so a versioned bucket
/// accumulates state across runs — a fixed prefix would make any test that counts
/// versions fail the second time it is run.
fn unique_prefix(base: &str) -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static SEQ: AtomicU32 = AtomicU32::new(0);
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{base}{stamp}-{seq}/")
}

/// Skip with a visible note rather than silently passing.
macro_rules! s3 {
    ($prefix:expr) => {
        match session($prefix) {
            Some(vfs) => vfs,
            None => {
                eprintln!("skipping: ROAM_S3_ENDPOINT is not set");
                return;
            }
        }
    };
}

#[tokio::test]
async fn writes_and_reads_back_over_http() {
    let vfs = s3!("basic/");

    let progress = Arc::new(TaskProgress::default());
    let source = tempfile::tempdir().unwrap();
    let file = source.path().join("hello.txt");
    std::fs::write(&file, b"hello from minio").unwrap();

    vfs.upload_from(file, "hello.txt", progress.clone())
        .await
        .unwrap();

    let bytes = vfs.read_prefix("hello.txt", 4096).await.unwrap();
    assert_eq!(bytes, b"hello from minio");
    assert_eq!(progress.done(), 16);
}

#[tokio::test]
async fn s3_advertises_presign_but_not_rename() {
    let vfs = s3!("caps/");

    // The asymmetry the capability-driven menus are built on, now confirmed
    // against a real server rather than a locally constructed Operator.
    assert!(vfs.capability().presign, "S3 can presign");
    assert!(vfs.capability().presign_read);
    assert!(!vfs.capability().rename, "S3 has no native rename");
    assert!(vfs.capability().copy, "but it does have server-side copy");
}

#[tokio::test]
async fn listing_reports_size_and_mtime() {
    let vfs = s3!("meta/");
    let progress = Arc::new(TaskProgress::default());

    let source = tempfile::tempdir().unwrap();
    let file = source.path().join("sized.txt");
    std::fs::write(&file, b"0123456789").unwrap();
    vfs.upload_from(file, "sized.txt", progress).await.unwrap();

    let entries = vfs.list_all("").await.unwrap();
    let entry = entries.iter().find(|e| &*e.name == "sized.txt").unwrap();

    // The design assumed ListObjectsV2 carries both; this is the check that it
    // actually does, which is what keeps `meta_complete` true and the viewport
    // from having to stat every row.
    assert_eq!(entry.size, Some(10));
    assert!(entry.modified.is_some());
    assert!(entry.meta_complete);
}

#[tokio::test]
async fn a_prefix_behaves_like_a_directory() {
    let vfs = s3!("tree/");
    let progress = Arc::new(TaskProgress::default());
    let source = tempfile::tempdir().unwrap();

    for path in ["top.txt", "nested/inner.txt", "nested/deeper/leaf.txt"] {
        let file = source.path().join("payload");
        std::fs::write(&file, path.as_bytes()).unwrap();
        vfs.upload_from(file, path, progress.clone()).await.unwrap();
    }

    let root = vfs.list_all("").await.unwrap();
    let mut names: Vec<String> = root.iter().map(|e| e.name.to_string()).collect();
    names.sort();
    assert_eq!(names, vec!["nested", "top.txt"]);

    // A prefix with no object of its own is still reported as a directory.
    let nested = root.iter().find(|e| &*e.name == "nested").unwrap();
    assert_eq!(nested.kind, EntryKind::Dir);
    assert!(nested.path.ends_with('/'));

    let flattened = vfs.list_recursive("").await.unwrap();
    let files: Vec<String> = flattened
        .iter()
        .filter(|e| !e.is_dir())
        .map(|e| e.path.to_string())
        .collect();
    assert_eq!(files.len(), 3, "got {files:?}");
}

#[tokio::test]
async fn a_multipart_upload_round_trips_exactly() {
    let vfs = s3!("multipart/");
    let progress = Arc::new(TaskProgress::default());

    // Larger than one chunk, so this goes through the concurrent multipart
    // writer — the path that never ran against a real server before.
    let size = CHUNK + 4096;
    let payload: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();

    let source = tempfile::tempdir().unwrap();
    let file = source.path().join("big.bin");
    std::fs::write(&file, &payload).unwrap();

    vfs.upload_from(file, "big.bin", progress.clone())
        .await
        .unwrap();
    assert_eq!(progress.done(), size as u64);

    let out = tempfile::tempdir().unwrap();
    let local = out.path().join("big.bin");
    vfs.download_to("big.bin", local.clone(), Arc::new(TaskProgress::default()))
        .await
        .unwrap();

    assert_eq!(std::fs::read(&local).unwrap(), payload);
}

#[tokio::test]
async fn listing_pages_through_more_than_one_response() {
    let vfs = s3!("paging/");
    let engine = TransferEngine::new(Rt::from_current().unwrap(), 16);

    // MinIO caps a listing response at 1000 keys, so this needs two round trips
    // and exercises the continuation token.
    let count = 1005;
    let source = tempfile::tempdir().unwrap();
    let file = source.path().join("tiny");
    std::fs::write(&file, b"x").unwrap();

    let uploads: Vec<Transfer> = (0..count)
        .map(|i| Transfer::Upload {
            vfs: vfs.clone(),
            local: file.clone(),
            remote: Arc::from(format!("obj-{i:05}.txt").as_str()),
        })
        .collect();
    engine.enqueue_all(uploads);

    for _ in 0..1200 {
        if !engine.is_active() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(!engine.is_active(), "uploads did not finish");
    assert!(
        engine.snapshot().iter().all(|t| t.state == TaskState::Done),
        "some uploads failed"
    );

    let entries = vfs.list_all("").await.unwrap();
    assert_eq!(entries.len(), count, "every page must be followed");
}

#[tokio::test]
async fn server_side_copy_does_not_move_bytes_through_us() {
    let vfs = s3!("copy/");
    let progress = Arc::new(TaskProgress::default());
    let source = tempfile::tempdir().unwrap();
    let file = source.path().join("original.txt");
    std::fs::write(&file, b"copy me").unwrap();
    vfs.upload_from(file, "original.txt", progress)
        .await
        .unwrap();

    vfs.copy("original.txt", "duplicate.txt").await.unwrap();

    assert_eq!(
        vfs.read_prefix("duplicate.txt", 64).await.unwrap(),
        b"copy me"
    );
    assert_eq!(
        vfs.read_prefix("original.txt", 64).await.unwrap(),
        b"copy me",
        "the original survives"
    );
}

/// On a **versioned** bucket this asserts something subtler than "the folder is
/// gone": every object under the prefix gets a delete marker, so no live content
/// remains — but `ListObjectsV2` still returns the prefix itself as a common
/// prefix, so the directory row survives. Verified against MinIO by listing the
/// raw XML. The UI therefore explains the outcome instead of pretending the row
/// vanished, which would mean lying about the backend's state.
#[tokio::test]
async fn recursive_delete_clears_a_whole_prefix() {
    let vfs = s3!("rmrf/");
    let progress = Arc::new(TaskProgress::default());
    let source = tempfile::tempdir().unwrap();
    let file = source.path().join("payload");
    std::fs::write(&file, b"x").unwrap();

    for path in [
        "doomed/a.txt",
        "doomed/b.txt",
        "doomed/deep/c.txt",
        "keep.txt",
    ] {
        vfs.upload_from(file.clone(), path, progress.clone())
            .await
            .unwrap();
    }

    let entries = vfs.list_all("").await.unwrap();
    let doomed = entries.iter().find(|e| &*e.name == "doomed").unwrap();
    vfs.delete(doomed).await.unwrap();

    // No live content is left underneath, which is what "deleted" means.
    let remaining = vfs.list_recursive("doomed/").await.unwrap();
    assert!(
        remaining.iter().all(|e| e.is_dir()),
        "every file under the prefix should be gone, got {:?}",
        remaining
            .iter()
            .map(|e| e.path.to_string())
            .collect::<Vec<_>>()
    );

    // The sibling is untouched either way.
    let names: Vec<String> = vfs
        .list_all("")
        .await
        .unwrap()
        .iter()
        .map(|e| e.name.to_string())
        .collect();
    assert!(names.contains(&"keep.txt".to_string()));

    if vfs.capability().list_with_versions {
        // Versioned: the prefix can persist as an empty directory. Asserted
        // rather than worked around, because hiding it client-side would show
        // something the backend does not report.
        let versions = vfs.list_versions("doomed/a.txt").await.unwrap();
        assert!(
            versions.iter().any(|v| v.is_delete_marker),
            "the delete is recorded as a marker, not an erasure"
        );
    } else {
        assert_eq!(names, vec!["keep.txt"], "unversioned: the prefix is gone");
    }
}

#[tokio::test]
async fn a_presigned_url_is_fetchable_without_credentials() {
    let vfs = s3!("presign/");
    let progress = Arc::new(TaskProgress::default());
    let source = tempfile::tempdir().unwrap();
    let file = source.path().join("shared.txt");
    std::fs::write(&file, b"anyone can read this").unwrap();
    vfs.upload_from(file, "shared.txt", progress).await.unwrap();

    let url = vfs
        .presign_read("shared.txt", std::time::Duration::from_secs(300))
        .await
        .unwrap()
        .expect("S3 can presign");

    // The point of a share link is that it works with no credentials attached,
    // so fetch it with a bare client.
    let body = reqwest::get(&url).await.unwrap();
    assert!(body.status().is_success(), "got {}", body.status());
    assert_eq!(body.text().await.unwrap(), "anyone can read this");
}

#[tokio::test]
async fn renaming_is_refused_before_a_request_is_made() {
    let vfs = s3!("rename/");

    // `capability.rename` is false for S3, so the guard fires locally.
    let err = vfs.rename("a.txt", "b.txt").await.unwrap_err();
    assert_eq!(err.user_message(), "MinIO 不支持重命名");
}

#[tokio::test]
async fn a_missing_object_reports_not_found() {
    let vfs = s3!("missing/");

    let err = vfs.stat("nope.txt").await.unwrap_err();
    assert_eq!(err.kind(), Some(opendal::ErrorKind::NotFound));
    assert_eq!(err.user_message(), "路径已不存在");

    // And listing an absent prefix is empty rather than an error, matching the
    // local fs behaviour the UI relies on.
    assert!(vfs.list_all("absent/").await.unwrap().is_empty());
}

#[tokio::test]
async fn bad_credentials_surface_as_permission_denied() {
    let Some(server) = server() else {
        eprintln!("skipping: ROAM_S3_ENDPOINT is not set");
        return;
    };

    let mut profile = Profile::new("bad", "错误凭据", format!("s3://{}/", server.bucket));
    profile.options.insert("endpoint".into(), server.endpoint);
    profile.options.insert("region".into(), "us-east-1".into());
    profile
        .options
        .insert("enable_virtual_host_style".into(), "false".into());
    profile
        .options
        .insert("access_key_id".into(), "wrong".into());
    profile
        .options
        .insert("secret_access_key".into(), "alsowrong".into());

    let vfs = Vfs::from_profile(Rt::from_current().unwrap(), &profile).unwrap();
    let err = vfs.list_all("").await.unwrap_err();

    // The message the connection dialog shows has to be the useful one.
    assert_eq!(
        err.kind(),
        Some(opendal::ErrorKind::PermissionDenied),
        "got {err:?}"
    );
    assert_eq!(err.user_message(), "没有访问权限");
    assert_eq!(err.recovery(), Some(roam_core::Recovery::EditCredentials));
}

#[tokio::test]
async fn cross_backend_copy_streams_from_s3_to_local_disk() {
    let vfs = s3!("cross/");
    let progress = Arc::new(TaskProgress::default());
    let source = tempfile::tempdir().unwrap();
    let file = source.path().join("payload.txt");
    std::fs::write(&file, b"crossing over").unwrap();
    vfs.upload_from(file, "payload.txt", progress)
        .await
        .unwrap();

    // A genuinely different backend, so this has to stream rather than take the
    // server-side copy shortcut.
    let target_dir = tempfile::tempdir().unwrap();
    let local = Vfs::local(
        Rt::from_current().unwrap(),
        target_dir.path().to_str().unwrap(),
    )
    .unwrap();
    assert!(!vfs.same_backend(&local));

    let engine = TransferEngine::new(Rt::from_current().unwrap(), DEFAULT_CONCURRENCY);
    engine.enqueue(Transfer::Copy {
        from: vfs,
        from_path: "payload.txt".into(),
        to: local,
        to_path: "arrived.txt".into(),
        size: None,
    });

    for _ in 0..200 {
        if !engine.is_active() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }

    assert_eq!(engine.snapshot()[0].state, TaskState::Done);
    assert_eq!(
        std::fs::read_to_string(target_dir.path().join("arrived.txt")).unwrap(),
        "crossing over"
    );
}

#[tokio::test]
async fn records_what_s3_advertises() {
    let vfs = s3!("caps2/");
    let cap = vfs.capability();
    eprintln!(
        "s3: write_can_multi={} copy={} rename={} presign={} create_dir={} delete_recursive={}",
        cap.write_can_multi,
        cap.copy,
        cap.rename,
        cap.presign,
        cap.create_dir,
        cap.delete_with_recursive
    );
    assert!(cap.write_can_multi, "S3 supports multipart");
}

// ── versions ──────────────────────────────────────────────────────────────
//
// These need bucket versioning enabled. `scripts/test-backends.sh` turns it on
// for the test bucket; without it a versioned listing returns a single entry and
// the assertions below would be meaningless, so they check for history first.

async fn write_object(vfs: &Vfs, path: &str, body: &[u8]) {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("payload");
    std::fs::write(&file, body).unwrap();
    vfs.upload_from(file, path, Arc::new(TaskProgress::default()))
        .await
        .unwrap();
}

#[tokio::test]
async fn s3_reports_versioning_support() {
    let vfs = s3!("versions-cap/");
    assert!(
        vfs.capability().list_with_versions,
        "the versions UI is gated on this"
    );
    assert!(vfs.capability().read_with_version);
}

#[tokio::test]
async fn every_write_adds_a_version_newest_first() {
    let vfs = s3!(&unique_prefix("versions-history/"));

    write_object(&vfs, "notes.txt", b"first").await;
    write_object(&vfs, "notes.txt", b"second").await;
    write_object(&vfs, "notes.txt", b"third").await;

    let versions = vfs.list_versions("notes.txt").await.unwrap();
    if versions.len() < 2 {
        eprintln!("skipping: bucket versioning is not enabled");
        return;
    }

    assert_eq!(versions.len(), 3, "one per write");
    assert!(versions[0].is_current, "the current version sorts first");
    assert_eq!(versions[0].size, Some(5), "the newest write was \"third\"");
    assert!(versions.iter().filter(|v| v.is_current).count() == 1);
    assert!(versions.iter().all(|v| v.id.is_some()));
}

#[tokio::test]
async fn an_old_version_is_still_readable() {
    let vfs = s3!(&unique_prefix("versions-read/"));

    write_object(&vfs, "doc.txt", b"original").await;
    write_object(&vfs, "doc.txt", b"replacement").await;

    let versions = vfs.list_versions("doc.txt").await.unwrap();
    if versions.len() < 2 {
        eprintln!("skipping: bucket versioning is not enabled");
        return;
    }

    // A plain read returns the newest.
    assert_eq!(
        vfs.read_prefix("doc.txt", 64).await.unwrap(),
        b"replacement"
    );

    let oldest = versions.last().unwrap();
    let bytes = vfs
        .read_version("doc.txt", oldest.id.as_ref().unwrap(), 64)
        .await
        .unwrap();
    assert_eq!(bytes, b"original");
}

#[tokio::test]
async fn restoring_appends_rather_than_rewriting_history() {
    let vfs = s3!(&unique_prefix("versions-restore/"));

    write_object(&vfs, "cfg.toml", b"good = true").await;
    write_object(&vfs, "cfg.toml", b"broken").await;

    let before = vfs.list_versions("cfg.toml").await.unwrap();
    if before.len() < 2 {
        eprintln!("skipping: bucket versioning is not enabled");
        return;
    }

    let good = before.last().unwrap().id.clone().unwrap();
    vfs.restore_version("cfg.toml", &good).await.unwrap();

    assert_eq!(
        vfs.read_prefix("cfg.toml", 64).await.unwrap(),
        b"good = true",
        "the current content is the restored bytes"
    );

    // History is append-only: restoring adds a version rather than removing the
    // bad one, which is what a versioned store actually does.
    let after = vfs.list_versions("cfg.toml").await.unwrap();
    assert_eq!(after.len(), before.len() + 1);
}

#[tokio::test]
async fn a_deleted_object_keeps_its_history_behind_a_marker() {
    let vfs = s3!(&unique_prefix("versions-deleted/"));

    write_object(&vfs, "gone.txt", b"still here").await;
    let listed = vfs.list_all("").await.unwrap();
    let entry = listed.iter().find(|e| &*e.name == "gone.txt").unwrap();
    vfs.delete(entry).await.unwrap();

    let versions = vfs.list_versions("gone.txt").await.unwrap();
    if versions.len() < 2 {
        eprintln!("skipping: bucket versioning is not enabled");
        return;
    }

    // The delete is an event in the history, not an erasure — without asking for
    // delete markers this would look like the object never existed.
    assert!(
        versions.iter().any(|v| v.is_delete_marker),
        "the delete marker is part of the history"
    );
    assert!(
        versions.iter().any(|v| !v.is_delete_marker),
        "and the content version is still listed"
    );
}

#[tokio::test]
async fn a_download_resumes_against_a_real_server() {
    let vfs = s3!(&unique_prefix("resume/"));

    // Big enough that a partial prefix is meaningful, small enough to be quick.
    let body: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    let source = tempfile::tempdir().unwrap();
    let file = source.path().join("payload.bin");
    std::fs::write(&file, &body).unwrap();
    vfs.upload_from(file, "payload.bin", Arc::new(TaskProgress::default()))
        .await
        .unwrap();

    let out = tempfile::tempdir().unwrap();
    let local = out.path().join("payload.bin");
    let part = std::path::PathBuf::from(format!(
        "{}{}",
        local.display(),
        roam_core::vfs::PART_SUFFIX
    ));

    // Stand in for an interrupted attempt, with the etag the server reports.
    std::fs::write(&part, &body[..50_000]).unwrap();
    let etag = vfs
        .stat("payload.bin")
        .await
        .unwrap()
        .etag()
        .map(str::to_string)
        .expect("S3 returns an etag");
    std::fs::write(format!("{}.etag", part.display()), &etag).unwrap();

    let progress = Arc::new(TaskProgress::default());
    vfs.download_to("payload.bin", local.clone(), progress.clone())
        .await
        .unwrap();

    assert_eq!(
        std::fs::read(&local).unwrap(),
        body,
        "the halves must join up"
    );
    assert!(!part.exists());
    assert_eq!(progress.done(), body.len() as u64);
}

#[tokio::test]
async fn a_resume_with_a_stale_etag_refetches_from_zero() {
    let vfs = s3!(&unique_prefix("resume-stale/"));

    let source = tempfile::tempdir().unwrap();
    let file = source.path().join("doc.txt");
    std::fs::write(&file, b"the current content").unwrap();
    vfs.upload_from(file, "doc.txt", Arc::new(TaskProgress::default()))
        .await
        .unwrap();

    let out = tempfile::tempdir().unwrap();
    let local = out.path().join("doc.txt");
    let part = std::path::PathBuf::from(format!(
        "{}{}",
        local.display(),
        roam_core::vfs::PART_SUFFIX
    ));
    std::fs::write(&part, b"STALE").unwrap();
    std::fs::write(format!("{}.etag", part.display()), "\"deadbeef\"").unwrap();

    vfs.download_to("doc.txt", local.clone(), Arc::new(TaskProgress::default()))
        .await
        .unwrap();

    // Splicing a stale prefix onto fresh bytes is silent corruption; refetching
    // is the only safe answer.
    assert_eq!(
        std::fs::read_to_string(&local).unwrap(),
        "the current content"
    );
}

#[tokio::test]
async fn moving_a_prefix_relocates_every_object() {
    let vfs = s3!(&unique_prefix("move/"));

    for path in ["src/top.txt", "src/nested/leaf.txt", "keep.txt"] {
        write_object(&vfs, path, path.as_bytes()).await;
    }

    // Object stores have no rename for a prefix, so this is copy-then-delete run
    // as one visible task.
    let plan = roam_core::transfer::plan_move_dir(&vfs, "src/", "dst/")
        .await
        .unwrap();
    let engine = TransferEngine::new(Rt::from_current().unwrap(), DEFAULT_CONCURRENCY);
    engine.enqueue(plan);

    for _ in 0..200 {
        if !engine.is_active() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert_eq!(engine.snapshot()[0].state, TaskState::Done);

    assert_eq!(
        vfs.read_prefix("dst/top.txt", 64).await.unwrap(),
        b"src/top.txt"
    );
    assert_eq!(
        vfs.read_prefix("dst/nested/leaf.txt", 64).await.unwrap(),
        b"src/nested/leaf.txt",
        "nesting survives the move"
    );

    // The source objects are gone (a versioned bucket keeps the prefix visible,
    // which is why this checks for live files rather than for the row).
    let left = vfs.list_recursive("src/").await.unwrap();
    assert!(
        left.iter().all(|e| e.is_dir()),
        "no live objects should remain, got {:?}",
        left.iter().map(|e| e.path.to_string()).collect::<Vec<_>>()
    );

    // The sibling outside the moved prefix is untouched.
    assert_eq!(vfs.read_prefix("keep.txt", 64).await.unwrap(), b"keep.txt");
}

#[tokio::test]
async fn a_cancelled_move_leaves_the_source_objects_in_place() {
    let vfs = s3!(&unique_prefix("move-cancel/"));

    for path in ["src/a.txt", "src/b.txt"] {
        write_object(&vfs, path, b"payload").await;
    }

    let plan = roam_core::transfer::plan_move_dir(&vfs, "src/", "dst/")
        .await
        .unwrap();
    let engine = TransferEngine::new(Rt::from_current().unwrap(), DEFAULT_CONCURRENCY);
    let id = engine.enqueue(plan);
    engine.cancel(id);

    for _ in 0..200 {
        if !engine.is_active() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert_eq!(engine.snapshot()[0].state, TaskState::Cancelled);

    // Nothing is lost by cancelling: the delete only runs after every copy has
    // succeeded, so the source is still whole.
    assert_eq!(vfs.read_prefix("src/a.txt", 64).await.unwrap(), b"payload");
    assert_eq!(vfs.read_prefix("src/b.txt", 64).await.unwrap(), b"payload");
}

/// The form's output has to be a *working* profile, not merely a valid one.
///
/// Every other test here builds its profile by hand, which means the option names
/// the connection form fills in were only ever checked against the schema — not
/// against a server. This goes the whole way: the values a person would type,
/// through `build_profile`, into a real connection.
#[tokio::test]
async fn a_profile_built_the_way_the_form_builds_it_connects() {
    let Some(server) = server() else {
        eprintln!("skipping: ROAM_S3_ENDPOINT is not set");
        return;
    };

    let prefix = unique_prefix("form-built");
    let values: std::collections::BTreeMap<String, String> = [
        ("bucket", server.bucket.clone()),
        ("prefix", prefix.clone()),
        ("endpoint", server.endpoint.clone()),
        ("region", "us-east-1".to_string()),
        ("access_key_id", server.key.clone()),
        ("secret_access_key", server.secret.clone()),
        // MinIO is path-style, which is this toggle's off value.
        ("enable_virtual_host_style", "false".to_string()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();

    let profile = service::build_profile("form".into(), "MinIO".into(), "s3", &values).unwrap();

    // The URI was composed, not typed.
    assert_eq!(
        profile.uri,
        format!("s3://{}/{}", server.bucket, prefix.trim_end_matches('/'))
    );

    let vfs = Vfs::from_profile(Rt::from_current().unwrap(), &profile).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("hello.txt");
    std::fs::write(&file, b"built by the form").unwrap();
    vfs.upload_from(
        file,
        "hello.txt",
        std::sync::Arc::new(roam_core::TaskProgress::new()),
    )
    .await
    .unwrap();

    let entries = vfs.list_all("").await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(&*entries[0].name, "hello.txt");
}
