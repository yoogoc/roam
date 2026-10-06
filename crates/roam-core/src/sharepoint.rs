//! SharePoint Online document libraries over Microsoft Graph.
//! A dedicated OpenDAL service keeps Graph pagination, paths and OAuth refresh
//! independent of the OneDrive backend's personal-drive assumptions.

use crate::Profile;
use crate::sharepoint_auth::{CertificateCredential, SharePointAuthMethod};
use bytes::Buf;
use http::{Request, Response, StatusCode, header};
use opendal::raw::oio::ReadStream;
use opendal::{raw::*, *};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;

const GRAPH: &str = "https://graph.microsoft.com/v1.0";
const TOKEN_URL: &str = "https://login.microsoftonline.com/organizations/oauth2/v2.0/token";
const UPLOAD_CHUNK: usize = 320 * 1024 * 10;
fn value<'a>(profile: &'a Profile, key: &str) -> &'a str {
    profile.options.get(key).map(|v| v.trim()).unwrap_or("")
}

pub(crate) fn validate(profile: &Profile) -> crate::Result<()> {
    let invalid = |message: &str| crate::Error::Config(format!("SharePoint: {message}"));
    let drive = value(profile, "drive_id");
    if drive.is_empty()
        || !drive
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!-_".contains(&b))
    {
        return Err(invalid("请填写文档库 Drive ID，不能使用站点地址或分享链接"));
    }
    let root = profile
        .uri
        .strip_prefix("sharepoint:///")
        .ok_or_else(|| invalid("连接 URI 必须为 sharepoint:///目录"))?;
    if root.split('/').any(|part| part == ".." || part == ".")
        || root.contains(['\\', ':', '?', '#', '%'])
        || root.chars().any(char::is_control)
    {
        return Err(invalid("目录必须为文档库内的路径，不能包含 .. 或保留字符"));
    }

    let access = value(profile, "access_token");
    let method = SharePointAuthMethod::from_profile(profile)?;
    for key in [
        "access_token",
        "refresh_token",
        "certificate_path",
        "client_secret",
    ] {
        if !value(profile, key).is_empty() && !method.includes_field(key) {
            return Err(invalid("请只填写所选认证方式的凭据"));
        }
    }
    for field in crate::service::for_scheme("sharepoint").unwrap().fields {
        if method.field_required(field.key) && value(profile, field.key).is_empty() {
            return Err(invalid(&format!("请填写{}", field.label)));
        }
    }
    let tenant = value(profile, "tenant_id");
    if !tenant.is_empty()
        && (tenant.len() > 253
            || !tenant
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".-".contains(&b))
            || tenant.starts_with(['.', '-'])
            || tenant.ends_with(['.', '-'])
            || (matches!(
                method,
                SharePointAuthMethod::ClientSecret | SharePointAuthMethod::Certificate
            ) && ["common", "organizations", "consumers"]
                .iter()
                .any(|s| tenant.eq_ignore_ascii_case(s))))
    {
        return Err(invalid(
            "Tenant ID 必须为租户 ID 或域名，不能填写登录 URL 或通用租户",
        ));
    }
    if !access.is_empty() && header::HeaderValue::from_str(&format!("Bearer {access}")).is_err() {
        return Err(invalid("访问令牌包含无效字符"));
    }
    Ok(())
}

pub(crate) fn operator(profile: &Profile) -> crate::Result<Operator> {
    validate(profile)?;
    build_operator(profile, HttpTransporter::default()).map_err(Into::into)
}

fn build_operator(profile: &Profile, transport: HttpTransporter) -> Result<Operator> {
    let config = Config {
        root: crate::service::field_values(profile)
            .get("prefix")
            .cloned()
            .unwrap_or_default(),
        drive_id: value(profile, "drive_id").into(),
        access_token: value(profile, "access_token").into(),
        refresh_token: value(profile, "refresh_token").into(),
        client_id: value(profile, "client_id").into(),
        client_secret: value(profile, "client_secret").into(),
        tenant_id: value(profile, "tenant_id").into(),
        auth_method: SharePointAuthMethod::from_profile(profile)
            .map_err(|_| Error::new(ErrorKind::ConfigInvalid, "invalid auth method"))?
            .as_str()
            .into(),
        certificate_path: value(profile, "certificate_path").into(),
        certificate_password: profile
            .options
            .get("certificate_password")
            .cloned()
            .unwrap_or_default(),
    };
    Ok(Operator::new(GraphBuilder(config))?
        .with_context(OperationContext::new().with_http_transport(transport)))
}

#[derive(Default, Serialize, Deserialize)]
struct Config {
    root: String,
    drive_id: String,
    access_token: String,
    refresh_token: String,
    client_id: String,
    client_secret: String,
    tenant_id: String,
    auth_method: String,
    certificate_path: String,
    certificate_password: String,
}
impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharePointConfig")
            .field("root", &self.root)
            .field("drive_id", &self.drive_id)
            .finish_non_exhaustive()
    }
}
impl Configurator for Config {
    type Builder = GraphBuilder;
    fn into_builder(self) -> Self::Builder {
        GraphBuilder(self)
    }
}
#[derive(Default)]
struct GraphBuilder(Config);
impl Builder for GraphBuilder {
    type Config = Config;
    fn build(self) -> Result<impl Service> {
        let config = self.0;
        Ok(GraphBackend(Arc::new(Core {
            root: normalize_root(&config.root),
            drive_id: config.drive_id,
            client_id: config.client_id,
            client_secret: config.client_secret,
            token_url: if config.tenant_id.is_empty() {
                TOKEN_URL.into()
            } else {
                format!(
                    "https://login.microsoftonline.com/{}/oauth2/v2.0/token",
                    config.tenant_id
                )
            },
            certificate_path: config.certificate_path,
            certificate_password: config.certificate_password,
            app_auth: matches!(config.auth_method.as_str(), "client_secret" | "certificate"),
            auth: Mutex::new(Auth {
                access_token: config.access_token,
                refresh_token: config.refresh_token,
                expires: None,
                certificate: None,
            }),
        })))
    }
}
struct Auth {
    access_token: String,
    refresh_token: String,
    expires: Option<Instant>,
    certificate: Option<Arc<CertificateCredential>>,
}
struct Core {
    root: String,
    drive_id: String,
    client_id: String,
    client_secret: String,
    token_url: String,
    certificate_path: String,
    certificate_password: String,
    app_auth: bool,
    auth: Mutex<Auth>,
}

fn json_error(error: serde_json::Error) -> Error {
    new_json_deserialize_error(error)
}
fn parse_json(buffer: Buffer) -> Result<Value> {
    serde_json::from_reader(buffer.reader()).map_err(json_error)
}
fn string<'a>(json: &'a Value, key: &str) -> Result<&'a str> {
    json.get(key).and_then(Value::as_str).ok_or_else(|| {
        Error::new(
            ErrorKind::Unexpected,
            format!("SharePoint response is missing {key}"),
        )
    })
}
fn metadata(item: &Value) -> Result<Metadata> {
    let mode = if item.get("folder").is_some() {
        EntryMode::DIR
    } else if item.get("file").is_some() {
        EntryMode::FILE
    } else {
        return Err(Error::new(
            ErrorKind::Unsupported,
            "SharePoint item is not a file or folder",
        ));
    };
    let mut meta = Metadata::new(mode);
    if let Some(size) = item.get("size").and_then(Value::as_u64) {
        meta.set_content_length(size);
    }
    if let Some(etag) = item.get("eTag").and_then(Value::as_str) {
        meta.set_etag(etag);
    }
    if let Some(date) = item.get("lastModifiedDateTime").and_then(Value::as_str) {
        meta.set_last_modified(date.parse::<Timestamp>()?);
    }
    Ok(meta)
}
fn graph_error(status: StatusCode, body: Buffer) -> Error {
    let kind = match status {
        StatusCode::NOT_FOUND => ErrorKind::NotFound,
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => ErrorKind::PermissionDenied,
        StatusCode::CONFLICT => ErrorKind::AlreadyExists,
        StatusCode::PRECONDITION_FAILED => ErrorKind::ConditionNotMatch,
        StatusCode::RANGE_NOT_SATISFIABLE => ErrorKind::RangeNotSatisfied,
        _ => ErrorKind::Unexpected,
    };
    // Keep server bodies (which may echo credentials) out of user-facing errors.
    let code = parse_json(body)
        .ok()
        .and_then(|json| json["error"]["code"].as_str().map(str::to_owned));
    let error = Error::new(
        kind,
        format!(
            "SharePoint HTTP {status}: {}",
            code.as_deref().unwrap_or("request failed")
        ),
    );
    if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
        error.set_temporary()
    } else {
        error
    }
}
fn request(method: http::Method, url: &str, body: Buffer) -> Result<Request<Buffer>> {
    Request::builder()
        .method(method)
        .uri(url)
        .body(body)
        .map_err(new_request_build_error)
}
impl Core {
    fn item_url(&self, path: &str) -> String {
        let root = format!("{GRAPH}/drives/{}/root", self.drive_id);
        let absolute = build_rooted_abs_path(&self.root, path)
            .trim_end_matches('/')
            .to_string();
        if absolute.is_empty() {
            root
        } else {
            format!("{root}:{}", percent_encode_path(&absolute))
        }
    }
    fn action_url(&self, path: &str, action: &str) -> String {
        let item = self.item_url(path);
        if item.ends_with("/root") {
            format!("{item}/{action}")
        } else {
            format!("{item}:/{action}")
        }
    }
    async fn sign(&self, ctx: &OperationContext, req: &mut Request<Buffer>) -> Result<()> {
        let mut auth = self.auth.lock().await;
        if (self.app_auth || !auth.refresh_token.is_empty())
            && (auth.access_token.is_empty()
                || auth.expires.is_none_or(|expiry| Instant::now() >= expiry))
        {
            let mut pairs = vec![("client_id", self.client_id.clone())];
            if self.app_auth {
                pairs.push(("grant_type", "client_credentials".into()));
                pairs.push(("scope", "https://graph.microsoft.com/.default".into()));
                if self.certificate_path.is_empty() {
                    pairs.push(("client_secret", self.client_secret.clone()));
                } else {
                    let cached = auth.certificate.clone();
                    let path = self.certificate_path.clone();
                    let password = self.certificate_password.clone();
                    let client = self.client_id.clone();
                    let audience = self.token_url.clone();
                    let (certificate, assertion) = tokio::task::spawn_blocking(move || {
                        let certificate = match cached {
                            Some(certificate) => certificate,
                            None => Arc::new(CertificateCredential::load(&path, &password)?),
                        };
                        let assertion = certificate.assertion(&client, &audience)?;
                        Ok::<_, Error>((certificate, assertion))
                    })
                    .await
                    .map_err(|_| {
                        Error::new(ErrorKind::Unexpected, "SharePoint certificate task failed")
                    })??;
                    auth.certificate = Some(certificate);
                    pairs.push((
                        "client_assertion_type",
                        "urn:ietf:params:oauth:client-assertion-type:jwt-bearer".into(),
                    ));
                    pairs.push(("client_assertion", assertion));
                }
            } else {
                pairs.push(("grant_type", "refresh_token".into()));
                pairs.push((
                    "scope",
                    "offline_access https://graph.microsoft.com/.default".into(),
                ));
                pairs.push(("refresh_token", auth.refresh_token.clone()));
                if !self.client_secret.is_empty() {
                    pairs.push(("client_secret", self.client_secret.clone()));
                }
            }
            let body = url::form_urlencoded::Serializer::new(String::new())
                .extend_pairs(pairs)
                .finish();
            let mut req = request(http::Method::POST, &self.token_url, Buffer::from(body))?;
            req.headers_mut().insert(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("application/x-www-form-urlencoded"),
            );
            let response = ctx.http_transport().send(req).await?;
            if !response.status().is_success() {
                return Err(graph_error(response.status(), response.into_body()));
            }
            let json = parse_json(response.into_body())?;
            let token = string(&json, "access_token")?;
            if token.is_empty()
                || header::HeaderValue::from_str(&format!("Bearer {token}")).is_err()
            {
                return Err(Error::new(
                    ErrorKind::Unexpected,
                    "SharePoint returned an invalid access token",
                ));
            }
            auth.access_token = token.into();
            if !self.app_auth
                && let Some(refresh) = json.get("refresh_token").and_then(Value::as_str)
            {
                auth.refresh_token = refresh.into();
            }
            let expires = json
                .get("expires_in")
                .and_then(Value::as_u64)
                .unwrap_or(3600);
            auth.expires = Some(
                Instant::now()
                    .checked_add(Duration::from_secs(expires.saturating_sub(60)))
                    .ok_or_else(|| {
                        Error::new(
                            ErrorKind::Unexpected,
                            "SharePoint returned an invalid token lifetime",
                        )
                    })?,
            );
        }
        let header = header::HeaderValue::from_str(&format!("Bearer {}", auth.access_token))
            .map_err(|error| {
                Error::new(ErrorKind::ConfigInvalid, "invalid SharePoint access token")
                    .set_source(error)
            })?;
        req.headers_mut().insert(header::AUTHORIZATION, header);
        Ok(())
    }
    async fn send(
        &self,
        ctx: &OperationContext,
        mut req: Request<Buffer>,
    ) -> Result<Response<Buffer>> {
        self.sign(ctx, &mut req).await?;
        let response = ctx.http_transport().send(req).await?;
        if !response.status().is_success() {
            return Err(graph_error(response.status(), response.into_body()));
        }
        Ok(response)
    }
    async fn get(&self, ctx: &OperationContext, path: &str) -> Result<Value> {
        parse_json(
            self.send(
                ctx,
                request(http::Method::GET, &self.item_url(path), Buffer::new())?,
            )
            .await?
            .into_body(),
        )
    }
    async fn create_dirs(&self, ctx: &OperationContext, path: &str) -> Result<()> {
        let mut parent = String::new();
        for name in path.trim_matches('/').split('/').filter(|s| !s.is_empty()) {
            let body = Buffer::from(
                serde_json::to_vec(
                    &json!({"name":name,"folder":{},"@microsoft.graph.conflictBehavior":"fail"}),
                )
                .map_err(new_json_serialize_error)?,
            );
            let mut req = request(
                http::Method::POST,
                &self.action_url(if parent.is_empty() { "/" } else { &parent }, "children"),
                body,
            )?;
            req.headers_mut().insert(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("application/json"),
            );
            let result = self.send(ctx, req).await;
            parent.push_str(name);
            parent.push('/');
            match result {
                Ok(_) => {}
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                    if !metadata(&self.get(ctx, &parent).await?)?.is_dir() {
                        return Err(Error::new(
                            ErrorKind::NotADirectory,
                            "SharePoint path is not a folder",
                        ));
                    }
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
}
struct GraphBackend(Arc<Core>);
impl std::fmt::Debug for GraphBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SharePointBackend")
    }
}
impl Service for GraphBackend {
    type Reader = oio::StreamReader<GraphReader>;
    type Writer = oio::OneShotWriter<GraphWriter>;
    type Lister = oio::PageLister<GraphLister>;
    type Deleter = oio::OneShotDeleter<GraphDeleter>;
    type Copier = oio::OneShotCopier;
    fn info(&self) -> ServiceInfo {
        ServiceInfo::new("sharepoint", &self.0.root, &self.0.drive_id)
    }
    fn capability(&self) -> Capability {
        Capability {
            stat: true,
            read: true,
            read_with_if_match: true,
            read_with_if_none_match: true,
            write: true,
            write_can_empty: true,
            list: true,
            list_with_limit: true,
            create_dir: true,
            delete: true,
            rename: true,
            copy: true,
            shared: true,
            ..Default::default()
        }
    }
    async fn stat(&self, ctx: &OperationContext, path: &str, _: OpStat) -> Result<RpStat> {
        Ok(RpStat::new(metadata(&self.0.get(ctx, path).await?)?))
    }
    fn read(&self, ctx: &OperationContext, path: &str, args: OpRead) -> Result<Self::Reader> {
        Ok(oio::StreamReader::new(GraphReader {
            core: self.0.clone(),
            ctx: ctx.clone(),
            path: path.into(),
            args,
        }))
    }
    fn write(&self, ctx: &OperationContext, path: &str, _: OpWrite) -> Result<Self::Writer> {
        Ok(oio::OneShotWriter::new(GraphWriter {
            core: self.0.clone(),
            ctx: ctx.clone(),
            path: path.into(),
        }))
    }
    fn list(&self, ctx: &OperationContext, path: &str, args: OpList) -> Result<Self::Lister> {
        Ok(oio::PageLister::new(GraphLister {
            core: self.0.clone(),
            ctx: ctx.clone(),
            path: path.into(),
            limit: args.limit(),
        }))
    }
    fn delete(&self, ctx: &OperationContext) -> Result<Self::Deleter> {
        Ok(oio::OneShotDeleter::new(GraphDeleter {
            core: self.0.clone(),
            ctx: ctx.clone(),
        }))
    }
    async fn create_dir(
        &self,
        ctx: &OperationContext,
        path: &str,
        _: OpCreateDir,
    ) -> Result<RpCreateDir> {
        self.0.create_dirs(ctx, path).await?;
        Ok(RpCreateDir::default())
    }
    async fn rename(
        &self,
        ctx: &OperationContext,
        from: &str,
        to: &str,
        _: OpRename,
    ) -> Result<RpRename> {
        if from == to {
            return Ok(RpRename::default());
        }
        let parent = get_parent(to);
        let item = self.0.get(ctx, parent).await?;
        let body = json!({"name":get_basename(to).trim_end_matches('/'),"parentReference":{"driveId":self.0.drive_id,"id":string(&item,"id")?},"@microsoft.graph.conflictBehavior":"replace"});
        let mut req = request(
            http::Method::PATCH,
            &self.0.item_url(from),
            Buffer::from(serde_json::to_vec(&body).map_err(new_json_serialize_error)?),
        )?;
        req.headers_mut().insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("application/json"),
        );
        self.0.send(ctx, req).await?;
        Ok(RpRename::default())
    }
    fn copy(
        &self,
        ctx: &OperationContext,
        from: &str,
        to: &str,
        _: OpCopy,
        _: OpCopier,
    ) -> Result<Self::Copier> {
        let core = self.0.clone();
        let ctx = ctx.clone();
        let from = from.to_string();
        let to = to.to_string();
        Ok(oio::OneShotCopier::new(async move {
            let parent = core.get(&ctx, get_parent(&to)).await?;
            let body = json!({"name":get_basename(&to),"parentReference":{"driveId":core.drive_id,"id":string(&parent,"id")?}});
            let mut req = request(
                http::Method::POST,
                &format!(
                    "{}?@microsoft.graph.conflictBehavior=replace",
                    core.action_url(&from, "copy")
                ),
                Buffer::from(serde_json::to_vec(&body).map_err(new_json_serialize_error)?),
            )?;
            req.headers_mut().insert(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("application/json"),
            );
            let response = core.send(&ctx, req).await?;
            let monitor = parse_location(response.headers())?
                .ok_or_else(|| {
                    Error::new(
                        ErrorKind::Unexpected,
                        "SharePoint copy returned no monitor URL",
                    )
                })?
                .to_string();
            validate_download_url(&monitor)?;
            for _ in 0..120 {
                // Copy monitor links are preauthenticated; do not send a Graph token.
                let response = ctx
                    .http_transport()
                    .send(request(http::Method::GET, &monitor, Buffer::new())?)
                    .await?;
                if !response.status().is_success() {
                    return Err(graph_error(response.status(), response.into_body()));
                }
                let body = parse_json(response.into_body())?;
                match body["status"].as_str() {
                    Some("completed") => return Ok(Metadata::default()),
                    Some("failed" | "deleteFailed") => {
                        return Err(Error::new(ErrorKind::Unexpected, "SharePoint copy failed"));
                    }
                    _ => tokio::time::sleep(Duration::from_secs(1)).await,
                }
            }
            Err(Error::new(
                ErrorKind::Unexpected,
                "SharePoint copy timed out",
            ))
        }))
    }
    async fn presign(&self, _: &OperationContext, _: &str, _: OpPresign) -> Result<RpPresign> {
        Err(Error::new(
            ErrorKind::Unsupported,
            "SharePoint share links are not implemented",
        ))
    }
}
fn validate_download_url(url: &str) -> Result<()> {
    let uri: http::Uri = url.parse().map_err(new_http_uri_invalid_error)?;
    if uri.scheme_str() != Some("https") {
        return Err(Error::new(
            ErrorKind::Unexpected,
            "SharePoint returned a non-HTTPS URL",
        ));
    }
    Ok(())
}
struct GraphReader {
    core: Arc<Core>,
    ctx: OperationContext,
    path: String,
    args: OpRead,
}
impl oio::StreamRead for GraphReader {
    async fn open(&self, range: BytesRange) -> Result<(RpRead, Box<dyn oio::ReadStreamDyn>)> {
        let mut req = request(
            http::Method::GET,
            &self.core.action_url(&self.path, "content"),
            Buffer::new(),
        )?;
        req.headers_mut().insert(
            header::RANGE,
            range
                .to_header()
                .parse()
                .map_err(|e| Error::new(ErrorKind::Unexpected, "invalid range").set_source(e))?,
        );
        for (key, value) in [
            (header::IF_MATCH, self.args.if_match()),
            (header::IF_NONE_MATCH, self.args.if_none_match()),
        ] {
            if let Some(value) = value {
                req.headers_mut().insert(
                    key,
                    value.parse().map_err(|e| {
                        Error::new(ErrorKind::ConfigInvalid, "invalid ETag").set_source(e)
                    })?,
                );
            }
        }
        self.core.sign(&self.ctx, &mut req).await?;
        let response = self.ctx.http_transport().fetch(req).await?;
        if !response.status().is_success() {
            let (parts, mut body) = response.into_parts();
            return Err(graph_error(parts.status, body.read_all().await?));
        }
        let meta = parse_into_metadata(&self.path, response.headers())?;
        Ok((RpRead::new(meta), Box::new(response.into_body())))
    }
}
struct GraphLister {
    core: Arc<Core>,
    ctx: OperationContext,
    path: String,
    limit: Option<usize>,
}
impl oio::PageList for GraphLister {
    async fn next_page(&self, page: &mut oio::PageContext) -> Result<()> {
        let url = if page.token.is_empty() {
            let url = self.core.action_url(&self.path, "children");
            match self.limit {
                Some(limit) => format!("{url}?$top={limit}"),
                None => url,
            }
        } else {
            // A pagination response must not send credentials outside this drive.
            let prefix = format!("{GRAPH}/drives/{}/", self.core.drive_id);
            if !page.token.starts_with(&prefix) {
                return Err(Error::new(
                    ErrorKind::Unexpected,
                    "SharePoint pagination URL is outside the selected drive",
                ));
            }
            page.token.clone()
        };
        let body = parse_json(
            self.core
                .send(&self.ctx, request(http::Method::GET, &url, Buffer::new())?)
                .await?
                .into_body(),
        )?;
        let items = body
            .get("value")
            .and_then(Value::as_array)
            .ok_or_else(|| Error::new(ErrorKind::Unexpected, "invalid SharePoint listing"))?;
        for item in items {
            let name = string(item, "name")?;
            if name.is_empty() || name.contains(['/', '\\']) || name == "." || name == ".." {
                return Err(Error::new(
                    ErrorKind::Unexpected,
                    "invalid SharePoint filename",
                ));
            }
            let meta = metadata(item)?;
            let mut path = format!("{}{name}", self.path.trim_start_matches('/'));
            if meta.is_dir() {
                path.push('/');
            }
            page.entries.push_back(oio::Entry::new(&path, meta));
        }
        page.token = body
            .get("@odata.nextLink")
            .and_then(Value::as_str)
            .unwrap_or("")
            .into();
        page.done = page.token.is_empty();
        Ok(())
    }
}
struct GraphWriter {
    core: Arc<Core>,
    ctx: OperationContext,
    path: String,
}
impl oio::OneShotWrite for GraphWriter {
    async fn write_once(&self, bytes: Buffer) -> Result<Metadata> {
        if bytes.len() <= 4 * 1024 * 1024 {
            let mut req = request(
                http::Method::PUT,
                &self.core.action_url(&self.path, "content"),
                bytes,
            )?;
            req.headers_mut().insert(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("application/octet-stream"),
            );
            return metadata(&parse_json(
                self.core.send(&self.ctx, req).await?.into_body(),
            )?);
        }
        let mut req = request(
            http::Method::POST,
            &self.core.action_url(&self.path, "createUploadSession"),
            Buffer::from(
                serde_json::to_vec(
                    &json!({"item":{"@microsoft.graph.conflictBehavior":"replace"}}),
                )
                .map_err(new_json_serialize_error)?,
            ),
        )?;
        req.headers_mut().insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("application/json"),
        );
        let session = parse_json(self.core.send(&self.ctx, req).await?.into_body())?;
        let url = string(&session, "uploadUrl")?;
        validate_download_url(url)?;
        let total = bytes.len();
        let bytes = bytes.to_bytes();
        for (index, chunk) in bytes.chunks(UPLOAD_CHUNK).enumerate() {
            let start = index * UPLOAD_CHUNK;
            let end = start + chunk.len() - 1;
            let mut req = request(
                http::Method::PUT,
                url,
                Buffer::from(bytes::Bytes::copy_from_slice(chunk)),
            )?;
            req.headers_mut().insert(
                header::CONTENT_RANGE,
                format!("bytes {start}-{end}/{total}")
                    .parse()
                    .expect("numeric range"),
            );
            req.headers_mut()
                .insert(header::CONTENT_LENGTH, chunk.len().into());
            let response = self.ctx.http_transport().send(req).await?;
            if !response.status().is_success() {
                return Err(graph_error(response.status(), response.into_body()));
            }
            if end + 1 == total {
                return metadata(&parse_json(response.into_body())?);
            }
        }
        Err(Error::new(
            ErrorKind::Unexpected,
            "SharePoint upload did not complete",
        ))
    }
}
struct GraphDeleter {
    core: Arc<Core>,
    ctx: OperationContext,
}
impl oio::OneShotDelete for GraphDeleter {
    async fn delete_once(&self, path: String, _: OpDelete) -> Result<()> {
        let result = self
            .core
            .send(
                &self.ctx,
                request(
                    http::Method::DELETE,
                    &self.core.item_url(&path),
                    Buffer::new(),
                )?,
            )
            .await;
        match result {
            Ok(_) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{Method, StatusCode};
    use serde_json::json;
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };

    fn profile() -> Profile {
        crate::service::build_profile(
            "sp".into(),
            "Team".into(),
            "sharepoint",
            &[
                ("drive_id".to_string(), "b!library".to_string()),
                ("access_token".to_string(), "test-token".to_string()),
            ]
            .into(),
        )
        .unwrap()
    }

    #[test]
    fn profiles_round_trip_and_redact_both_kinds_of_tokens() {
        for refresh in [false, true] {
            let mut profile = profile();
            profile.uri = "sharepoint:///团队/Reports".into();
            if refresh {
                profile.options.remove("access_token");
                profile
                    .options
                    .insert("refresh_token".into(), "refresh-token".into());
                profile.options.insert("client_id".into(), "app-id".into());
                profile
                    .options
                    .insert("client_secret".into(), "app-secret".into());
            }
            profile.validate().unwrap();
            let fields = crate::service::field_values(&profile);
            assert_eq!(fields["prefix"], "团队/Reports");
            let rebuilt = crate::service::build_profile(
                profile.id.clone(),
                profile.name.clone(),
                "sharepoint",
                &fields,
            )
            .unwrap();
            assert_eq!(rebuilt, profile);
            assert_eq!(
                profile.secret_keys(),
                if refresh {
                    vec!["refresh_token", "client_secret"]
                } else {
                    vec!["access_token"]
                }
            );
            // This is the actual dispatch path used by the connection picker.
            let vfs = crate::Vfs::from_profile(crate::Rt::new().unwrap(), &profile).unwrap();
            assert!(vfs.capability().list && vfs.capability().read && vfs.capability().write);
            assert!(!vfs.capability().list_with_versions);
        }
    }

    #[test]
    fn invalid_profiles_are_rejected_before_network_io() {
        for (key, value) in [
            ("drive_id", ""),
            ("drive_id", "https://contoso.sharepoint.com/sites/team"),
            ("drive_id", "../other"),
            ("drive_id", "b!id?query"),
            ("access_token", ""),
            ("access_token", "token\nheader"),
            ("refresh_token", "both-token-types"),
        ] {
            let mut profile = profile();
            profile.options.insert(key.into(), value.into());
            assert!(profile.validate().is_err(), "{key} was accepted");
        }
        for uri in [
            "sharepoint://host/path",
            "sharepoint:///../other",
            "sharepoint:///path?query",
        ] {
            let mut profile = profile();
            profile.uri = uri.into();
            assert!(profile.validate().is_err());
        }
        let mut profile = profile();
        profile.options.remove("access_token");
        profile
            .options
            .insert("refresh_token".into(), "refresh".into());
        assert!(profile.validate().is_err(), "refresh requires a client ID");
    }

    struct Reply {
        method: Method,
        path: String,
        status: StatusCode,
        body: Vec<u8>,
        location: Option<String>,
    }

    impl Reply {
        fn json(method: Method, path: &str, status: StatusCode, body: Value) -> Self {
            Self {
                method,
                path: path.into(),
                status,
                body: serde_json::to_vec(&body).unwrap(),
                location: None,
            }
        }
    }

    #[derive(Clone)]
    struct Script {
        replies: Arc<Mutex<VecDeque<Reply>>>,
        requests: Arc<Mutex<Vec<Request<Buffer>>>>,
    }

    impl HttpTransport for Script {
        async fn fetch(&self, request: Request<Buffer>) -> Result<Response<HttpBody>> {
            let reply = self
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected request");
            assert_eq!(request.method(), reply.method);
            assert_eq!(request.uri().path(), reply.path);
            if request.uri().host() == Some("graph.microsoft.com") {
                assert_eq!(
                    request.headers()[header::AUTHORIZATION],
                    "Bearer test-token"
                );
            } else {
                assert!(!request.headers().contains_key(header::AUTHORIZATION));
            }
            self.requests.lock().unwrap().push(request);
            let parts = Response::builder()
                .status(reply.status)
                .header(header::CONTENT_TYPE, "application/json")
                .body(())
                .unwrap()
                .into_parts()
                .0;
            let len = reply.body.len() as u64;
            let mut parts = parts;
            if let Some(location) = reply.location {
                parts
                    .headers
                    .insert(header::LOCATION, location.parse().unwrap());
            }
            parts.headers.insert(header::CONTENT_LENGTH, len.into());
            Ok(Response::from_parts(
                parts,
                HttpBody::new(
                    futures::stream::iter(vec![Ok(Buffer::from(reply.body))]),
                    Some(len),
                ),
            ))
        }
    }

    fn setup(profile: &Profile, replies: Vec<Reply>) -> (Operator, Script) {
        let script = Script {
            replies: Arc::new(Mutex::new(replies.into())),
            requests: Arc::default(),
        };
        let operator = build_operator(profile, HttpTransporter::new(script.clone())).unwrap();
        (operator, script)
    }

    fn item(name: &str, parent: &str, folder: bool) -> Value {
        let mut item = json!({
            "id": "item-id", "name": name, "size": 5,
            "eTag": "\"etag\"", "lastModifiedDateTime": "2026-10-01T00:00:00Z",
            "parentReference": {"driveId": "b!library", "id": "parent", "path": parent}
        });
        item[if folder { "folder" } else { "file" }] = if folder {
            json!({"childCount": 0})
        } else {
            json!({"mimeType": "text/plain"})
        };
        item
    }

    #[tokio::test]
    async fn nested_paginated_listings_preserve_paths_and_metadata() {
        let mut profile = profile();
        profile.uri = "sharepoint:///Team Reports".into();
        let folder = "/v1.0/drives/b!library/root:/Team%20Reports";
        let children = format!("{folder}:/children");
        let next =
            format!("{GRAPH}/drives/b!library/root:/Team%20Reports:/children?$skiptoken=page2");
        let (op, script) = setup(
            &profile,
            vec![
                Reply::json(
                    Method::GET,
                    &children,
                    StatusCode::OK,
                    json!({"value": [item("docs", "/drives/b!library/root:/Team%20Reports", true)], "@odata.nextLink": next}),
                ),
                Reply::json(
                    Method::GET,
                    &children,
                    StatusCode::OK,
                    json!({"value": [item("报告.txt", "/drives/b!library/root:/Team%20Reports", false)]}),
                ),
            ],
        );
        let entries = op.list("/").await.unwrap();
        let paths: Vec<_> = entries.iter().map(|e| e.path()).collect();
        assert!(paths.contains(&"docs/"), "{paths:?}");
        assert!(paths.contains(&"报告.txt"), "{paths:?}");
        assert_eq!(
            entries
                .iter()
                .find(|e| e.path() == "报告.txt")
                .unwrap()
                .metadata()
                .content_length(),
            5
        );
        assert!(script.replies.lock().unwrap().is_empty());
        let requests = script.requests.lock().unwrap();
        assert_eq!(requests[1].uri().query(), Some("$skiptoken=page2"));
        assert_eq!(requests.len(), 2, "no automatic version HTTP calls");
    }

    #[tokio::test]
    async fn reads_writes_folders_renames_and_deletes_use_the_selected_library() {
        let root = "/v1.0/drives/b!library/root";
        let file = format!("{root}:/note.txt");
        let mut root_item = item("root", "", true);
        root_item["parentReference"] = json!({"driveId":"b!library"});
        let (op, script) = setup(
            &profile(),
            vec![
                Reply::json(
                    Method::GET,
                    &format!("{root}:/folder"),
                    StatusCode::OK,
                    root_item.clone(),
                ),
                Reply {
                    method: Method::GET,
                    path: format!("{file}:/content"),
                    status: StatusCode::OK,
                    body: b"hello".to_vec(),
                    location: None,
                },
                Reply::json(
                    Method::PUT,
                    &format!("{file}:/content"),
                    StatusCode::CREATED,
                    item("note.txt", "/drives/b!library/root:", false),
                ),
                Reply::json(
                    Method::POST,
                    &format!("{root}/children"),
                    StatusCode::CREATED,
                    item("new", "/drives/b!library/root:", true),
                ),
                Reply::json(Method::GET, root, StatusCode::OK, root_item),
                Reply::json(
                    Method::PATCH,
                    &file,
                    StatusCode::OK,
                    json!({"id":"item-id"}),
                ),
                Reply::json(
                    Method::DELETE,
                    &format!("{root}:/renamed.txt"),
                    StatusCode::NO_CONTENT,
                    Value::Null,
                ),
            ],
        );
        assert!(op.stat("folder/").await.unwrap().is_dir());
        assert_eq!(op.read("note.txt").await.unwrap().to_vec(), b"hello");
        op.write("note.txt", "hello").await.unwrap();
        op.create_dir("new/").await.unwrap();
        op.rename("note.txt", "renamed.txt").await.unwrap();
        op.delete("renamed.txt").await.unwrap();
        assert!(script.replies.lock().unwrap().is_empty());
        let requests = script.requests.lock().unwrap();
        assert_eq!(requests[2].body().to_vec(), b"hello");
        let rename: Value = serde_json::from_reader(requests[5].body().clone().reader()).unwrap();
        assert_eq!(rename["parentReference"]["driveId"], "b!library");
        assert_eq!(rename["name"], "renamed.txt");
    }

    #[tokio::test]
    async fn refresh_auth_uses_consented_graph_scopes_and_caches_the_token() {
        let mut profile = profile();
        profile.options.remove("access_token");
        profile
            .options
            .insert("refresh_token".into(), "refresh-original".into());
        profile.options.insert("client_id".into(), "app-id".into());
        let root = "/v1.0/drives/b!library/root:/folder";
        let (op, script) = setup(
            &profile,
            vec![
                Reply::json(
                    Method::POST,
                    "/organizations/oauth2/v2.0/token",
                    StatusCode::OK,
                    json!({"access_token":"test-token", "refresh_token":"refresh-new", "expires_in":3600}),
                ),
                Reply::json(Method::GET, root, StatusCode::OK, item("root", "", true)),
                Reply::json(Method::GET, root, StatusCode::OK, item("root", "", true)),
            ],
        );
        op.stat("folder/").await.unwrap();
        op.stat("folder/").await.unwrap();
        assert!(script.replies.lock().unwrap().is_empty());
        let requests = script.requests.lock().unwrap();
        let payload = token_form(&requests[0]);
        assert_eq!(
            payload["scope"],
            "offline_access https://graph.microsoft.com/.default"
        );
        assert_eq!(payload["client_id"], "app-id");
        assert_eq!(payload["refresh_token"], "refresh-original");
        assert!(!payload.contains_key("client_secret"));
    }

    fn token_form(req: &Request<Buffer>) -> std::collections::BTreeMap<String, String> {
        url::form_urlencoded::parse(&req.body().to_vec())
            .into_owned()
            .collect()
    }

    fn app_profile() -> Profile {
        let mut profile = profile();
        profile.options.remove("access_token");
        profile
            .options
            .insert("tenant_id".into(), "contoso.onmicrosoft.com".into());
        profile.options.insert("client_id".into(), "app-id".into());
        profile
    }

    fn token_reply(expires: u64) -> Reply {
        Reply::json(
            Method::POST,
            "/contoso.onmicrosoft.com/oauth2/v2.0/token",
            StatusCode::OK,
            json!({"access_token":"test-token", "expires_in":expires}),
        )
    }

    #[tokio::test]
    async fn client_secret_auth_uses_the_tenant_and_safely_encodes_credentials_once_for_concurrent_reads()
     {
        let mut profile = app_profile();
        let secret = "secret+&=/%";
        profile
            .options
            .insert("client_secret".into(), secret.into());
        profile.validate().unwrap();
        let root = "/v1.0/drives/b!library/root:/folder";
        let (op, script) = setup(
            &profile,
            vec![
                token_reply(3600),
                Reply::json(Method::GET, root, StatusCode::OK, item("folder", "", true)),
                Reply::json(Method::GET, root, StatusCode::OK, item("folder", "", true)),
            ],
        );
        let (first, second) = tokio::join!(op.stat("folder/"), op.stat("folder/"));
        first.unwrap();
        second.unwrap();
        assert!(script.replies.lock().unwrap().is_empty());
        let requests = script.requests.lock().unwrap();
        let payload = token_form(&requests[0]);
        assert_eq!(payload["grant_type"], "client_credentials");
        assert_eq!(payload["scope"], "https://graph.microsoft.com/.default");
        assert_eq!(payload["client_secret"], secret);
        assert_eq!(payload.len(), 4);
        assert_eq!(requests[0].uri().host(), Some("login.microsoftonline.com"));
    }

    #[tokio::test]
    async fn certificate_auth_renews_tokens_with_fresh_assertions_and_keeps_the_key_in_memory() {
        use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
        let mut profile = app_profile();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.pfx");
        std::fs::write(
            &path,
            include_bytes!("../tests/fixtures/sharepoint-test.pfx"),
        )
        .unwrap();
        profile
            .options
            .insert("certificate_path".into(), path.to_str().unwrap().into());
        profile
            .options
            .insert("certificate_password".into(), " pfx+test password ".into());
        profile.validate().unwrap();
        let fields = crate::service::field_values(&profile);
        assert_eq!(
            crate::service::build_profile(
                profile.id.clone(),
                profile.name.clone(),
                "sharepoint",
                &fields
            )
            .unwrap(),
            profile
        );
        assert_eq!(profile.secret_keys(), vec!["certificate_password"]);
        let root = "/v1.0/drives/b!library/root:/folder";
        let (op, script) = setup(
            &profile,
            vec![
                token_reply(60),
                Reply::json(Method::GET, root, StatusCode::OK, item("folder", "", true)),
                token_reply(3600),
                Reply::json(Method::GET, root, StatusCode::OK, item("folder", "", true)),
            ],
        );
        op.stat("folder/").await.unwrap();
        std::fs::remove_file(&path).unwrap();
        op.stat("folder/").await.unwrap();
        assert!(script.replies.lock().unwrap().is_empty());
        let requests = script.requests.lock().unwrap();
        let mut ids = Vec::new();
        for index in [0, 2] {
            let payload = token_form(&requests[index]);
            assert_eq!(payload["grant_type"], "client_credentials");
            assert_eq!(
                payload["client_assertion_type"],
                "urn:ietf:params:oauth:client-assertion-type:jwt-bearer"
            );
            assert_eq!(payload["scope"], "https://graph.microsoft.com/.default");
            assert!(!payload.contains_key("client_secret"));
            let assertion = &payload["client_assertion"];
            let claims: Value = serde_json::from_slice(
                &URL_SAFE_NO_PAD
                    .decode(assertion.split('.').nth(1).unwrap())
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(claims["aud"], requests[index].uri().to_string());
            ids.push(claims["jti"].clone());
        }
        assert_ne!(ids[0], ids[1]);
    }

    #[test]
    fn app_auth_requires_tenant_client_and_exactly_one_credential() {
        let mut profile = app_profile();
        profile
            .options
            .insert("client_secret".into(), "secret".into());
        for (key, value) in [
            ("tenant_id", ""),
            ("tenant_id", "common"),
            ("tenant_id", "https://evil.example/"),
            ("tenant_id", "tenant/path"),
            ("client_id", ""),
            ("client_secret", ""),
            ("certificate_path", "identity.pfx"),
            ("access_token", "token"),
            ("auth_method", "unknown"),
        ] {
            let mut invalid = profile.clone();
            invalid.options.insert(key.into(), value.into());
            assert!(invalid.validate().is_err(), "accepted {key}={value}");
        }
        let mut cert = profile;
        cert.options.remove("client_secret");
        cert.options
            .insert("certificate_path".into(), "identity.pfx".into());
        cert.validate().unwrap();
    }

    #[tokio::test]
    async fn large_uploads_use_ordered_chunks_without_sending_graph_credentials() {
        let mut uploaded = item("large.bin", "", false);
        uploaded["size"] = json!(5 * 1024 * 1024);
        let (op, script) = setup(
            &profile(),
            vec![
                Reply::json(
                    Method::POST,
                    "/v1.0/drives/b!library/root:/large.bin:/createUploadSession",
                    StatusCode::OK,
                    json!({"uploadUrl":"https://contoso.sharepoint.com/upload/session"}),
                ),
                Reply::json(
                    Method::PUT,
                    "/upload/session",
                    StatusCode::ACCEPTED,
                    json!({"nextExpectedRanges":["3276800-"]}),
                ),
                Reply::json(
                    Method::PUT,
                    "/upload/session",
                    StatusCode::CREATED,
                    uploaded,
                ),
            ],
        );
        let bytes = vec![0x5a; 5 * 1024 * 1024];
        op.write("large.bin", bytes.clone()).await.unwrap();
        assert!(script.replies.lock().unwrap().is_empty());
        let requests = script.requests.lock().unwrap();
        assert_eq!(
            requests[1].headers()[header::CONTENT_RANGE],
            "bytes 0-3276799/5242880"
        );
        assert_eq!(
            requests[2].headers()[header::CONTENT_RANGE],
            "bytes 3276800-5242879/5242880"
        );
        let actual: Vec<_> = requests
            .iter()
            .skip(1)
            .flat_map(|request| request.body().to_vec())
            .collect();
        assert_eq!(actual, bytes);
    }

    #[tokio::test]
    async fn server_side_copy_waits_for_completion() {
        let mut accepted = Reply::json(
            Method::POST,
            "/v1.0/drives/b!library/root:/note.txt:/copy",
            StatusCode::ACCEPTED,
            Value::Null,
        );
        accepted.location = Some("https://contoso.sharepoint.com/_api/v2.0/monitor/job".into());
        let (op, script) = setup(
            &profile(),
            vec![
                Reply::json(
                    Method::GET,
                    "/v1.0/drives/b!library/root",
                    StatusCode::OK,
                    item("root", "", true),
                ),
                accepted,
                Reply::json(
                    Method::GET,
                    "/_api/v2.0/monitor/job",
                    StatusCode::OK,
                    json!({"status":"completed"}),
                ),
            ],
        );
        op.copy("note.txt", "copy.txt").await.unwrap();
        assert!(script.replies.lock().unwrap().is_empty());
        let requests = script.requests.lock().unwrap();
        let copy = parse_json(requests[1].body().clone()).unwrap();
        assert_eq!(copy["name"], "copy.txt");
        assert_eq!(copy["parentReference"]["driveId"], "b!library");
    }

    #[tokio::test]
    async fn pagination_does_not_send_tokens_to_another_host_or_drive() {
        for next in [
            "https://example.com/leak",
            "https://graph.microsoft.com/v1.0/drives/other/root/children",
        ] {
            let (op, script) = setup(
                &profile(),
                vec![Reply::json(
                    Method::GET,
                    "/v1.0/drives/b!library/root/children",
                    StatusCode::OK,
                    json!({"value":[], "@odata.nextLink":next}),
                )],
            );
            assert!(op.list("/").await.is_err());
            assert_eq!(script.requests.lock().unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn directory_previews_can_walk_nested_library_folders() {
        let mut profile = profile();
        profile.uri = "sharepoint:///Team Reports".into();
        let (op, script) = setup(
            &profile,
            vec![
                Reply::json(
                    Method::GET,
                    "/v1.0/drives/b!library/root:/Team%20Reports/projects:/children",
                    StatusCode::OK,
                    json!({"value":[item("archive", "", true),item("note.txt", "", false)]}),
                ),
                Reply::json(
                    Method::GET,
                    "/v1.0/drives/b!library/root:/Team%20Reports/projects/archive:/children",
                    StatusCode::OK,
                    json!({"value":[item("report.txt", "", false)]}),
                ),
            ],
        );
        let vfs = crate::Vfs::from_operator(crate::Rt::from_current().unwrap(), op, "Team");
        let (entries, truncated) = vfs.list_recursive_limited("projects/", 10).await.unwrap();
        let tree = crate::preview::directory_tree("projects", "projects/", entries, truncated);
        assert_eq!(
            tree.body,
            "projects/\n├── archive/\n│   └── report.txt\n└── note.txt\n"
        );
        assert!(!tree.truncated);
        assert!(script.replies.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn permission_errors_are_preserved() {
        let (op, script) = setup(
            &profile(),
            vec![Reply::json(
                Method::GET,
                "/v1.0/drives/b!library/root:/folder",
                StatusCode::FORBIDDEN,
                json!({"error":{"code":"accessDenied", "message":"No access to this library"}}),
            )],
        );
        let error = op.stat("folder/").await.unwrap_err();
        assert_eq!(error.kind(), opendal::ErrorKind::PermissionDenied);
        assert!(script.replies.lock().unwrap().is_empty());
    }
}
