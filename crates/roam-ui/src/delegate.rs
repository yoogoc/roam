use std::rc::Rc;

use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::table::{Column, ColumnSort, TableDelegate, TableState};
use gpui_kit::component::{ActiveTheme, Icon, IconName, h_flex};
use gpui_kit::{App, Context, IntoElement, ParentElement, Styled, Window, div, px};
use roam_core::{DirEntry, EntryAction, EntryKind, SortKey, Vfs, ViewOptions, fmt, view_indices};

/// Invoked when a context-menu item is chosen. The `Browser` installs this so
/// all behaviour stays in the view that owns the session, while the delegate
/// only knows how to build the menu.
pub type ActionHandler = Rc<dyn Fn(EntryAction, DirEntry, &mut Window, &mut App)>;

const COL_NAME: usize = 0;
const COL_SIZE: usize = 1;
const COL_MODIFIED: usize = 2;

/// Supplies rows to the virtualized table.
///
/// The entry list is never reordered. Sorting produces a `Vec<u32>` index view
/// over it, so a sort click on a 100k-entry directory costs one index sort
/// rather than moving the entries themselves.
pub struct EntriesDelegate {
    entries: Vec<DirEntry>,
    view: Vec<u32>,
    columns: Vec<Column>,
    sort_key: SortKey,
    ascending: bool,
    /// Filter box text. Applies to the index view only.
    filter: String,
    /// Whether dotfiles are listed. Off by default, like a file manager.
    show_hidden: bool,
    loading: bool,
    /// The session the rows belong to; supplies the capability set the context
    /// menu is derived from.
    vfs: Option<Vfs>,
    on_action: Option<ActionHandler>,
}

impl EntriesDelegate {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            view: Vec::new(),
            columns: vec![
                Column::new("name", "名称").width(px(340.)).ascending(),
                Column::new("size", "大小")
                    .width(px(110.))
                    .text_right()
                    .sortable(),
                Column::new("modified", "修改时间")
                    .width(px(170.))
                    .sortable(),
            ],
            sort_key: SortKey::Name,
            ascending: true,
            filter: String::new(),
            show_hidden: false,
            loading: false,
            vfs: None,
            on_action: None,
        }
    }

    pub fn set_vfs(&mut self, vfs: Vfs) {
        self.vfs = Some(vfs);
    }

    pub fn set_action_handler(&mut self, handler: ActionHandler) {
        self.on_action = Some(handler);
    }

    /// The entry shown on `row_ix`, resolved through the sort index view.
    pub fn entry(&self, row_ix: usize) -> Option<&DirEntry> {
        self.view
            .get(row_ix)
            .and_then(|&i| self.entries.get(i as usize))
    }

    pub fn entries(&self) -> &[DirEntry] {
        &self.entries
    }

    pub fn is_loading(&self) -> bool {
        self.loading
    }

    /// Replace the contents outright, e.g. when serving a cache hit or applying
    /// the authoritative list once a directory finishes loading.
    pub fn reset(&mut self, entries: Vec<DirEntry>, loading: bool) {
        self.entries = entries;
        self.loading = loading;
        self.resort();
    }

    /// Append a streamed batch.
    ///
    /// Deliberately does not re-sort: re-sorting on every batch would make a
    /// large directory quadratic. Rows appear in arrival order while loading and
    /// get sorted once by `reset` at completion.
    pub fn extend(&mut self, batch: Vec<DirEntry>) {
        let start = self.entries.len() as u32;
        self.entries.extend(batch);

        // Newly streamed rows still have to pass the active filter *and* the
        // hidden-files setting, or either one would leak rows in while a large
        // directory loads.
        // Collected first so the immutable borrow of `entries` ends before `view`
        // is borrowed mutably. Bounded by the batch, not the directory.
        let admitted: Vec<u32> = (start..self.entries.len() as u32)
            .filter(|&i| {
                let entry = &self.entries[i as usize];
                (self.show_hidden || !entry.is_hidden())
                    && roam_core::matches_filter(&entry.name, &self.filter)
            })
            .collect();
        self.view.extend(admitted);
    }

    /// Set the filter and rebuild the index view.
    pub fn set_filter(&mut self, filter: &str) {
        self.filter = filter.to_string();
        self.resort();
    }

    pub fn filter(&self) -> &str {
        &self.filter
    }

    /// How many entries exist regardless of the filter, for the status bar.
    pub fn total(&self) -> usize {
        self.entries.len()
    }

    /// How many rows pass the filter, i.e. how many the table draws.
    pub fn shown(&self) -> usize {
        self.view.len()
    }

    fn resort(&mut self) {
        self.view = view_indices(&self.entries, self.options());
    }

    fn options(&self) -> ViewOptions<'_> {
        ViewOptions {
            key: self.sort_key,
            ascending: self.ascending,
            filter: &self.filter,
            show_hidden: self.show_hidden,
        }
    }

    /// Are dotfiles listed?
    pub fn show_hidden(&self) -> bool {
        self.show_hidden
    }

    pub fn set_show_hidden(&mut self, show: bool) {
        self.show_hidden = show;
        self.resort();
    }
}

impl Default for EntriesDelegate {
    fn default() -> Self {
        Self::new()
    }
}

impl TableDelegate for EntriesDelegate {
    fn columns_count(&self, _: &App) -> usize {
        self.columns.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.view.len()
    }

    // Returns by value now (upstream changed the trait). We still own the
    // columns — the sort arrow reads from them — so this is a clone of a small
    // descriptor, not of any row data.
    fn column(&self, col_ix: usize, _: &App) -> Column {
        self.columns[col_ix].clone()
    }

    fn loading(&self, _: &App) -> bool {
        // Only show the skeleton before anything has arrived. Once rows exist,
        // keep showing them and let more stream in underneath.
        self.loading && self.entries.is_empty()
    }

    fn perform_sort(
        &mut self,
        col_ix: usize,
        sort: ColumnSort,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) {
        self.sort_key = match col_ix {
            COL_SIZE => SortKey::Size,
            COL_MODIFIED => SortKey::Modified,
            _ => SortKey::Name,
        };
        self.ascending = sort != ColumnSort::Descending;

        // The header arrow reads from the columns we own, so move the active
        // marker here and clear it everywhere else.
        for (ix, column) in self.columns.iter_mut().enumerate() {
            column.sort = Some(if ix == col_ix {
                sort
            } else {
                ColumnSort::Default
            });
        }

        self.resort();
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let Some(entry) = self.entry(row_ix) else {
            return div().into_any_element();
        };

        match col_ix {
            COL_NAME => {
                let icon = if entry.is_dir() {
                    IconName::Folder
                } else {
                    IconName::File
                };
                let color = if entry.is_dir() {
                    cx.theme().accent_foreground
                } else {
                    cx.theme().muted_foreground
                };

                h_flex()
                    .gap_2()
                    .items_center()
                    .child(Icon::new(icon).size_4().text_color(color))
                    .child(entry.name.to_string())
                    .into_any_element()
            }
            COL_SIZE => {
                // Directories have no size; files whose listing omitted it show
                // an em dash rather than a misleading "0 B".
                let text = if entry.is_dir() {
                    fmt::MISSING.to_string()
                } else {
                    fmt::size(entry.size)
                };

                div()
                    .w_full()
                    .text_right()
                    .text_color(cx.theme().muted_foreground)
                    .child(text)
                    .into_any_element()
            }
            COL_MODIFIED => div()
                .text_color(cx.theme().muted_foreground)
                .child(fmt::modified(entry.modified))
                .into_any_element(),
            _ => div().into_any_element(),
        }
    }

    fn context_menu(
        &mut self,
        row_ix: usize,
        menu: PopupMenu,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> PopupMenu {
        let (Some(vfs), Some(entry)) = (self.vfs.clone(), self.entry(row_ix).cloned()) else {
            return menu;
        };

        let mut menu = menu;
        for item in vfs.entry_menu(&entry) {
            let handler = self.on_action.clone();
            let action = item.action;
            let entry = entry.clone();

            menu = menu.item(
                PopupMenuItem::new(item.display_label())
                    .disabled(!item.enabled)
                    .on_click(move |_, window, cx| {
                        if let Some(handler) = handler.as_ref() {
                            handler(action, entry.clone(), window, cx);
                        }
                    }),
            );
        }

        menu
    }

    fn render_empty(
        &mut self,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        h_flex()
            .size_full()
            .justify_center()
            .items_center()
            .gap_2()
            .text_color(cx.theme().muted_foreground)
            .child(Icon::new(IconName::Inbox).size_5())
            .child("这个目录是空的")
            .into_any_element()
    }
}

/// A row's kind, for callers that only need the classification.
pub fn kind_label(kind: EntryKind) -> &'static str {
    match kind {
        EntryKind::Dir => "目录",
        EntryKind::File => "文件",
        EntryKind::Unknown => "未知",
    }
}

#[cfg(test)]
mod scale_tests {
    //! The view-layer half of the 100k claim. `roam-core/tests/scale.rs` measures
    //! the index view; this checks that the delegate the table renders from
    //! behaves at that size — resolving a row near the far end, re-sorting, and
    //! filtering, all without touching the entry list.

    use super::*;
    use std::time::{Duration, Instant};

    const ROWS: usize = 100_000;

    fn entries(count: usize) -> Vec<DirEntry> {
        (0..count)
            .map(|i| {
                let name = format!("row-{i:06}.txt");
                DirEntry {
                    name: name.as_str().into(),
                    path: name.as_str().into(),
                    kind: EntryKind::File,
                    size: Some(i as u64),
                    modified: None,
                    etag: None,
                    meta_complete: true,
                }
            })
            .collect()
    }

    #[test]
    fn the_delegate_resolves_the_last_row_of_a_hundred_thousand() {
        let mut delegate = EntriesDelegate::new();
        delegate.reset(entries(ROWS), false);

        assert_eq!(delegate.shown(), ROWS);
        assert_eq!(delegate.total(), ROWS);

        // Row lookup goes through the index view, so the far end must be as cheap
        // as the near end — a linear scan per row would show up here.
        let start = Instant::now();
        let last = delegate.entry(ROWS - 1).expect("the last row exists");
        let elapsed = start.elapsed();

        assert_eq!(&*last.name, "row-099999.txt");
        assert!(elapsed < Duration::from_millis(50), "took {elapsed:?}");
        assert!(
            delegate.entry(ROWS).is_none(),
            "and one past the end is None"
        );
    }

    #[test]
    fn filtering_a_hundred_thousand_rows_keeps_the_entries_intact() {
        let mut delegate = EntriesDelegate::new();
        delegate.reset(entries(ROWS), false);

        let start = Instant::now();
        delegate.set_filter("row-0999");
        let filter_time = start.elapsed();

        assert!(delegate.shown() < ROWS / 10);
        assert_eq!(
            delegate.total(),
            ROWS,
            "filtering hides rows; it must not discard them"
        );

        let start = Instant::now();
        delegate.set_filter("");
        let clear_time = start.elapsed();
        assert_eq!(delegate.shown(), ROWS, "clearing restores every row");

        eprintln!("100k filter: {filter_time:?}, clear: {clear_time:?}");
        assert!(filter_time < Duration::from_secs(5));
        assert!(clear_time < Duration::from_secs(5));
    }

    #[test]
    fn streaming_batches_into_a_hundred_thousand_rows_stays_linear() {
        // Mirrors what a real listing does: 200 batches of 500. Re-sorting on
        // every batch would make this quadratic, which is why `extend` appends
        // instead.
        let mut delegate = EntriesDelegate::new();
        let all = entries(ROWS);

        let start = Instant::now();
        for chunk in all.chunks(500) {
            delegate.extend(chunk.to_vec());
        }
        let elapsed = start.elapsed();

        eprintln!("100k in 500-row batches: {elapsed:?}");
        assert_eq!(delegate.shown(), ROWS);
        assert!(
            elapsed < Duration::from_secs(5),
            "streaming took {elapsed:?}, which suggests a re-sort per batch"
        );
    }

    #[test]
    fn a_filter_applies_to_rows_that_arrive_later() {
        let mut delegate = EntriesDelegate::new();
        delegate.set_filter("row-00001");

        // Typing a filter while a large directory is still loading must not let
        // non-matching rows leak in behind it.
        for chunk in entries(10_000).chunks(500) {
            delegate.extend(chunk.to_vec());
        }

        assert!(delegate.shown() > 0);
        assert!(
            delegate.shown() < 100,
            "only matching rows should be visible, got {}",
            delegate.shown()
        );
        for ix in 0..delegate.shown() {
            let entry = delegate.entry(ix).unwrap();
            assert!(entry.name.contains("row-00001"), "leaked {}", entry.name);
        }
    }
}

#[cfg(test)]
mod hidden_tests {
    //! Exercises the delegate's filtering directly, without window setup.
    //! Browser tests cover the same behavior through GPUI interactions.

    use super::*;

    fn entry(name: &str, kind: EntryKind) -> DirEntry {
        DirEntry {
            name: name.into(),
            path: name.into(),
            kind,
            size: Some(1),
            modified: None,
            etag: None,
            meta_complete: true,
        }
    }

    fn listing() -> Vec<DirEntry> {
        vec![
            entry("visible.txt", EntryKind::File),
            entry(".hidden.txt", EntryKind::File),
            entry(".git", EntryKind::Dir),
            entry("docs", EntryKind::Dir),
        ]
    }

    fn names(delegate: &EntriesDelegate) -> Vec<String> {
        (0..delegate.shown())
            .filter_map(|ix| delegate.entry(ix))
            .map(|entry| entry.name.to_string())
            .collect()
    }

    #[test]
    fn dotfiles_are_hidden_until_asked_for() {
        let mut delegate = EntriesDelegate::new();
        delegate.reset(listing(), false);

        assert!(!delegate.show_hidden());
        assert_eq!(names(&delegate), vec!["docs", "visible.txt"]);
        // The count of what exists is unchanged; only what is drawn differs.
        assert_eq!(delegate.total(), 4);
        assert_eq!(delegate.shown(), 2);

        delegate.set_show_hidden(true);
        assert_eq!(
            names(&delegate),
            vec![".git", "docs", ".hidden.txt", "visible.txt"]
        );
    }

    #[test]
    fn the_setting_survives_a_new_listing() {
        // Navigating into another directory must not silently re-hide them —
        // otherwise the toggle would appear to undo itself on every click into a
        // folder.
        let mut delegate = EntriesDelegate::new();
        delegate.set_show_hidden(true);
        delegate.reset(listing(), false);

        assert!(delegate.show_hidden());
        assert_eq!(delegate.shown(), 4);
    }

    #[test]
    fn streamed_rows_respect_the_setting() {
        // `extend` has its own filtering path, separate from `resort`. A batch
        // arriving while hidden files are off used to be the way they leaked in.
        let mut delegate = EntriesDelegate::new();
        delegate.reset(Vec::new(), true);
        delegate.extend(listing());

        assert_eq!(delegate.shown(), 2, "streamed dotfiles stayed hidden");

        delegate.set_show_hidden(true);
        delegate.extend(vec![entry(".env", EntryKind::File)]);
        assert_eq!(delegate.shown(), 5, "and are admitted once shown");
    }

    #[test]
    fn the_filter_and_the_setting_compose() {
        let mut delegate = EntriesDelegate::new();
        delegate.reset(listing(), false);

        delegate.set_filter("hidden");
        assert_eq!(delegate.shown(), 0, "hidden wins over a matching filter");

        delegate.set_show_hidden(true);
        assert_eq!(names(&delegate), vec![".hidden.txt"]);
    }
}
