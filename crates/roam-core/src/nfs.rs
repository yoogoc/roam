//! NFSv3 adapter for the same OpenDAL operator used by the other backends.
//! RPC connections are leased exclusively and discarded on cancellation/error:
//! a partially consumed RPC reply must never be reused by another operation.

use crate::Profile;
use nfs3_client::{
    Nfs3Connection, Nfs3ConnectionBuilder,
    nfs3_types::{
        nfs3::*,
        rpc::{auth_unix, opaque_auth},
        xdr_codec::Opaque,
    },
    tokio::{TokioConnector, TokioIo},
};
use opendal::{raw::*, *};
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    ops::{Deref, DerefMut},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::{
    net::TcpStream,
    sync::{Mutex, MutexGuard},
};

const BLOCK: usize = 64 * 1024;
type Connection = Nfs3Connection<TokioIo<TcpStream>>;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct NfsConfig {
    server: String,
    export: String,
    uid: u32,
    gid: u32,
    nfs_port: Option<u16>,
    mount_port: Option<u16>,
}

impl NfsConfig {
    pub(crate) fn from_profile(profile: &Profile) -> crate::Result<Self> {
        let invalid = |msg: String| crate::Error::Config(format!("NFS: {msg}"));
        let value = |key: &str| profile.options.get(key).map(|v| v.trim()).unwrap_or("");
        let raw_server = value("server");
        let server = raw_server
            .strip_prefix('[')
            .and_then(|s| s.strip_suffix(']'))
            .unwrap_or(raw_server);
        if server.is_empty()
            || server.contains(['/', '\\', '@'])
            || server.chars().any(char::is_whitespace)
            || (server.parse::<std::net::IpAddr>().is_err()
                && server.contains([':', '[', ']', '?', '#']))
        {
            return Err(invalid(
                "请填写服务器主机名或 IP 地址，不含协议或路径".into(),
            ));
        }
        let export = value("export");
        if !export.starts_with('/') || export.contains('\0') || export.split('/').any(|p| p == "..")
        {
            return Err(invalid("共享路径必须是绝对路径，且不能包含 ..".into()));
        }
        let number = |key: &str, default: u32| -> crate::Result<u32> {
            if value(key).is_empty() {
                return Ok(default);
            }
            value(key)
                .parse()
                .map_err(|_| invalid(format!("{key} 必须为 0 到 4294967295 的整数")))
        };
        let port = |key: &str| -> crate::Result<Option<u16>> {
            if value(key).is_empty() {
                return Ok(None);
            }
            let n = value(key)
                .parse::<u16>()
                .ok()
                .filter(|n| *n > 0)
                .ok_or_else(|| invalid(format!("{key} 必须为 1 到 65535 的整数")))?;
            Ok(Some(n))
        };
        Ok(Self {
            server: server.into(),
            export: export.into(),
            uid: number("uid", 65534)?,
            gid: number("gid", 65534)?,
            nfs_port: port("nfs_port")?,
            mount_port: port("mount_port")?,
        })
    }

    async fn connect(&self) -> Result<Connection> {
        let address =
            tokio::net::lookup_host((self.server.as_str(), self.nfs_port.unwrap_or(2049)))
                .await
                .map_err(transport)?
                .next()
                .ok_or_else(|| Error::new(ErrorKind::ConfigInvalid, "NFS server has no address"))?;
        let auth = auth_unix {
            machinename: Opaque::owned(b"roam".to_vec()),
            uid: self.uid,
            gid: self.gid,
            ..Default::default()
        };
        let mut builder =
            Nfs3ConnectionBuilder::new(TokioConnector, address.ip().to_string(), &self.export)
                .connect_from_privileged_port(false)
                .credential(opaque_auth::auth_unix(&auth));
        if let Some(port) = self.nfs_port {
            builder = builder.nfs3_port(port);
        }
        if let Some(port) = self.mount_port {
            builder = builder.mount_port(port);
        }
        builder.mount().await.map_err(transport)
    }
}

impl Configurator for NfsConfig {
    type Builder = NfsBuilder;
    fn into_builder(self) -> Self::Builder {
        NfsBuilder(self)
    }
}
#[derive(Default)]
pub(crate) struct NfsBuilder(pub NfsConfig);
impl Builder for NfsBuilder {
    type Config = NfsConfig;
    fn build(self) -> Result<impl Service> {
        Ok(NfsBackend(Arc::new(Core {
            config: self.0,
            connection: Mutex::new(None),
        })))
    }
}

struct Core {
    config: NfsConfig,
    connection: Mutex<Option<Connection>>,
}
struct Lease<'a> {
    slot: MutexGuard<'a, Option<Connection>>,
    connection: Option<Connection>,
    reusable: bool,
}
impl Deref for Lease<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        self.connection.as_ref().expect("live lease")
    }
}
impl DerefMut for Lease<'_> {
    fn deref_mut(&mut self) -> &mut Connection {
        self.connection.as_mut().expect("live lease")
    }
}
impl Drop for Lease<'_> {
    fn drop(&mut self) {
        if self.reusable {
            *self.slot = self.connection.take();
        }
    }
}
impl Core {
    async fn lease(&self) -> Result<Lease<'_>> {
        let mut slot = self.connection.lock().await;
        let connection = match slot.take() {
            Some(c) => c,
            None => self.config.connect().await?,
        };
        Ok(Lease {
            slot,
            connection: Some(connection),
            reusable: false,
        })
    }
}
fn transport(error: impl std::error::Error + Send + Sync + 'static) -> Error {
    Error::new(ErrorKind::Unexpected, "NFS connection or RPC failed").set_source(error)
}
fn result<T, E>(res: Nfs3Result<T, E>) -> Result<T> {
    match res {
        Nfs3Result::Ok(v) => Ok(v),
        Nfs3Result::Err((status, _)) => {
            let kind = match status {
                nfsstat3::NFS3ERR_NOENT => ErrorKind::NotFound,
                nfsstat3::NFS3ERR_EXIST => ErrorKind::AlreadyExists,
                nfsstat3::NFS3ERR_PERM | nfsstat3::NFS3ERR_ACCES | nfsstat3::NFS3ERR_ROFS => {
                    ErrorKind::PermissionDenied
                }
                nfsstat3::NFS3ERR_NOTDIR => ErrorKind::NotADirectory,
                nfsstat3::NFS3ERR_ISDIR => ErrorKind::IsADirectory,
                nfsstat3::NFS3ERR_NOTEMPTY => ErrorKind::Unexpected,
                nfsstat3::NFS3ERR_NOTSUPP => ErrorKind::Unsupported,
                _ => ErrorKind::Unexpected,
            };
            Err(Error::new(kind, format!("NFS: {status}")))
        }
    }
}
fn parts(path: &str) -> Result<Vec<&str>> {
    let parts: Vec<_> = path
        .split('/')
        .filter(|p| !p.is_empty() && *p != ".")
        .collect();
    if parts.iter().any(|p| *p == ".." || p.contains('\0')) {
        return Err(Error::new(
            ErrorKind::PermissionDenied,
            "path escapes NFS export",
        ));
    }
    Ok(parts)
}
fn dirarg<'a>(dir: nfs_fh3, name: &'a str) -> diropargs3<'a> {
    diropargs3 {
        dir,
        name: name.as_bytes().into(),
    }
}
async fn lookup(c: &mut Connection, path: &str) -> Result<nfs_fh3> {
    let mut handle = c.root_nfs_fh3();
    for name in parts(path)? {
        let found = result(
            c.lookup(&LOOKUP3args {
                what: dirarg(handle, name),
            })
            .await
            .map_err(transport)?,
        )?;
        let attr = attributes(c, found.object.clone()).await?;
        // Following remote symlinks can leave the export selected by the user.
        if attr.type_ == ftype3::NF3LNK {
            return Err(Error::new(
                ErrorKind::Unsupported,
                "NFS symbolic links are not followed",
            ));
        }
        handle = found.object;
    }
    Ok(handle)
}
async fn attributes(c: &mut Connection, object: nfs_fh3) -> Result<fattr3> {
    Ok(result(
        c.getattr(&GETATTR3args { object })
            .await
            .map_err(transport)?,
    )?
    .obj_attributes)
}
async fn parent(c: &mut Connection, path: &str) -> Result<(nfs_fh3, String)> {
    let mut components = parts(path)?;
    let name = components
        .pop()
        .ok_or_else(|| Error::new(ErrorKind::PermissionDenied, "cannot mutate NFS export root"))?
        .to_string();
    Ok((lookup(c, &components.join("/")).await?, name))
}
fn metadata(a: fattr3) -> Result<Metadata> {
    let mode = match a.type_ {
        ftype3::NF3DIR => EntryMode::DIR,
        ftype3::NF3REG => EntryMode::FILE,
        _ => EntryMode::Unknown,
    };
    let mut meta = Metadata::new(mode);
    meta.set_content_length(a.size);
    let time = jiff::Timestamp::new(i64::from(a.mtime.seconds), a.mtime.nseconds as i32)
        .map_err(|e| Error::new(ErrorKind::Unexpected, "invalid NFS timestamp").set_source(e))?;
    meta.set_last_modified(time.into());
    Ok(meta)
}
async fn rename(c: &mut Connection, from: &str, to: &str) -> Result<()> {
    let (from_dir, from_name) = parent(c, from).await?;
    let (to_dir, to_name) = parent(c, to).await?;
    result(
        c.rename(&RENAME3args {
            from: dirarg(from_dir, &from_name),
            to: dirarg(to_dir, &to_name),
        })
        .await
        .map_err(transport)?,
    )?;
    Ok(())
}

struct NfsBackend(Arc<Core>);
impl std::fmt::Debug for NfsBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("NfsBackend")
    }
}
impl Service for NfsBackend {
    type Reader = oio::PositionReader<NfsReader>;
    type Writer = NfsWriter;
    type Lister = NfsLister;
    type Deleter = oio::OneShotDeleter<NfsDeleter>;
    type Copier = ();
    fn copy(
        &self,
        _: &OperationContext,
        _: &str,
        _: &str,
        _: OpCopy,
        _: OpCopier,
    ) -> Result<Self::Copier> {
        Err(Error::new(
            ErrorKind::Unsupported,
            "NFSv3 has no server-side copy",
        ))
    }
    async fn presign(&self, _: &OperationContext, _: &str, _: OpPresign) -> Result<RpPresign> {
        Err(Error::new(
            ErrorKind::Unsupported,
            "NFSv3 has no share links",
        ))
    }
    fn info(&self) -> ServiceInfo {
        ServiceInfo::new("nfs", &self.0.config.export, &self.0.config.server)
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
    async fn stat(&self, _: &OperationContext, path: &str, _: OpStat) -> Result<RpStat> {
        let mut c = self.0.lease().await?;
        let handle = lookup(&mut c, path).await?;
        let meta = metadata(attributes(&mut c, handle).await?)?;
        c.reusable = true;
        Ok(RpStat::new(meta))
    }
    fn read(&self, _: &OperationContext, path: &str, _: OpRead) -> Result<Self::Reader> {
        Ok(oio::PositionReader::new(NfsReader {
            core: self.0.clone(),
            path: path.into(),
        }))
    }
    fn list(&self, _: &OperationContext, path: &str, _: OpList) -> Result<Self::Lister> {
        Ok(NfsLister {
            core: self.0.clone(),
            path: path.into(),
            cookie: 0,
            verifier: cookieverf3::default(),
            eof: false,
            entries: VecDeque::new(),
        })
    }
    fn write(&self, _: &OperationContext, path: &str, _: OpWrite) -> Result<Self::Writer> {
        static SERIAL: AtomicU64 = AtomicU64::new(0);
        let (parent, _) = path.rsplit_once('/').unwrap_or(("", path));
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let temporary = format!(
            "{parent}/.roam-{stamp}-{}-{}.roampart",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        );
        Ok(NfsWriter {
            core: self.0.clone(),
            target: path.into(),
            temporary,
            offset: 0,
            initialized: false,
            closed: false,
        })
    }
    fn delete(&self, _: &OperationContext) -> Result<Self::Deleter> {
        Ok(oio::OneShotDeleter::new(NfsDeleter(self.0.clone())))
    }
    async fn create_dir(
        &self,
        _: &OperationContext,
        path: &str,
        _: OpCreateDir,
    ) -> Result<RpCreateDir> {
        let mut c = self.0.lease().await?;
        let mut dir = c.root_nfs_fh3();
        for name in parts(path)? {
            let found = result(
                c.lookup(&LOOKUP3args {
                    what: dirarg(dir.clone(), name),
                })
                .await
                .map_err(transport)?,
            );
            let handle = match found {
                Ok(found) => found.object,
                Err(e) if e.kind() == ErrorKind::NotFound => {
                    let res = result(
                        c.mkdir(&MKDIR3args {
                            where_: dirarg(dir.clone(), name),
                            attributes: sattr3 {
                                mode: Nfs3Option::Some(0o755),
                                ..Default::default()
                            },
                        })
                        .await
                        .map_err(transport)?,
                    );
                    if let Err(e) = res
                        && e.kind() != ErrorKind::AlreadyExists
                    {
                        return Err(e);
                    }
                    result(
                        c.lookup(&LOOKUP3args {
                            what: dirarg(dir, name),
                        })
                        .await
                        .map_err(transport)?,
                    )?
                    .object
                }
                Err(e) => return Err(e),
            };
            if attributes(&mut c, handle.clone()).await?.type_ != ftype3::NF3DIR {
                return Err(Error::new(
                    ErrorKind::NotADirectory,
                    "NFS path is not a directory",
                ));
            }
            dir = handle;
        }
        c.reusable = true;
        Ok(RpCreateDir::default())
    }
    async fn rename(
        &self,
        _: &OperationContext,
        from: &str,
        to: &str,
        _: OpRename,
    ) -> Result<RpRename> {
        let mut c = self.0.lease().await?;
        rename(&mut c, from, to).await?;
        c.reusable = true;
        Ok(RpRename::default())
    }
}

struct NfsReader {
    core: Arc<Core>,
    path: String,
}
impl oio::PositionRead for NfsReader {
    type Handle = Self;
    async fn open(&self) -> Result<Self> {
        Ok(Self {
            core: self.core.clone(),
            path: self.path.clone(),
        })
    }
    async fn read_at(reader: &Self, offset: u64, size: usize) -> Result<Buffer> {
        let mut c = reader.core.lease().await?;
        let file = lookup(&mut c, &reader.path).await?;
        let data = result(
            c.read(&READ3args {
                file,
                offset,
                count: size.min(BLOCK) as u32,
            })
            .await
            .map_err(transport)?,
        )?;
        if data.data.as_ref().len() != data.count as usize {
            return Err(Error::new(ErrorKind::Unexpected, "invalid NFS read length"));
        }
        let bytes = data.data.as_ref().to_vec();
        c.reusable = true;
        Ok(bytes.into())
    }
}

struct NfsLister {
    core: Arc<Core>,
    path: String,
    cookie: u64,
    verifier: cookieverf3,
    eof: bool,
    entries: VecDeque<oio::Entry>,
}
impl oio::List for NfsLister {
    async fn next(&mut self) -> Result<Option<oio::Entry>> {
        while self.entries.is_empty() && !self.eof {
            let mut c = self.core.lease().await?;
            let dir = lookup(&mut c, &self.path).await?;
            let res = result(
                c.readdirplus(&READDIRPLUS3args {
                    dir: dir.clone(),
                    cookie: self.cookie,
                    cookieverf: self.verifier,
                    dircount: 16 * 1024,
                    maxcount: BLOCK as u32,
                })
                .await
                .map_err(transport)?,
            )?;
            let mut entries = VecDeque::new();
            let mut cookie = self.cookie;
            for e in res.reply.entries.into_inner() {
                cookie = e.cookie;
                let name = std::str::from_utf8(e.name.as_ref())
                    .map_err(|_| Error::new(ErrorKind::Unsupported, "NFS filename is not UTF-8"))?;
                if name == "." || name == ".." {
                    continue;
                }
                if name.contains(['/', '\0']) {
                    return Err(Error::new(
                        ErrorKind::Unexpected,
                        "invalid NFS directory entry",
                    ));
                }
                let attr = match e.name_attributes {
                    Nfs3Option::Some(a) => a,
                    Nfs3Option::None => {
                        let h = result(
                            c.lookup(&LOOKUP3args {
                                what: dirarg(dir.clone(), name),
                            })
                            .await
                            .map_err(transport)?,
                        )?
                        .object;
                        attributes(&mut c, h).await?
                    }
                };
                let is_dir = attr.type_ == ftype3::NF3DIR;
                let path = format!(
                    "{}{name}{}",
                    if self.path == "/" {
                        "".to_string()
                    } else {
                        self.path.clone()
                    },
                    if is_dir { "/" } else { "" }
                );
                entries.push_back(oio::Entry::new(&path, metadata(attr)?));
            }
            if !res.reply.eof && cookie == self.cookie {
                return Err(Error::new(
                    ErrorKind::Unexpected,
                    "NFS listing made no progress",
                ));
            }
            self.cookie = cookie;
            self.verifier = res.cookieverf;
            self.eof = res.reply.eof;
            self.entries = entries;
            c.reusable = true;
        }
        Ok(self.entries.pop_front())
    }
}
struct NfsDeleter(Arc<Core>);
impl oio::OneShotDelete for NfsDeleter {
    async fn delete_once(&self, path: String, _: OpDelete) -> Result<()> {
        let mut c = self.0.lease().await?;
        let (dir, name) = match parent(&mut c, &path).await {
            Ok(parent) => parent,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        let found = result(
            c.lookup(&LOOKUP3args {
                what: dirarg(dir.clone(), &name),
            })
            .await
            .map_err(transport)?,
        );
        match found {
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e),
            Ok(found) => {
                let attr = attributes(&mut c, found.object).await?;
                if attr.type_ == ftype3::NF3DIR {
                    result(
                        c.rmdir(&RMDIR3args {
                            object: dirarg(dir, &name),
                        })
                        .await
                        .map_err(transport)?,
                    )?;
                } else {
                    result(
                        c.remove(&REMOVE3args {
                            object: dirarg(dir, &name),
                        })
                        .await
                        .map_err(transport)?,
                    )?;
                }
            }
        }
        c.reusable = true;
        Ok(())
    }
}

struct NfsWriter {
    core: Arc<Core>,
    target: String,
    temporary: String,
    offset: u64,
    initialized: bool,
    closed: bool,
}
impl NfsWriter {
    async fn file(&mut self, c: &mut Connection) -> Result<nfs_fh3> {
        if !self.initialized {
            let (dir, name) = parent(c, &self.temporary).await?;
            result(
                c.create(&CREATE3args {
                    where_: dirarg(dir, &name),
                    how: createhow3::UNCHECKED(sattr3 {
                        mode: Nfs3Option::Some(0o644),
                        ..Default::default()
                    }),
                })
                .await
                .map_err(transport)?,
            )?;
            self.initialized = true;
        }
        lookup(c, &self.temporary).await
    }
}
impl oio::Write for NfsWriter {
    async fn write(&mut self, bs: Buffer) -> Result<()> {
        let core = self.core.clone();
        let mut c = core.lease().await?;
        let file = self.file(&mut c).await?;
        let data = bs.to_vec();
        let mut written = 0;
        while written < data.len() {
            let chunk = &data[written..data.len().min(written + BLOCK)];
            let res = result(
                c.write(&WRITE3args {
                    file: file.clone(),
                    offset: self.offset + written as u64,
                    count: chunk.len() as u32,
                    stable: stable_how::FILE_SYNC,
                    data: Opaque::borrowed(chunk),
                })
                .await
                .map_err(transport)?,
            )?;
            if res.count == 0 || res.count as usize > chunk.len() {
                return Err(Error::new(
                    ErrorKind::Unexpected,
                    "invalid NFS write length",
                ));
            }
            if res.committed != stable_how::FILE_SYNC {
                return Err(Error::new(
                    ErrorKind::Unsupported,
                    "NFS server did not honor FILE_SYNC",
                ));
            }
            written += res.count as usize;
        }
        self.offset += written as u64;
        c.reusable = true;
        Ok(())
    }
    async fn close(&mut self) -> Result<Metadata> {
        let core = self.core.clone();
        let mut c = core.lease().await?;
        if !self.closed {
            self.file(&mut c).await?;
            rename(&mut c, &self.temporary, &self.target).await?;
            self.closed = true;
        }
        c.reusable = true;
        Ok(Metadata::new(EntryMode::FILE).with_content_length(self.offset))
    }
    async fn abort(&mut self) -> Result<()> {
        if !self.closed {
            use oio::OneShotDelete;
            NfsDeleter(self.core.clone())
                .delete_once(self.temporary.clone(), OpDelete::default())
                .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nfs3_server::{
        memfs::MemFs,
        tcp::{NFSTcp, NFSTcpListener},
    };

    struct Fixture {
        op: Operator,
        profile: Profile,
        server: tokio::task::JoinHandle<()>,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.server.abort();
        }
    }
    async fn fixture() -> Fixture {
        fixture_with_fs(MemFs::default()).await
    }
    async fn fixture_with_fs(fs: MemFs) -> Fixture {
        let listener = NFSTcpListener::bind("127.0.0.1:0", fs).await.unwrap();
        let port = listener.get_listen_port().to_string();
        let server = tokio::spawn(async move {
            listener.handle_forever().await.unwrap();
        });
        let values = [
            ("server", "127.0.0.1"),
            ("export", "/"),
            ("uid", "1000"),
            ("gid", "1000"),
            ("nfs_port", &port),
            ("mount_port", &port),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let profile =
            crate::service::build_profile("nfs-test".into(), "NFS test".into(), "nfs", &values)
                .unwrap();
        let op = Operator::new(NfsBuilder(NfsConfig::from_profile(&profile).unwrap())).unwrap();
        Fixture {
            op,
            profile,
            server,
        }
    }
    #[tokio::test]
    async fn network_roundtrip_lists_reads_ranges_and_mutates() {
        let f = fixture().await;
        let op = &f.op;
        op.create_dir("deep/nested/").await.unwrap();
        let bytes: Vec<_> = (0..BLOCK * 3 + 17).map(|i| (i % 251) as u8).collect();
        op.write("deep/nested/中文.bin", bytes.clone())
            .await
            .unwrap();
        assert_eq!(
            op.stat("deep/nested/中文.bin")
                .await
                .unwrap()
                .content_length(),
            bytes.len() as u64
        );
        assert_eq!(
            op.read("deep/nested/中文.bin").await.unwrap().to_vec(),
            bytes
        );
        assert_eq!(
            op.read_with("deep/nested/中文.bin")
                .range(123..BLOCK as u64 + 22)
                .await
                .unwrap()
                .to_vec(),
            bytes[123..BLOCK + 22]
        );
        let entries = op.list("deep/nested/").await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path(), "deep/nested/中文.bin");
        op.rename("deep/nested/中文.bin", "deep/nested/moved.bin")
            .await
            .unwrap();
        assert_eq!(
            op.stat("deep/nested/中文.bin").await.unwrap_err().kind(),
            ErrorKind::NotFound
        );
        op.write("empty", Vec::<u8>::new()).await.unwrap();
        assert!(op.read("empty").await.unwrap().is_empty());
        op.delete("empty").await.unwrap();
        op.delete("empty").await.unwrap();
        op.delete("deep/nested/moved.bin").await.unwrap();
        op.delete("deep/nested/").await.unwrap();
        op.delete("deep/nested/missing").await.unwrap();
    }
    #[tokio::test]
    async fn aborted_upload_preserves_original_and_cleans_temporary_file() {
        let f = fixture().await;
        f.op.write("file", "original").await.unwrap();
        let mut writer = f.op.writer("file").await.unwrap();
        writer.write("replacement").await.unwrap();
        assert_eq!(f.op.read("file").await.unwrap().to_vec(), b"original");
        writer.abort().await.unwrap();
        assert_eq!(f.op.read("file").await.unwrap().to_vec(), b"original");
        let entries = f.op.list("/").await.unwrap();
        assert_eq!(entries.len(), 1);
        let mut writer = f.op.writer("file").await.unwrap();
        writer.write("short").await.unwrap();
        writer.close().await.unwrap();
        assert_eq!(f.op.read("file").await.unwrap().to_vec(), b"short");
    }
    #[tokio::test]
    async fn saved_profile_uses_nfs_in_the_existing_vfs() {
        let f = fixture().await;
        f.op.write("hello.txt", "hello").await.unwrap();
        let vfs = crate::Vfs::from_profile(crate::Rt::from_current().unwrap(), &f.profile).unwrap();
        let entries = vfs.list_all("").await.unwrap();
        assert_eq!(entries.len(), 1);
        vfs.create_dir("folder").await.unwrap();
        assert!(f.op.stat("folder/").await.unwrap().is_dir());
        let back = crate::service::field_values(&f.profile);
        assert_eq!(
            crate::service::build_profile(
                f.profile.id.clone(),
                f.profile.name.clone(),
                "nfs",
                &back
            )
            .unwrap(),
            f.profile
        );
    }
    #[test]
    fn rejects_invalid_connection_fields_before_network_io() {
        let mut p = Profile::new("nfs", "NFS", "nfs:///");
        p.options.insert("server".into(), "nas.local".into());
        p.options.insert("export".into(), "/share".into());
        assert!(p.validate().is_ok());
        for (key, value) in [
            ("server", ""),
            ("server", "nfs://nas/share"),
            ("server", "[]"),
            ("server", "nas.local:2049"),
            ("export", "relative"),
            ("export", "/a/../b"),
            ("uid", "-1"),
            ("gid", "4294967296"),
            ("nfs_port", "0"),
            ("mount_port", "65536"),
        ] {
            let mut invalid = p.clone();
            invalid.options.insert(key.into(), value.into());
            assert!(invalid.validate().is_err(), "accepted {key}={value}");
        }
        assert!(parts("a/../b").is_err());
        assert!(parts("a\0b").is_err());
    }

    #[tokio::test]
    async fn listing_paginates_without_skipping_or_duplicating_entries() {
        let mut config = nfs3_server::memfs::MemFsConfig::default();
        let expected: std::collections::BTreeSet<_> = (0..600)
            .map(|i| format!("file-{i:04}-{}", "x".repeat(100)))
            .collect();
        for name in &expected {
            config.add_file(&format!("/{name}"), b"data".to_vec());
        }
        let f = fixture_with_fs(MemFs::new(config).unwrap()).await;
        let entries = f.op.list("/").await.unwrap();
        assert_eq!(entries.len(), expected.len());
        assert_eq!(
            entries
                .into_iter()
                .map(|e| e.path().to_string())
                .collect::<std::collections::BTreeSet<_>>(),
            expected
        );
    }

    #[tokio::test]
    async fn vfs_streams_large_uploads_and_downloads() {
        let f = fixture().await;
        let vfs = crate::Vfs::from_profile(crate::Rt::from_current().unwrap(), &f.profile).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.bin");
        let destination = dir.path().join("download.bin");
        let data: Vec<_> = (0..crate::transfer::CHUNK + 173)
            .map(|i| (i % 251) as u8)
            .collect();
        tokio::fs::write(&source, &data).await.unwrap();
        vfs.upload_from(source, "large.bin", Arc::new(crate::TaskProgress::new()))
            .await
            .unwrap();
        vfs.download_to(
            "large.bin",
            destination.clone(),
            Arc::new(crate::TaskProgress::new()),
        )
        .await
        .unwrap();
        assert_eq!(tokio::fs::read(destination).await.unwrap(), data);
    }
}
