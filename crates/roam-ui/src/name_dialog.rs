use gpui::{App, AppContext, Entity, Window};
use gpui_component::input::InputState;

/// A single-field prompt for a new folder name or a rename.
pub struct NameDialog {
    pub input: Entity<InputState>,
}

impl NameDialog {
    pub fn new(
        initial: &str,
        placeholder: &'static str,
        window: &mut Window,
        cx: &mut App,
    ) -> Self {
        let initial = initial.to_string();

        let input = cx.new(|cx| {
            let state = InputState::new(window, cx).placeholder(placeholder);
            if initial.is_empty() {
                state
            } else {
                state.default_value(initial)
            }
        });

        Self { input }
    }

    /// The typed name, trimmed.
    ///
    /// Validation deliberately lives with the caller (`Browser::create_folder`,
    /// `Browser::rename_entry`) rather than here, so the rules have exactly one
    /// home and the tests exercise the same path the dialog does.
    pub fn value(&self, cx: &App) -> String {
        self.input.read(cx).value().trim().to_string()
    }
}
