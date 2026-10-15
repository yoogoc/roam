//! Session handles stay in memory. The system clipboard carries only paths
//! and a marker, so copying text elsewhere invalidates a pending file paste.
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use gpui_kit::{App, ClipboardItem, Global};
use roam_core::{DirEntry, Vfs};

static NEXT_COPY: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
pub(crate) struct CopiedEntries {
    pub source: Vfs,
    pub entries: Arc<Vec<DirEntry>>,
    marker: String,
}

#[derive(Default)]
pub(crate) struct FileClipboard(pub Option<CopiedEntries>);
impl Global for FileClipboard {}

pub(crate) fn copy(source: Vfs, entries: Vec<DirEntry>, cx: &mut App) {
    let marker = format!(
        "roam-files:{}:{}",
        std::process::id(),
        NEXT_COPY.fetch_add(1, Ordering::Relaxed)
    );
    let paths = entries
        .iter()
        .map(|entry| entry.path.as_ref())
        .collect::<Vec<_>>()
        .join("\n");
    cx.write_to_clipboard(ClipboardItem::new_string_with_metadata(
        paths,
        marker.clone(),
    ));
    cx.set_global(FileClipboard(Some(CopiedEntries {
        source,
        entries: Arc::new(entries),
        marker,
    })));
}

pub(crate) fn read(cx: &mut App) -> Option<CopiedEntries> {
    let copied = cx.try_global::<FileClipboard>()?.0.clone()?;
    if cx
        .read_from_clipboard()
        .as_ref()
        .and_then(ClipboardItem::metadata)
        == Some(&copied.marker)
    {
        Some(copied)
    } else {
        cx.set_global(FileClipboard::default());
        None
    }
}
