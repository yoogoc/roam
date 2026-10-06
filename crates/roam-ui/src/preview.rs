use crate::preview_audio::AudioPlayer;
use gpui_kit::component::{ActiveTheme, Disableable, Icon, IconName, Sizable, h_flex, v_flex};
use gpui_kit::component::{
    button::{Button, ButtonVariants},
    scroll::ScrollableElement,
    table::{Column, ColumnSort, DataTable, TableDelegate, TableState},
    text::TextView,
};
use gpui_kit::{
    App, AppContext, Context, Entity, Image, ImageFormat, InteractiveElement, IntoElement,
    ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, Task, Window, div,
    img, prelude::FluentBuilder, px, uniform_list,
};
use roam_core::preview::{
    self, ImageKind, PreviewKind,
    documents::{DocumentPreview, PagedImage},
    structured::{Node, StructuredPreview},
    tables::{PAGE_SIZE, SharedTable, TablePage, TablePreview},
};
use roam_core::{DirEntry, Vfs, fmt};
use std::{collections::HashSet, rc::Rc, sync::Arc, time::Duration};

struct TableView {
    session: SharedTable,
    sources: Vec<String>,
    source: usize,
    page: TablePage,
    sort: Option<(usize, bool)>,
    loading: bool,
    error: Option<String>,
}
struct PagesView {
    document: PagedImage,
    index: usize,
    zoom: f32,
    image: Arc<Image>,
    loading: bool,
    error: Option<String>,
}
struct DocumentView {
    document: Arc<DocumentPreview>,
    index: usize,
    images: Vec<DocumentImage>,
    thumbnails: Vec<(usize, Arc<Image>)>,
    loading: bool,
    error: Option<String>,
}
struct DocumentImage {
    image: Arc<Image>,
    aspect_ratio: f32,
}
enum State {
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
    Tree {
        body: SharedString,
        truncated: bool,
    },
    Image(Arc<Image>),
    Table(TableView),
    Structured {
        document: StructuredPreview,
        collapsed: HashSet<String>,
        raw: bool,
    },
    Pages(PagesView),
    Document(DocumentView),
    Audio(AudioPlayer),
    Video {
        image: Arc<Image>,
        metadata: String,
    },
    Unavailable(SharedString),
}
pub struct PreviewPanel {
    vfs: Vfs,
    entry: Option<DirEntry>,
    state: State,
    generation: u64,
    task: Option<Task<()>>,
    operation: Option<Task<()>>,
    operation_generation: u64,
    table: Option<Entity<TableState<PreviewDelegate>>>,
    audio_tick: Option<Task<()>>,
}
impl PreviewPanel {
    pub fn new(vfs: Vfs) -> Self {
        Self {
            vfs,
            entry: None,
            state: State::Idle,
            generation: 0,
            task: None,
            operation: None,
            operation_generation: 0,
            table: None,
            audio_tick: None,
        }
    }
    pub fn set_vfs(&mut self, vfs: Vfs, cx: &mut Context<Self>) {
        self.vfs = vfs;
        self.set_entry(None, cx);
    }
    pub fn stop_audio(&mut self, cx: &mut Context<Self>) {
        if matches!(self.state, State::Audio(_)) {
            self.set_entry(None, cx);
        }
    }
    pub fn set_entry(&mut self, entry: Option<DirEntry>, cx: &mut Context<Self>) {
        if self.entry.as_ref().map(|e| &e.path) == entry.as_ref().map(|e| &e.path) {
            return;
        }
        self.generation += 1;
        let generation = self.generation;
        self.entry = entry.clone();
        self.task = None;
        self.operation = None;
        self.table = None;
        self.audio_tick = None;
        let Some(entry) = entry else {
            self.state = State::Idle;
            cx.notify();
            return;
        };
        self.state = State::Loading;
        cx.notify();
        let vfs = self.vfs.clone();
        let path = entry.path.to_string();
        let name = entry.name.to_string();
        if entry.is_dir() {
            self.task = Some(cx.spawn(async move |this, cx| {
                let result = vfs
                    .list_recursive_limited(&path, preview::TREE_ENTRY_LIMIT)
                    .await;
                let state = match result {
                    Ok((entries, truncated)) => {
                        cx.background_executor()
                            .spawn(async move {
                                tree_state(preview::directory_tree(
                                    &name, &path, entries, truncated,
                                ))
                            })
                            .await
                    }
                    Err(error) => State::Unavailable(error.full_message().into()),
                };
                let _ = this.update(cx, |this, cx| {
                    if generation == this.generation {
                        this.state = state;
                        cx.notify();
                    }
                });
            }));
            return;
        }
        let kind = preview::classify(&name, entry.size);
        if let PreviewKind::None(reason) = kind {
            self.state = State::Unavailable(reason.into());
            cx.notify();
            return;
        }
        let limit = preview::read_limit(&kind);
        self.task = Some(cx.spawn(async move |this, cx| {
            let state = match vfs.read_prefix(&path, limit).await {
                Ok(bytes) => {
                    cx.background_executor()
                        .spawn(async move {
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                prepare(&kind, &name, bytes, limit)
                            }))
                            .unwrap_or_else(|_| State::Unavailable("文件损坏，预览解析失败".into()))
                        })
                        .await
                }
                Err(error) => State::Unavailable(error.full_message().into()),
            };
            let _ = this.update(cx, |this, cx| {
                if generation != this.generation {
                    return;
                }
                this.state = state;
                if matches!(this.state, State::Audio(_)) {
                    this.audio_tick = Some(cx.spawn(async move |this, cx| {
                        loop {
                            cx.background_executor()
                                .timer(Duration::from_millis(250))
                                .await;
                            if this
                                .update(cx, |this, cx| {
                                    cx.notify();
                                    matches!(this.state, State::Audio(_))
                                })
                                .ok()
                                != Some(true)
                            {
                                break;
                            }
                        }
                    }));
                }
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
            State::Tree { .. } => "tree",
            State::Image(_) => "image",
            State::Table(_) => "table",
            State::Structured { .. } => "structured",
            State::Pages(_) => "pages",
            State::Document(_) => "document",
            State::Audio(_) => "audio",
            State::Video { .. } => "video",
            State::Unavailable(_) => "unavailable",
        }
    }
    #[cfg(test)]
    pub(crate) fn body_text(&self) -> Option<String> {
        match &self.state {
            State::Text { body, .. }
            | State::Markdown { body, .. }
            | State::Tree { body, .. }
            | State::Unavailable(body) => Some(body.to_string()),
            State::Table(view) => Some(
                view.page
                    .rows
                    .iter()
                    .map(|r| r.join("\t"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            State::Structured { document, .. } => Some(document.formatted.clone()),
            _ => None,
        }
    }
    #[cfg(test)]
    pub(crate) fn is_truncated(&self) -> bool {
        match &self.state {
            State::Text { truncated, .. }
            | State::Markdown { truncated, .. }
            | State::Tree { truncated, .. } => *truncated,
            State::Table(v) => v.page.sampled,
            State::Structured { document, .. } => document.truncated,
            State::Pages(v) => v.document.truncated,
            State::Document(v) => v.document.truncated,
            _ => false,
        }
    }

    fn table_page(
        &mut self,
        source: usize,
        offset: usize,
        sort: Option<(usize, bool)>,
        cx: &mut Context<Self>,
    ) {
        let State::Table(view) = &mut self.state else {
            return;
        };
        let session = view.session.clone();
        view.loading = true;
        view.error = None;
        self.operation_generation += 1;
        let operation = self.operation_generation;
        let generation = self.generation;
        cx.notify();
        self.operation = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    session
                        .lock()
                        .map_err(|e| e.to_string())?
                        .page(source, offset, sort)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if generation != this.generation || operation != this.operation_generation {
                    return;
                }
                let State::Table(view) = &mut this.state else {
                    return;
                };
                view.loading = false;
                match result {
                    Ok(page) => {
                        view.page = page;
                        view.source = source;
                        view.sort = sort;
                        this.table = None;
                    }
                    Err(error) => view.error = Some(error),
                }
                cx.notify();
            });
        }));
    }
    fn page(&mut self, index: usize, zoom: f32, cx: &mut Context<Self>) {
        let generation = self.generation;
        self.operation_generation += 1;
        let operation = self.operation_generation;
        match &mut self.state {
            State::Pages(view) if index < view.document.count => {
                let document = view.document.clone();
                view.loading = true;
                view.error = None;
                self.operation = Some(cx.spawn(async move |this, cx| {
                    let result = cx
                        .background_executor()
                        .spawn(async move { guarded(|| document.render(index, zoom)) })
                        .await;
                    let _ = this.update(cx, |this, cx| {
                        if generation != this.generation || operation != this.operation_generation {
                            return;
                        }
                        if let State::Pages(view) = &mut this.state {
                            view.loading = false;
                            match result {
                                Ok(bytes) => {
                                    view.image = png_image(bytes);
                                    view.index = index;
                                    view.zoom = zoom;
                                }
                                Err(error) => view.error = Some(error),
                            }
                            cx.notify();
                        }
                    });
                }));
            }
            State::Document(view) if index < view.document.pages.len() => {
                let document = view.document.clone();
                view.loading = true;
                view.error = None;
                self.operation = Some(cx.spawn(async move |this, cx| {
                    let result = cx
                        .background_executor()
                        .spawn(async move { guarded(|| document_images(&document, index)) })
                        .await;
                    let _ = this.update(cx, |this, cx| {
                        if generation != this.generation || operation != this.operation_generation {
                            return;
                        }
                        if let State::Document(view) = &mut this.state {
                            view.loading = false;
                            match result {
                                Ok((images, thumbnails)) => {
                                    view.images = images;
                                    view.thumbnails = thumbnails;
                                    view.index = index;
                                }
                                Err(error) => view.error = Some(error),
                            }
                            cx.notify();
                        }
                    });
                }));
            }
            _ => {}
        }
        cx.notify();
    }
    fn render_body(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let note = |text: &str| {
            h_flex()
                .size_full()
                .p_4()
                .justify_center()
                .items_center()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(text.to_string())
                .into_any_element()
        };
        match &self.state {
            State::Idle => note("选中一个文件查看预览"),
            State::Loading => note("正在载入…"),
            State::Unavailable(reason) => note(reason),
            State::Text { body, truncated }
            | State::Markdown { body, truncated }
            | State::Tree { body, truncated } => {
                let markdown = matches!(self.state, State::Markdown { .. });
                v_flex()
                    .size_full()
                    .min_h_0()
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .p_3()
                            .overflow_y_scrollbar()
                            .when(markdown, |el| {
                                el.child(TextView::markdown("preview-md", body.clone()))
                            })
                            .when(!markdown, |el| {
                                el.font_family("ui-monospace").text_xs().child(body.clone())
                            }),
                    )
                    .when(*truncated, |el| {
                        el.child(footer(
                            if matches!(self.state, State::Tree { .. }) {
                                "目录树达到预览上限"
                            } else {
                                "只显示前 128 KiB"
                            },
                            cx,
                        ))
                    })
                    .into_any_element()
            }
            State::Image(image) => image_view(image.clone(), 1.).into_any_element(),
            State::Table(view) => {
                let source = view.source;
                let offset = view.page.offset;
                let sort = view.sort;
                v_flex()
                    .size_full()
                    .min_h_0()
                    .gap_1()
                    .child(
                        h_flex()
                            .p_2()
                            .gap_1()
                            .overflow_x_scrollbar()
                            .h(px(40.))
                            .flex_none()
                            .children(view.sources.iter().enumerate().map(|(index, name)| {
                                Button::new(("table-source", index))
                                    .label(name.clone())
                                    .small()
                                    .when(index == source, |button| button.primary())
                                    .disabled(view.loading)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.table_page(index, 0, None, cx)
                                    }))
                            })),
                    )
                    .child(
                        h_flex()
                            .h(px(32.))
                            .flex_none()
                            .px_2()
                            .gap_2()
                            .child(
                                Button::new("previous-rows")
                                    .label("上一页")
                                    .small()
                                    .disabled(view.loading || offset == 0)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.table_page(
                                            source,
                                            offset.saturating_sub(PAGE_SIZE),
                                            sort,
                                            cx,
                                        )
                                    })),
                            )
                            .child(div().text_xs().flex_1().child(if view.loading {
                                "正在查询…".into()
                            } else {
                                format!(
                                    "{}–{} 行 · {} 列",
                                    if view.page.rows.is_empty() {
                                        0
                                    } else {
                                        offset + 1
                                    },
                                    offset + view.page.rows.len(),
                                    view.page.columns.len()
                                )
                            }))
                            .child(
                                Button::new("next-rows")
                                    .label("下一页")
                                    .small()
                                    .disabled(view.loading || !view.page.more)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.table_page(source, offset + PAGE_SIZE, sort, cx)
                                    })),
                            ),
                    )
                    .when_some(self.table.clone(), |el, table| {
                        el.child(
                            div()
                                .flex_1()
                                .min_h_0()
                                .child(DataTable::new(&table).stripe(true)),
                        )
                    })
                    .when(view.page.sampled, |el| {
                        el.child(footer("数据已采样：最多 100,000 行、64 列、32 张表", cx))
                    })
                    .when_some(view.error.clone(), |el, error| el.child(footer(error, cx)))
                    .into_any_element()
            }
            State::Structured {
                document,
                collapsed,
                raw,
            } => {
                let language = if document.language == "yml" {
                    "yaml"
                } else {
                    &document.language
                };
                let body = if *raw {
                    let formatted = document
                        .formatted
                        .chars()
                        .take(preview::TEXT_LIMIT as usize)
                        .collect::<String>();
                    div()
                        .flex_1()
                        .min_h_0()
                        .p_3()
                        .overflow_y_scrollbar()
                        .child(TextView::markdown(
                            "structured-raw",
                            format!("~~~~~~~~{language}\n{formatted}\n~~~~~~~~"),
                        ))
                        .into_any_element()
                } else {
                    let mut rows = Vec::new();
                    flatten(&document.root, "0", 0, collapsed, &mut rows);
                    let rows = Arc::new(rows);
                    uniform_list(
                        "structured-tree",
                        rows.len(),
                        cx.processor(move |_, range: std::ops::Range<usize>, _, cx| {
                            range
                                .map(|index| {
                                    let (id, depth, label, branch, expanded) = &rows[index];
                                    let id = id.clone();
                                    h_flex()
                                        .id(("structured-row", index))
                                        .h(px(28.))
                                        .pl(px(12. + *depth as f32 * 14.))
                                        .pr_2()
                                        .gap_1()
                                        .text_xs()
                                        .font_family("ui-monospace")
                                        .child(if *branch {
                                            if *expanded { "▾" } else { "▸" }
                                        } else {
                                            "·"
                                        })
                                        .child(div().text_ellipsis().child(label.clone()))
                                        .when(*branch, |el| {
                                            el.cursor_pointer().on_click(cx.listener(
                                                move |this, _, _, cx| {
                                                    if let State::Structured { collapsed, .. } =
                                                        &mut this.state
                                                    {
                                                        if !collapsed.remove(&id) {
                                                            collapsed.insert(id.clone());
                                                        }
                                                        cx.notify();
                                                    }
                                                },
                                            ))
                                        })
                                        .into_any_element()
                                })
                                .collect()
                        }),
                    )
                    .size_full()
                    .into_any_element()
                };
                v_flex()
                    .size_full()
                    .min_h_0()
                    .child(
                        h_flex().p_2().child(
                            Button::new("structured-mode")
                                .small()
                                .label(if *raw {
                                    "目录视图"
                                } else {
                                    "格式化文本"
                                })
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if let State::Structured { raw, .. } = &mut this.state {
                                        *raw = !*raw;
                                        cx.notify();
                                    }
                                })),
                        ),
                    )
                    .child(body)
                    .when(document.truncated, |el| {
                        el.child(footer("只显示前 2,000 个节点；格式化文本最多 128 KiB", cx))
                    })
                    .into_any_element()
            }
            State::Pages(view) => v_flex()
                .size_full()
                .min_h_0()
                .child(self.page_toolbar(
                    view.index,
                    view.document.count,
                    view.zoom,
                    view.loading,
                    true,
                    cx,
                ))
                .child(
                    div().flex_1().min_h_0().overflow_scrollbar().child(
                        div()
                            .p_3()
                            .child(img(view.image.clone()).w(px(440. * view.zoom))),
                    ),
                )
                .when(view.document.truncated, |el| {
                    el.child(footer("只预览前 500 页", cx))
                })
                .when_some(view.error.clone(), |el, error| el.child(footer(error, cx)))
                .into_any_element(),
            State::Document(view) => {
                let page = &view.document.pages[view.index];
                v_flex()
                    .size_full()
                    .min_h_0()
                    .when(view.document.slides, |el| {
                        el.child(self.page_toolbar(
                            view.index,
                            view.document.pages.len(),
                            1.,
                            view.loading,
                            false,
                            cx,
                        ))
                    })
                    .when(view.document.slides, |el| {
                        el.child(
                            h_flex()
                                .p_2()
                                .gap_1()
                                .overflow_x_scrollbar()
                                .h(px(80.))
                                .flex_none()
                                .children(view.thumbnails.iter().map(|(index, image)| {
                                    let index = *index;
                                    Button::new(("slide", index))
                                        .small()
                                        .h(px(60.))
                                        .label((index + 1).to_string())
                                        .child(img(image.clone()).w(px(72.)).h(px(44.)))
                                        .disabled(view.loading)
                                        .when(index == view.index, |button| button.primary())
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.page(index, 1., cx)
                                        }))
                                })),
                        )
                    })
                    .child(
                        v_flex()
                            .flex_1()
                            .min_h_0()
                            .p_3()
                            .gap_3()
                            .overflow_y_scrollbar()
                            .when(!view.document.slides, |el| {
                                el.child(TextView::markdown(
                                    "document-content",
                                    page.markdown.clone(),
                                ))
                            })
                            .children(view.images.iter().map(|image| {
                                div()
                                    .relative()
                                    .w_full()
                                    .aspect_ratio(image.aspect_ratio)
                                    .flex_none()
                                    .child(img(image.image.clone()).absolute().size_full())
                            }))
                            .when(view.document.slides, |el| {
                                el.child(TextView::markdown("slide-content", page.markdown.clone()))
                            }),
                    )
                    .child(footer(
                        if view.document.slides {
                            "幻灯片内容预览 · 复杂主题、动画及母版可能简化"
                        } else {
                            "文档内容预览 · 保留标题、段落、表格和内嵌图片"
                        },
                        cx,
                    ))
                    .when(view.document.truncated, |el| {
                        el.child(footer("内容达到预览上限", cx))
                    })
                    .when_some(view.error.clone(), |el, error| el.child(footer(error, cx)))
                    .into_any_element()
            }
            State::Audio(player) => {
                let snapshot = player.snapshot();
                v_flex()
                    .size_full()
                    .p_4()
                    .gap_4()
                    .justify_center()
                    .items_center()
                    .child(Icon::new(IconName::File).size(px(64.)))
                    .child(div().text_sm().child(snapshot.metadata))
                    .child(div().font_family("ui-monospace").child(format!(
                            "{} / {}",
                            preview::media::timestamp(snapshot.position),
                            snapshot
                                .duration
                                .map(preview::media::timestamp)
                                .unwrap_or_else(|| "未知时长".into())
                        )))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("audio-back")
                                    .label("−10s")
                                    .on_click(cx.listener(|this, _, _, _| {
                                        if let State::Audio(p) = &this.state {
                                            p.seek(-10)
                                        }
                                    })),
                            )
                            .child(
                                Button::new("audio-play")
                                    .primary()
                                    .label(if snapshot.paused { "播放" } else { "暂停" })
                                    .on_click(cx.listener(|this, _, _, _| {
                                        if let State::Audio(p) = &this.state {
                                            p.toggle()
                                        }
                                    })),
                            )
                            .child(Button::new("audio-forward").label("+10s").on_click(
                                cx.listener(|this, _, _, _| {
                                    if let State::Audio(p) = &this.state {
                                        p.seek(10)
                                    }
                                }),
                            )),
                    )
                    .when_some(snapshot.error, |el, error| {
                        el.child(div().text_xs().child(error))
                    })
                    .into_any_element()
            }
            State::Video { image, metadata } => v_flex()
                .size_full()
                .p_3()
                .gap_3()
                .justify_center()
                .child(img(image.clone()).w_full())
                .child(div().text_sm().child(metadata.clone()))
                .child(footer("视频首帧预览", cx))
                .into_any_element(),
        }
    }
    fn page_toolbar(
        &self,
        index: usize,
        count: usize,
        zoom: f32,
        loading: bool,
        zoomable: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        h_flex()
            .p_2()
            .gap_2()
            .child(
                Button::new("page-back")
                    .label("上一页")
                    .small()
                    .disabled(index == 0 || loading)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.page(index.saturating_sub(1), zoom, cx)
                    })),
            )
            .child(div().flex_1().text_xs().child(if loading {
                "正在渲染…".into()
            } else {
                format!("{} / {count}", index + 1)
            }))
            .child(
                Button::new("page-next")
                    .label("下一页")
                    .small()
                    .disabled(index + 1 >= count || loading)
                    .on_click(cx.listener(move |this, _, _, cx| this.page(index + 1, zoom, cx))),
            )
            .when(zoomable, |el| {
                el.child(
                    Button::new("page-zoom")
                        .small()
                        .label(format!("{}%", (zoom * 100.) as u32))
                        .disabled(loading)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.page(index, if zoom >= 2. { 0.5 } else { zoom + 0.5 }, cx)
                        })),
                )
            })
    }
}
fn footer(text: impl Into<SharedString>, cx: &App) -> impl IntoElement {
    div()
        .px_3()
        .py_2()
        .text_xs()
        .border_t_1()
        .border_color(cx.theme().border)
        .text_color(cx.theme().muted_foreground)
        .child(text.into())
}
fn image_view(image: Arc<Image>, zoom: f32) -> impl IntoElement {
    div()
        .w_full()
        .p_3()
        .flex()
        .justify_center()
        .items_center()
        .child(img(image).w(px(440. * zoom)).max_w_full())
}
fn png_image(bytes: Vec<u8>) -> Arc<Image> {
    Arc::new(Image::from_bytes(ImageFormat::Png, bytes))
}
fn tree_state(tree: preview::TreePreview) -> State {
    State::Tree {
        body: tree.body.into(),
        truncated: tree.truncated,
    }
}
type DocumentImages = (Vec<DocumentImage>, Vec<(usize, Arc<Image>)>);
fn document_images(doc: &DocumentPreview, index: usize) -> Result<DocumentImages, String> {
    let images = doc.pages[index]
        .images
        .iter()
        .map(|bytes| {
            let png = if doc.slides {
                preview::documents::svg(bytes)?
            } else {
                bytes.clone()
            };
            // All document images are normalized to PNG by the core renderer.
            // A proportional container avoids GPUI using the intrinsic pixel
            // height when the image width is a percentage of the preview panel.
            let dimensions = png.get(16..24).ok_or("图片尺寸无效")?;
            let width = u32::from_be_bytes(dimensions[..4].try_into().unwrap());
            let height = u32::from_be_bytes(dimensions[4..].try_into().unwrap());
            if width == 0 || height == 0 {
                return Err("图片尺寸无效".into());
            }
            Ok(DocumentImage {
                image: png_image(png),
                aspect_ratio: width as f32 / height as f32,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let thumbnails = if doc.slides {
        (index.saturating_sub(2)..(index + 3).min(doc.pages.len()))
            .map(|index| {
                preview::documents::thumbnail(&doc.pages[index].images[0])
                    .map(|bytes| (index, png_image(bytes)))
            })
            .collect::<Result<Vec<_>, _>>()?
    } else {
        Vec::new()
    };
    Ok((images, thumbnails))
}
fn prepare(kind: &PreviewKind, name: &str, bytes: Vec<u8>, limit: u64) -> State {
    match guarded(|| prepare_result(kind, name, bytes, limit)) {
        Ok(state) => state,
        Err(error) => State::Unavailable(error.into()),
    }
}
fn guarded<T>(operation: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(operation))
        .unwrap_or_else(|_| Err("文件损坏，预览解析失败".into()))
}
fn prepare_result(
    kind: &PreviewKind,
    name: &str,
    bytes: Vec<u8>,
    limit: u64,
) -> Result<State, String> {
    let truncated = bytes.len() as u64 >= limit;
    if !matches!(kind, PreviewKind::Text | PreviewKind::Markdown) && truncated {
        return Err("文件超过预览读取上限".into());
    }
    Ok(match kind {
        PreviewKind::Image(ImageKind::Svg) => {
            State::Image(png_image(preview::documents::svg(&bytes)?))
        }
        PreviewKind::Image(ImageKind::Ico) => {
            State::Image(png_image(preview::documents::raster(&bytes)?))
        }
        PreviewKind::Image(ImageKind::Tiff) | PreviewKind::Pdf => {
            let document =
                PagedImage::open(bytes, matches!(kind, PreviewKind::Image(ImageKind::Tiff)))?;
            let image = png_image(document.render(0, 1.)?);
            State::Pages(PagesView {
                document,
                index: 0,
                zoom: 1.,
                image,
                loading: false,
                error: None,
            })
        }
        PreviewKind::Image(image_kind) => State::Image(Arc::new(Image::from_bytes(
            match image_kind {
                ImageKind::Png => ImageFormat::Png,
                ImageKind::Jpeg => ImageFormat::Jpeg,
                ImageKind::Gif => ImageFormat::Gif,
                ImageKind::Webp => ImageFormat::Webp,
                ImageKind::Bmp => ImageFormat::Bmp,
                _ => unreachable!(),
            },
            bytes,
        ))),
        PreviewKind::Text | PreviewKind::Markdown => {
            if preview::looks_binary(&bytes) {
                return Err("二进制文件，暂不预览".into());
            }
            let body = String::from_utf8_lossy(&bytes).into_owned().into();
            if matches!(kind, PreviewKind::Markdown) {
                State::Markdown { body, truncated }
            } else {
                State::Text { body, truncated }
            }
        }
        PreviewKind::Zip => tree_state(preview::zip_tree(name, &bytes)?),
        PreviewKind::Archive => tree_state(preview::archives::tree(name, &bytes)?),
        PreviewKind::Structured => {
            let table = name.to_lowercase().ends_with(".json")
                && serde_json::from_slice::<serde_json::Value>(&bytes)
                    .ok()
                    .is_some_and(|v| {
                        v.as_array().is_some_and(|a| {
                            !a.is_empty() && a.iter().all(serde_json::Value::is_object)
                        })
                    });
            if table {
                table_state(name, &bytes)?
            } else {
                State::Structured {
                    document: preview::structured::parse(name, &bytes)?,
                    collapsed: HashSet::new(),
                    raw: false,
                }
            }
        }
        PreviewKind::Table => table_state(name, &bytes)?,
        PreviewKind::Document => {
            let document = Arc::new(preview::documents::open(name, &bytes)?);
            let (images, thumbnails) = document_images(&document, 0)?;
            State::Document(DocumentView {
                document,
                index: 0,
                images,
                thumbnails,
                loading: false,
                error: None,
            })
        }
        PreviewKind::Audio => State::Audio(AudioPlayer::new(bytes)?),
        PreviewKind::Video => {
            let video = preview::media::video(name, &bytes)?;
            State::Video {
                image: png_image(video.image),
                metadata: video.metadata,
            }
        }
        PreviewKind::None(reason) => return Err((*reason).into()),
    })
}
fn table_state(name: &str, bytes: &[u8]) -> Result<State, String> {
    let session = TablePreview::open(name, bytes)?;
    let table = session.lock().map_err(|e| e.to_string())?;
    let sources = table.sources.clone();
    let page = table.page(0, 0, None)?;
    drop(table);
    Ok(State::Table(TableView {
        session,
        sources,
        source: 0,
        page,
        sort: None,
        loading: false,
        error: None,
    }))
}
type StructuredRow = (String, usize, String, bool, bool);
fn flatten(
    node: &Node,
    id: &str,
    depth: usize,
    collapsed: &HashSet<String>,
    rows: &mut Vec<StructuredRow>,
) {
    let expanded = !collapsed.contains(id);
    rows.push((
        id.to_owned(),
        depth,
        node.label.clone(),
        !node.children.is_empty(),
        expanded,
    ));
    if expanded {
        for (i, node) in node.children.iter().enumerate() {
            flatten(node, &format!("{id}/{i}"), depth + 1, collapsed, rows);
        }
    }
}
type SortHandler = Rc<dyn Fn(usize, bool, &mut App)>;
struct PreviewDelegate {
    page: TablePage,
    columns: Vec<Column>,
    on_sort: SortHandler,
}
impl TableDelegate for PreviewDelegate {
    fn columns_count(&self, _: &App) -> usize {
        self.columns.len()
    }
    fn rows_count(&self, _: &App) -> usize {
        self.page.rows.len()
    }
    fn column(&self, index: usize, _: &App) -> Column {
        self.columns[index].clone()
    }
    fn perform_sort(
        &mut self,
        index: usize,
        sort: ColumnSort,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) {
        (self.on_sort)(index, sort != ColumnSort::Descending, cx);
    }
    fn render_td(
        &mut self,
        row: usize,
        col: usize,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        div()
            .text_xs()
            .font_family("ui-monospace")
            .text_ellipsis()
            .child(self.page.rows[row][col].clone())
    }
}
impl Render for PreviewPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.table.is_none()
            && let State::Table(view) = &self.state
        {
            let weak = cx.weak_entity();
            let source = view.source;
            let columns = view
                .page
                .columns
                .iter()
                .enumerate()
                .map(|(index, (name, kind))| {
                    let mut column = Column::new(index.to_string(), format!("{name} · {kind}"))
                        .width(px(180.))
                        .sortable();
                    column.sort = Some(
                        view.sort
                            .filter(|(i, _)| *i == index)
                            .map(|(_, asc)| {
                                if asc {
                                    ColumnSort::Ascending
                                } else {
                                    ColumnSort::Descending
                                }
                            })
                            .unwrap_or(ColumnSort::Default),
                    );
                    column
                })
                .collect();
            let delegate = PreviewDelegate {
                page: view.page.clone(),
                columns,
                on_sort: Rc::new(move |col, asc, cx| {
                    let _ = weak.update(cx, |this, cx| {
                        this.table_page(source, 0, Some((col, asc)), cx)
                    });
                }),
            };
            self.table = Some(cx.new(|cx| {
                TableState::new(delegate, window, cx)
                    .sortable(true)
                    .row_selectable(false)
            }));
        }
        let width = if matches!(
            self.state,
            State::Table(_) | State::Pages(_) | State::Document(_) | State::Structured { .. }
        ) {
            520.
        } else {
            320.
        };
        v_flex()
            .w(px(width))
            .max_w(gpui_kit::relative(0.65))
            .flex_none()
            .h_full()
            .border_l_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().background)
            .when_some(self.entry.clone(), |el, entry| {
                el.child(
                    v_flex()
                        .gap_1()
                        .px_3()
                        .py_2()
                        .border_b_1()
                        .border_color(cx.theme().border)
                        .child(
                            div()
                                .text_sm()
                                .text_ellipsis()
                                .child(entry.name.to_string()),
                        )
                        .child(
                            h_flex()
                                .gap_2()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(if entry.is_dir() {
                                    "目录".into()
                                } else {
                                    fmt::size(entry.size)
                                })
                                .child(fmt::modified(entry.modified)),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_ellipsis()
                                .text_color(cx.theme().muted_foreground)
                                .child(entry.path.to_string()),
                        ),
                )
            })
            .child(div().flex_1().min_h_0().child(self.render_body(cx)))
    }
}
