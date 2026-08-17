//! Integration tests for the non-S3 backends, against local servers.
//!
//! Each backend has its own quirks, and those quirks are the point of these
//! tests — not re-checking that `Vfs` works, which the fs and S3 suites already
//! cover:
//!
//! - **azblob** has a flat namespace with no real directories, and needs its
//!   container created out of band.
//! - **gcs** signs differently and uses a resumable-upload protocol for large
//!   objects.
//! - **webdav** is a genuinely hierarchical store reached over PROPFIND/MKCOL,
//!   so it behaves more like a filesystem than an object store.
//! - **sftp** is not an HTTP backend at all: OpenDAL drives the system `ssh`
//!   binary through the `openssh` crate, so it depends on the host toolchain and
//!   on host-key policy in a way none of the others do.
//!
//! Every test skips unless its endpoint variable is set. `scripts/test-backends.sh`
//! starts the servers and prints what to export.

use std::sync::Arc;

use roam_core::transfer::{CHUNK, TaskProgress};
use roam_core::{EntryKind, Profile, Rt, Vfs};

/// Build a session, or `None` when the backend's endpoint is not configured.
fn session(
    env_key: &str,
    id: &str,
    uri: &str,
    options: &[(&str, String)],
    secrets: &[(&str, String)],
) -> Option<Vfs> {
    let endpoint = std::env::var(env_key).ok()?;

    let mut profile = Profile::new(id, id, uri);
    profile.options.insert("endpoint".into(), endpoint);
    for (key, value) in options {
        profile.options.insert((*key).into(), value.clone());
    }

    // Credentials are ordinary options now; the parameter stays separate only
    // so each backend's call site still reads as "config, then secrets".
    for (key, value) in secrets {
        profile.options.insert((*key).into(), value.clone());
    }

    Some(Vfs::from_profile(Rt::from_current().unwrap(), &profile).unwrap())
}

fn env(key: &str, fallback: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| fallback.into())
}

macro_rules! backend {
    ($vfs:expr, $what:expr) => {
        match $vfs {
            Some(vfs) => vfs,
            None => {
                eprintln!("skipping: {} is not configured", $what);
                return;
            }
        }
    };
}

// ── azblob (Azurite) ──────────────────────────────────────────────────────

fn azblob(prefix: &str) -> Option<Vfs> {
    session(
        "ROAM_AZBLOB_ENDPOINT",
        "azurite",
        &format!(
            "azblob://{}/{prefix}",
            env("ROAM_AZBLOB_CONTAINER", "roam-test")
        ),
        &[(
            "account_name",
            env("ROAM_AZBLOB_ACCOUNT", "devstoreaccount1"),
        )],
        &[(
            "account_key",
            env(
                "ROAM_AZBLOB_KEY",
                "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==",
            ),
        )],
    )
}

#[tokio::test]
async fn azblob_writes_and_reads_back() {
    let vfs = backend!(azblob("basic/"), "ROAM_AZBLOB_ENDPOINT");

    let source = tempfile::tempdir().unwrap();
    let file = source.path().join("hello.txt");
    std::fs::write(&file, b"hello from azurite").unwrap();

    vfs.upload_from(file, "hello.txt", Arc::new(TaskProgress::new()))
        .await
        .unwrap();

    assert_eq!(
        vfs.read_prefix("hello.txt", 4096).await.unwrap(),
        b"hello from azurite"
    );
}

#[tokio::test]
async fn azblob_synthesises_directories_from_a_flat_namespace() {
    let vfs = backend!(azblob("tree/"), "ROAM_AZBLOB_ENDPOINT");
    let source = tempfile::tempdir().unwrap();
    let file = source.path().join("payload");
    std::fs::write(&file, b"x").unwrap();

    for path in ["top.txt", "nested/inner.txt", "nested/deep/leaf.txt"] {
        vfs.upload_from(file.clone(), path, Arc::new(TaskProgress::new()))
            .await
            .unwrap();
    }

    // Blob storage has no directories at all; the delimiter is what makes
    // `nested` appear, and the UI depends on it being reported as a Dir.
    let root = vfs.list_all("").await.unwrap();
    let mut names: Vec<String> = root.iter().map(|e| e.name.to_string()).collect();
    names.sort();
    assert_eq!(names, vec!["nested", "top.txt"]);

    let nested = root.iter().find(|e| &*e.name == "nested").unwrap();
    assert_eq!(nested.kind, EntryKind::Dir);

    let files = vfs.list_recursive("").await.unwrap();
    assert_eq!(files.iter().filter(|e| !e.is_dir()).count(), 3);
}

#[tokio::test]
async fn azblob_reports_size_in_a_listing() {
    let vfs = backend!(azblob("meta/"), "ROAM_AZBLOB_ENDPOINT");
    let source = tempfile::tempdir().unwrap();
    let file = source.path().join("sized.txt");
    std::fs::write(&file, b"0123456789").unwrap();

    vfs.upload_from(file, "sized.txt", Arc::new(TaskProgress::new()))
        .await
        .unwrap();

    let entries = vfs.list_all("").await.unwrap();
    let entry = entries.iter().find(|e| &*e.name == "sized.txt").unwrap();
    assert_eq!(entry.size, Some(10));
}

#[tokio::test]
async fn azblob_block_upload_round_trips_exactly() {
    let vfs = backend!(azblob("blocks/"), "ROAM_AZBLOB_ENDPOINT");

    // Past one chunk, so this goes through block-list upload rather than a
    // single PUT.
    let size = CHUNK + 2048;
    let payload: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
    let source = tempfile::tempdir().unwrap();
    let file = source.path().join("big.bin");
    std::fs::write(&file, &payload).unwrap();

    let progress = Arc::new(TaskProgress::new());
    vfs.upload_from(file, "big.bin", progress.clone())
        .await
        .unwrap();
    assert_eq!(progress.done(), size as u64);

    let out = tempfile::tempdir().unwrap();
    let local = out.path().join("big.bin");
    vfs.download_to("big.bin", local.clone(), Arc::new(TaskProgress::new()))
        .await
        .unwrap();
    assert_eq!(std::fs::read(&local).unwrap(), payload);
}

#[tokio::test]
async fn azblob_recursive_delete_clears_a_prefix() {
    let vfs = backend!(azblob("rmrf/"), "ROAM_AZBLOB_ENDPOINT");
    let source = tempfile::tempdir().unwrap();
    let file = source.path().join("payload");
    std::fs::write(&file, b"x").unwrap();

    for path in ["doomed/a.txt", "doomed/deep/b.txt", "keep.txt"] {
        vfs.upload_from(file.clone(), path, Arc::new(TaskProgress::new()))
            .await
            .unwrap();
    }

    let entries = vfs.list_all("").await.unwrap();
    let doomed = entries.iter().find(|e| &*e.name == "doomed").unwrap();
    vfs.delete(doomed).await.unwrap();

    let left: Vec<String> = vfs
        .list_all("")
        .await
        .unwrap()
        .iter()
        .map(|e| e.name.to_string())
        .collect();
    assert_eq!(left, vec!["keep.txt"]);
}

#[tokio::test]
async fn azblob_capabilities_are_read_from_the_server() {
    let vfs = backend!(azblob("caps/"), "ROAM_AZBLOB_ENDPOINT");

    // Recorded rather than assumed: this is what drives the menus, and blob
    // storage differs from S3 here.
    let cap = vfs.capability();
    eprintln!(
        "azblob: write_can_multi={} copy={} rename={} presign={} create_dir={} delete_recursive={}",
        cap.write_can_multi,
        cap.copy,
        cap.rename,
        cap.presign,
        cap.create_dir,
        cap.delete_with_recursive
    );
    assert!(cap.read && cap.write && cap.list);
    assert!(!cap.rename, "blob storage has no native rename either");
}

// ── gcs (fake-gcs-server) ─────────────────────────────────────────────────

fn gcs(prefix: &str) -> Option<Vfs> {
    session(
        "ROAM_GCS_ENDPOINT",
        "fake-gcs",
        &format!("gcs://{}/{prefix}", env("ROAM_GCS_BUCKET", "roam-test")),
        &[],
        // fake-gcs-server does not verify credentials, but OpenDAL still needs
        // something to put in the Authorization header.
        &[("token", env("ROAM_GCS_TOKEN", "fake-token"))],
    )
}

#[tokio::test]
async fn gcs_writes_and_reads_back() {
    let vfs = backend!(gcs("basic/"), "ROAM_GCS_ENDPOINT");

    let source = tempfile::tempdir().unwrap();
    let file = source.path().join("hello.txt");
    std::fs::write(&file, b"hello from fake gcs").unwrap();

    vfs.upload_from(file, "hello.txt", Arc::new(TaskProgress::new()))
        .await
        .unwrap();

    assert_eq!(
        vfs.read_prefix("hello.txt", 4096).await.unwrap(),
        b"hello from fake gcs"
    );
}

#[tokio::test]
async fn gcs_lists_a_prefix_as_a_directory() {
    let vfs = backend!(gcs("tree/"), "ROAM_GCS_ENDPOINT");
    let source = tempfile::tempdir().unwrap();
    let file = source.path().join("payload");
    std::fs::write(&file, b"x").unwrap();

    for path in ["top.txt", "nested/inner.txt"] {
        vfs.upload_from(file.clone(), path, Arc::new(TaskProgress::new()))
            .await
            .unwrap();
    }

    let root = vfs.list_all("").await.unwrap();
    let mut names: Vec<String> = root.iter().map(|e| e.name.to_string()).collect();
    names.sort();
    assert_eq!(names, vec!["nested", "top.txt"]);
    assert_eq!(
        root.iter().find(|e| &*e.name == "nested").unwrap().kind,
        EntryKind::Dir
    );
}

/// Opt-in via `ROAM_GCS_MULTIPART=1`.
///
/// OpenDAL's concurrent GCS writer uses the **XML API** multipart endpoint
/// (`POST ...?uploads`), and `fake-gcs-server` answers that with 404 — it
/// implements the JSON API's resumable upload instead. Real GCS supports both,
/// so this is an emulator gap rather than a defect here, but it does mean the
/// large-object GCS path stays unverified locally.
#[tokio::test]
async fn gcs_multipart_upload_round_trips_exactly() {
    if std::env::var("ROAM_GCS_MULTIPART").is_err() {
        eprintln!("skipping: set ROAM_GCS_MULTIPART=1 against a server with the XML API");
        return;
    }
    let vfs = backend!(gcs("resumable/"), "ROAM_GCS_ENDPOINT");
    let size = CHUNK + 2048;
    let payload: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
    let source = tempfile::tempdir().unwrap();
    let file = source.path().join("big.bin");
    std::fs::write(&file, &payload).unwrap();

    let progress = Arc::new(TaskProgress::new());
    vfs.upload_from(file, "big.bin", progress.clone())
        .await
        .unwrap();
    assert_eq!(progress.done(), size as u64);

    let out = tempfile::tempdir().unwrap();
    let local = out.path().join("big.bin");
    vfs.download_to("big.bin", local.clone(), Arc::new(TaskProgress::new()))
        .await
        .unwrap();
    assert_eq!(std::fs::read(&local).unwrap(), payload);
}

#[tokio::test]
async fn gcs_capabilities_are_read_from_the_server() {
    let vfs = backend!(gcs("caps/"), "ROAM_GCS_ENDPOINT");

    let cap = vfs.capability();
    eprintln!(
        "gcs: write_can_multi={} copy={} rename={} presign={} create_dir={} delete_recursive={}",
        cap.write_can_multi,
        cap.copy,
        cap.rename,
        cap.presign,
        cap.create_dir,
        cap.delete_with_recursive
    );
    assert!(cap.read && cap.write && cap.list);
}

// ── webdav ────────────────────────────────────────────────────────────────

fn webdav(prefix: &str) -> Option<Vfs> {
    session(
        "ROAM_WEBDAV_ENDPOINT",
        "webdav",
        &format!("webdav:///{prefix}"),
        &[("username", env("ROAM_WEBDAV_USER", "roamtest"))],
        &[("password", env("ROAM_WEBDAV_PASSWORD", "roamtest-secret"))],
    )
}

#[tokio::test]
async fn webdav_writes_and_reads_back() {
    let vfs = backend!(webdav("basic/"), "ROAM_WEBDAV_ENDPOINT");

    let source = tempfile::tempdir().unwrap();
    let file = source.path().join("hello.txt");
    std::fs::write(&file, b"hello from webdav").unwrap();

    vfs.upload_from(file, "hello.txt", Arc::new(TaskProgress::new()))
        .await
        .unwrap();

    assert_eq!(
        vfs.read_prefix("hello.txt", 4096).await.unwrap(),
        b"hello from webdav"
    );
}

#[tokio::test]
async fn webdav_has_real_directories() {
    let vfs = backend!(webdav("dirs/"), "ROAM_WEBDAV_ENDPOINT");

    // Unlike the object stores, WebDAV has genuine collections, so create_dir
    // is a MKCOL rather than a zero-byte marker object.
    vfs.create_dir("made").await.unwrap();

    let entries = vfs.list_all("").await.unwrap();
    let made = entries
        .iter()
        .find(|e| &*e.name == "made")
        .expect("the created collection should be listed");
    assert_eq!(made.kind, EntryKind::Dir);
}

#[tokio::test]
async fn webdav_lists_nested_collections() {
    let vfs = backend!(webdav("nested/"), "ROAM_WEBDAV_ENDPOINT");
    let source = tempfile::tempdir().unwrap();
    let file = source.path().join("payload");
    std::fs::write(&file, b"x").unwrap();

    vfs.create_dir("inner").await.unwrap();
    vfs.upload_from(
        file.clone(),
        "inner/leaf.txt",
        Arc::new(TaskProgress::new()),
    )
    .await
    .unwrap();

    let inner = vfs.list_all("inner/").await.unwrap();
    let names: Vec<String> = inner.iter().map(|e| e.name.to_string()).collect();
    assert_eq!(names, vec!["leaf.txt"]);
}

#[tokio::test]
async fn webdav_upload_larger_than_one_chunk() {
    let vfs = backend!(webdav("big/"), "ROAM_WEBDAV_ENDPOINT");

    // WebDAV reports `write_can_multi = false`, so this is the case where asking
    // for a concurrent chunked write would be asking for something the backend
    // cannot do.
    let size = CHUNK + 2048;
    let payload: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
    let source = tempfile::tempdir().unwrap();
    let file = source.path().join("big.bin");
    std::fs::write(&file, &payload).unwrap();

    vfs.upload_from(file, "big.bin", Arc::new(TaskProgress::new()))
        .await
        .unwrap();

    let out = tempfile::tempdir().unwrap();
    let local = out.path().join("big.bin");
    vfs.download_to("big.bin", local.clone(), Arc::new(TaskProgress::new()))
        .await
        .unwrap();
    assert_eq!(std::fs::read(&local).unwrap(), payload);
}

/// Apache decides between a strong and a weak etag partly by how recently the
/// file was modified, so which path this takes is not fixed:
///
/// - **strong etag** — the part file is trusted and the remainder is appended;
/// - **weak etag** (`W/"..."`) — untrustworthy for byte-identity, so the object
///   is refetched from zero.
///
/// The invariant that matters is the same either way: the file on disk ends up as
/// the object's exact bytes. Asserting the path instead of the outcome would make
/// this test depend on the server's timing.
#[tokio::test]
async fn webdav_download_is_byte_exact_whether_it_resumes_or_refetches() {
    let vfs = backend!(webdav("resume/"), "ROAM_WEBDAV_ENDPOINT");

    let body: Vec<u8> = (0..40_000u32).map(|i| (i % 251) as u8).collect();
    let source = tempfile::tempdir().unwrap();
    let file = source.path().join("payload.bin");
    std::fs::write(&file, &body).unwrap();
    vfs.upload_from(file, "payload.bin", Arc::new(TaskProgress::new()))
        .await
        .unwrap();

    let out = tempfile::tempdir().unwrap();
    let local = out.path().join("payload.bin");
    let part = std::path::PathBuf::from(format!(
        "{}{}",
        local.display(),
        roam_core::vfs::PART_SUFFIX
    ));

    // A genuine prefix, which is the only kind this code ever writes.
    std::fs::write(&part, &body[..10_000]).unwrap();
    let served = vfs.stat("payload.bin").await.unwrap();
    let etag = served.etag().map(str::to_string);
    if let Some(etag) = &etag {
        std::fs::write(format!("{}.etag", part.display()), etag).unwrap();
        eprintln!(
            "webdav etag: {etag} ({})",
            if etag.starts_with("W/") {
                "weak — will refetch"
            } else {
                "strong — will resume"
            }
        );
    }

    vfs.download_to("payload.bin", local.clone(), Arc::new(TaskProgress::new()))
        .await
        .unwrap();

    assert_eq!(std::fs::read(&local).unwrap(), body);
    assert!(!part.exists(), "the part file is consumed either way");
}

#[tokio::test]
async fn webdav_capabilities_are_read_from_the_server() {
    let vfs = backend!(webdav("caps/"), "ROAM_WEBDAV_ENDPOINT");

    let cap = vfs.capability();
    eprintln!(
        "webdav: write_can_multi={} copy={} rename={} presign={} create_dir={} delete_recursive={}",
        cap.write_can_multi,
        cap.copy,
        cap.rename,
        cap.presign,
        cap.create_dir,
        cap.delete_with_recursive
    );
    assert!(cap.read && cap.write && cap.list);
    assert!(!cap.presign, "WebDAV cannot presign");
}

// ── sftp (local sshd) ─────────────────────────────────────────────────────
//
// Unlike the others, this backend is not HTTP: `opendal-service-sftp` builds on
// the `openssh` crate, which **shells out to the system `ssh` binary**. That has
// consequences worth stating plainly:
//
// - the host needs a working `ssh`/`sftp` in `PATH`;
// - host-key checking is the system's, so a fresh server needs a known-hosts
//   policy — hence `known_hosts_strategy`;
// - authentication is whatever ssh would do, so a key has to be reachable on
//   disk. There is no way to hand it bytes from the keychain, which is why the
//   profile stores a *path* here rather than a secret.

fn sftp(prefix: &str) -> Option<Vfs> {
    // Windows has no sftp backend compiled in at all, so there is nothing here to
    // test even if a server is configured — skip rather than fail.
    if cfg!(windows) {
        return None;
    }

    // The file, not just the variable: `up` exports every backend's environment
    // even when only one server was started, so checking the variable alone turns
    // "not configured" into a failure instead of a skip.
    let key = std::env::var("ROAM_SFTP_KEY")
        .ok()
        .filter(|path| std::path::Path::new(path).is_file())?;
    session(
        "ROAM_SFTP_ENDPOINT",
        "sftp",
        &format!("sftp:///upload/{prefix}"),
        &[
            ("user", env("ROAM_SFTP_USER", "roamtest")),
            ("key", key),
            // A throwaway container gets a new host key every time, so refusing
            // to accept it would just mean never connecting.
            ("known_hosts_strategy", "accept".into()),
        ],
        &[],
    )
}

#[tokio::test]
async fn sftp_writes_and_reads_back() {
    let vfs = backend!(sftp("basic/"), "ROAM_SFTP_ENDPOINT + ROAM_SFTP_KEY");

    let source = tempfile::tempdir().unwrap();
    let file = source.path().join("hello.txt");
    std::fs::write(&file, b"hello over ssh").unwrap();

    vfs.upload_from(file, "hello.txt", Arc::new(TaskProgress::new()))
        .await
        .unwrap();

    assert_eq!(
        vfs.read_prefix("hello.txt", 4096).await.unwrap(),
        b"hello over ssh"
    );
}

#[tokio::test]
async fn sftp_has_real_directories() {
    let vfs = backend!(sftp("dirs/"), "ROAM_SFTP_ENDPOINT + ROAM_SFTP_KEY");

    // A real filesystem behind it, so mkdir is mkdir.
    vfs.create_dir("made").await.unwrap();

    let entries = vfs.list_all("").await.unwrap();
    let made = entries
        .iter()
        .find(|e| &*e.name == "made")
        .expect("the created directory should be listed");
    assert_eq!(made.kind, EntryKind::Dir);
}

#[tokio::test]
async fn sftp_lists_nested_directories() {
    let vfs = backend!(sftp("nested/"), "ROAM_SFTP_ENDPOINT + ROAM_SFTP_KEY");
    let source = tempfile::tempdir().unwrap();
    let file = source.path().join("payload");
    std::fs::write(&file, b"x").unwrap();

    vfs.create_dir("inner").await.unwrap();
    vfs.upload_from(file, "inner/leaf.txt", Arc::new(TaskProgress::new()))
        .await
        .unwrap();

    let inner = vfs.list_all("inner/").await.unwrap();
    let names: Vec<String> = inner.iter().map(|e| e.name.to_string()).collect();
    assert_eq!(names, vec!["leaf.txt"]);
}

#[tokio::test]
async fn sftp_upload_larger_than_one_chunk() {
    let vfs = backend!(sftp("big/"), "ROAM_SFTP_ENDPOINT + ROAM_SFTP_KEY");

    let size = CHUNK + 2048;
    let payload: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
    let source = tempfile::tempdir().unwrap();
    let file = source.path().join("big.bin");
    std::fs::write(&file, &payload).unwrap();

    vfs.upload_from(file, "big.bin", Arc::new(TaskProgress::new()))
        .await
        .unwrap();

    let out = tempfile::tempdir().unwrap();
    let local = out.path().join("big.bin");
    vfs.download_to("big.bin", local.clone(), Arc::new(TaskProgress::new()))
        .await
        .unwrap();
    assert_eq!(std::fs::read(&local).unwrap(), payload);
}

#[tokio::test]
async fn sftp_recursive_delete_removes_a_directory() {
    let vfs = backend!(sftp("rmrf/"), "ROAM_SFTP_ENDPOINT + ROAM_SFTP_KEY");
    let source = tempfile::tempdir().unwrap();
    let file = source.path().join("payload");
    std::fs::write(&file, b"x").unwrap();

    vfs.create_dir("doomed").await.unwrap();
    vfs.upload_from(file.clone(), "doomed/a.txt", Arc::new(TaskProgress::new()))
        .await
        .unwrap();
    vfs.upload_from(file, "keep.txt", Arc::new(TaskProgress::new()))
        .await
        .unwrap();

    let entries = vfs.list_all("").await.unwrap();
    let doomed = entries.iter().find(|e| &*e.name == "doomed").unwrap();
    vfs.delete(doomed).await.unwrap();

    let left: Vec<String> = vfs
        .list_all("")
        .await
        .unwrap()
        .iter()
        .map(|e| e.name.to_string())
        .collect();
    assert_eq!(left, vec!["keep.txt"]);
}

#[tokio::test]
async fn sftp_capabilities_are_read_from_the_server() {
    let vfs = backend!(sftp("caps/"), "ROAM_SFTP_ENDPOINT + ROAM_SFTP_KEY");

    let cap = vfs.capability();
    eprintln!(
        "sftp: write_can_multi={} copy={} rename={} presign={} create_dir={} delete_recursive={}",
        cap.write_can_multi,
        cap.copy,
        cap.rename,
        cap.presign,
        cap.create_dir,
        cap.delete_with_recursive
    );
    assert!(cap.read && cap.write && cap.list);
    assert!(!cap.presign, "sftp has no notion of a signed URL");
}

// ── sftp with a password ──────────────────────────────────────────────────
//
// OpenDAL's sftp service has no password option, so this exercises the one path
// that works: the `SSH_ASKPASS` helper in `roam_core::sftp_auth`. Without a real
// server the mechanism cannot be checked at all — a unit test can only confirm
// that two string transforms agree.

fn sftp_password(prefix: &str) -> Option<Vfs> {
    if cfg!(windows) {
        return None;
    }

    let endpoint = std::env::var("ROAM_SFTP_PW_ENDPOINT").ok()?;
    let user = env("ROAM_SFTP_PW_USER", "pwuser");
    let password = env("ROAM_SFTP_PW_PASSWORD", "pwsecret");

    let mut profile = Profile::new("sftp-pw", "sftp-pw", format!("sftp:///upload/{prefix}"));
    profile.options.insert("endpoint".into(), endpoint);
    profile.options.insert("user".into(), user);
    profile.options.insert("password".into(), password);
    profile
        .options
        .insert("known_hosts_strategy".into(), "accept".into());

    Some(Vfs::from_profile(Rt::from_current().unwrap(), &profile).unwrap())
}

#[tokio::test]
async fn sftp_authenticates_with_a_password() {
    let Some(vfs) = sftp_password(&format!("pw-basic-{}/", std::process::id())) else {
        eprintln!("skipping: ROAM_SFTP_PW_ENDPOINT is not set");
        return;
    };

    let source = tempfile::tempdir().unwrap();
    let file = source.path().join("hello.txt");
    std::fs::write(&file, b"authenticated with a password").unwrap();

    vfs.upload_from(file, "hello.txt", Arc::new(TaskProgress::new()))
        .await
        .unwrap();

    let entries = vfs.list_all("").await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(&*entries[0].name, "hello.txt");

    let body = vfs.read_prefix("hello.txt", 1024).await.unwrap();
    assert_eq!(body, b"authenticated with a password");
}

#[tokio::test]
async fn a_wrong_sftp_password_is_refused() {
    if cfg!(windows) || std::env::var("ROAM_SFTP_PW_ENDPOINT").is_err() {
        eprintln!("skipping: ROAM_SFTP_PW_ENDPOINT is not set");
        return;
    }

    // A *different* user on purpose. Passwords are keyed by `user@host`, and the
    // environment they live in is process-global, so reusing `pwuser` here would
    // overwrite the good password and make the other test fail instead — which is
    // exactly what happened the first time this was written.
    let mut profile = Profile::new("sftp-bad", "sftp-bad", "sftp:///upload/");
    profile.options.insert(
        "endpoint".into(),
        std::env::var("ROAM_SFTP_PW_ENDPOINT").unwrap(),
    );
    profile.options.insert("user".into(), "nosuchuser".into());
    profile
        .options
        .insert("password".into(), "definitely-wrong".into());
    profile
        .options
        .insert("known_hosts_strategy".into(), "accept".into());

    let vfs = Vfs::from_profile(Rt::from_current().unwrap(), &profile).unwrap();
    let err = vfs.list_all("").await.unwrap_err();

    // What matters is that it fails rather than connecting anonymously.
    println!("wrong password gave: {}", err.user_message());
}
