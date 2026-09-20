//! Keyboard actions, editable bindings, and their on-disk settings.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use gpui_kit::{
    Action, App, KeyBinding, Keystroke, Menu, MenuItem, SharedString, SystemMenuType, Unbind,
    actions,
};
use serde::{Deserialize, Serialize};

pub const BROWSER_CONTEXT: &str = "Browser";
pub const WORKSPACE_CONTEXT: &str = "Workspace";

actions!(
    roam,
    [
        OpenSelected,
        GoUp,
        GoBack,
        GoForward,
        Reload,
        FocusFilter,
        NewFolder,
        DeleteSelected,
        TogglePreview,
        ToggleHidden,
        DownloadSelected,
        NewTab,
        CloseTab,
        NextTab,
        PrevTab,
        QuitApp,
    ]
);

#[derive(Clone, Copy)]
pub struct ShortcutDefinition {
    pub id: &'static str,
    pub label: &'static str,
    pub group: &'static str,
    pub default: &'static str,
    pub context: Option<&'static str>,
}

pub const SHORTCUTS: &[ShortcutDefinition] = &[
    ShortcutDefinition {
        id: "open_selected",
        label: "打开所选项目",
        group: "浏览",
        default: "enter",
        context: Some(BROWSER_CONTEXT),
    },
    ShortcutDefinition {
        id: "go_up",
        label: "上一级目录",
        group: "浏览",
        default: "backspace",
        context: Some(BROWSER_CONTEXT),
    },
    ShortcutDefinition {
        id: "go_back",
        label: "后退",
        group: "浏览",
        default: "cmd-[",
        context: Some(BROWSER_CONTEXT),
    },
    ShortcutDefinition {
        id: "go_forward",
        label: "前进",
        group: "浏览",
        default: "cmd-]",
        context: Some(BROWSER_CONTEXT),
    },
    ShortcutDefinition {
        id: "reload",
        label: "刷新",
        group: "浏览",
        default: "cmd-r",
        context: Some(BROWSER_CONTEXT),
    },
    ShortcutDefinition {
        id: "focus_filter",
        label: "过滤当前目录",
        group: "浏览",
        default: "cmd-f",
        context: Some(BROWSER_CONTEXT),
    },
    ShortcutDefinition {
        id: "new_folder",
        label: "新建文件夹",
        group: "文件",
        default: "cmd-shift-n",
        context: Some(BROWSER_CONTEXT),
    },
    ShortcutDefinition {
        id: "delete_selected",
        label: "删除所选项目",
        group: "文件",
        default: "cmd-backspace",
        context: Some(BROWSER_CONTEXT),
    },
    ShortcutDefinition {
        id: "toggle_preview",
        label: "显示或隐藏预览",
        group: "文件",
        default: "space",
        context: Some(BROWSER_CONTEXT),
    },
    ShortcutDefinition {
        id: "toggle_hidden",
        label: "显示或隐藏点文件",
        group: "文件",
        default: "cmd-shift-.",
        context: Some(BROWSER_CONTEXT),
    },
    ShortcutDefinition {
        id: "download_selected",
        label: "下载所选项目",
        group: "文件",
        default: "cmd-d",
        context: Some(BROWSER_CONTEXT),
    },
    ShortcutDefinition {
        id: "new_tab",
        label: "新建标签页",
        group: "标签页与应用",
        default: "cmd-t",
        context: None,
    },
    ShortcutDefinition {
        id: "close_tab",
        label: "关闭标签页",
        group: "标签页与应用",
        default: "cmd-w",
        context: None,
    },
    ShortcutDefinition {
        id: "next_tab",
        label: "下一个标签页",
        group: "标签页与应用",
        default: "cmd-shift-]",
        context: None,
    },
    ShortcutDefinition {
        id: "prev_tab",
        label: "上一个标签页",
        group: "标签页与应用",
        default: "cmd-shift-[",
        context: None,
    },
    ShortcutDefinition {
        id: "quit_app",
        label: "退出应用",
        group: "标签页与应用",
        default: "cmd-q",
        context: None,
    },
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShortcutSettings {
    bindings: BTreeMap<String, String>,
}

impl Default for ShortcutSettings {
    fn default() -> Self {
        Self {
            bindings: SHORTCUTS
                .iter()
                .map(|item| (item.id.to_string(), item.default.to_string()))
                .collect(),
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
struct ShortcutFile {
    #[serde(default)]
    shortcuts: BTreeMap<String, String>,
}

impl ShortcutSettings {
    pub fn get(&self, id: &str) -> &str {
        self.bindings
            .get(id)
            .map(String::as_str)
            .or_else(|| {
                SHORTCUTS
                    .iter()
                    .find(|item| item.id == id)
                    .map(|item| item.default)
            })
            .unwrap_or("")
    }

    pub fn set(&mut self, id: &str, binding: String) {
        self.bindings.insert(id.to_string(), binding);
    }

    pub fn path_for_profiles(profiles: &Path) -> PathBuf {
        profiles.with_file_name("shortcuts.toml")
    }

    pub fn load(path: &Path) -> roam_core::Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => {
                return Err(roam_core::Error::Config(format!(
                    "读取 {} 失败: {error}",
                    path.display()
                )));
            }
        };
        let file: ShortcutFile = toml::from_str(&text).map_err(|error| {
            roam_core::Error::Config(format!("解析 {} 失败: {error}", path.display()))
        })?;
        let mut settings = Self::default();
        for definition in SHORTCUTS {
            if let Some(binding) = file.shortcuts.get(definition.id) {
                settings.set(definition.id, binding.clone());
            }
        }
        settings.validate()?;
        Ok(settings)
    }

    pub fn save(&self, path: &Path) -> roam_core::Result<()> {
        self.validate()?;
        let text = toml::to_string_pretty(&ShortcutFile {
            shortcuts: self.bindings.clone(),
        })
        .map_err(|error| roam_core::Error::Config(format!("序列化快捷键失败: {error}")))?;
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent).map_err(|error| {
                roam_core::Error::Config(format!("创建 {} 失败: {error}", parent.display()))
            })?;
        }
        std::fs::write(path, text).map_err(|error| {
            roam_core::Error::Config(format!("写入 {} 失败: {error}", path.display()))
        })?;
        Ok(())
    }

    pub fn validate(&self) -> roam_core::Result<()> {
        let mut used: HashMap<String, &'static str> = HashMap::new();
        for definition in SHORTCUTS {
            let value = self.get(definition.id).trim();
            if value.is_empty() {
                return Err(roam_core::Error::Config(format!(
                    "请为「{}」设置快捷键",
                    definition.label
                )));
            }
            let mut normalized_strokes = Vec::new();
            for stroke in value.split_whitespace() {
                let parsed = Keystroke::parse(stroke).map_err(|_| {
                    roam_core::Error::Config(format!(
                        "「{}」的快捷键格式无效：{value}",
                        definition.label
                    ))
                })?;
                if parsed.key.is_empty() {
                    return Err(roam_core::Error::Config(format!(
                        "「{}」的快捷键格式无效：{value}",
                        definition.label
                    )));
                }
                normalized_strokes.push(parsed.unparse().to_ascii_lowercase());
            }
            let normalized = normalized_strokes.join(" ");
            if let Some(previous) = used.insert(normalized, definition.label) {
                return Err(roam_core::Error::Config(format!(
                    "「{previous}」与「{}」使用了相同快捷键：{value}",
                    definition.label
                )));
            }
        }
        Ok(())
    }
}

fn action_name(id: &str) -> SharedString {
    match id {
        "open_selected" => OpenSelected.name(),
        "go_up" => GoUp.name(),
        "go_back" => GoBack.name(),
        "go_forward" => GoForward.name(),
        "reload" => Reload.name(),
        "focus_filter" => FocusFilter.name(),
        "new_folder" => NewFolder.name(),
        "delete_selected" => DeleteSelected.name(),
        "toggle_preview" => TogglePreview.name(),
        "toggle_hidden" => ToggleHidden.name(),
        "download_selected" => DownloadSelected.name(),
        "new_tab" => NewTab.name(),
        "close_tab" => CloseTab.name(),
        "next_tab" => NextTab.name(),
        "prev_tab" => PrevTab.name(),
        "quit_app" => QuitApp.name(),
        _ => unreachable!("unknown shortcut id"),
    }
    .into()
}

fn binding(definition: &ShortcutDefinition, keys: &str) -> KeyBinding {
    let context = definition.context;
    match definition.id {
        "open_selected" => KeyBinding::new(keys, OpenSelected, context),
        "go_up" => KeyBinding::new(keys, GoUp, context),
        "go_back" => KeyBinding::new(keys, GoBack, context),
        "go_forward" => KeyBinding::new(keys, GoForward, context),
        "reload" => KeyBinding::new(keys, Reload, context),
        "focus_filter" => KeyBinding::new(keys, FocusFilter, context),
        "new_folder" => KeyBinding::new(keys, NewFolder, context),
        "delete_selected" => KeyBinding::new(keys, DeleteSelected, context),
        "toggle_preview" => KeyBinding::new(keys, TogglePreview, context),
        "toggle_hidden" => KeyBinding::new(keys, ToggleHidden, context),
        "download_selected" => KeyBinding::new(keys, DownloadSelected, context),
        "new_tab" => KeyBinding::new(keys, NewTab, context),
        "close_tab" => KeyBinding::new(keys, CloseTab, context),
        "next_tab" => KeyBinding::new(keys, NextTab, context),
        "prev_tab" => KeyBinding::new(keys, PrevTab, context),
        "quit_app" => KeyBinding::new(keys, QuitApp, context),
        _ => unreachable!("unknown shortcut id"),
    }
}

pub fn init(cx: &mut App) {
    init_with_shortcuts(cx, &ShortcutSettings::default());
}

pub fn init_with_shortcuts(cx: &mut App, settings: &ShortcutSettings) {
    cx.bind_keys(
        SHORTCUTS
            .iter()
            .map(|item| binding(item, settings.get(item.id))),
    );
    install_menus(cx);
}

/// Replace only Roam's bindings, leaving GPUI Kit's component bindings intact.
pub fn rebind(cx: &mut App, old: &ShortcutSettings, new: &ShortcutSettings) {
    let unbinds = SHORTCUTS
        .iter()
        .map(|item| KeyBinding::new(old.get(item.id), Unbind(action_name(item.id)), item.context));
    let bindings = SHORTCUTS.iter().map(|item| binding(item, new.get(item.id)));
    cx.bind_keys(unbinds.chain(bindings));
    install_menus(cx);
}

/// macOS reserves Command-W and Command-Q for native menu items. Associating
/// those items with Roam actions makes them enter the same confirmation flow as
/// every other invocation, while the keymap still supplies editable accelerators.
fn install_menus(cx: &mut App) {
    cx.set_menus([
        Menu::new("Roam").items([
            MenuItem::os_submenu("服务", SystemMenuType::Services),
            MenuItem::separator(),
            MenuItem::action("退出 Roam", QuitApp),
        ]),
        Menu::new("文件").items([MenuItem::action("关闭标签页", CloseTab)]),
    ]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_cover_every_action_and_validate() {
        let settings = ShortcutSettings::default();
        assert_eq!(settings.bindings.len(), SHORTCUTS.len());
        settings.validate().unwrap();
    }

    #[test]
    fn duplicate_and_invalid_bindings_are_rejected() {
        let mut settings = ShortcutSettings::default();
        settings.set("quit_app", "cmd-w".into());
        assert!(
            settings
                .validate()
                .unwrap_err()
                .full_message()
                .contains("相同快捷键")
        );
        settings.set("quit_app", "cmd-".into());
        assert!(
            settings
                .validate()
                .unwrap_err()
                .full_message()
                .contains("格式无效")
        );
    }

    #[test]
    fn settings_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shortcuts.toml");
        let mut settings = ShortcutSettings::default();
        settings.set("new_tab", "cmd-shift-t".into());
        settings.save(&path).unwrap();
        assert_eq!(
            ShortcutSettings::load(&path).unwrap().get("new_tab"),
            "cmd-shift-t"
        );
    }
}
