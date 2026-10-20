use std::{collections::HashSet, sync::Arc, time::Duration};

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::progress::Progress;
use gpui_kit::component::scroll::ScrollableElement;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{
    ActiveTheme, Disableable, Icon, IconName, Root, Sizable, TitleBar, h_flex, v_flex,
};
use gpui_kit::{
    ClickEvent, Context, InteractiveElement, IntoElement, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, Task, UniformListScrollHandle, Window, div,
    prelude::FluentBuilder, px, uniform_list,
};
use roam_core::transfer::{
    DownloadGroupHandle, DownloadGroupSnapshot, TaskId, TaskSnapshot, TaskState, TransferHandle,
    TransferListItem, TransferOverview,
};
use roam_core::{TransferEngine, fmt};

/// Progress repaint interval — about 10Hz.
///
/// Deliberately a poll rather than a notify-per-chunk: an 8 MB chunk can land
/// every few milliseconds per task, and re-rendering on each one would spend
/// more time drawing the panel than moving bytes.
const POLL: Duration = Duration::from_millis(100);

#[derive(Clone)]
enum Row {
    File { task: TransferHandle, nested: bool },
    Folder(Arc<DownloadGroupHandle>),
}

/// The independent transfer window content, with progress and task controls.
pub struct TransferPanel {
    engine: TransferEngine,
    /// Counts are refreshed separately from the lightweight list structure.
    overview: TransferOverview,
    items: Vec<TransferListItem>,
    rows: Vec<Row>,
    expanded: HashSet<TaskId>,
    revision: Option<u64>,
    /// Held so the poll stops when the panel goes away.
    poll: Option<Task<()>>,
    scroll: UniformListScrollHandle,
}

impl TransferPanel {
    pub fn new(engine: TransferEngine, _: &mut Window, _: &mut Context<Self>) -> Self {
        Self {
            engine,
            overview: TransferOverview {
                total: 0,
                active: 0,
                failed: 0,
                recent: Vec::new(),
            },
            items: Vec::new(),
            rows: Vec::new(),
            expanded: HashSet::new(),
            revision: None,
            poll: None,
            scroll: UniformListScrollHandle::new(),
        }
    }

    pub fn engine(&self) -> &TransferEngine {
        &self.engine
    }

    /// Pick up newly queued work and start polling until everything settles.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.update_list();
        cx.notify();

        if self.poll.is_some() || self.overview.active == 0 {
            return;
        }

        self.poll = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL).await;

                let still_running = this.update(cx, |this, cx| {
                    this.update_list();
                    cx.notify();
                    this.overview.active > 0
                });

                match still_running {
                    Ok(true) => continue,
                    // Either everything finished or the panel is gone; either
                    // way stop burning a timer.
                    _ => break,
                }
            }

            let _ = this.update(cx, |this, cx| {
                this.poll = None;
                this.update_list();
                cx.notify();
            });
        }));
    }

    fn update_list(&mut self) {
        self.overview = self.engine.overview(0);
        let revision = self.engine.revision();
        if self.revision != Some(revision) {
            self.items = self.engine.list_items();
            self.revision = Some(revision);
            let groups: HashSet<_> = self
                .items
                .iter()
                .filter_map(|item| match item {
                    TransferListItem::Folder(folder) => Some(folder.id()),
                    _ => None,
                })
                .collect();
            self.expanded.retain(|id| groups.contains(id));
            self.rebuild_rows();
        }
    }

    fn rebuild_rows(&mut self) {
        self.rows.clear();
        for item in &self.items {
            match item {
                TransferListItem::Task(task) => self.rows.push(Row::File {
                    task: task.clone(),
                    nested: false,
                }),
                TransferListItem::Folder(folder) => {
                    self.rows.push(Row::Folder(Arc::new(folder.clone())));
                    if self.expanded.contains(&folder.id()) {
                        self.rows
                            .extend(folder.children.iter().map(|task| Row::File {
                                task: task.clone(),
                                nested: true,
                            }));
                    }
                }
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.overview.total == 0
    }

    fn active_count(&self) -> usize {
        self.overview.active
    }

    fn failed_count(&self) -> usize {
        self.overview.failed
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.active_count();
        let failed = self.failed_count();

        h_flex()
            .flex_none()
            .px_3()
            .py_2()
            .gap_2()
            .items_center()
            .border_b_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().secondary)
            .child(div().text_sm().child(SharedString::from(format!(
                "传输 · {} 项",
                self.items.len()
            ))))
            .when(active > 0, |el| {
                el.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().foreground)
                        .child(SharedString::from(format!("{active} 进行中"))),
                )
            })
            .when(failed > 0, |el| {
                el.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().danger)
                        .child(SharedString::from(format!("{failed} 失败"))),
                )
            })
            .child(div().flex_1())
            .child(
                Button::new("cancel-all")
                    .label("全部取消")
                    .ghost()
                    .xsmall()
                    .disabled(active == 0)
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.engine.cancel_all();
                        this.refresh(cx);
                    })),
            )
            .when(failed > 0, |el| {
                el.child(
                    Button::new("retry-failed")
                        .label("重试失败")
                        .outline()
                        .xsmall()
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.engine.retry_all_failed();
                            this.refresh(cx);
                        })),
                )
            })
            .child(
                Button::new("clear-finished")
                    .label("清理已完成")
                    .ghost()
                    .xsmall()
                    .disabled(self.overview.total == active)
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.engine.clear_finished();
                        this.refresh(cx);
                    })),
            )
    }

    fn render_task(
        &self,
        task: &TaskSnapshot,
        group: Option<&DownloadGroupSnapshot>,
        nested: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let id = task.id;
        let finished = task.state.is_finished();
        let fraction = task
            .fraction()
            .or_else(|| group.map(|group| group.completed as f32 / group.files.max(1) as f32));

        // Progress as a number too: a bar alone cannot say "3.2 MB of 40 MB",
        // and for an unknown total the bar would be a lie.
        let mut detail = match (task.total, &task.state) {
            (_, TaskState::Failed(reason)) => reason.clone(),
            (Some(total), _) => format!(
                "{} / {}",
                fmt::size(Some(task.done)),
                fmt::size(Some(total))
            ),
            (None, _) => fmt::size(Some(task.done)),
        };
        if let Some(group) = group {
            detail = format!(
                "{} / {} 文件 · {}{}{}",
                group.completed,
                group.files,
                detail,
                if group.failed > 0 {
                    format!(" · {} 失败", group.failed)
                } else {
                    String::new()
                },
                if group.cancelled > 0 {
                    format!(" · {} 已取消", group.cancelled)
                } else {
                    String::new()
                }
            );
        }
        let is_group = group.is_some();

        let detail_tooltip = detail.clone();
        let speed = task
            .bytes_per_sec
            .map(|rate| format!("{}/s", fmt::size(Some(rate))));

        h_flex()
            .debug_selector(move || format!("transfer-row-{id}"))
            .flex_none()
            .h(px(64.))
            .w_full()
            .px_3()
            .when(nested, |row| row.pl(px(44.)))
            .py_1p5()
            .gap_3()
            .items_center()
            .border_b_1()
            .border_color(cx.theme().border)
            .when(is_group, |row| {
                row.child(
                    Button::new(("toggle-transfer-group", id as usize))
                        .icon(if self.expanded.contains(&id) {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .ghost()
                        .xsmall()
                        .debug_selector(move || format!("transfer-group-toggle-{id}"))
                        .tooltip("展开或折叠文件列表")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if !this.expanded.remove(&id) {
                                this.expanded.insert(id);
                            }
                            this.rebuild_rows();
                            cx.notify();
                        })),
                )
            })
            .child(
                Icon::new(match task.operation {
                    roam_core::Operation::Download if is_group => IconName::Folder,
                    roam_core::Operation::Download => IconName::ArrowDown,
                    roam_core::Operation::Upload => IconName::ArrowUp,
                    roam_core::Operation::Copy => IconName::Copy,
                    roam_core::Operation::Move => IconName::ArrowRight,
                })
                .size_4()
                .flex_none()
                .text_color(cx.theme().muted_foreground),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1()
                    .child(div().text_sm().truncate().child(task.label.to_string()))
                    .child(
                        h_flex()
                            .min_w_0()
                            .gap_2()
                            .text_xs()
                            .text_color(if matches!(task.state, TaskState::Failed(_)) {
                                cx.theme().danger
                            } else {
                                cx.theme().muted_foreground
                            })
                            .child(div().flex_none().child(task.state.label()))
                            .child(
                                div()
                                    .id(("task-detail", id as usize))
                                    .min_w_0()
                                    .truncate()
                                    .child(detail)
                                    .tooltip(move |window, cx| {
                                        Tooltip::new(detail_tooltip.clone()).build(window, cx)
                                    }),
                            )
                            .when_some(speed, |el, speed| el.child(div().flex_none().child(speed))),
                    ),
            )
            .child(
                div()
                    .w(px(160.))
                    .flex_none()
                    .when_some(fraction, |el, fraction| {
                        el.child(Progress::new("task-progress").value(fraction * 100.))
                    })
                    // Without a total there is nothing honest to draw, so show
                    // the byte count only.
                    .when(fraction.is_none(), |el| {
                        el.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child("大小未知"),
                        )
                    }),
            )
            .when(task.retryable, |el| {
                // A download resumes from its part file; an upload starts over,
                // because OpenDAL exposes no way to continue a multipart session.
                el.child(
                    Button::new(("retry-task", id as usize))
                        .icon(IconName::Redo)
                        .ghost()
                        .xsmall()
                        .tooltip("重试")
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            if is_group {
                                this.engine.retry_group(id);
                            } else {
                                this.engine.retry(id);
                            }
                            this.refresh(cx);
                        })),
                )
            })
            .child(
                Button::new(("cancel-task", id as usize))
                    .icon(IconName::Close)
                    .ghost()
                    .xsmall()
                    .tooltip("取消")
                    .disabled(finished)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        if is_group {
                            this.engine.cancel_group(id);
                        } else {
                            this.engine.cancel(id);
                        }
                        this.refresh(cx);
                    })),
            )
    }
}

impl Render for TransferPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = if self.is_empty() {
            v_flex()
                .flex_1()
                .min_h_0()
                .items_center()
                .justify_center()
                .gap_3()
                .child(
                    Icon::new(IconName::ArrowDown)
                        .size_8()
                        .text_color(cx.theme().muted_foreground),
                )
                .child(div().text_sm().child("暂无传输任务"))
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child("上传或下载文件后，进度会显示在这里。"),
                )
                .into_any_element()
        } else {
            div()
                .id("transfer-tasks")
                .debug_selector(|| "transfer-list".into())
                .relative()
                .flex_1()
                .min_h_0()
                .child(
                    uniform_list(
                        "all-transfer-tasks",
                        self.rows.len(),
                        cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                            range
                                .filter_map(|index| {
                                    let row = this.rows.get(index)?.clone();
                                    Some(match row {
                                        Row::File { task, nested } => this
                                            .render_task(&task.snapshot(), None, nested, cx)
                                            .into_any_element(),
                                        Row::Folder(folder) => {
                                            let snapshot = folder.snapshot();
                                            this.render_task(
                                                &snapshot.task,
                                                Some(&snapshot),
                                                false,
                                                cx,
                                            )
                                            .into_any_element()
                                        }
                                    })
                                })
                                .collect::<Vec<_>>()
                        }),
                    )
                    .size_full()
                    .track_scroll(&self.scroll),
                )
                .vertical_scrollbar(&self.scroll.0.borrow().base_handle)
                .into_any_element()
        };
        let note = format!(
            "全部 {} 个任务 · 文件夹可展开 · 关闭窗口后传输继续",
            self.overview.total
        );
        v_flex()
            .relative()
            .size_full()
            .min_h_0()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(TitleBar::new().child(div().text_sm().child("Roam · 传输任务")))
            .child(self.render_header(cx))
            .child(content)
            .child(
                div()
                    .flex_none()
                    .px_3()
                    .py_2()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(note),
            )
            .children(Root::render_dialog_layer(window, cx))
            .children(Root::render_notification_layer(window, cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::{
        AppContext, Entity, Modifiers, ScrollStrategy, TestAppContext, VisualTestContext,
    };
    use roam_core::{Rt, Vfs, transfer::Transfer};
    use std::{cell::RefCell, rc::Rc};

    fn harness(
        engine: TransferEngine,
        cx: &mut TestAppContext,
    ) -> (Entity<TransferPanel>, VisualTestContext) {
        cx.background_executor.allow_parking();
        cx.update(gpui_kit::init);
        let holder = Rc::new(RefCell::new(None));
        let output = holder.clone();
        let window = cx.add_window(move |window, cx| {
            let panel = cx.new(|cx| {
                let mut panel = TransferPanel::new(engine, window, cx);
                panel.refresh(cx);
                panel
            });
            *output.borrow_mut() = Some(panel.clone());
            Root::new(panel, window, cx)
        });
        let panel = holder.borrow_mut().take().unwrap();
        (panel, VisualTestContext::from_window(window.into(), cx))
    }

    fn draw(cx: &mut VisualTestContext) {
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
    }

    #[gpui_kit::test]
    fn the_virtual_list_can_scroll_to_old_tasks_beyond_fifty(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().unwrap();
        let rt = Rt::new().unwrap();
        let vfs = Vfs::local(rt.clone(), directory.path().to_str().unwrap()).unwrap();
        let engine = TransferEngine::new(rt, 4);
        engine.enqueue_all((0..120).map(|index| Transfer::Download {
            vfs: vfs.clone(),
            remote: "missing.txt".into(),
            local: directory.path().join(format!("out-{index}")),
            size: None,
        }));
        let (panel, mut visual) = harness(engine, cx);
        draw(&mut visual);
        assert_eq!(panel.read_with(&visual, |panel, _| panel.rows.len()), 120);
        assert!(
            visual.debug_bounds("transfer-row-1").is_none(),
            "offscreen rows must be virtualized"
        );
        panel.update(&mut visual, |panel, cx| {
            panel.scroll.scroll_to_item(119, ScrollStrategy::Top);
            cx.notify();
        });
        draw(&mut visual);
        assert!(
            visual.debug_bounds("transfer-row-1").is_some(),
            "the oldest task must remain reachable"
        );
    }

    #[gpui_kit::test]
    fn folder_downloads_expand_into_children_and_collapse_again(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().unwrap();
        let rt = Rt::new().unwrap();
        let vfs = Vfs::local(rt.clone(), directory.path().to_str().unwrap()).unwrap();
        let engine = TransferEngine::new(rt, 4);
        engine.enqueue_download_group(
            "photos/",
            (0..80)
                .map(|index| Transfer::Download {
                    vfs: vfs.clone(),
                    remote: format!("photos/{index}.jpg").into(),
                    local: directory.path().join(format!("out-{index}")),
                    size: None,
                })
                .collect(),
        );
        let (panel, mut visual) = harness(engine, cx);
        draw(&mut visual);
        assert_eq!(panel.read_with(&visual, |panel, _| panel.rows.len()), 1);
        let toggle = visual
            .debug_bounds("transfer-group-toggle-1")
            .unwrap()
            .center();
        visual.simulate_click(toggle, Modifiers::default());
        draw(&mut visual);
        assert_eq!(panel.read_with(&visual, |panel, _| panel.rows.len()), 81);
        panel.update(&mut visual, |panel, cx| {
            panel.scroll.scroll_to_item(80, ScrollStrategy::Top);
            cx.notify();
        });
        draw(&mut visual);
        assert!(
            visual.debug_bounds("transfer-row-2").is_some(),
            "expanded folders must expose every child"
        );
        panel.update(&mut visual, |panel, cx| {
            panel.scroll.scroll_to_item(0, ScrollStrategy::Top);
            cx.notify();
        });
        draw(&mut visual);
        let toggle = visual
            .debug_bounds("transfer-group-toggle-1")
            .unwrap()
            .center();
        visual.simulate_click(toggle, Modifiers::default());
        assert_eq!(panel.read_with(&visual, |panel, _| panel.rows.len()), 1);
    }
}
