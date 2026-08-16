use std::time::Duration;

use gpui::{
    ClickEvent, Context, IntoElement, ParentElement, Render, SharedString, Styled, Task, Window,
    div, prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::progress::Progress;
use gpui_component::scroll::ScrollableElement;
use gpui_component::{ActiveTheme, Disableable, Icon, IconName, Sizable, h_flex, v_flex};
use roam_core::transfer::{TaskSnapshot, TaskState};
use roam_core::{TransferEngine, fmt};

/// Progress repaint interval — about 10Hz.
///
/// Deliberately a poll rather than a notify-per-chunk: an 8 MB chunk can land
/// every few milliseconds per task, and re-rendering on each one would spend
/// more time drawing the panel than moving bytes.
const POLL: Duration = Duration::from_millis(100);

/// The bottom transfer panel: one row per task, with progress and a cancel.
pub struct TransferPanel {
    engine: TransferEngine,
    tasks: Vec<TaskSnapshot>,
    /// Held so the poll stops when the panel goes away.
    poll: Option<Task<()>>,
    expanded: bool,
}

impl TransferPanel {
    pub fn new(engine: TransferEngine, _: &mut Window, _: &mut Context<Self>) -> Self {
        Self {
            engine,
            tasks: Vec::new(),
            poll: None,
            expanded: true,
        }
    }

    pub fn engine(&self) -> &TransferEngine {
        &self.engine
    }

    /// Pick up newly queued work and start polling until everything settles.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.tasks = self.engine.snapshot();
        cx.notify();

        if self.poll.is_some() || !self.engine.is_active() {
            return;
        }

        self.poll = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL).await;

                let still_running = this.update(cx, |this, cx| {
                    this.tasks = this.engine.snapshot();
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
                this.tasks = this.engine.snapshot();
                cx.notify();
            });
        }));
    }

    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    fn active_count(&self) -> usize {
        self.tasks
            .iter()
            .filter(|task| !task.state.is_finished())
            .count()
    }

    fn failed_count(&self) -> usize {
        self.tasks
            .iter()
            .filter(|task| matches!(task.state, TaskState::Failed(_)))
            .count()
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.active_count();
        let failed = self.failed_count();

        h_flex()
            .px_3()
            .py_1p5()
            .gap_2()
            .items_center()
            .border_t_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().secondary)
            .child(
                Button::new("toggle-transfers")
                    .icon(if self.expanded {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronUp
                    })
                    .ghost()
                    .xsmall()
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.expanded = !this.expanded;
                        cx.notify();
                    })),
            )
            .child(div().text_sm().child(SharedString::from(format!(
                "传输 · {} 项",
                self.tasks.len()
            ))))
            .when(active > 0, |el| {
                el.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().accent_foreground)
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
                    .disabled(self.tasks.len() == active)
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

        let speed = task
            .bytes_per_sec
            .map(|rate| format!("{}/s", fmt::size(Some(rate))));

        h_flex()
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
                    .child(div().text_sm().child(task.label.to_string()))
                    .child(
                        h_flex()
                            .gap_2()
                            .text_xs()
                            .text_color(if matches!(task.state, TaskState::Failed(_)) {
                                cx.theme().danger
                            } else {
                                cx.theme().muted_foreground
                            })
                            .child(task.state.label())
                            .child(detail)
                            .when_some(speed, |el, speed| el.child(speed)),
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
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Nothing queued yet: stay out of the way entirely.
        if self.tasks.is_empty() {
            return div().into_any_element();
        }

        let rows: Vec<_> = self
            .tasks
            .iter()
            .rev()
            .take(50)
            .map(|task| self.render_task(task, cx).into_any_element())
            .collect();

        v_flex()
            .flex_none()
            .bg(cx.theme().background)
            .child(self.render_header(cx))
            .when(self.expanded, |el| {
                el.child(
                    v_flex()
                        .max_h(px(220.))
                        .overflow_y_scrollbar()
                        .children(rows),
                )
            })
            .into_any_element()
    }
}
