//! SSH/password authentication and SFTP IO implemented directly with russh.
//! The custom Service only adapts those operations to Roam's existing VFS;
//! OpenDAL's SFTP service, external ssh processes and agents are never used.
use crate::Profile;
use base64::Engine;
use opendal::{raw::*, *};
use russh::{
    client,
    keys::{HashAlg, PublicKeyOrCertificate, ssh_key::PublicKey},
};
use russh_sftp::{
    client::{RawSftpSession, error::Error as SftpError},
    protocol::{FileAttributes, OpenFlags, Packet, StatusCode},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::Mutex;

const BLOCK: usize = 32 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SftpHostKey {
    pub server: String,
    pub port: u16,
    pub fingerprint: String,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct SftpConfig {
    server: String,
    port: u16,
    username: String,
    password: String,
    root: String,
    host_key: Option<String>,
}
impl std::fmt::Debug for SftpConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SftpConfig")
            .field("server", &self.server)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("root", &self.root)
            .field("host_key", &self.host_key)
            .finish()
    }
}
impl SftpConfig {
    pub(crate) fn from_profile(profile: &Profile) -> crate::Result<Self> {
        let invalid = |text: &str| crate::Error::Config(format!("SFTP：{text}"));
        let value = |key: &str| profile.options.get(key).map(String::as_str).unwrap_or("");
        let raw = value("server").trim();
        let server = raw
            .strip_prefix('[')
            .and_then(|s| s.strip_suffix(']'))
            .unwrap_or(raw);
        if server.is_empty()
            || server.contains(['/', '\\', '@', '\0'])
            || server.chars().any(char::is_whitespace)
            || (server.parse::<std::net::IpAddr>().is_err()
                && server.contains([':', '[', ']', '?', '#']))
        {
            return Err(invalid("请填写主机名或 IP 地址，不含协议、端口或路径"));
        }
        let port = if value("port").trim().is_empty() {
            22
        } else {
            value("port")
                .trim()
                .parse::<u16>()
                .ok()
                .filter(|p| *p > 0)
                .ok_or_else(|| invalid("端口必须为 1 到 65535 的整数"))?
        };
        let username = value("username").trim();
        if username.is_empty() || username.contains(['\0', '\n', '\r']) {
            return Err(invalid("请填写用户名"));
        }
        if value("password").is_empty() {
            return Err(invalid("请填写密码"));
        }
        let root = profile
            .uri
            .split_once("://")
            .map(|(_, rest)| rest)
            .unwrap_or("");
        let root = format!("/{}", root.trim_matches('/'));
        if root.contains('\0') || root.split('/').any(|part| part == "..") {
            return Err(invalid("远程目录不能包含空字符或 .."));
        }
        let host_key = value("host_key").trim();
        if !host_key.is_empty() {
            let valid = host_key
                .strip_prefix("SHA256:")
                .and_then(|hash| {
                    base64::engine::general_purpose::STANDARD_NO_PAD
                        .decode(hash)
                        .ok()
                })
                .is_some_and(|hash| hash.len() == 32);
            if !valid {
                return Err(invalid("服务器指纹必须为 SHA256: 开头的 SSH 指纹"));
            }
        }
        Ok(Self {
            server: server.into(),
            port,
            username: username.into(),
            password: value("password").into(),
            root,
            host_key: (!host_key.is_empty()).then(|| host_key.into()),
        })
    }
    fn remote(&self, path: &str) -> Result<String> {
        if path.contains('\0') || path.split('/').any(|p| p == "..") {
            return Err(Error::new(
                ErrorKind::ConfigInvalid,
                "SFTP 路径不能包含空字符或 ..",
            ));
        }
        let path = path.trim_matches('/');
        Ok(if path.is_empty() {
            self.root.clone()
        } else {
            format!("{}/{path}", self.root.trim_end_matches('/'))
        })
    }
    async fn connect(&self) -> Result<Arc<Connection>> {
        let mut ssh = ssh_connect(self, false, Arc::new(StdMutex::new(None))).await?;
        if !ssh
            .authenticate_password(&self.username, &self.password)
            .await
            .map_err(transport)?
            .success()
        {
            return Err(Error::new(
                ErrorKind::PermissionDenied,
                "SFTP 用户名或密码错误，或服务器未启用密码认证",
            ));
        }
        let channel = ssh.channel_open_session().await.map_err(transport)?;
        channel
            .request_subsystem(true, "sftp")
            .await
            .map_err(transport)?;
        let raw = RawSftpSession::new_with_config(
            channel.into_stream(),
            russh_sftp::client::Config {
                request_timeout_secs: 120,
                ..Default::default()
            },
        );
        let version = raw.init().await.map_err(protocol)?;
        if version.version != 3 {
            return Err(Error::new(ErrorKind::Unsupported, "服务器不支持 SFTP v3"));
        }
        let posix_rename = version.extensions.contains_key("posix-rename@openssh.com");
        Ok(Arc::new(Connection {
            ssh,
            raw,
            posix_rename,
            healthy: AtomicBool::new(true),
        }))
    }
}

#[derive(Debug, thiserror::Error)]
enum SshError {
    #[error("{0}")]
    Ssh(#[from] russh::Error),
    #[error("{0}")]
    Host(String),
}
struct VerifyHost {
    server: String,
    port: u16,
    expected: Option<String>,
    probe: bool,
    captured: Arc<StdMutex<Option<PublicKey>>>,
}
fn trusted(
    server: &str,
    port: u16,
    expected: Option<&str>,
    key: &PublicKey,
) -> std::result::Result<bool, SshError> {
    let actual = key.fingerprint(HashAlg::Sha256).to_string();
    if let Some(expected) = expected {
        return if actual == expected {
            Ok(true)
        } else {
            Err(SshError::Host(format!(
                "SFTP 服务器指纹已变化；已保存 {expected}，当前 {actual}。请核实服务器身份后再编辑连接。"
            )))
        };
    }
    russh::keys::check_known_hosts(server, port, key).map_err(|_| SshError::Host(format!("SFTP 服务器密钥与 known_hosts 不一致，或 known_hosts 无法读取；当前指纹 {actual}。请核实服务器身份。")))
}
impl client::Handler for VerifyHost {
    type Error = SshError;
    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> std::result::Result<bool, Self::Error> {
        let key = key.public_key();
        *self.captured.lock().unwrap() = Some(key.clone());
        // Probes finish the key exchange only; they never send credentials.
        if self.probe {
            return Ok(true);
        }
        if trusted(&self.server, self.port, self.expected.as_deref(), &key)? {
            Ok(true)
        } else {
            Err(SshError::Host(format!(
                "请先确认 SFTP 服务器指纹：{}",
                key.fingerprint(HashAlg::Sha256)
            )))
        }
    }
}
async fn ssh_connect(
    config: &SftpConfig,
    probe: bool,
    captured: Arc<StdMutex<Option<PublicKey>>>,
) -> Result<client::Handle<VerifyHost>> {
    let settings = client::Config {
        keepalive_interval: Some(Duration::from_secs(30)),
        keepalive_max: 3,
        ..Default::default()
    };
    let handler = VerifyHost {
        server: config.server.clone(),
        port: config.port,
        expected: config.host_key.clone(),
        probe,
        captured,
    };
    tokio::time::timeout(
        Duration::from_secs(20),
        client::connect(
            Arc::new(settings),
            (config.server.as_str(), config.port),
            handler,
        ),
    )
    .await
    .map_err(|_| Error::new(ErrorKind::Unexpected, "SFTP 连接超时").set_temporary())?
    .map_err(|error| match error {
        SshError::Host(message) => Error::new(ErrorKind::PermissionDenied, message),
        SshError::Ssh(error) => transport(error),
    })
}
pub(crate) async fn check_host(profile: Profile) -> crate::Result<Option<SftpHostKey>> {
    let config = SftpConfig::from_profile(&profile)?;
    let captured = Arc::new(StdMutex::new(None));
    let ssh = ssh_connect(&config, true, captured.clone()).await?;
    let key = captured
        .lock()
        .unwrap()
        .clone()
        .ok_or_else(|| crate::Error::Config("SFTP 服务器未提供主机密钥".into()))?;
    let _ = ssh
        .disconnect(
            russh::Disconnect::ByApplication,
            "Host key probe completed",
            "",
        )
        .await;
    if trusted(
        &config.server,
        config.port,
        config.host_key.as_deref(),
        &key,
    )
    .map_err(|e| crate::Error::Config(e.to_string()))?
    {
        Ok(None)
    } else {
        Ok(Some(SftpHostKey {
            server: config.server,
            port: config.port,
            fingerprint: key.fingerprint(HashAlg::Sha256).to_string(),
        }))
    }
}
fn transport(error: impl std::fmt::Display) -> Error {
    Error::new(ErrorKind::Unexpected, format!("SFTP 连接失败：{error}")).set_temporary()
}
fn protocol(error: SftpError) -> Error {
    let kind = match &error {
        SftpError::Status(status) => match status.status_code {
            StatusCode::NoSuchFile => ErrorKind::NotFound,
            StatusCode::PermissionDenied => ErrorKind::PermissionDenied,
            StatusCode::OpUnsupported => ErrorKind::Unsupported,
            _ => ErrorKind::Unexpected,
        },
        _ => return transport(error),
    };
    Error::new(kind, format!("SFTP：{error}"))
}
struct Connection {
    ssh: client::Handle<VerifyHost>,
    raw: RawSftpSession,
    posix_rename: bool,
    healthy: AtomicBool,
}
impl Connection {
    fn result<T>(&self, result: std::result::Result<T, SftpError>) -> Result<T> {
        result.map_err(|error| {
            if !matches!(error, SftpError::Status(_)) {
                self.healthy.store(false, Ordering::Relaxed);
            }
            protocol(error)
        })
    }
    async fn rename(&self, from: String, to: String) -> Result<()> {
        if self.posix_rename {
            let mut data = Vec::new();
            for path in [&from, &to] {
                data.extend_from_slice(&(path.len() as u32).to_be_bytes());
                data.extend_from_slice(path.as_bytes());
            }
            match self.result(self.raw.extended("posix-rename@openssh.com", data).await)? {
                Packet::Status(status) if status.status_code == StatusCode::Ok => Ok(()),
                Packet::Status(status) => Err(protocol(SftpError::Status(status))),
                _ => Err(Error::new(ErrorKind::Unexpected, "SFTP rename 回复无效")),
            }
        } else {
            self.result(self.raw.rename(from, to).await)?;
            Ok(())
        }
    }
}
struct Core {
    config: SftpConfig,
    connection: Mutex<Option<Arc<Connection>>>,
}
impl Core {
    async fn connection(&self) -> Result<Arc<Connection>> {
        let mut slot = self.connection.lock().await;
        if let Some(connection) = slot.as_ref()
            && !connection.ssh.is_closed()
            && connection.healthy.load(Ordering::Relaxed)
        {
            return Ok(connection.clone());
        }
        *slot = None;
        let connection = tokio::time::timeout(Duration::from_secs(25), self.config.connect())
            .await
            .map_err(|_| {
                Error::new(ErrorKind::Unexpected, "SFTP 认证或子系统启动超时").set_temporary()
            })??;
        *slot = Some(connection.clone());
        Ok(connection)
    }
    fn mutation_path(&self, path: &str) -> Result<String> {
        if path.trim_matches('/').is_empty() {
            return Err(Error::new(
                ErrorKind::PermissionDenied,
                "不能删除或替换 SFTP 连接的根目录",
            ));
        }
        self.config.remote(path)
    }
}
struct RemoteHandle {
    connection: Arc<Connection>,
    handle: Option<String>,
    runtime: tokio::runtime::Handle,
    cleanup: Option<String>,
}
impl RemoteHandle {
    fn new(connection: Arc<Connection>, handle: String) -> Self {
        Self {
            connection,
            handle: Some(handle),
            runtime: tokio::runtime::Handle::current(),
            cleanup: None,
        }
    }
    // Keep OPEN/OPENDIR alive until their replies arrive. Dropping a caller
    // during a network round trip must not lose the server's new handle.
    async fn open(
        connection: Arc<Connection>,
        path: String,
        options: Option<(OpenFlags, FileAttributes)>,
        cleanup: Option<String>,
    ) -> Result<Self> {
        tokio::spawn(async move {
            let result = match options {
                Some((flags, attributes)) => connection.raw.open(path, flags, attributes).await,
                None => connection.raw.opendir(path).await,
            };
            let id = connection.result(result)?.handle;
            let mut handle = Self::new(connection, id);
            handle.cleanup = cleanup;
            Ok(handle)
        })
        .await
        .map_err(transport)?
    }
    fn id(&self) -> &str {
        self.handle.as_deref().expect("live SFTP handle")
    }
    async fn close(&mut self) -> Result<()> {
        if let Some(handle) = self.handle.as_ref() {
            self.connection
                .result(self.connection.raw.close(handle.clone()).await)?;
            self.handle = None;
        }
        Ok(())
    }
}
impl Drop for RemoteHandle {
    fn drop(&mut self) {
        let handle = self.handle.take();
        let cleanup = self.cleanup.take();
        if handle.is_some() || cleanup.is_some() {
            let connection = self.connection.clone();
            self.runtime.spawn(async move {
                if let Some(handle) = handle {
                    let _ = connection.raw.close(handle).await;
                }
                if let Some(path) = cleanup {
                    let _ = connection.raw.remove(path).await;
                }
            });
        }
    }
}

fn metadata(attributes: FileAttributes) -> Metadata {
    // Do not recurse through symbolic links; directory loops are possible.
    let mode = if attributes.is_dir() {
        EntryMode::DIR
    } else if attributes.is_regular() || attributes.is_symlink() {
        EntryMode::FILE
    } else {
        EntryMode::Unknown
    };
    let mut meta = Metadata::new(mode);
    if let Some(size) = attributes.size {
        meta.set_content_length(size);
    }
    if let Some(time) = attributes.mtime
        && let Ok(time) = jiff::Timestamp::from_second(i64::from(time))
    {
        meta.set_last_modified(time.into());
    }
    meta
}
impl Configurator for SftpConfig {
    type Builder = SftpBuilder;
    fn into_builder(self) -> Self::Builder {
        SftpBuilder(self)
    }
}
#[derive(Default)]
pub(crate) struct SftpBuilder(pub SftpConfig);
impl Builder for SftpBuilder {
    type Config = SftpConfig;
    fn build(self) -> Result<impl Service> {
        Ok(SftpBackend(Arc::new(Core {
            config: self.0,
            connection: Mutex::new(None),
        })))
    }
}
struct SftpBackend(Arc<Core>);
impl std::fmt::Debug for SftpBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SftpBackend")
    }
}
impl Service for SftpBackend {
    type Reader = oio::PositionReader<SftpReader>;
    type Writer = SftpWriter;
    type Lister = SftpLister;
    type Deleter = oio::OneShotDeleter<SftpDeleter>;
    type Copier = ();
    fn info(&self) -> ServiceInfo {
        ServiceInfo::new("sftp", &self.0.config.root, &self.0.config.server)
    }
    fn capability(&self) -> Capability {
        Capability {
            stat: true,
            read: true,
            list: true,
            write: true,
            write_can_empty: true,
            write_can_multi: true,
            create_dir: true,
            delete: true,
            rename: true,
            shared: true,
            ..Default::default()
        }
    }
    fn copy(&self, _: &OperationContext, _: &str, _: &str, _: OpCopy, _: OpCopier) -> Result<()> {
        Err(Error::new(
            ErrorKind::Unsupported,
            "SFTP 通过流式传输复制文件",
        ))
    }
    async fn presign(&self, _: &OperationContext, _: &str, _: OpPresign) -> Result<RpPresign> {
        Err(Error::new(ErrorKind::Unsupported, "SFTP 不支持分享链接"))
    }
    async fn stat(&self, _: &OperationContext, path: &str, _: OpStat) -> Result<RpStat> {
        let connection = self.0.connection().await?;
        let attrs = connection
            .result(connection.raw.stat(self.0.config.remote(path)?).await)?
            .attrs;
        Ok(RpStat::new(metadata(attrs)))
    }
    fn read(&self, _: &OperationContext, path: &str, _: OpRead) -> Result<Self::Reader> {
        Ok(oio::PositionReader::new(SftpReader {
            core: self.0.clone(),
            path: self.0.config.remote(path)?,
        }))
    }
    fn list(&self, _: &OperationContext, path: &str, _: OpList) -> Result<Self::Lister> {
        Ok(SftpLister {
            core: self.0.clone(),
            path: path.trim_matches('/').into(),
            handle: None,
            entries: VecDeque::new(),
            done: false,
        })
    }
    fn write(&self, _: &OperationContext, path: &str, _: OpWrite) -> Result<Self::Writer> {
        let target = self.0.mutation_path(path)?;
        let parent = target
            .rsplit_once('/')
            .map(|(parent, _)| parent)
            .unwrap_or("");
        let temporary = format!("{parent}/.roam-{}.roampart", uuid::Uuid::new_v4());
        Ok(SftpWriter {
            core: self.0.clone(),
            target,
            temporary,
            handle: None,
            offset: 0,
            committed: false,
        })
    }
    fn delete(&self, _: &OperationContext) -> Result<Self::Deleter> {
        Ok(oio::OneShotDeleter::new(SftpDeleter(self.0.clone())))
    }
    async fn create_dir(
        &self,
        _: &OperationContext,
        path: &str,
        _: OpCreateDir,
    ) -> Result<RpCreateDir> {
        let connection = self.0.connection().await?;
        self.0.config.remote(path)?;
        let mut current = String::new();
        for part in path
            .trim_matches('/')
            .split('/')
            .filter(|part| !part.is_empty())
        {
            current.push('/');
            current.push_str(part);
            let remote = self.0.config.remote(&current)?;
            match connection.result(connection.raw.stat(&remote).await) {
                Ok(attrs) if attrs.attrs.is_dir() => {}
                Ok(_) => {
                    return Err(Error::new(
                        ErrorKind::NotADirectory,
                        "SFTP 路径已存在且不是目录",
                    ));
                }
                Err(error) if error.kind() == ErrorKind::NotFound => {
                    if let Err(error) = connection.result(
                        connection
                            .raw
                            .mkdir(
                                &remote,
                                FileAttributes {
                                    permissions: Some(0o755),
                                    ..Default::default()
                                },
                            )
                            .await,
                    ) {
                        let attrs = connection.result(connection.raw.stat(&remote).await)?;
                        if !attrs.attrs.is_dir() {
                            return Err(error);
                        }
                    }
                }
                Err(error) => return Err(error),
            }
        }
        Ok(RpCreateDir::default())
    }
    async fn rename(
        &self,
        _: &OperationContext,
        from: &str,
        to: &str,
        _: OpRename,
    ) -> Result<RpRename> {
        let connection = self.0.connection().await?;
        connection
            .rename(self.0.mutation_path(from)?, self.0.mutation_path(to)?)
            .await?;
        Ok(RpRename::default())
    }
}
struct SftpReader {
    core: Arc<Core>,
    path: String,
}
impl oio::PositionRead for SftpReader {
    type Handle = RemoteHandle;
    async fn open(&self) -> Result<Self::Handle> {
        let connection = self.core.connection().await?;
        RemoteHandle::open(
            connection,
            self.path.clone(),
            Some((OpenFlags::READ, FileAttributes::default())),
            None,
        )
        .await
    }
    async fn read_at(reader: &Self::Handle, offset: u64, size: usize) -> Result<Buffer> {
        match reader
            .connection
            .raw
            .read(reader.id(), offset, size.min(BLOCK) as u32)
            .await
        {
            Ok(data) => Ok(data.data.into()),
            Err(SftpError::Status(status)) if status.status_code == StatusCode::Eof => {
                Ok(Buffer::new())
            }
            Err(error) => reader.connection.result(Err(error)),
        }
    }
}
struct SftpLister {
    core: Arc<Core>,
    path: String,
    handle: Option<RemoteHandle>,
    entries: VecDeque<oio::Entry>,
    done: bool,
}
impl oio::List for SftpLister {
    async fn next(&mut self) -> Result<Option<oio::Entry>> {
        loop {
            if let Some(entry) = self.entries.pop_front() {
                return Ok(Some(entry));
            }
            if self.done {
                return Ok(None);
            }
            if self.handle.is_none() {
                let connection = self.core.connection().await?;
                self.handle = Some(
                    RemoteHandle::open(
                        connection,
                        self.core.config.remote(&self.path)?,
                        None,
                        None,
                    )
                    .await?,
                );
            }
            let handle = self.handle.as_mut().unwrap();
            match handle.connection.raw.readdir(handle.id()).await {
                Ok(page) => {
                    if page.files.is_empty() {
                        return Err(Error::new(ErrorKind::Unexpected, "SFTP 目录分页没有进展"));
                    }
                    for file in page.files {
                        if file.filename == "." || file.filename == ".." {
                            continue;
                        }
                        if file.filename.is_empty() || file.filename.contains(['/', '\0']) {
                            return Err(Error::new(ErrorKind::Unexpected, "SFTP 目录项名称无效"));
                        }
                        let meta = metadata(file.attrs);
                        let path = format!(
                            "{}{}/{}{}",
                            if self.path.is_empty() { "" } else { "/" },
                            self.path,
                            file.filename,
                            if meta.is_dir() { "/" } else { "" }
                        );
                        self.entries
                            .push_back(oio::Entry::new(path.trim_start_matches('/'), meta));
                    }
                }
                Err(SftpError::Status(status)) if status.status_code == StatusCode::Eof => {
                    handle.close().await?;
                    self.handle = None;
                    self.done = true;
                }
                Err(error) => return handle.connection.result(Err(error)),
            }
        }
    }
}
struct SftpDeleter(Arc<Core>);
impl oio::OneShotDelete for SftpDeleter {
    async fn delete_once(&self, path: String, _: OpDelete) -> Result<()> {
        let remote = self.0.mutation_path(&path)?;
        let connection = self.0.connection().await?;
        let attrs = match connection.result(connection.raw.lstat(&remote).await) {
            Ok(attrs) => attrs.attrs,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        let result = if attrs.is_dir() {
            connection.raw.rmdir(remote).await
        } else {
            connection.raw.remove(remote).await
        };
        match connection.result(result) {
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            result => result.map(|_| ()),
        }
    }
}
struct SftpWriter {
    core: Arc<Core>,
    target: String,
    temporary: String,
    handle: Option<RemoteHandle>,
    offset: u64,
    committed: bool,
}
impl SftpWriter {
    async fn initialize(&mut self) -> Result<()> {
        if self.handle.is_none() {
            let connection = self.core.connection().await?;
            self.handle = Some(
                RemoteHandle::open(
                    connection,
                    self.temporary.clone(),
                    Some((
                        OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::EXCLUDE,
                        FileAttributes {
                            permissions: Some(0o600),
                            ..Default::default()
                        },
                    )),
                    Some(self.temporary.clone()),
                )
                .await?,
            );
        }
        Ok(())
    }
}
impl oio::Write for SftpWriter {
    async fn write(&mut self, buffer: Buffer) -> Result<()> {
        self.initialize().await?;
        let handle = self.handle.as_ref().unwrap();
        let bytes = buffer.to_bytes();
        for (index, block) in bytes.chunks(BLOCK).enumerate() {
            handle.connection.result(
                handle
                    .connection
                    .raw
                    .write(
                        handle.id(),
                        self.offset + (index * BLOCK) as u64,
                        block.to_vec(),
                    )
                    .await,
            )?;
        }
        self.offset += bytes.len() as u64;
        Ok(())
    }
    async fn close(&mut self) -> Result<Metadata> {
        if !self.committed {
            self.initialize().await?;
            let handle = self.handle.as_mut().unwrap();
            handle.close().await?;
            handle
                .connection
                .rename(self.temporary.clone(), self.target.clone())
                .await?;
            handle.cleanup = None;
            self.committed = true;
        }
        let mut meta = Metadata::new(EntryMode::FILE);
        meta.set_content_length(self.offset);
        Ok(meta)
    }
    async fn abort(&mut self) -> Result<()> {
        if self.committed {
            return Ok(());
        }
        if let Some(handle) = &mut self.handle {
            handle.close().await?;
            match handle
                .connection
                .result(handle.connection.raw.remove(&self.temporary).await)
            {
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                result => {
                    result?;
                }
            }
            handle.cleanup = None;
        }
        self.handle = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
