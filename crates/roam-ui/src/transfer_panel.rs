use std::time::Duration;

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::progress::Progress;
use gpui_kit::component::scroll::ScrollableElement;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{
    ActiveTheme, Disableable, Icon, IconName, Root, Sizable, TitleBar, h_flex, v_flex,
};
use gpui_kit::{
    ClickEvent, Context, InteractiveElement, IntoElement, ParentElement, Render, ScrollHandle,
    SharedString, StatefulInteractiveElement, Styled, Task, Window, div, prelude::FluentBuilder,
    px,
};
use roam_core::transfer::{TaskSnapshot, TaskState, TransferOverview};
use roam_core::{TransferEngine, fmt};

/// Progress repaint interval — about 10Hz.
///
/// Deliberately a poll rather than a notify-per-chunk: an 8 MB chunk can land
/// every few milliseconds per task, and re-rendering on each one would spend
/// more time drawing the panel than moving bytes.
const POLL: Duration = Duration::from_millis(100);

/// How many rows the panel draws. Dropping a folder makes one task per file, so
/// the list has always been capped — what changed is that the engine is no longer
/// asked to describe the rest.
const VISIBLE_ROWS: usize = 50;

/// The independent transfer window content, with progress and task controls.
pub struct TransferPanel {
    engine: TransferEngine,
    /// Counts over every task plus the newest [`VISIBLE_ROWS`]. Polled at 10 Hz
    /// while transfers run, which is why it must not be proportional to the task
    /// count: 100k queued uploads used to mean cloning 100k snapshots ten times a
    /// second for a list that shows fifty.
    overview: TransferOverview,
    /// Held so the poll stops when the panel goes away.
    poll: Option<Task<()>>,
    scroll: ScrollHandle,
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
            poll: None,
            scroll: ScrollHandle::default(),
        }
    }

    pub fn engine(&self) -> &TransferEngine {
        &self.engine
    }

    /// Pick up newly queued work and start polling until everything settles.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.overview = self.engine.overview(VISIBLE_ROWS);
        cx.notify();

        if self.poll.is_some() || !self.engine.is_active() {
            return;
        }

        self.poll = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL).await;

                let still_running = this.update(cx, |this, cx| {
                    this.overview = this.engine.overview(VISIBLE_ROWS);
                    cx.notify();
                    this.engine.is_active()
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
                this.overview = this.engine.overview(VISIBLE_ROWS);
                cx.notify();
            });
        }));
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
                self.overview.total
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

    fn render_task(&self, task: &TaskSnapshot, cx: &mut Context<Self>) -> impl IntoElement {
        let id = task.id;
        let finished = task.state.is_finished();

        // Progress as a number too: a bar alone cannot say "3.2 MB of 40 MB",
        // and for an unknown total the bar would be a lie.
        let detail = match (task.total, &task.state) {
            (_, TaskState::Failed(reason)) => reason.clone(),
            (Some(total), _) => format!(
                "{} / {}",
                fmt::size(Some(task.done)),
                fmt::size(Some(total))
            ),
            (None, _) => fmt::size(Some(task.done)),
        };

        let detail_tooltip = detail.clone();
        let speed = task
            .bytes_per_sec
            .map(|rate| format!("{}/s", fmt::size(Some(rate))));

        h_flex()
            .debug_selector(move || format!("transfer-row-{id}"))
            .flex_none()
            .px_3()
            .py_1p5()
            .gap_3()
            .items_center()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                Icon::new(match task.operation {
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
                    .when_some(task.fraction(), |el, fraction| {
                        el.child(Progress::new("task-progress").value(fraction * 100.))
                    })
                    // Without a total there is nothing honest to draw, so show
                    // the byte count only.
                    .when(task.fraction().is_none(), |el| {
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
                            this.engine.retry(id);
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
                        this.engine.cancel(id);
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
            let rows: Vec<_> = self
                .overview
                .recent
                .iter()
                .map(|task| self.render_task(task, cx).into_any_element())
                .collect();
            v_flex()
                .id("transfer-tasks")
                .debug_selector(|| "transfer-list".into())
                .relative()
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .track_scroll(&self.scroll)
                .children(rows)
                .vertical_scrollbar(&self.scroll)
                .into_any_element()
        };
        let note = if self.overview.total > self.overview.recent.len() {
            format!(
                "显示最近 {} 项任务 · 关闭窗口后传输继续",
                self.overview.recent.len()
            )
        } else {
            "关闭窗口后传输继续，可从主窗口左下角重新打开。".into()
        };
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
