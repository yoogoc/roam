//! The new/edit connection dialog.
//!
//! Every input here is generated from [`roam_core::service`], so the form asks
//! for exactly what the chosen backend needs and nothing else. It used to be two
//! free-text boxes of `key = value` lines, which meant a typo in an option name
//! was only discovered at connect time — and the URI had to be hand-written.
//!
//! This is a view rather than a plain struct because picking a different service
//! rebuilds the field list, and a rebuild has to re-render.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::scroll::ScrollableElement;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{
    ActiveTheme, Disableable, Icon, IconName, Selectable, Sizable, StyledExt, h_flex, v_flex,
};
use gpui_kit::{
    AnyElement, App, AppContext, ClickEvent, Context, Entity, ExternalPaths, InteractiveElement,
    IntoElement, ParentElement, PathPromptOptions, Render, SharedString, Styled, Task, Window, div,
    prelude::FluentBuilder, px,
};
use roam_core::service::{self, Field, FieldKind, Service};
use roam_core::{Profile, ProfileId, Result, SharePointAuthMethod, profile};

use crate::placeholders;

/// One rendered field, holding whichever kind of state it needs.
struct FieldInput {
    field: Field,
    /// Text, secret and path fields. `None` for a toggle.
    state: Option<Entity<InputState>>,
    on: bool,
}

impl FieldInput {
    fn value(&self, cx: &App) -> String {
        match (&self.state, self.field.kind) {
            (Some(state), _) => {
                let value = state.read(cx).value();
                if self.field.is_secret() {
                    value.to_string()
                } else {
                    value.trim().to_string()
                }
            }
            (None, FieldKind::Toggle { on, off }) => {
                if self.on {
                    on.to_string()
                } else {
                    off.to_string()
                }
            }
            (None, _) => String::new(),
        }
    }
}

pub struct ConnectionForm {
    /// `Some` when editing, so saving replaces rather than adds.
    pub editing: Option<ProfileId>,
    name: Entity<InputState>,
    scheme: &'static str,
    fields: Vec<FieldInput>,
    sharepoint_auth_explicit: bool,
    certificate_name: Option<String>,
    certificate_bytes: Option<Arc<Vec<u8>>>,
    certificate_error: Option<String>,
    certificate_loading: bool,
    certificate_import: Option<Task<()>>,
    saving: bool,
}

impl ConnectionForm {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let service = &service::SERVICES[0];
        let mut this = Self {
            editing: None,
            name: cx
                .new(|cx| InputState::new(window, cx).placeholder(placeholders::CONNECTION_NAME)),
            scheme: service.scheme,
            fields: Vec::new(),
            sharepoint_auth_explicit: true,
            certificate_name: None,
            certificate_bytes: None,
            certificate_error: None,
            certificate_loading: false,
            certificate_import: None,
            saving: false,
        };
        this.rebuild(&BTreeMap::new(), window, cx);
        this
    }

    pub fn editing(profile: &Profile, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut this = Self::new(window, cx);
        this.editing = Some(profile.id.clone());
        this.sharepoint_auth_explicit = profile.options.contains_key("auth_method");
        this.certificate_name = profile.options.get("certificate_name").cloned();

        this.name.update(cx, |state, cx| {
            state.set_value(profile.name.clone(), window, cx)
        });

        // An unknown scheme keeps whatever it had: the form cannot render fields
        // it has no schema for, but it must not silently rewrite the profile.
        if let Some(service) = service::for_scheme(profile.scheme()) {
            this.scheme = service.scheme;
        }
        this.rebuild(&service::field_values(profile), window, cx);
        this
    }

    fn service(&self) -> &'static Service {
        service::for_scheme(self.scheme).unwrap_or(&service::SERVICES[0])
    }

    /// Recreate the inputs for the current service, seeded from `values`.
    fn rebuild(
        &mut self,
        values: &BTreeMap<String, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.fields = self
            .service()
            .fields
            .iter()
            .map(|field| {
                let mut seeded = values
                    .get(field.key)
                    .cloned()
                    .unwrap_or_else(|| field.default_value().to_string());
                if field.key == "auth_method" && seeded.is_empty() {
                    let inferred = if values.get("access_token").is_some_and(|v| !v.is_empty()) {
                        SharePointAuthMethod::AccessToken
                    } else if values.get("refresh_token").is_some_and(|v| !v.is_empty()) {
                        SharePointAuthMethod::RefreshToken
                    } else if values
                        .get("certificate_path")
                        .is_some_and(|v| !v.is_empty())
                    {
                        SharePointAuthMethod::Certificate
                    } else {
                        SharePointAuthMethod::ClientSecret
                    };
                    seeded = inferred.as_str().into();
                }

                match field.kind {
                    FieldKind::Toggle { on, .. } => FieldInput {
                        field: *field,
                        state: None,
                        on: seeded == on,
                    },
                    _ => {
                        let hint = match field.key {
                            "uid" | "gid" => "默认 65534",
                            "nfs_port" | "mount_port" => "自动发现",
                            _ => field.hint,
                        };
                        let masked = field.is_secret();
                        let state = cx.new(|cx| {
                            let mut input = InputState::new(window, cx);
                            if !hint.is_empty() {
                                input = input.placeholder(hint);
                            }
                            if masked {
                                input = input.masked(true);
                            }
                            input
                        });
                        state.update(cx, |state, cx| state.set_value(seeded, window, cx));
                        FieldInput {
                            field: *field,
                            state: Some(state),
                            on: false,
                        }
                    }
                }
            })
            .collect();
    }

    /// Preselect a service by scheme. Unknown schemes are ignored.
    pub fn select_service(&mut self, scheme: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(service) = service::for_scheme(scheme) {
            self.set_scheme(service.scheme, window, cx);
        }
    }

    /// Switch service, carrying over any field the new one also has.
    ///
    /// Keeping `endpoint` and `prefix` across an s3 → gcs switch is the whole
    /// point: they are the same values, and retyping them is exactly the tedium
    /// this form exists to remove.
    fn set_scheme(&mut self, scheme: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        if self.scheme == scheme || self.saving {
            return;
        }
        let carried = self.values(cx);
        self.scheme = scheme;
        self.certificate_import = None;
        self.certificate_loading = false;
        self.certificate_bytes = None;
        self.certificate_name = None;
        self.certificate_error = None;
        self.sharepoint_auth_explicit = true;
        self.rebuild(&carried, window, cx);
        cx.notify();
    }

    fn values(&self, cx: &App) -> BTreeMap<String, String> {
        let auth = self.sharepoint_auth(cx);
        let mut values: BTreeMap<_, _> = self
            .fields
            .iter()
            .filter(|input| self.scheme != "sharepoint" || auth.includes_field(input.field.key))
            .map(|input| (input.field.key.to_string(), input.value(cx)))
            .collect();
        if self.scheme == "sharepoint" && self.sharepoint_auth_explicit {
            values.insert("auth_method".into(), auth.as_str().into());
        }
        values
    }

    fn sharepoint_auth(&self, cx: &App) -> SharePointAuthMethod {
        let value = self
            .fields
            .iter()
            .find(|input| input.field.key == "auth_method")
            .map(|input| input.value(cx))
            .unwrap_or_default();
        SharePointAuthMethod::ALL
            .into_iter()
            .find(|method| method.as_str() == value)
            .unwrap_or(SharePointAuthMethod::ClientSecret)
    }

    fn set_sharepoint_auth(
        &mut self,
        method: SharePointAuthMethod,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.saving {
            return;
        }
        if let Some(state) = self
            .fields
            .iter()
            .find(|input| input.field.key == "auth_method")
            .and_then(|input| input.state.clone())
        {
            state.update(cx, |state, cx| state.set_value(method.as_str(), window, cx));
            self.sharepoint_auth_explicit = true;
            cx.notify();
        }
    }

    fn render_sharepoint_auth(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.scheme != "sharepoint" {
            return div().into_any_element();
        }
        let selected = self.sharepoint_auth(cx);
        let hint = match selected {
            SharePointAuthMethod::ClientSecret => {
                "填写租户、Client ID 和密钥，Roam 自动获取及续期访问令牌。"
            }
            SharePointAuthMethod::Certificate => {
                "拖入 PFX 证书，Roam 会保存副本；对应公钥需已注册到 Entra 应用。"
            }
            SharePointAuthMethod::AccessToken => {
                "使用已有 Microsoft Graph 令牌；到期后需要手动更新。"
            }
            SharePointAuthMethod::RefreshToken => {
                "使用委托刷新令牌自动续期；公共客户端无需 Client Secret。"
            }
        };
        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .flex_wrap()
                    .children(SharePointAuthMethod::ALL.map(|method| {
                        let label = match method {
                            SharePointAuthMethod::ClientSecret => "Client Secret",
                            SharePointAuthMethod::Certificate => "PFX 证书",
                            SharePointAuthMethod::AccessToken => "访问令牌",
                            SharePointAuthMethod::RefreshToken => "刷新令牌",
                        };
                        Button::new(SharedString::from(format!("sp-auth-{}", method.as_str())))
                            .small()
                            .outline()
                            .label(label)
                            .selected(selected == method)
                            .disabled(self.saving)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.set_sharepoint_auth(method, window, cx)
                            }))
                    })),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(hint),
            )
            .into_any_element()
    }

    /// Validate and assemble the profile to save.
    pub fn build(&self, taken: &[ProfileId], cx: &App) -> Result<Profile> {
        if self.certificate_loading {
            return Err(roam_core::Error::Config("正在读取证书，请稍候".into()));
        }
        if self.scheme == "sharepoint"
            && self.sharepoint_auth(cx) == SharePointAuthMethod::Certificate
            && let Some(error) = &self.certificate_error
        {
            return Err(roam_core::Error::Config(error.clone()));
        }
        let name = self.name.read(cx).value().trim().to_string();
        if name.is_empty() {
            return Err(roam_core::Error::Config("请填写连接名称".into()));
        }

        let id = match &self.editing {
            Some(id) => id.clone(),
            None => profile::unique_id(&name, taken),
        };

        let mut profile = service::build_profile(id, name, self.scheme, &self.values(cx))?;
        if self.scheme == "sharepoint"
            && self.sharepoint_auth(cx) == SharePointAuthMethod::Certificate
            && let Some(name) = &self.certificate_name
        {
            profile
                .options
                .insert("certificate_name".into(), name.clone());
        }
        Ok(profile)
    }

    pub(crate) fn certificate_bytes(&self) -> Option<Arc<Vec<u8>>> {
        self.certificate_bytes.clone()
    }

    pub(crate) fn is_saving(&self) -> bool {
        self.saving
    }

    pub(crate) fn set_saving(&mut self, saving: bool, cx: &mut Context<Self>) {
        self.saving = saving;
        cx.notify();
    }

    /// Accept drops anywhere in the form while SharePoint is selected. Read
    /// once in the background, so saving no longer depends on the source path.
    pub(crate) fn drop_certificate(
        &mut self,
        paths: &[PathBuf],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.scheme != "sharepoint" || self.saving {
            return;
        }
        self.certificate_import = None;
        self.certificate_loading = false;
        if paths.len() != 1 {
            self.certificate_error = Some("一次只能导入一个 PFX 证书文件".into());
            cx.notify();
            return;
        }
        let path = paths[0].clone();
        self.set_sharepoint_auth(SharePointAuthMethod::Certificate, window, cx);
        self.certificate_loading = true;
        self.certificate_error = None;
        let handle = window.window_handle();
        self.certificate_import = Some(cx.spawn(async move |this, cx| {
            let source = path.clone();
            let result = cx
                .background_executor()
                .spawn(async move { profile::read_pfx_file(&source) })
                .await;
            let _ = handle.update(cx, |_, window, cx| {
                let _ = this.update(cx, |form, cx| {
                    form.certificate_loading = false;
                    match result {
                        Ok(bytes) => {
                            form.certificate_bytes = Some(Arc::new(bytes));
                            form.certificate_name = Some(
                                path.file_name()
                                    .unwrap_or_default()
                                    .to_string_lossy()
                                    .into_owned(),
                            );
                            if let Some(state) = form
                                .fields
                                .iter()
                                .find(|input| input.field.key == "certificate_path")
                                .and_then(|input| input.state.clone())
                            {
                                state.update(cx, |state, cx| {
                                    state.set_value(path.to_string_lossy().into_owned(), window, cx)
                                });
                            }
                        }
                        Err(error) => form.certificate_error = Some(error.full_message()),
                    }
                    cx.notify();
                });
            });
        }));
        cx.notify();
    }

    fn choose_certificate(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        let picker = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("导入 PFX 证书".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = picker.await {
                let _ = this.update_in(cx, |form, window, cx| {
                    form.drop_certificate(&paths, window, cx)
                });
            }
        })
        .detach();
    }

    fn render_certificate(&self, cx: &mut Context<Self>) -> AnyElement {
        let path = self
            .fields
            .iter()
            .find(|f| f.field.key == "certificate_path")
            .map(|f| f.value(cx))
            .unwrap_or_default();
        let name = self.certificate_name.clone().unwrap_or_else(|| {
            Path::new(&path)
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        });
        let message = if self.certificate_loading {
            "正在读取证书…"
        } else if self.certificate_bytes.is_some() {
            "已导入，保存连接后可移走原始文件"
        } else if !path.is_empty() {
            "证书将在保存时由 Roam 保管"
        } else {
            "将一个 .pfx 或 .p12 文件拖入此弹窗"
        };
        labelled(
            "PFX 证书".into(),
            true,
            v_flex()
                .id("pfx-certificate-drop-zone")
                .gap_2()
                .p_3()
                .rounded_md()
                .border_1()
                .border_color(cx.theme().border)
                .bg(cx.theme().secondary)
                .drag_over::<ExternalPaths>(|style, _, _, cx| {
                    style.border_color(cx.theme().primary)
                })
                .child(
                    h_flex()
                        .gap_3()
                        .child(Icon::new(IconName::File).size_5().flex_none())
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .gap_1()
                                .child(div().text_sm().truncate().child(if name.is_empty() {
                                    "拖入 PFX 证书".to_string()
                                } else {
                                    name
                                }))
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(message),
                                ),
                        )
                        .child(
                            Button::new("choose-pfx-certificate")
                                .small()
                                .outline()
                                .label(if path.is_empty() {
                                    "选择文件"
                                } else {
                                    "更换证书"
                                })
                                .disabled(self.saving || self.certificate_loading)
                                .on_click(cx.listener(Self::choose_certificate)),
                        ),
                )
                .when_some(self.certificate_error.clone(), |el, error| {
                    el.child(div().text_xs().text_color(cx.theme().danger).child(error))
                })
                .into_any_element(),
            cx,
        )
        .into_any_element()
    }
}

/// Display copy stays in the UI; the core service schema still owns all fields.
fn service_presentation(service: &Service) -> (&str, &str, IconName) {
    match service.scheme {
        "fs" => ("本机磁盘", "本地文件夹", IconName::FolderClosed),
        "s3" => ("S3", "兼容对象存储", IconName::Inbox),
        "gcs" => ("Google Cloud", "Cloud Storage", IconName::Globe),
        "azblob" => ("Azure Blob", "Microsoft Azure", IconName::Building2),
        "webdav" => ("WebDAV", "远程文件服务", IconName::Globe),
        "sftp" => ("SFTP", "SSH · 用户名与密码", IconName::Globe),
        "nfs" => ("NFS", "网络共享 · v3", IconName::FolderClosed),
        "sharepoint" => ("SharePoint", "Microsoft 365 文档库", IconName::Building2),
        _ => (service.label, "", IconName::Globe),
    }
}

fn service_description(scheme: &str) -> &'static str {
    match scheme {
        "fs" => "选择一个本地文件夹，作为浏览文件的起点。",
        "s3" => "连接 AWS S3 或兼容的对象存储服务。",
        "gcs" => "连接 Google Cloud Storage 存储桶。",
        "azblob" => "连接 Azure Blob Storage 容器。",
        "webdav" => "通过 WebDAV 访问服务器或 NAS 上的文件。",
        "sftp" => "使用 SSH 用户名和密码访问文件，首次连接时确认服务器指纹。",
        "nfs" => "直接访问 NFSv3 共享，无需先挂载到系统。",
        "sharepoint" => {
            "通过 Microsoft Graph 访问 SharePoint Online 文档库，支持应用密钥与 PFX 证书认证。"
        }
        _ => "填写连接信息以开始浏览文件。",
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FieldSection {
    Location,
    Identity,
    Advanced,
}

fn field_section(field: &Field) -> FieldSection {
    match field.key {
        "nfs_port" | "mount_port" | "host_key" => FieldSection::Advanced,
        "username" | "access_key_id" | "uid" | "gid" | "client_id" | "tenant_id"
        | "certificate_path" => FieldSection::Identity,
        _ if field.is_secret() => FieldSection::Identity,
        _ => FieldSection::Location,
    }
}

impl Render for ConnectionForm {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let compact = window.viewport_size().width < px(760.);
        let mut buttons = Vec::new();
        for service in service::SERVICES {
            let target = service.scheme;
            let selected = target == self.scheme;
            let (label, detail, icon) = service_presentation(service);
            buttons.push(
                Button::new(SharedString::from(format!("svc-{target}")))
                    .ghost()
                    .accessibility_label(service.label)
                    .h(px(if compact { 36. } else { 58. }))
                    .px_3()
                    .when(!compact, |button| button.w_full())
                    .when(selected, |button| button.bg(cx.theme().accent))
                    .child(
                        h_flex()
                            .w_full()
                            .gap_3()
                            .text_color(if selected {
                                cx.theme().primary
                            } else {
                                cx.theme().muted_foreground
                            })
                            .child(Icon::new(icon).size_4().flex_shrink_0())
                            .child(
                                v_flex()
                                    .flex_1()
                                    .items_start()
                                    .gap_1()
                                    .child(div().text_sm().font_medium().child(label.to_string()))
                                    .when(!compact, |el| {
                                        el.child(
                                            div()
                                                .text_xs()
                                                .text_color(cx.theme().muted_foreground)
                                                .child(detail.to_string()),
                                        )
                                    }),
                            )
                            .when(selected, |el| {
                                el.child(Icon::new(IconName::Check).size_3().flex_shrink_0())
                            }),
                    )
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.set_scheme(target, window, cx)
                    })),
            );
        }
        let choices = v_flex()
            .gap_1()
            .when(compact, |el| el.flex_row().flex_wrap())
            .children(buttons);
        let choices = if compact {
            choices.into_any_element()
        } else {
            choices
                .flex_1()
                .min_h_0()
                .overflow_y_scrollbar()
                .into_any_element()
        };
        let picker = v_flex()
            .gap_3()
            .flex_shrink_0()
            .when(!compact, |el| el.w(px(180.)).h_full())
            .child(
                div()
                    .px_3()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("连接类型"),
            )
            .child(choices);

        // Snapshot before installing listeners, which need a mutable context.
        let auth = self.sharepoint_auth(cx);
        let snapshot: Vec<_> = self
            .fields
            .iter()
            .enumerate()
            .filter(|(_, input)| {
                self.scheme != "sharepoint" || auth.includes_field(input.field.key)
            })
            .map(|(ix, input)| {
                let mut field = input.field;
                if self.scheme == "sharepoint" {
                    field.required |= auth.field_required(field.key);
                }
                (ix, field, input.state.clone(), input.on)
            })
            .collect();
        let mut sections = Vec::new();
        for (section, title) in [
            (FieldSection::Location, "连接信息"),
            (
                FieldSection::Identity,
                if self.scheme == "nfs" {
                    "访问身份"
                } else {
                    "身份凭据"
                },
            ),
            (FieldSection::Advanced, "高级选项"),
        ] {
            let inputs: Vec<_> = snapshot
                .iter()
                .filter(|(_, field, _, _)| field_section(field) == section)
                .collect();
            if inputs.is_empty() {
                continue;
            }
            let paired = self.scheme == "nfs" && section != FieldSection::Location;
            let rows: Vec<_> = inputs
                .iter()
                .map(|(ix, field, state, on)| {
                    div().min_w_0().when(paired, |el| el.flex_1()).child(
                        if field.key == "certificate_path" {
                            self.render_certificate(cx)
                        } else {
                            Self::render_field(*ix, *field, state.clone(), *on, self.saving, cx)
                        },
                    )
                })
                .collect();
            let hint = match section {
                FieldSection::Identity if self.scheme == "nfs" => {
                    Some("填写服务端用户和组的数字 ID，以 AUTH_SYS 身份访问共享。")
                }
                FieldSection::Identity
                    if inputs.iter().any(|(_, field, _, _)| field.is_secret()) =>
                {
                    Some(placeholders::CREDENTIAL_STORAGE_NOTE)
                }
                FieldSection::Advanced if self.scheme == "sftp" => {
                    Some("已知服务器可使用 known_hosts；首次连接确认后，指纹保存到此连接。")
                }
                FieldSection::Advanced => Some("通常无需修改，留空时自动发现服务端口。"),
                _ => None,
            };
            let auth_picker = if section == FieldSection::Identity {
                self.render_sharepoint_auth(cx)
            } else {
                div().into_any_element()
            };
            sections.push(
                v_flex()
                    .gap_3()
                    .child(
                        h_flex()
                            .gap_3()
                            .child(
                                div()
                                    .text_xs()
                                    .font_medium()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(title),
                            )
                            .child(div().flex_1().h(px(1.)).bg(cx.theme().border)),
                    )
                    .child(auth_picker)
                    .when_some(hint, |el, hint| {
                        el.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(hint),
                        )
                    })
                    .child(
                        v_flex()
                            .gap_3()
                            .when(paired, |el| el.flex_row())
                            .children(rows),
                    ),
            );
        }

        let details = v_flex()
            .flex_1()
            .min_w_0()
            .gap_5()
            .when(compact, |el| el.flex_none())
            .when(!compact, |el| {
                el.border_l_1().border_color(cx.theme().border).pl_5()
            })
            .when(compact, |el| {
                el.border_t_1().border_color(cx.theme().border).pt_4()
            })
            .child(
                v_flex()
                    .gap_1()
                    .child(div().text_lg().font_semibold().child(self.service().label))
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(service_description(self.scheme)),
                    ),
            )
            .child(labelled(
                "连接名称".into(),
                true,
                Input::new(&self.name)
                    .disabled(self.saving)
                    .into_any_element(),
                cx,
            ))
            .children(sections);

        let details = if compact {
            details.into_any_element()
        } else {
            details
                .h_full()
                .overflow_y_scrollbar()
                .id(SharedString::from(format!(
                    "connection-fields-{}",
                    self.scheme
                )))
                .into_any_element()
        };
        let body = h_flex()
            .id("connection-form-body")
            .size_full()
            .items_start()
            .gap_5()
            .when(compact, |el| el.flex_col().items_stretch())
            .child(picker)
            .child(details)
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                this.drop_certificate(paths.paths(), window, cx);
            }));
        if compact {
            body.overflow_y_scrollbar()
                .id(SharedString::from(format!(
                    "connection-body-{}",
                    self.scheme
                )))
                .into_any_element()
        } else {
            body.into_any_element()
        }
    }
}

impl ConnectionForm {
    fn render_field(
        ix: usize,
        field: Field,
        state: Option<Entity<InputState>>,
        on: bool,
        saving: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match (state, field.kind) {
            (Some(state), _) => labelled(
                field.label.into(),
                field.required,
                Input::new(&state).disabled(saving).into_any_element(),
                cx,
            )
            .into_any_element(),
            (None, FieldKind::Toggle { .. }) => h_flex()
                .items_start()
                .gap_3()
                .p_3()
                .rounded_md()
                .bg(cx.theme().secondary)
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_1()
                        .child(div().text_sm().child(field.label))
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(field.hint),
                        ),
                )
                .child(
                    Switch::new(SharedString::from(format!("tg-{ix}")))
                        .checked(on)
                        .disabled(saving)
                        .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                            if let Some(slot) = this.fields.get_mut(ix) {
                                slot.on = *checked;
                            }
                            cx.notify();
                        })),
                )
                .into_any_element(),
            (None, _) => div().into_any_element(),
        }
    }
}

fn labelled(
    label: SharedString,
    required: bool,
    control: AnyElement,
    cx: &App,
) -> impl IntoElement + use<> {
    v_flex()
        .gap_2()
        .child(
            h_flex().gap_2().child(div().text_sm().child(label)).child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(if required { "必填" } else { "选填" }),
            ),
        )
        .child(control)
}

#[cfg(test)]
impl ConnectionForm {
    pub(crate) fn choose_sharepoint_auth(
        &mut self,
        method: SharePointAuthMethod,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_sharepoint_auth(method, window, cx);
    }

    pub(crate) fn set_name(&self, value: &str, window: &mut Window, cx: &mut App) {
        let value = value.to_string();
        self.name
            .update(cx, |state, cx| state.set_value(value, window, cx));
    }

    /// Type into a field by its schema key. Panics on an unknown key: a test
    /// that names a field the service does not have is asserting nothing.
    pub(crate) fn set_field(&self, key: &str, value: &str, window: &mut Window, cx: &mut App) {
        let input = self
            .fields
            .iter()
            .find(|f| f.field.key == key)
            .unwrap_or_else(|| panic!("no field {key} for scheme {}", self.scheme));

        let value = value.to_string();
        match &input.state {
            Some(state) => state.update(cx, |state, cx| state.set_value(value, window, cx)),
            None => panic!("{key} is a toggle; use toggle_field"),
        }
    }

    pub(crate) fn choose_scheme(
        &mut self,
        scheme: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_service(scheme, window, cx);
    }

    pub(crate) fn scheme(&self) -> &'static str {
        self.scheme
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The schema is what the form renders, so a field the form cannot draw
    /// would be invisible-but-required — the connection would simply refuse to
    /// save with no input to fix it.
    #[test]
    fn every_field_kind_has_a_control() {
        for service in service::SERVICES {
            for field in service.fields {
                match field.kind {
                    FieldKind::Text | FieldKind::Secret | FieldKind::Path => {}
                    FieldKind::Toggle { on, off } => {
                        assert!(!on.is_empty() && !off.is_empty(), "{}", field.key);
                    }
                }
            }
        }
    }
}
