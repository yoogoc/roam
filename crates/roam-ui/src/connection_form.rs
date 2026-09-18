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

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{ActiveTheme, Icon, IconName, StyledExt, h_flex, v_flex};
use gpui_kit::{
    AnyElement, App, AppContext, ClickEvent, Context, Entity, IntoElement, ParentElement, Render,
    SharedString, Styled, Window, div, prelude::FluentBuilder, px,
};
use roam_core::service::{self, Field, FieldKind, Service};
use roam_core::{Profile, ProfileId, Result, profile};

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
            (Some(state), _) => state.read(cx).value().trim().to_string(),
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
        };
        this.rebuild(&BTreeMap::new(), window, cx);
        this
    }

    pub fn editing(profile: &Profile, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut this = Self::new(window, cx);
        this.editing = Some(profile.id.clone());

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
                let seeded = values
                    .get(field.key)
                    .cloned()
                    .unwrap_or_else(|| field.default_value().to_string());

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
        if self.scheme == scheme {
            return;
        }
        let carried = self.values(cx);
        self.scheme = scheme;
        self.rebuild(&carried, window, cx);
        cx.notify();
    }

    fn values(&self, cx: &App) -> BTreeMap<String, String> {
        self.fields
            .iter()
            .map(|input| (input.field.key.to_string(), input.value(cx)))
            .collect()
    }

    /// Validate and assemble the profile to save.
    pub fn build(&self, taken: &[ProfileId], cx: &App) -> Result<Profile> {
        let name = self.name.read(cx).value().trim().to_string();
        if name.is_empty() {
            return Err(roam_core::Error::Config("请填写连接名称".into()));
        }

        let id = match &self.editing {
            Some(id) => id.clone(),
            None => profile::unique_id(&name, taken),
        };

        service::build_profile(id, name, self.scheme, &self.values(cx))
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
        "nfs" => ("NFS", "网络共享 · v3", IconName::FolderClosed),
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
        "nfs" => "直接访问 NFSv3 共享，无需先挂载到系统。",
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
        "nfs_port" | "mount_port" => FieldSection::Advanced,
        "username" | "access_key_id" | "uid" | "gid" => FieldSection::Identity,
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
        let picker = v_flex()
            .gap_3()
            .flex_shrink_0()
            .when(!compact, |el| el.w(px(180.)))
            .child(
                div()
                    .px_3()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("连接类型"),
            )
            .child(
                v_flex()
                    .gap_1()
                    .when(compact, |el| el.flex_row().flex_wrap())
                    .children(buttons),
            );

        // Snapshot before installing listeners, which need a mutable context.
        let snapshot: Vec<_> = self
            .fields
            .iter()
            .enumerate()
            .map(|(ix, input)| (ix, input.field, input.state.clone(), input.on))
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
                    div()
                        .min_w_0()
                        .when(paired, |el| el.flex_1())
                        .child(Self::render_field(*ix, *field, state.clone(), *on, cx))
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
                FieldSection::Advanced => Some("通常无需修改，留空时自动发现服务端口。"),
                _ => None,
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
                Input::new(&self.name).into_any_element(),
                cx,
            ))
            .children(sections);

        // Only the dialog body scrolls. Never cap this content: a capped inner
        // scroll wrapper clips the last fields instead of exposing overflow.
        h_flex()
            .w_full()
            .items_start()
            .gap_5()
            .when(compact, |el| el.flex_col().items_stretch())
            .child(picker)
            .child(details)
    }
}

impl ConnectionForm {
    fn render_field(
        ix: usize,
        field: Field,
        state: Option<Entity<InputState>>,
        on: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match (state, field.kind) {
            (Some(state), _) => labelled(
                field.label.into(),
                field.required,
                Input::new(&state).into_any_element(),
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
