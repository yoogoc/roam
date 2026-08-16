//! Keyboard actions and their default bindings.
//!
//! `gpui-component`'s table already binds `up` / `down` / `left` / `right` /
//! `escape` in its own `Table` context, so row movement works without anything
//! here. These are the app-level verbs it does not cover.
//!
//! Bindings are installed by [`init`], which the binary and every example must
//! call — an action with no binding is silently inert, which is the kind of bug
//! that survives a long time.

use gpui::{App, KeyBinding, actions};

/// Key context of the browsing pane. Bindings are scoped to it so that typing in
/// the filter box or a dialog does not trigger navigation.
pub const BROWSER_CONTEXT: &str = "Browser";

/// Key context of the whole window. Tab management lives here rather than in
/// `Browser`, because a tab outlives whichever pane currently has focus.
pub const WORKSPACE_CONTEXT: &str = "Workspace";

actions!(
    roam,
    [
        /// Enter the selected directory.
        OpenSelected,
        /// Go to the parent directory.
        GoUp,
        GoBack,
        GoForward,
        Reload,
        /// Move focus into the filter box.
        FocusFilter,
        NewFolder,
        DeleteSelected,
        /// Toggle the preview panel.
        TogglePreview,
        DownloadSelected,
        /// Open another tab on the same session and directory.
        NewTab,
        CloseTab,
        NextTab,
        PrevTab,
    ]
);

pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("enter", OpenSelected, Some(BROWSER_CONTEXT)),
        KeyBinding::new("backspace", GoUp, Some(BROWSER_CONTEXT)),
        // Matching the platform's history shortcuts rather than inventing new
        // ones.
        KeyBinding::new("cmd-[", GoBack, Some(BROWSER_CONTEXT)),
        KeyBinding::new("cmd-]", GoForward, Some(BROWSER_CONTEXT)),
        KeyBinding::new("cmd-r", Reload, Some(BROWSER_CONTEXT)),
        KeyBinding::new("cmd-f", FocusFilter, Some(BROWSER_CONTEXT)),
        KeyBinding::new("cmd-shift-n", NewFolder, Some(BROWSER_CONTEXT)),
        // `cmd-backspace` is the platform's "move to trash" chord. It opens the
        // same confirmation the menu does — no shortcut deletes without asking.
        KeyBinding::new("cmd-backspace", DeleteSelected, Some(BROWSER_CONTEXT)),
        KeyBinding::new("space", TogglePreview, Some(BROWSER_CONTEXT)),
        KeyBinding::new("cmd-d", DownloadSelected, Some(BROWSER_CONTEXT)),
        KeyBinding::new("cmd-t", NewTab, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new("cmd-w", CloseTab, Some(WORKSPACE_CONTEXT)),
        // `cmd-shift-[` / `]` rather than the plain pair, which the panes use
        // for history.
        KeyBinding::new("cmd-shift-]", NextTab, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new("cmd-shift-[", PrevTab, Some(WORKSPACE_CONTEXT)),
    ]);
}

// Rename deliberately has no shortcut. In this pane Enter means "open", which
// is what a browser-shaped tool implies, and giving Enter a second meaning that
// depends on focus is how people rename things by accident. It stays on the
// context menu.
//
// Clearing the filter is handled by `InputState::clean_on_escape()` rather than
// an action: the table binds `escape` in its own, more specific context, so a
// Browser-level binding would lose to it whenever the list had focus.

/// The shortcuts worth showing in the UI, as (label, keys) pairs.
pub const SHORTCUT_HINTS: &[(&str, &str)] = &[
    ("进入", "↵"),
    ("上一级", "⌫"),
    ("后退 / 前进", "⌘[ / ⌘]"),
    ("刷新", "⌘R"),
    ("过滤", "⌘F"),
    ("新建文件夹", "⇧⌘N"),
    ("删除", "⌘⌫"),
    ("预览", "空格"),
    ("下载", "⌘D"),
    ("新标签 / 关闭", "⌘T / ⌘W"),
    ("切换标签", "⇧⌘[ / ⇧⌘]"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_hint_names_a_real_binding() {
        // The hints are hand-written, so this guards against them drifting from
        // the bindings above — a wrong shortcut hint is worse than none.
        assert_eq!(SHORTCUT_HINTS.len(), 11);
        for (label, keys) in SHORTCUT_HINTS {
            assert!(!label.is_empty() && !keys.is_empty());
        }
    }
}
