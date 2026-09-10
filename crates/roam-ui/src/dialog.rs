//! The buttons a dialog has to draw for itself.
//!
//! gpui-component turns `DialogButtonProps` into actual buttons only for an
//! `AlertDialog`. A plain `Dialog` takes the same props as its Enter and Escape
//! handlers and renders no footer at all, so every dialog in this app opened
//! with nothing to click: the only way to confirm was a key nothing on screen
//! mentioned. The delete confirmation was the worst of them — it also turns off
//! the backdrop and the close cross on purpose, which left Escape as the single
//! way out of a dialog whose whole job is to ask a question.
//!
//! Hence this extension trait. Every dialog in the app goes through it, so the
//! footer is one decision made once rather than five that can drift apart.

use gpui_kit::component::WindowExt;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants};
use gpui_kit::component::dialog::{Dialog, DialogFooter};
use gpui_kit::{App, ParentElement, SharedString, Window};

/// The action behind a confirm button. Returns true when the dialog should
/// close — the same contract as [`Dialog::on_ok`], so a rejected input can keep
/// the dialog open with whatever was typed in it.
pub(crate) trait DialogButtons {
    /// A confirm button and a cancel button. Enter runs the same action.
    fn confirm_cancel(
        self,
        confirm: impl Into<SharedString>,
        cancel: impl Into<SharedString>,
        action: impl Fn(&mut Window, &mut App) -> bool + 'static,
    ) -> Self;

    /// The same, with the confirm button in red — for an action that cannot be
    /// undone.
    fn danger_cancel(
        self,
        confirm: impl Into<SharedString>,
        cancel: impl Into<SharedString>,
        action: impl Fn(&mut Window, &mut App) -> bool + 'static,
    ) -> Self;

    /// One button, which closes the dialog. For a dialog that only shows
    /// something and has nothing to confirm.
    fn dismiss(self, label: impl Into<SharedString>) -> Self;
}

impl DialogButtons for Dialog {
    fn confirm_cancel(
        self,
        confirm: impl Into<SharedString>,
        cancel: impl Into<SharedString>,
        action: impl Fn(&mut Window, &mut App) -> bool + 'static,
    ) -> Self {
        with_footer(
            self,
            confirm.into(),
            ButtonVariant::Primary,
            cancel.into(),
            action,
        )
    }

    fn danger_cancel(
        self,
        confirm: impl Into<SharedString>,
        cancel: impl Into<SharedString>,
        action: impl Fn(&mut Window, &mut App) -> bool + 'static,
    ) -> Self {
        with_footer(
            self,
            confirm.into(),
            ButtonVariant::Danger,
            cancel.into(),
            action,
        )
    }

    fn dismiss(self, label: impl Into<SharedString>) -> Self {
        self.footer(DialogFooter::new().child(close_button("dismiss", label.into())))
    }
}

fn with_footer(
    dialog: Dialog,
    confirm: SharedString,
    variant: ButtonVariant,
    cancel: SharedString,
    action: impl Fn(&mut Window, &mut App) -> bool + 'static,
) -> Dialog {
    // The click and the Enter key run the same closure, so the two can never
    // decide differently about whether the dialog may close.
    let action = std::rc::Rc::new(action);
    let clicked = action.clone();

    dialog
        .footer(
            DialogFooter::new()
                .child(close_button("cancel", cancel))
                .child(
                    Button::new("dialog-confirm")
                        .label(confirm)
                        .with_variant(variant)
                        .on_click(move |_, window, cx| {
                            if clicked(window, cx) {
                                window.close_dialog(cx);
                            }
                        }),
                ),
        )
        .on_ok(move |_, window, cx| action(window, cx))
}

fn close_button(id: &'static str, label: SharedString) -> Button {
    Button::new(id)
        .label(label)
        .outline()
        .on_click(|_, window, cx| window.close_dialog(cx))
}

#[cfg(test)]
mod tests {
    /// Every dialog in the app has to go through this module, because a dialog
    /// that sets `button_props` and nothing else renders no buttons — and looks
    /// perfectly reasonable in review.
    #[test]
    fn no_view_builds_its_own_dialog_buttons() {
        let sources = [
            include_str!("browser.rs"),
            include_str!("workspace.rs"),
            include_str!("preview.rs"),
            include_str!("transfer_panel.rs"),
        ];

        for src in sources {
            assert!(
                !src.contains("DialogButtonProps"),
                "a view is setting DialogButtonProps; those draw nothing on a \
                 plain Dialog — use the DialogButtons trait instead"
            );
        }
    }
}
