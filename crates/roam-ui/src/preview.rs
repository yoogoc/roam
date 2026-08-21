use std::sync::Arc;

use gpui::{
    Image, ImageFormat, IntoElement, ParentElement, Render, SharedString, Styled, Task, Window,
    div, img, prelude::FluentBuilder, px,
};
use gpui_component::scroll::ScrollableElement;
use gpui_component::text::TextView;
use gpui_component::{ActiveTheme, Icon, IconName, h_flex, v_flex};
use roam_core::preview::{self, ImageKind, PreviewKind};
use roam_core::{DirEntry, Vfs, fmt};

/// What the panel is currently showing.
enum State {
    /// Nothing selected.
    Idle,
    Loading,
    Text {
        body: SharedString,
        truncated: bool,
    },
    Markdown {
        body: SharedString,
        truncated: bool,
    },
    Image(Arc<Image>),
    /// Why there is nothing to show.
    Unavailable(SharedString),
}

/// The right-hand preview panel.
pub struct PreviewPanel {
    vfs: Vfs,
    entry: Option<DirEntry>,
    state: State,
    /// Bumped per request. A read that finishes after the selection moved on is
    /// discarded, the same guard the listing uses.
    generation: u64,
    task: Option<Task<()>>,
}

impl PreviewPanel {
    pub fn new(vfs: Vfs) -> Self {
        Self {
            vfs,
            entry: None,
            state: State::Idle,
            generation: 0,
            task: None,
        }
    }

    pub fn set_vfs(&mut self, vfs: Vfs, cx: &mut gpui::Context<Self>) {
        self.vfs = vfs;
        self.set_entry(None, cx);
    }

    /// Show `entry`, or clear the panel when `None`.
    pub fn set_entry(&mut self, entry: Option<DirEntry>, cx: &mut gpui::Context<Self>) {
        // Re-selecting the same row must not refetch: Space toggling the panel
        // would otherwise re-read the object every time.
        if self.entry.as_ref().map(|e| e.path.clone()) == entry.as_ref().map(|e| e.path.clone()) {
            return;
        }

        self.generation += 1;
        let generation = self.generation;
        self.entry = entry.clone();
        self.task = None;

        let Some(entry) = entry else {
            self.state = State::Idle;
            cx.notify();
            return;
        };

        if entry.is_dir() {
            self.state = State::Unavailable("目录没有预览".into());
            cx.notify();
            return;
        }

        let kind = preview::classify(&entry.name, entry.size);
        if let PreviewKind::None(reason) = kind {
            self.state = State::Unavailable(reason.into());
            cx.notify();
            return;
        }

        self.state = State::Loading;
        cx.notify();

        let vfs = self.vfs.clone();
        let path = entry.path.to_string();
        let limit = preview::read_limit(&kind);

        self.task = Some(cx.spawn(async move |this, cx| {
            let bytes = vfs.read_prefix(&path, limit).await;

            let _ = this.update(cx, |this, cx| {
                if generation != this.generation {
                    return; // the selection moved on
                }

                this.state = match bytes {
                    Ok(bytes) => render_state(&kind, bytes, limit),
                    Err(err) => State::Unavailable(err.full_message().into()),
                };
                cx.notify();
            });
        }));
    }

    pub fn entry(&self) -> Option<&DirEntry> {
        self.entry.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn state_label(&self) -> &'static str {
        match self.state {
            State::Idle => "idle",
            State::Loading => "loading",
            State::Text { .. } => "text",
            State::Markdown { .. } => "markdown",
            State::Image(_) => "image",
            State::Unavailable(_) => "unavailable",
        }
    }

    #[cfg(test)]
    pub(crate) fn body_text(&self) -> Option<String> {
        match &self.state {
            State::Text { body, .. } | State::Markdown { body, .. } => Some(body.to_string()),
            State::Unavailable(reason) => Some(reason.to_string()),
            _ => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn is_truncated(&self) -> bool {
        matches!(
            self.state,
            State::Text {
                truncated: true,
                ..
            } | State::Markdown {
                truncated: true,
                ..
            }
        )
    }

    fn render_meta(&self, entry: &DirEntry, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        v_flex()
            .gap_1()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(div().text_sm().child(entry.name.to_string()))
            .child(
                h_flex()
                    .gap_2()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(if entry.is_dir() {
                        "目录".to_string()
                    } else {
                        fmt::size(entry.size)
                    })
                    .child(fmt::modified(entry.modified)),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(entry.path.to_string()),
            )
    }

    fn render_body(&self, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let note = |text: &str, cx: &mut gpui::Context<Self>| {
            h_flex()
                .size_full()
                .justify_center()
                .items_center()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(text.to_string())
                .into_any_element()
        };

        match &self.state {
            State::Idle => note("选中一个文件查看预览", cx),
            State::Loading => note("正在载入…", cx),
            State::Unavailable(reason) => note(reason, cx),

            State::Text { body, truncated } => v_flex()
                .size_full()
                .min_h_0()
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .p_3()
                        .font_family("ui-monospace")
                        .text_xs()
                        .overflow_y_scrollbar()
                        .child(body.clone()),
                )
                .when(*truncated, |el| el.child(truncation_note(cx)))
                .into_any_element(),

            State::Markdown { body, truncated } => v_flex()
                .size_full()
                .min_h_0()
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .p_3()
                        .overflow_y_scrollbar()
                        .child(TextView::markdown("preview-md", body.clone())),
                )
                .when(*truncated, |el| el.child(truncation_note(cx)))
                .into_any_element(),

            State::Image(image) => div()
                .size_full()
                .p_3()
                .flex()
                .justify_center()
                .items_center()
                .child(img(image.clone()).max_w_full().max_h_full())
                .into_any_element(),
        }
    }
}

fn truncation_note(cx: &mut gpui::Context<PreviewPanel>) -> impl IntoElement {
    // Saying so matters: a preview that silently stops mid-file looks like a
    // truncated file.
    h_flex()
        .px_3()
        .py_1()
        .gap_1()
        .items_center()
        .text_xs()
        .border_t_1()
        .border_color(cx.theme().border)
        .text_color(cx.theme().muted_foreground)
        .child(Icon::new(IconName::Info).size_3())
        .child(SharedString::from(format!(
            "只显示前 {}",
            fmt::size(Some(preview::TEXT_LIMIT))
        )))
}

/// Turn fetched bytes into a renderable state.
fn render_state(kind: &PreviewKind, bytes: Vec<u8>, limit: u64) -> State {
    // A ranged read returning exactly the limit means there is very likely more.
    let truncated = bytes.len() as u64 >= limit;

    match kind {
        PreviewKind::Image(image_kind) => State::Image(Arc::new(Image::from_bytes(
            image_format(*image_kind),
            bytes,
        ))),

        PreviewKind::Text | PreviewKind::Markdown => {
            // The extension was a guess for unknown types; the bytes get the
            // final say, so an unlabelled binary does not render as mojibake.
            if preview::looks_binary(&bytes) {
                return State::Unavailable("二进制文件，暂不预览".into());
            }

            let body: SharedString = String::from_utf8_lossy(&bytes).into_owned().into();
            if matches!(kind, PreviewKind::Markdown) {
                State::Markdown { body, truncated }
            } else {
                State::Text { body, truncated }
            }
        }

        PreviewKind::None(reason) => State::Unavailable((*reason).into()),
    }
}

fn image_format(kind: ImageKind) -> ImageFormat {
    match kind {
        ImageKind::Png => ImageFormat::Png,
        ImageKind::Jpeg => ImageFormat::Jpeg,
        ImageKind::Gif => ImageFormat::Gif,
        ImageKind::Webp => ImageFormat::Webp,
        ImageKind::Bmp => ImageFormat::Bmp,
    }
}

impl Render for PreviewPanel {
    fn render(&mut self, _: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        v_flex()
            .w(px(320.))
            .flex_none()
            .h_full()
            .border_l_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().background)
            .when_some(self.entry.clone(), |el, entry| {
                el.child(self.render_meta(&entry, cx))
            })
            .child(div().flex_1().min_h_0().child(self.render_body(cx)))
    }
}
