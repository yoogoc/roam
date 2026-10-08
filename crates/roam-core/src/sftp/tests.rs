use super::*;
use russh::{
    Channel, ChannelId,
    server::{self, Auth, Msg, Session},
};
use russh_sftp::protocol::{Attrs, Data, File, Handle, Name, Status, Version};
use std::{
    collections::HashMap,
    io::{Read, Seek, SeekFrom, Write},
    path::PathBuf,
    sync::atomic::AtomicUsize,
};

struct Fixture {
    directory: tempfile::TempDir,
    profile: Profile,
    listener: tokio::task::JoinHandle<()>,
    passwords: Arc<AtomicUsize>,
    handles: Arc<AtomicUsize>,
}
impl Fixture {
    async fn new() -> Self {
        Self::with_posix_rename(true).await
    }
    async fn with_posix_rename(posix_rename: bool) -> Self {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("root")).unwrap();
        let key =
            russh::keys::PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519)
                .unwrap();
        let fingerprint = key.public_key().fingerprint(HashAlg::Sha256).to_string();
        let config = Arc::new(server::Config {
            keys: vec![key],
            auth_rejection_time: Duration::ZERO,
            auth_rejection_time_initial: Some(Duration::ZERO),
            ..Default::default()
        });
        let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = socket.local_addr().unwrap().port();
        let passwords = Arc::new(AtomicUsize::new(0));
        let handles = Arc::new(AtomicUsize::new(0));
        let root = directory.path().to_path_buf();
        let calls = passwords.clone();
        let open = handles.clone();
        let listener = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                let Ok((stream, _)) = socket.accept().await else {
                    break;
                };
                let config = config.clone();
                let handler = SshServer {
                    root: root.clone(),
                    passwords: calls.clone(),
                    handles: open.clone(),
                    channels: HashMap::new(),
                    posix_rename,
                };
                connections.spawn(async move {
                    if let Ok(session) = server::run_stream(config, stream, handler).await {
                        let _ = session.await;
                    }
                });
            }
        });
        let mut profile = Profile::new("sftp-test", "SFTP test", "sftp:///root");
        for (key, value) in [
            ("server", "127.0.0.1".into()),
            ("port", port.to_string()),
            ("username", "alice".into()),
            ("password", " secret ".into()),
            ("host_key", fingerprint),
        ] {
            profile.options.insert(key.into(), value);
        }
        Self {
            directory,
            profile,
            listener,
            passwords,
            handles,
        }
    }
    fn operator(&self) -> Operator {
        Operator::new(SftpBuilder(
            SftpConfig::from_profile(&self.profile).unwrap(),
        ))
        .unwrap()
    }
    async fn all_handles_closed(&self) {
        for _ in 0..100 {
            if self.handles.load(Ordering::SeqCst) == 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(self.handles.load(Ordering::SeqCst), 0);
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.listener.abort();
    }
}
struct SshServer {
    root: PathBuf,
    passwords: Arc<AtomicUsize>,
    handles: Arc<AtomicUsize>,
    channels: HashMap<ChannelId, Channel<Msg>>,
    posix_rename: bool,
}
impl server::Handler for SshServer {
    type Error = russh::Error;
    async fn auth_password(
        &mut self,
        user: &str,
        password: &str,
    ) -> std::result::Result<Auth, Self::Error> {
        self.passwords.fetch_add(1, Ordering::SeqCst);
        Ok(if user == "alice" && password == " secret " {
            Auth::Accept
        } else {
            Auth::reject()
        })
    }
    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: server::ChannelOpenHandle,
        _: &mut Session,
    ) -> std::result::Result<(), Self::Error> {
        self.channels.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }
    async fn subsystem_request(
        &mut self,
        id: ChannelId,
        name: &str,
        session: &mut Session,
    ) -> std::result::Result<(), Self::Error> {
        if name != "sftp" {
            session.channel_failure(id)?;
            return Ok(());
        }
        session.channel_success(id)?;
        let channel = self.channels.remove(&id).unwrap();
        let filesystem = Filesystem {
            root: self.root.clone(),
            handles: HashMap::new(),
            open: self.handles.clone(),
            serial: 0,
            posix_rename: self.posix_rename,
        };
        tokio::spawn(async move {
            russh_sftp::server::run(channel.into_stream(), filesystem).await;
        });
        Ok(())
    }
}
enum FileHandle {
    File(std::fs::File),
    Directory(VecDeque<File>),
}
struct Filesystem {
    root: PathBuf,
    handles: HashMap<String, FileHandle>,
    open: Arc<AtomicUsize>,
    serial: usize,
    posix_rename: bool,
}
fn io(error: std::io::Error) -> StatusCode {
    match error.kind() {
        std::io::ErrorKind::NotFound => StatusCode::NoSuchFile,
        std::io::ErrorKind::PermissionDenied => StatusCode::PermissionDenied,
        _ => StatusCode::Failure,
    }
}
fn ok(id: u32) -> Status {
    Status {
        id,
        status_code: StatusCode::Ok,
        error_message: String::new(),
        language_tag: String::new(),
    }
}
impl Filesystem {
    fn path(&self, path: &str) -> std::result::Result<PathBuf, StatusCode> {
        if path.split('/').any(|part| part == "..") {
            return Err(StatusCode::PermissionDenied);
        }
        Ok(self.root.join(path.trim_start_matches('/')))
    }
    fn add(&mut self, id: u32, file: FileHandle) -> Handle {
        self.serial += 1;
        let handle = self.serial.to_string();
        self.handles.insert(handle.clone(), file);
        self.open.fetch_add(1, Ordering::SeqCst);
        Handle { id, handle }
    }
    fn file(&mut self, handle: &str) -> std::result::Result<&mut std::fs::File, StatusCode> {
        match self.handles.get_mut(handle) {
            Some(FileHandle::File(file)) => Ok(file),
            _ => Err(StatusCode::Failure),
        }
    }
}
impl Drop for Filesystem {
    fn drop(&mut self) {
        self.open.fetch_sub(self.handles.len(), Ordering::SeqCst);
    }
}
impl russh_sftp::server::Handler for Filesystem {
    type Error = StatusCode;
    fn unimplemented(&self) -> Self::Error {
        StatusCode::OpUnsupported
    }
    async fn init(
        &mut self,
        _: u32,
        _: HashMap<String, String>,
    ) -> std::result::Result<Version, StatusCode> {
        let mut version = Version::new();
        if self.posix_rename {
            version
                .extensions
                .insert("posix-rename@openssh.com".into(), "1".into());
        }
        Ok(version)
    }
    async fn open(
        &mut self,
        id: u32,
        filename: String,
        flags: OpenFlags,
        _: FileAttributes,
    ) -> std::result::Result<Handle, StatusCode> {
        let file = std::fs::OpenOptions::from(flags)
            .open(self.path(&filename)?)
            .map_err(io)?;
        let handle = self.add(id, FileHandle::File(file));
        if filename.contains("/slow/") {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        Ok(handle)
    }
    async fn close(&mut self, id: u32, handle: String) -> std::result::Result<Status, StatusCode> {
        if self.handles.remove(&handle).is_some() {
            self.open.fetch_sub(1, Ordering::SeqCst);
        }
        Ok(ok(id))
    }
    async fn read(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        len: u32,
    ) -> std::result::Result<Data, StatusCode> {
        let file = self.file(&handle)?;
        file.seek(SeekFrom::Start(offset)).map_err(io)?;
        let mut data = vec![0; len.min(16384) as usize];
        let count = file.read(&mut data).map_err(io)?;
        if count == 0 {
            return Err(StatusCode::Eof);
        }
        data.truncate(count);
        Ok(Data { id, data })
    }
    async fn write(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        data: Vec<u8>,
    ) -> std::result::Result<Status, StatusCode> {
        let file = self.file(&handle)?;
        file.seek(SeekFrom::Start(offset)).map_err(io)?;
        file.write_all(&data).map_err(io)?;
        Ok(ok(id))
    }
    async fn stat(&mut self, id: u32, path: String) -> std::result::Result<Attrs, StatusCode> {
        Ok(Attrs {
            id,
            attrs: (&std::fs::metadata(self.path(&path)?).map_err(io)?).into(),
        })
    }
    async fn lstat(&mut self, id: u32, path: String) -> std::result::Result<Attrs, StatusCode> {
        Ok(Attrs {
            id,
            attrs: (&std::fs::symlink_metadata(self.path(&path)?).map_err(io)?).into(),
        })
    }
    async fn opendir(&mut self, id: u32, path: String) -> std::result::Result<Handle, StatusCode> {
        let entries = std::fs::read_dir(self.path(&path)?)
            .map_err(io)?
            .map(|entry| {
                let entry = entry.map_err(io)?;
                Ok(File::new(
                    entry.file_name().to_string_lossy(),
                    (&entry.metadata().map_err(io)?).into(),
                ))
            })
            .collect::<std::result::Result<VecDeque<_>, StatusCode>>()?;
        let handle = self.add(id, FileHandle::Directory(entries));
        if path.ends_with("/slow") {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        Ok(handle)
    }
    async fn readdir(&mut self, id: u32, handle: String) -> std::result::Result<Name, StatusCode> {
        let Some(FileHandle::Directory(entries)) = self.handles.get_mut(&handle) else {
            return Err(StatusCode::Failure);
        };
        if entries.is_empty() {
            return Err(StatusCode::Eof);
        }
        let files = (0..2).filter_map(|_| entries.pop_front()).collect();
        Ok(Name { id, files })
    }
    async fn mkdir(
        &mut self,
        id: u32,
        path: String,
        _: FileAttributes,
    ) -> std::result::Result<Status, StatusCode> {
        std::fs::create_dir(self.path(&path)?).map_err(io)?;
        Ok(ok(id))
    }
    async fn rmdir(&mut self, id: u32, path: String) -> std::result::Result<Status, StatusCode> {
        std::fs::remove_dir(self.path(&path)?).map_err(io)?;
        Ok(ok(id))
    }
    async fn remove(&mut self, id: u32, path: String) -> std::result::Result<Status, StatusCode> {
        std::fs::remove_file(self.path(&path)?).map_err(io)?;
        Ok(ok(id))
    }
    async fn rename(
        &mut self,
        id: u32,
        from: String,
        to: String,
    ) -> std::result::Result<Status, StatusCode> {
        if self.path(&to)?.exists() {
            return Err(StatusCode::Failure);
        }
        std::fs::rename(self.path(&from)?, self.path(&to)?).map_err(io)?;
        Ok(ok(id))
    }
    async fn extended(
        &mut self,
        id: u32,
        request: String,
        data: Vec<u8>,
    ) -> std::result::Result<Packet, StatusCode> {
        if request != "posix-rename@openssh.com" {
            return Err(StatusCode::OpUnsupported);
        }
        let mut data = data.as_slice();
        let mut paths = Vec::new();
        for _ in 0..2 {
            let len = u32::from_be_bytes(
                data.get(..4)
                    .ok_or(StatusCode::BadMessage)?
                    .try_into()
                    .unwrap(),
            ) as usize;
            data = &data[4..];
            paths.push(
                std::str::from_utf8(data.get(..len).ok_or(StatusCode::BadMessage)?)
                    .map_err(|_| StatusCode::BadMessage)?
                    .to_string(),
            );
            data = &data[len..];
        }
        std::fs::rename(self.path(&paths[0])?, self.path(&paths[1])?).map_err(io)?;
        Ok(Packet::Status(ok(id)))
    }
}

#[tokio::test]
async fn password_authentication_and_streamed_file_operations() {
    let fixture = Fixture::new().await;
    let op = fixture.operator();
    op.create_dir("目录/nested/").await.unwrap();
    let payload: Vec<u8> = (0..250_000).map(|value| (value % 251) as u8).collect();
    op.write("目录/nested/报告.bin", payload.clone())
        .await
        .unwrap();
    assert_eq!(
        op.read("目录/nested/报告.bin").await.unwrap().to_vec(),
        payload
    );
    assert_eq!(
        op.read_with("目录/nested/报告.bin")
            .range(31_999..67_777)
            .await
            .unwrap()
            .to_vec(),
        payload[31_999..67_777]
    );
    assert_eq!(
        op.stat("目录/nested/报告.bin")
            .await
            .unwrap()
            .content_length(),
        250_000
    );
    op.write("目录/nested/报告.bin", "replacement")
        .await
        .unwrap();
    assert_eq!(
        op.read("目录/nested/报告.bin").await.unwrap().to_vec(),
        b"replacement"
    );
    op.rename("目录/nested/报告.bin", "目录/nested/new.bin")
        .await
        .unwrap();
    for index in 0..7 {
        op.write(&format!("目录/nested/{index}.txt"), "data")
            .await
            .unwrap();
    }
    assert_eq!(op.list("目录/nested/").await.unwrap().len(), 8);
    let mut upload = op.writer("目录/nested/aborted.bin").await.unwrap();
    upload.write("partial").await.unwrap();
    upload.abort().await.unwrap();
    assert!(!op.exists("目录/nested/aborted.bin").await.unwrap());
    assert_eq!(op.list("目录/nested/").await.unwrap().len(), 8);
    op.write("empty.txt", "").await.unwrap();
    assert_eq!(op.stat("empty.txt").await.unwrap().content_length(), 0);
    op.delete_with("目录/").recursive(true).await.unwrap();
    assert!(!op.exists("目录/").await.unwrap());
    op.delete("already-missing.txt").await.unwrap();
    fixture.all_handles_closed().await;
    assert_eq!(fixture.passwords.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn dropping_uploads_and_failed_overwrites_preserve_the_original() {
    let fixture = Fixture::with_posix_rename(false).await;
    let op = fixture.operator();
    op.write("original.txt", "original").await.unwrap();
    assert!(op.write("original.txt", "replacement").await.is_err());
    assert_eq!(op.read("original.txt").await.unwrap().to_vec(), b"original");
    let mut writer = op.writer("cancelled.txt").await.unwrap();
    writer.write("partial").await.unwrap();
    drop(writer);
    // Cleanup runs on Tokio even when a writer is dropped by a foreign executor.
    for _ in 0..100 {
        if op.list("/").await.unwrap().len() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(op.list("/").await.unwrap().len(), 1);
    fixture.all_handles_closed().await;
}

#[tokio::test]
async fn cancellation_during_open_releases_the_handle_and_partial_file() {
    let fixture = Fixture::new().await;
    std::fs::create_dir(fixture.directory.path().join("root/slow")).unwrap();
    let op = fixture.operator();
    for directory in [false, true] {
        let op = op.clone();
        let pending = tokio::spawn(async move {
            if directory {
                op.list("slow/").await.map(|_| ())
            } else {
                op.write("slow/cancel.bin", "partial").await.map(|_| ())
            }
        });
        for _ in 0..100 {
            if fixture.handles.load(Ordering::SeqCst) > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(fixture.handles.load(Ordering::SeqCst) > 0);
        pending.abort();
        assert!(pending.await.unwrap_err().is_cancelled());
        fixture.all_handles_closed().await;
    }
    for _ in 0..100 {
        if std::fs::read_dir(fixture.directory.path().join("root/slow"))
            .unwrap()
            .count()
            == 0
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        std::fs::read_dir(fixture.directory.path().join("root/slow"))
            .unwrap()
            .count(),
        0
    );
}

#[tokio::test]
async fn unknown_host_probe_and_changed_key_never_send_passwords() {
    let fixture = Fixture::new().await;
    let mut unknown = fixture.profile.clone();
    unknown.options.remove("host_key");
    let host = check_host(unknown).await.unwrap().unwrap();
    assert_eq!(host.fingerprint, fixture.profile.options["host_key"]);
    assert_eq!(fixture.passwords.load(Ordering::SeqCst), 0);
    assert!(check_host(fixture.profile.clone()).await.unwrap().is_none());
    assert_eq!(fixture.passwords.load(Ordering::SeqCst), 0);
    let mut changed = fixture.profile.clone();
    changed.options.insert(
        "host_key".into(),
        format!(
            "SHA256:{}",
            base64::engine::general_purpose::STANDARD_NO_PAD.encode([0u8; 32])
        ),
    );
    let config = SftpConfig::from_profile(&changed).unwrap();
    let error = match config.connect().await {
        Ok(_) => panic!("changed key must be rejected"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), ErrorKind::PermissionDenied);
    assert!(error.message().contains("指纹已变化"));
    assert_eq!(fixture.passwords.load(Ordering::SeqCst), 0);
    let mut wrong = fixture.profile.clone();
    wrong.options.insert("password".into(), "wrong".into());
    let config = SftpConfig::from_profile(&wrong).unwrap();
    let error = match config.connect().await {
        Ok(_) => panic!("wrong password must fail"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), ErrorKind::PermissionDenied);
    assert_eq!(fixture.passwords.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn saved_profile_works_through_vfs_and_releases_cancelled_listing() {
    let fixture = Fixture::new().await;
    for index in 0..8 {
        std::fs::write(
            fixture.directory.path().join(format!("root/{index}.txt")),
            "hello",
        )
        .unwrap();
    }
    let vfs =
        crate::Vfs::from_profile(crate::Rt::from_current().unwrap(), &fixture.profile).unwrap();
    assert_eq!(vfs.read_prefix("0.txt", 3).await.unwrap(), b"hel");
    let (entries, truncated) = vfs.list_recursive_limited("/", 3).await.unwrap();
    assert_eq!(entries.len(), 3);
    assert!(truncated);
    fixture.all_handles_closed().await;
    assert!(vfs.capability().write_can_multi);
    assert!(!vfs.capability().presign);
}

#[tokio::test]
async fn vfs_upload_download_and_cross_backend_copy_stream_large_files() {
    let fixture = Fixture::new().await;
    let rt = crate::Rt::from_current().unwrap();
    let vfs = crate::Vfs::from_profile(rt.clone(), &fixture.profile).unwrap();
    let local = tempfile::tempdir().unwrap();
    let source = local.path().join("source.bin");
    let payload: Vec<u8> = (0..crate::transfer::CHUNK + 33)
        .map(|i| (i % 251) as u8)
        .collect();
    std::fs::write(&source, &payload).unwrap();
    vfs.upload_from(
        source,
        "uploaded.bin",
        Arc::new(crate::transfer::TaskProgress::new()),
    )
    .await
    .unwrap();
    let downloaded = local.path().join("downloaded.bin");
    vfs.download_to(
        "uploaded.bin",
        downloaded.clone(),
        Arc::new(crate::transfer::TaskProgress::new()),
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read(downloaded).unwrap(), payload);
    let target = crate::Vfs::local(rt, local.path().to_str().unwrap()).unwrap();
    vfs.copy_to(
        "uploaded.bin",
        &target,
        "copied.bin",
        Arc::new(crate::transfer::TaskProgress::new()),
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read(local.path().join("copied.bin")).unwrap(),
        payload
    );
    fixture.all_handles_closed().await;
}

#[test]
fn invalid_configuration_and_traversal_are_rejected() {
    let mut profile = Profile::new("test", "SFTP", "sftp:///");
    for (key, value) in [
        ("server", "[::1]"),
        ("username", "alice"),
        ("password", " password "),
    ] {
        profile.options.insert(key.into(), value.into());
    }
    let config = SftpConfig::from_profile(&profile).unwrap();
    assert_eq!(config.port, 22);
    assert_eq!(config.server, "::1");
    assert_eq!(config.password, " password ");
    assert_eq!(
        config.remote("folder/报告.csv").unwrap(),
        "/folder/报告.csv"
    );
    assert!(config.remote("../escape").is_err());
    for (key, value) in [
        ("port", "0"),
        ("port", "65536"),
        ("server", "ssh://example.com"),
        ("username", ""),
        ("password", ""),
        ("host_key", "SHA256:bad"),
    ] {
        let mut bad = profile.clone();
        bad.options.insert(key.into(), value.into());
        assert!(bad.validate().is_err(), "{key}");
    }
    profile.uri = "sftp:///../outside".into();
    assert!(profile.validate().is_err());
}
