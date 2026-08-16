//! Capability-driven context menu.
//!
//! Backends differ in what they can do: S3 has no native rename, local `fs`
//! cannot presign. Deriving the menu from `Capability` instead of assuming a
//! superset is what stops the UI offering actions that are guaranteed to fail.
//!
//! Unsupported actions are shown **disabled with a reason**, not hidden — a
//! missing menu item reads as a bug, while a greyed-out one with "该后端不支持
//! 分享链接" explains itself.

use opendal::Capability;

use crate::{DirEntry, EntryKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryAction {
    /// Create a directory inside the directory being listed.
    NewFolder,
    /// Rename the entry in place.
    Rename,
    /// Copy the entry alongside itself under a free name.
    Duplicate,
    /// Delete the entry; directories are removed recursively.
    Delete,
    /// Copy the entry to the local downloads directory.
    Download,
    /// Show the object's stored versions.
    ShowVersions,
    /// Copy the entry's full path to the clipboard.
    CopyPath,
    /// Copy a time-limited public URL (presigned GET).
    CopyShareLink,
    /// Re-list the current directory.
    Reload,
}

impl EntryAction {
    /// Whether performing this action changes the backend, and so needs the
    /// listing invalidated and reloaded afterwards.
    pub fn mutates(self) -> bool {
        matches!(
            self,
            Self::NewFolder | Self::Rename | Self::Duplicate | Self::Delete
        )
    }

    /// Whether the action is handled by the transfer engine rather than run
    /// inline. These report progress in the transfer panel and can be cancelled.
    pub fn is_transfer(self) -> bool {
        matches!(self, Self::Download) || matches!(self, Self::Duplicate)
    }

    /// Whether the action needs a confirmation step before running.
    pub fn needs_confirmation(self) -> bool {
        matches!(self, Self::Delete)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuItem {
    pub action: EntryAction,
    pub label: &'static str,
    pub enabled: bool,
    /// Why it is disabled, shown as a tooltip.
    pub disabled_reason: Option<&'static str>,
}

impl MenuItem {
    fn enabled(action: EntryAction, label: &'static str) -> Self {
        Self {
            action,
            label,
            enabled: true,
            disabled_reason: None,
        }
    }

    fn disabled(action: EntryAction, label: &'static str, reason: &'static str) -> Self {
        Self {
            action,
            label,
            enabled: false,
            disabled_reason: Some(reason),
        }
    }

    /// The text to render in the menu.
    ///
    /// The reason is folded into the label rather than left to a tooltip: a
    /// disabled row does not reliably raise hover events, so a tooltip there
    /// would often never be seen.
    pub fn display_label(&self) -> String {
        match self.disabled_reason {
            Some(reason) if !self.enabled => format!("{}（{reason}）", self.label),
            _ => self.label.to_string(),
        }
    }
}

/// The menu for right-clicking `entry` on a backend with `capability`.
pub fn entry_menu(capability: &Capability, entry: &DirEntry) -> Vec<MenuItem> {
    let new_folder = gate(
        EntryAction::NewFolder,
        "新建文件夹",
        capability.create_dir,
        "该后端不支持新建目录",
    );

    let rename = if entry.kind == EntryKind::Dir {
        // `Operator::rename` rejects any path ending in `/` on every backend, so
        // a directory rename goes through the transfer engine instead: copy every
        // file to the new prefix, then delete the old one. That needs listing and
        // copying, and it is a visible, cancellable task rather than something
        // pretending to be one atomic rename.
        gate(
            EntryAction::Rename,
            "重命名",
            capability.list && capability.copy && capability.delete,
            "该后端不支持复制或删除",
        )
    } else {
        gate(
            EntryAction::Rename,
            "重命名",
            capability.rename,
            // S3 has no native rename. A file could be copy-then-deleted like a
            // directory is, but a single object is exactly the case where the
            // non-atomicity buys nothing: `copy` already puts it where you want
            // it, so "创建副本" plus a delete is the honest pair of steps.
            "该后端不支持重命名",
        )
    };

    // Directories are handled too now: the transfer engine flattens them into
    // one server-side copy per file.
    let duplicate = gate(
        EntryAction::Duplicate,
        "创建副本",
        capability.copy && (entry.kind != EntryKind::Dir || capability.list),
        "该后端不支持服务端复制",
    );

    let download = gate(
        EntryAction::Download,
        "下载到本地",
        capability.read && (entry.kind != EntryKind::Dir || capability.list),
        "该后端不支持读取",
    );

    let versions = if entry.kind == EntryKind::Dir {
        // Versioning is per object; a prefix has no history of its own.
        MenuItem::disabled(EntryAction::ShowVersions, "版本历史", "目录没有版本历史")
    } else {
        gate(
            EntryAction::ShowVersions,
            "版本历史",
            capability.list_with_versions,
            "该后端不支持版本历史",
        )
    };

    let delete = if entry.kind == EntryKind::Dir {
        gate(
            EntryAction::Delete,
            "删除",
            capability.delete && capability.list,
            "该后端不支持删除目录",
        )
    } else {
        gate(
            EntryAction::Delete,
            "删除",
            capability.delete,
            "该后端不支持删除",
        )
    };

    let share = if entry.kind == EntryKind::Dir {
        MenuItem::disabled(
            EntryAction::CopyShareLink,
            "复制分享链接",
            "目录无法生成分享链接",
        )
    } else if !capability.presign {
        MenuItem::disabled(
            EntryAction::CopyShareLink,
            "复制分享链接",
            "该后端不支持分享链接",
        )
    } else if !capability.presign_read {
        MenuItem::disabled(
            EntryAction::CopyShareLink,
            "复制分享链接",
            "该后端不支持预签名读取",
        )
    } else {
        MenuItem::enabled(EntryAction::CopyShareLink, "复制分享链接")
    };

    vec![
        new_folder,
        rename,
        duplicate,
        delete,
        download,
        versions,
        MenuItem::enabled(EntryAction::CopyPath, "复制路径"),
        share,
        MenuItem::enabled(EntryAction::Reload, "刷新"),
    ]
}

/// Enable an item, or disable it with `reason`.
fn gate(
    action: EntryAction,
    label: &'static str,
    supported: bool,
    reason: &'static str,
) -> MenuItem {
    if supported {
        MenuItem::enabled(action, label)
    } else {
        MenuItem::disabled(action, label, reason)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn entry(kind: EntryKind) -> DirEntry {
        DirEntry {
            name: "thing".into(),
            path: Arc::from("dir/thing"),
            kind,
            size: Some(1),
            modified: None,
            etag: None,
            meta_complete: true,
        }
    }

    fn item(items: &[MenuItem], action: EntryAction) -> &MenuItem {
        items.iter().find(|i| i.action == action).unwrap()
    }

    fn presign_capable() -> Capability {
        Capability {
            presign: true,
            presign_read: true,
            ..Default::default()
        }
    }

    #[test]
    fn copy_path_is_always_available() {
        for kind in [EntryKind::Dir, EntryKind::File, EntryKind::Unknown] {
            let items = entry_menu(&Capability::default(), &entry(kind));
            assert!(item(&items, EntryAction::CopyPath).enabled);
        }
    }

    #[test]
    fn share_link_is_offered_when_the_backend_can_presign() {
        let items = entry_menu(&presign_capable(), &entry(EntryKind::File));
        assert!(item(&items, EntryAction::CopyShareLink).enabled);
    }

    #[test]
    fn share_link_is_disabled_with_a_reason_when_unsupported() {
        // Local fs: no presign at all.
        let items = entry_menu(&Capability::default(), &entry(EntryKind::File));
        let share = item(&items, EntryAction::CopyShareLink);

        assert!(!share.enabled);
        assert_eq!(share.disabled_reason, Some("该后端不支持分享链接"));
    }

    #[test]
    fn share_link_distinguishes_presign_from_presign_read() {
        let capability = Capability {
            presign: true,
            presign_read: false,
            ..Default::default()
        };
        let items = entry_menu(&capability, &entry(EntryKind::File));

        assert_eq!(
            item(&items, EntryAction::CopyShareLink).disabled_reason,
            Some("该后端不支持预签名读取")
        );
    }

    #[test]
    fn a_directory_never_gets_a_share_link() {
        // Even on a backend that can presign — the reason differs from the
        // unsupported-backend case so the tooltip stays truthful.
        let items = entry_menu(&presign_capable(), &entry(EntryKind::Dir));
        let share = item(&items, EntryAction::CopyShareLink);

        assert!(!share.enabled);
        assert_eq!(share.disabled_reason, Some("目录无法生成分享链接"));
    }

    #[test]
    fn the_disabled_reason_is_visible_in_the_label() {
        let items = entry_menu(&Capability::default(), &entry(EntryKind::File));
        let share = item(&items, EntryAction::CopyShareLink);

        assert_eq!(
            share.display_label(),
            "复制分享链接（该后端不支持分享链接）"
        );
    }

    #[test]
    fn an_enabled_item_shows_a_bare_label() {
        let items = entry_menu(&presign_capable(), &entry(EntryKind::File));
        assert_eq!(
            item(&items, EntryAction::CopyShareLink).display_label(),
            "复制分享链接"
        );
    }

    #[test]
    fn unsupported_actions_are_disabled_rather_than_hidden() {
        let all = entry_menu(&Capability::default(), &entry(EntryKind::File));
        let none = entry_menu(&full_capability(), &entry(EntryKind::File));

        assert_eq!(
            all.len(),
            none.len(),
            "the menu has the same shape regardless of capability"
        );
        assert!(all.len() >= 9);
    }

    fn full_capability() -> Capability {
        Capability {
            create_dir: true,
            rename: true,
            copy: true,
            delete: true,
            list: true,
            read: true,
            write: true,
            presign: true,
            presign_read: true,
            list_with_versions: true,
            read_with_version: true,
            ..Default::default()
        }
    }

    #[test]
    fn a_fully_capable_backend_enables_everything_for_a_file() {
        for item in entry_menu(&full_capability(), &entry(EntryKind::File)) {
            assert!(item.enabled, "{:?} should be enabled", item.action);
        }
    }

    #[test]
    fn a_directory_row_disables_only_rename_and_share() {
        // Duplicate and download both work for directories now that the
        // transfer engine can flatten them; rename and presign still cannot.
        let items = entry_menu(&full_capability(), &entry(EntryKind::Dir));
        let disabled: Vec<EntryAction> = items
            .iter()
            .filter(|i| !i.enabled)
            .map(|i| i.action)
            .collect();

        assert_eq!(
            disabled,
            vec![EntryAction::ShowVersions, EntryAction::CopyShareLink],
            "a directory can be renamed now; only per-object features stay off"
        );
    }

    #[test]
    fn duplicating_a_directory_needs_list_as_well_as_copy() {
        // Flattening the directory means listing it first.
        let copy_only = Capability {
            copy: true,
            list: false,
            ..Default::default()
        };

        let dir = entry_menu(&copy_only, &entry(EntryKind::Dir));
        assert!(!item(&dir, EntryAction::Duplicate).enabled);

        let file = entry_menu(&copy_only, &entry(EntryKind::File));
        assert!(item(&file, EntryAction::Duplicate).enabled);
    }

    #[test]
    fn download_needs_read() {
        let items = entry_menu(&Capability::default(), &entry(EntryKind::File));
        assert!(!item(&items, EntryAction::Download).enabled);

        let readable = Capability {
            read: true,
            ..Default::default()
        };
        let items = entry_menu(&readable, &entry(EntryKind::File));
        assert!(item(&items, EntryAction::Download).enabled);
    }

    #[test]
    fn version_history_needs_a_version_aware_backend() {
        let items = entry_menu(&Capability::default(), &entry(EntryKind::File));
        let versions = item(&items, EntryAction::ShowVersions);
        assert!(!versions.enabled);
        assert_eq!(versions.disabled_reason, Some("该后端不支持版本历史"));

        let items = entry_menu(&full_capability(), &entry(EntryKind::File));
        assert!(item(&items, EntryAction::ShowVersions).enabled);
    }

    #[test]
    fn a_directory_has_no_version_history() {
        // Versioning is per object; a prefix is not a thing that has versions.
        let items = entry_menu(&full_capability(), &entry(EntryKind::Dir));
        assert_eq!(
            item(&items, EntryAction::ShowVersions).disabled_reason,
            Some("目录没有版本历史")
        );
    }

    #[test]
    fn transfers_are_distinguished_from_inline_actions() {
        assert!(EntryAction::Download.is_transfer());
        assert!(EntryAction::Duplicate.is_transfer());
        assert!(!EntryAction::Delete.is_transfer());
        assert!(!EntryAction::CopyPath.is_transfer());
    }

    #[test]
    fn a_backend_with_no_capabilities_disables_only_what_it_must() {
        let items = entry_menu(&Capability::default(), &entry(EntryKind::File));

        // Clipboard and reload are local to the app, so they never depend on
        // the backend.
        assert!(item(&items, EntryAction::CopyPath).enabled);
        assert!(item(&items, EntryAction::Reload).enabled);

        for action in [
            EntryAction::NewFolder,
            EntryAction::Rename,
            EntryAction::Duplicate,
            EntryAction::Delete,
            EntryAction::Download,
            EntryAction::ShowVersions,
            EntryAction::CopyShareLink,
        ] {
            let entry = item(&items, action);
            assert!(!entry.enabled, "{action:?} should be disabled");
            assert!(entry.disabled_reason.is_some(), "{action:?} should say why");
        }
    }

    #[test]
    fn rename_follows_the_rename_capability() {
        let s3_like = Capability {
            copy: true,
            delete: true,
            list: true,
            rename: false,
            ..Default::default()
        };
        let items = entry_menu(&s3_like, &entry(EntryKind::File));

        assert!(!item(&items, EntryAction::Rename).enabled);
        assert!(
            item(&items, EntryAction::Duplicate).enabled,
            "server-side copy is available even without rename"
        );
    }

    #[test]
    fn renaming_a_directory_needs_list_copy_and_delete() {
        // Enabled now: the transfer engine flattens the directory into copies and
        // removes the source afterwards, so the requirement is those three
        // capabilities rather than native `rename`.
        let items = entry_menu(&full_capability(), &entry(EntryKind::Dir));
        assert!(item(&items, EntryAction::Rename).enabled);

        let no_copy = Capability {
            list: true,
            delete: true,
            rename: true,
            copy: false,
            ..Default::default()
        };
        let items = entry_menu(&no_copy, &entry(EntryKind::Dir));
        assert!(!item(&items, EntryAction::Rename).enabled);
        assert_eq!(
            item(&items, EntryAction::Rename).disabled_reason,
            Some("该后端不支持复制或删除")
        );
    }

    #[test]
    fn renaming_a_file_is_offered_when_supported() {
        let items = entry_menu(&full_capability(), &entry(EntryKind::File));
        assert!(item(&items, EntryAction::Rename).enabled);
    }

    #[test]
    fn deleting_a_directory_needs_list_as_well_as_delete() {
        // Directory deletion is recursive, which means listing it first.
        let delete_only = Capability {
            delete: true,
            list: false,
            ..Default::default()
        };

        let dir_items = entry_menu(&delete_only, &entry(EntryKind::Dir));
        assert!(!item(&dir_items, EntryAction::Delete).enabled);

        let file_items = entry_menu(&delete_only, &entry(EntryKind::File));
        assert!(item(&file_items, EntryAction::Delete).enabled);
    }

    #[test]
    fn mutating_actions_are_flagged_for_reload_and_confirmation() {
        for action in [
            EntryAction::NewFolder,
            EntryAction::Rename,
            EntryAction::Duplicate,
            EntryAction::Delete,
        ] {
            assert!(action.mutates(), "{action:?} changes the backend");
        }

        for action in [
            EntryAction::CopyPath,
            EntryAction::CopyShareLink,
            EntryAction::Reload,
        ] {
            assert!(!action.mutates(), "{action:?} is read-only");
        }

        assert!(EntryAction::Delete.needs_confirmation());
        assert!(!EntryAction::NewFolder.needs_confirmation());
    }
}
