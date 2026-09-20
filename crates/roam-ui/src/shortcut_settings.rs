//! Editable shortcut settings shown inside the settings dialog.

use gpui_kit::component::button::Button;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{ActiveTheme, Sizable, h_flex, v_flex};
use gpui_kit::{
    App, AppContext, ClickEvent, Context, Entity, IntoElement, ParentElement, Render, Styled,
    Window, div,
};

use crate::actions::{SHORTCUTS, ShortcutSettings};

pub struct ShortcutSettingsForm {
    inputs: Vec<(&'static str, Entity<InputState>)>,
}

impl ShortcutSettingsForm {
    pub fn new(settings: &ShortcutSettings, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let inputs = SHORTCUTS
            .iter()
            .map(|definition| {
                let value = settings.get(definition.id).to_string();
                let input = cx.new(|cx| {
                    InputState::new(window, cx)
                        .default_value(value)
                        .placeholder(definition.default)
                });
                (definition.id, input)
            })
            .collect();
        Self { inputs }
    }

    pub fn settings(&self, cx: &App) -> ShortcutSettings {
        let mut settings = ShortcutSettings::default();
        for (id, input) in &self.inputs {
            settings.set(id, input.read(cx).value().trim().to_string());
        }
        settings
    }

    fn restore_defaults(&self, window: &mut Window, cx: &mut Context<Self>) {
        let defaults = ShortcutSettings::default();
        for (id, input) in &self.inputs {
            let value = defaults.get(id).to_string();
            input.update(cx, |state, cx| state.set_value(value, window, cx));
        }
        cx.notify();
    }
}

impl Render for ShortcutSettingsForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut sections = Vec::new();
        for group in ["浏览", "文件", "标签页与应用"] {
            let rows = SHORTCUTS
                .iter()
                .filter(|definition| definition.group == group)
                .filter_map(|definition| {
                    let input = self
                        .inputs
                        .iter()
                        .find(|(id, _)| *id == definition.id)?
                        .1
                        .clone();
                    Some(
                        h_flex()
                            .w_full()
                            .items_center()
                            .gap_4()
                            .child(
                                div()
                                    .w_40()
                                    .flex_shrink_0()
                                    .text_sm()
                                    .child(definition.label),
                            )
                            .child(div().flex_1().min_w_0().child(Input::new(&input))),
                    )
                });

            sections.push(
                v_flex()
                    .gap_3()
                    .child(
                        h_flex()
                            .gap_3()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(group),
                            )
                            .child(div().flex_1().h_px().bg(cx.theme().border)),
                    )
                    .children(rows),
            );
        }

        v_flex()
            .gap_5()
            .child(
                h_flex()
                    .items_start()
                    .justify_between()
                    .gap_4()
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child("使用 GPUI 键名，例如 cmd-w、cmd-shift-n 或 space。保存后立即生效。"),
                    )
                    .child(
                        Button::new("restore-shortcut-defaults")
                            .label("恢复默认")
                            .outline()
                            .small()
                            .on_click(cx.listener(
                                |this, _: &ClickEvent, window, cx| {
                                    this.restore_defaults(window, cx)
                                },
                            )),
                    ),
            )
            .children(sections)
    }
}
