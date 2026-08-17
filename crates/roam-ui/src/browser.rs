use std::future::Future;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    AppContext, ClickEvent, ClipboardItem, Context, Entity, FocusHandle, InteractiveElement,
    IntoElement, ParentElement, Render, SharedString, Styled, Subscription, Task, Window, div,
    prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariant, ButtonVariants};
use gpui_component::dialog::DialogButtonProps;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::table::{DataTable, TableEvent, TableState};
use gpui_component::{
    ActiveTheme, Disableable, Icon, IconName, Selectable, Sizable, WindowExt,
    breadcrumb::Breadcrumb, breadcrumb::BreadcrumbItem, h_flex, v_flex,
};
use roam_core::transfer::{
    Transfer, plan_download, plan_duplicate_dir, plan_move_dir, plan_upload,
};
use roam_core::{
    DirEntry, EntryAction, Error, Generation, ListingCache, ObjectVersion, RemotePath, SessionId,
    TransferEngine, Vfs, path,
};

use crate::actions::{
    BROWSER_CONTEXT, DeleteSelected, DownloadSelected, FocusFilter, GoBack, GoForward, GoUp,
    NewFolder, OpenSelected, Reload, ToggleHidden, TogglePreview,
};
use crate::delegate::EntriesDelegate;
use crate::name_dialog::NameDialog;
use crate::placeholders;
use crate::preview::PreviewPanel;

/// How long a copied share link stays valid.
const SHARE_LINK_TTL: Duration = Duration::from_secs(600);

/// Fired after transfers are queued, so the panel can start polling.
type TransfersQueued = Rc<dyn Fn(&mut Window, &mut gpui::App)>;

/// Fired when the shown directory changes, so the sidebar tree can follow.
type DirectoryChanged = Rc<dyn Fn(Arc<str>, &mut gpui::App)>;

/// One browsing pane over one backend session.
pub struct Browser {
    vfs: Vfs,
    engine: TransferEngine,
    /// Called after transfers are queued so the panel starts polling.
    on_transfers_queued: Option<TransfersQueued>,
    /// Called when the shown directory changes, so the sidebar tree can follow.
    on_directory_changed: Option<DirectoryChanged>,
    cache: Arc<ListingCache>,
    session: SessionId,
    table: Entity<TableState<EntriesDelegate>>,

    /// Current directory in OpenDAL directory form (`""` is the root).
    cwd: Arc<str>,
    back: Vec<Arc<str>>,
    forward: Vec<Arc<str>>,

    /// Incremented on every navigation. Batches tagged with an older generation
    /// are discarded, so a slow listing cannot overwrite a newer directory.
    generation: Generation,
    loading: bool,
    error: Option<Error>,

    /// Mutations in flight. Kept as state rather than fire-and-forget so the
    /// pane can report that it is busy — and so tests can wait for the work
    /// instead of guessing at a delay.
    pending_mutations: usize,

    /// Holding the task here means dropping/replacing it aborts the listing.
    listing: Option<Task<()>>,
    /// Keeps the active name prompt's input state alive while its dialog is up.
    name_dialog: Option<Arc<NameDialog>>,

    /// Filter box. Its text drives the table's index view; the entries
    /// themselves are never touched, so clearing it costs nothing.
    filter: Entity<InputState>,
    focus: FocusHandle,
    preview_open: bool,
    preview: Entity<PreviewPanel>,

    _subscriptions: Vec<Subscription>,
}

impl Browser {
    pub fn new(
        vfs: Vfs,
        engine: TransferEngine,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let table = cx.new(|cx| {
            TableState::new(EntriesDelegate::new(), window, cx)
                .row_selectable(true)
                .sortable(true)
        });

        let filter = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(placeholders::FILTER)
                // Escape clears the box natively; see the note in actions.rs for
                // why this is not an action.
                .clean_on_escape()
        });

        let preview = cx.new(|_| PreviewPanel::new(vfs.clone()));

        let subscriptions = vec![
            cx.subscribe_in(&table, window, Self::on_table_event),
            cx.subscribe_in(&filter, window, Self::on_filter_event),
        ];

        // The delegate builds the context menu from the session's capability
        // set, but routes the chosen action back here, where the session lives.
        let weak = cx.weak_entity();
        table.update(cx, |state, _| {
            state.delegate_mut().set_vfs(vfs.clone());
            state
                .delegate_mut()
                .set_action_handler(Rc::new(move |action, entry, window, cx| {
                    let _ = weak.update(cx, |this, cx| {
                        this.on_entry_action(action, entry, window, cx);
                    });
                }));
        });

        let mut this = Self {
            vfs,
            engine,
            on_transfers_queued: None,
            on_directory_changed: None,
            cache: Arc::new(ListingCache::new()),
            session: 0,
            table,
            cwd: "".into(),
            back: Vec::new(),
            forward: Vec::new(),
            generation: 0,
            loading: false,
            error: None,
            pending_mutations: 0,
            listing: None,
            name_dialog: None,
            filter,
            focus: cx.focus_handle(),
            preview_open: false,
            preview,
            _subscriptions: subscriptions,
        };

        this.navigate("".into(), false, cx);
        this
    }

    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    pub fn label(&self) -> &str {
        self.vfs.label()
    }

    pub fn vfs(&self) -> &Vfs {
        &self.vfs
    }

    /// Navigate from outside — the sidebar tree uses this.
    pub fn navigate_to_dir(&mut self, dir: &str, cx: &mut Context<Self>) {
        self.navigate(dir.into(), true, cx);
    }

    /// Install a callback fired whenever the shown directory changes, so the
    /// sidebar tree can follow along.
    pub fn on_directory_changed(&mut self, callback: impl Fn(Arc<str>, &mut gpui::App) + 'static) {
        self.on_directory_changed = Some(Rc::new(callback));
    }

    /// Install a callback fired whenever transfers are queued, so the transfer
    /// panel can begin polling for progress.
    pub fn on_transfers_queued(
        &mut self,
        callback: impl Fn(&mut Window, &mut gpui::App) + 'static,
    ) {
        self.on_transfers_queued = Some(Rc::new(callback));
    }

    /// The pane's current error, if any — a failed listing shows here rather
    /// than in the connection banner.
    pub fn error_message(&self) -> Option<String> {
        self.error.as_ref().map(|err| err.user_message())
    }

    /// True while a listing or a mutation is still running.
    pub fn is_busy(&self) -> bool {
        self.loading || self.pending_mutations > 0
    }

    /// Switch to a different backend session.
    ///
    /// History and cache are per-session, so both are cleared: a path from the
    /// previous connection means nothing here, and reusing the cache would show
    /// one backend's listing under another's name.
    pub fn set_vfs(&mut self, vfs: Vfs, cx: &mut Context<Self>) {
        self.vfs = vfs.clone();
        self.cache = Arc::new(ListingCache::new());
        self.back.clear();
        self.forward.clear();
        self.error = None;

        self.table.update(cx, |state, _| {
            state.delegate_mut().set_vfs(vfs.clone());
        });
        self.preview.update(cx, |panel, cx| panel.set_vfs(vfs, cx));

        self.navigate("".into(), false, cx);
    }

    fn on_entry_action(
        &mut self,
        action: EntryAction,
        entry: roam_core::DirEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            EntryAction::NewFolder => self.prompt_new_folder(window, cx),
            EntryAction::Rename => self.prompt_rename(entry, window, cx),
            EntryAction::Duplicate => self.duplicate(entry, window, cx),
            EntryAction::Download => self.download(entry, window, cx),
            EntryAction::ShowVersions => self.show_versions(entry, window, cx),
            EntryAction::Delete => self.confirm_delete(entry, window, cx),
            EntryAction::CopyPath => {
                cx.write_to_clipboard(ClipboardItem::new_string(entry.path.to_string()));
                window.push_notification("已复制路径", cx);
            }
            EntryAction::Reload => self.reload(cx),
            EntryAction::CopyShareLink => {
                let vfs = self.vfs.clone();
                let path = entry.path.to_string();
                let handle = window.window_handle();

                cx.spawn(async move |_, cx| {
                    let signed = vfs.presign_read(&path, SHARE_LINK_TTL).await;

                    let message = match signed {
                        Ok(Some(url)) => {
                            cx.update(|cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(url));
                            });
                            "已复制分享链接（10 分钟内有效）".to_string()
                        }
                        // The menu item is disabled in this case, so reaching
                        // here means the capability check and the menu
                        // disagreed — say so rather than fail silently.
                        Ok(None) => "该后端不支持分享链接".to_string(),
                        Err(err) => err.user_message(),
                    };

                    let _ = handle.update(cx, |_, window, cx| {
                        window.push_notification(message, cx);
                    });
                })
                .detach();
            }
        }
    }

    // ── keyboard ──────────────────────────────────────────────────────────

    fn on_filter_event(
        &mut self,
        _: &Entity<InputState>,
        event: &InputEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if matches!(event, InputEvent::Change) {
            let filter = self.filter.read(cx).value().to_string();
            self.table.update(cx, |state, cx| {
                state.delegate_mut().set_filter(&filter);
                state.refresh(cx);
            });
            cx.notify();
        }
    }

    /// The row the keyboard acts on. `None` when nothing is selected, which is
    /// why every shortcut below is a no-op rather than acting on row 0.
    fn selected_entry(&self, cx: &Context<Self>) -> Option<DirEntry> {
        let state = self.table.read(cx);
        state.delegate().entry(state.selected_row()?).cloned()
    }

    fn action_open(&mut self, _: &OpenSelected, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(entry) = self.selected_entry(cx).filter(|e| e.is_dir()) {
            self.navigate(entry.path.clone(), true, cx);
        }
    }

    fn action_go_up(&mut self, _: &GoUp, _: &mut Window, cx: &mut Context<Self>) {
        self.go_up(cx);
    }

    fn action_go_back(&mut self, _: &GoBack, _: &mut Window, cx: &mut Context<Self>) {
        self.go_back(cx);
    }

    fn action_go_forward(&mut self, _: &GoForward, _: &mut Window, cx: &mut Context<Self>) {
        self.go_forward(cx);
    }

    fn action_reload(&mut self, _: &Reload, _: &mut Window, cx: &mut Context<Self>) {
        self.reload(cx);
    }

    fn action_focus_filter(
        &mut self,
        _: &FocusFilter,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.filter.update(cx, |state, cx| state.focus(window, cx));
    }

    fn action_new_folder(&mut self, _: &NewFolder, window: &mut Window, cx: &mut Context<Self>) {
        if self.vfs.capability().create_dir {
            self.prompt_new_folder(window, cx);
        }
    }

    fn action_delete(&mut self, _: &DeleteSelected, window: &mut Window, cx: &mut Context<Self>) {
        // Goes through the same confirmation as the menu — a shortcut must not
        // be a faster way to lose data.
        if let Some(entry) = self.selected_entry(cx) {
            self.confirm_delete(entry, window, cx);
        }
    }

    fn action_download(
        &mut self,
        _: &DownloadSelected,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(entry) = self.selected_entry(cx) {
            self.download(entry, window, cx);
        }
    }

    fn action_toggle_hidden(&mut self, _: &ToggleHidden, _: &mut Window, cx: &mut Context<Self>) {
        self.toggle_hidden(cx);
    }

    /// Show or hide dotfiles.
    ///
    /// Rebuilds the index view rather than re-listing: the entries are already
    /// here, and re-fetching a large directory to change a display setting would
    /// be the wrong trade entirely.
    fn toggle_hidden(&mut self, cx: &mut Context<Self>) {
        self.table.update(cx, |state, cx| {
            let delegate = state.delegate_mut();
            let show = !delegate.show_hidden();
            delegate.set_show_hidden(show);
            state.refresh(cx);
        });
        cx.notify();
    }

    /// Whether dotfiles are currently listed, for the toolbar button's state.
    fn show_hidden(&self, cx: &gpui::App) -> bool {
        self.table.read(cx).delegate().show_hidden()
    }

    fn action_toggle_preview(&mut self, _: &TogglePreview, _: &mut Window, cx: &mut Context<Self>) {
        self.preview_open = !self.preview_open;

        // Selection may have moved by keyboard while the panel was hidden, so
        // sync on open rather than only on the SelectRow event.
        if self.preview_open {
            let entry = self.selected_entry(cx);
            self.preview
                .update(cx, |panel, cx| panel.set_entry(entry, cx));
        }
        cx.notify();
    }

    pub fn preview_open(&self) -> bool {
        self.preview_open
    }

    fn key(&self, dir: &str) -> RemotePath {
        RemotePath::new(self.session, dir)
    }

    // ── navigation ────────────────────────────────────────────────────────

    fn navigate(&mut self, to: Arc<str>, push_history: bool, cx: &mut Context<Self>) {
        if push_history && *self.cwd != *to {
            self.back.push(self.cwd.clone());
            self.forward.clear();
        }

        self.generation += 1;
        let generation = self.generation;
        self.cwd = to.clone();
        self.error = None;
        self.loading = true;

        // Serve the cached listing first so revisiting a directory paints
        // immediately; the fresh listing replaces it when it completes.
        let cached = self.cache.get(&self.key(&to));
        self.table.update(cx, |state, cx| {
            let entries = cached
                .map(|hit| hit.entries.as_ref().clone())
                .unwrap_or_default();
            state.delegate_mut().reset(entries, true);
            state.refresh(cx);
        });

        let vfs = self.vfs.clone();
        let dir = to.clone();

        if let Some(callback) = self.on_directory_changed.clone() {
            let dir = to.clone();
            // Deferred: this runs during a Browser update, and the callback
            // updates a sibling entity.
            cx.defer(move |cx| callback(dir, cx));
        }

        self.listing = Some(cx.spawn(async move |this, cx| {
            let mut listing = vfs.list(&dir);
            let mut collected = Vec::new();
            let mut first_batch = true;

            while let Some(batch) = listing.next_batch().await {
                match batch {
                    Ok(batch) => {
                        collected.extend(batch.iter().cloned());
                        let replace = std::mem::take(&mut first_batch);
                        let applied = this
                            .update(cx, |this, cx| this.on_batch(generation, batch, replace, cx));
                        // The view is gone — stop listing rather than keep
                        // paying for a pane nobody is looking at.
                        if applied.is_err() {
                            return;
                        }
                    }
                    Err(err) => {
                        let _ = this.update(cx, |this, cx| this.on_error(generation, err, cx));
                        return;
                    }
                }
            }

            let _ = this.update(cx, |this, cx| this.on_complete(generation, collected, cx));
        }));

        cx.notify();
    }

    fn on_batch(
        &mut self,
        generation: Generation,
        batch: Vec<roam_core::DirEntry>,
        replace: bool,
        cx: &mut Context<Self>,
    ) {
        if generation != self.generation {
            return; // stale: the user navigated away
        }

        self.table.update(cx, |state, cx| {
            let delegate = state.delegate_mut();
            if replace {
                // First fresh batch supersedes whatever the cache painted.
                delegate.reset(batch, true);
            } else {
                delegate.extend(batch);
            }
            state.refresh(cx);
        });

        cx.notify();
    }

    fn on_complete(
        &mut self,
        generation: Generation,
        entries: Vec<roam_core::DirEntry>,
        cx: &mut Context<Self>,
    ) {
        if generation != self.generation {
            return;
        }

        self.cache
            .put(self.key(&self.cwd.clone()), entries.clone(), true);

        // Apply the full list once, sorted. This is also what makes an empty
        // fresh listing correctly clear stale cached rows.
        self.table.update(cx, |state, cx| {
            state.delegate_mut().reset(entries, false);
            state.refresh(cx);
        });

        self.loading = false;
        cx.notify();
    }

    fn on_error(&mut self, generation: Generation, err: Error, cx: &mut Context<Self>) {
        if generation != self.generation || err.is_cancelled() {
            return;
        }

        self.loading = false;
        self.error = Some(err);
        self.table.update(cx, |state, cx| {
            state.delegate_mut().reset(Vec::new(), false);
            state.refresh(cx);
        });
        cx.notify();
    }

    fn on_table_event(
        &mut self,
        _: &Entity<TableState<EntriesDelegate>>,
        event: &TableEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let TableEvent::SelectRow(row_ix) = event {
            let entry = self.table.read(cx).delegate().entry(*row_ix).cloned();
            self.preview
                .update(cx, |panel, cx| panel.set_entry(entry, cx));
        }

        if let TableEvent::DoubleClickedRow(row_ix) = event {
            let target = self
                .table
                .read(cx)
                .delegate()
                .entry(*row_ix)
                .filter(|entry| entry.is_dir())
                .map(|entry| entry.path.clone());

            if let Some(target) = target {
                self.navigate(target, true, cx);
            }
        }
    }

    // ── mutations ─────────────────────────────────────────────────────────

    /// Run a backend mutation, report the outcome, and reload on success.
    ///
    /// Reloading is not optional: after a rename or delete the cached listing is
    /// wrong, and showing a stale row that no longer exists is worse than a
    /// brief spinner.
    fn run_mutation<F>(
        &mut self,
        done_message: impl Into<SharedString>,
        operation: F,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) where
        F: Future<Output = roam_core::Result<()>> + Send + 'static,
    {
        let done_message: SharedString = done_message.into();
        let handle = window.window_handle();
        self.pending_mutations += 1;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let outcome = operation.await;

            let message = match &outcome {
                Ok(()) => done_message.to_string(),
                Err(err) => err.user_message(),
            };
            let _ = handle.update(cx, |_, window, cx| {
                window.push_notification(message, cx);
            });

            let _ = this.update(cx, |this, cx| {
                if outcome.is_ok() {
                    this.reload(cx);
                }
                this.pending_mutations = this.pending_mutations.saturating_sub(1);
                cx.notify();
            });
        })
        .detach();
    }

    /// Validate a name and create the folder. `Err(reason)` means the name is
    /// unusable and the prompt should stay open.
    ///
    /// Split out of the dialog closure so the rules have one home and the tests
    /// can drive the operation without simulating a click.
    fn create_folder(
        &mut self,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), &'static str> {
        path::validate_name(name)?;

        let target = path::join_dir(&self.cwd, name);
        let operation = self.vfs.create_dir(&target);
        self.run_mutation("新建文件夹完成", operation, window, cx);
        Ok(())
    }

    /// Rename `entry`. Renaming to the current name is a no-op, not an error.
    fn rename_entry(
        &mut self,
        entry: &DirEntry,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), &'static str> {
        path::validate_name(name)?;

        if name == &*entry.name {
            return Ok(());
        }

        let target = path::sibling(&entry.path, name);

        if entry.is_dir() {
            // No backend renames a directory in one call, so this becomes a
            // visible move task: copy every file, then delete the source. Shown
            // in the transfer panel with progress and a cancel, rather than a
            // spinner pretending to be an atomic rename.
            let vfs = self.vfs.clone();
            let from = entry.path.to_string();
            self.plan_and_queue("移动", window, cx, async move {
                plan_move_dir(&vfs, &from, &target)
                    .await
                    .map(|task| vec![task])
            });
            return Ok(());
        }

        let operation = self.vfs.rename(&entry.path, &target);
        self.run_mutation("重命名完成", operation, window, cx);
        Ok(())
    }

    fn delete_entry(&mut self, entry: &DirEntry, window: &mut Window, cx: &mut Context<Self>) {
        // On a version-aware backend, deleting a directory writes a delete
        // marker per object; the *prefix* can still come back in a listing, so
        // the row stays even though everything under it is gone. Saying so turns
        // "did that even work?" into an explanation.
        let versioned_dir = entry.is_dir() && self.vfs.capability().list_with_versions;
        let done: SharedString = if versioned_dir {
            "已删除目录内容；该后端保留版本历史，空目录名可能仍会显示".into()
        } else {
            "删除完成".into()
        };

        let operation = self.vfs.delete(entry);
        self.run_mutation(done, operation, window, cx);
    }

    /// Open the new-folder prompt. Public because the toolbar is not the only
    /// caller: `examples/name_dialog.rs` uses it to lay the dialog out against
    /// the real platform text system, which the tests cannot do.
    pub fn open_new_folder_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.prompt_new_folder(window, cx);
    }

    fn prompt_new_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dialog = Arc::new(NameDialog::new("", placeholders::NEW_FOLDER, window, cx));
        self.name_dialog = Some(dialog.clone());

        let this = cx.weak_entity();

        window.open_dialog(cx, move |builder, _window, _cx| {
            let dialog = dialog.clone();
            let this = this.clone();

            builder
                .title("新建文件夹")
                .w(px(420.))
                .button_props(
                    DialogButtonProps::default()
                        .ok_text("创建")
                        .cancel_text("取消")
                        .show_cancel(true),
                )
                .child(Input::new(&dialog.input))
                .on_ok(move |_, window, cx| {
                    let name = dialog.value(cx);

                    this.update(cx, |browser, cx| {
                        match browser.create_folder(&name, window, cx) {
                            Ok(()) => true,
                            Err(reason) => {
                                // Keep the dialog open so the typing survives.
                                window.push_notification(reason, cx);
                                false
                            }
                        }
                    })
                    .unwrap_or(false)
                })
        });
    }

    fn prompt_rename(
        &mut self,
        entry: roam_core::DirEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let dialog = Arc::new(NameDialog::new(
            &entry.name,
            placeholders::RENAME,
            window,
            cx,
        ));
        self.name_dialog = Some(dialog.clone());

        let this = cx.weak_entity();
        let title: SharedString = format!("重命名 · {}", entry.name).into();

        window.open_dialog(cx, move |builder, _window, _cx| {
            let dialog = dialog.clone();
            let this = this.clone();
            let entry = entry.clone();

            builder
                .title(title.clone())
                .w(px(420.))
                .button_props(
                    DialogButtonProps::default()
                        .ok_text("重命名")
                        .cancel_text("取消")
                        .show_cancel(true),
                )
                .child(Input::new(&dialog.input))
                .on_ok(move |_, window, cx| {
                    let name = dialog.value(cx);

                    this.update(cx, |browser, cx| {
                        match browser.rename_entry(&entry, &name, window, cx) {
                            Ok(()) => true,
                            Err(reason) => {
                                window.push_notification(reason, cx);
                                false
                            }
                        }
                    })
                    .unwrap_or(false)
                })
        });
    }

    /// A free name for a copy of `entry`, checked against the current listing so
    /// the copy cannot silently overwrite a sibling.
    fn free_copy_name(&self, entry: &DirEntry, cx: &Context<Self>) -> String {
        let taken: Vec<String> = self
            .table
            .read(cx)
            .delegate()
            .entries()
            .iter()
            .map(|e| e.name.to_string())
            .collect();
        let taken: Vec<&str> = taken.iter().map(String::as_str).collect();

        path::duplicate_name(&entry.name, &taken)
    }

    fn duplicate(&mut self, entry: DirEntry, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.free_copy_name(&entry, cx);
        let target = path::sibling(&entry.path, &name);

        if !entry.is_dir() {
            // A single object: one server-side copy, fast enough to run inline
            // and report as a mutation.
            let operation = self.vfs.copy(&entry.path, &target);
            self.run_mutation("创建副本完成", operation, window, cx);
            return;
        }

        // A directory has to be flattened into one copy per file, which is
        // transfer-engine work — it needs progress and a cancel.
        let vfs = self.vfs.clone();
        self.plan_and_queue("创建副本", window, cx, async move {
            plan_duplicate_dir(&vfs, &entry, &target).await
        });
    }

    fn download(&mut self, entry: DirEntry, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dest) = roam_core::dirs::downloads() else {
            window.push_notification("找不到下载目录", cx);
            return;
        };

        let vfs = self.vfs.clone();
        self.plan_and_queue("下载", window, cx, async move {
            plan_download(&vfs, &entry, &dest).await
        });
    }

    /// Upload paths dropped from the Finder into the current directory.
    fn upload_dropped(
        &mut self,
        paths: Vec<std::path::PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if paths.is_empty() {
            return;
        }

        if !self.vfs.capability().write {
            window.push_notification(
                SharedString::from(format!("{} 不支持写入", self.vfs.label())),
                cx,
            );
            return;
        }

        // Walking local directories touches the filesystem, so it happens off
        // the render thread even though it is usually quick.
        let vfs = self.vfs.clone();
        let cwd = self.cwd.to_string();
        self.plan_and_queue("上传", window, cx, async move {
            Ok(plan_upload(&vfs, &paths, &cwd))
        });
    }

    /// Expand a plan, queue it, and report what happened.
    ///
    /// Planning is async because it may list a remote directory; queueing is
    /// instant. Nothing is queued when the plan is empty, so an empty folder
    /// does not leave a phantom task in the panel.
    fn plan_and_queue<F>(
        &mut self,
        what: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
        plan: F,
    ) where
        F: Future<Output = roam_core::Result<Vec<Transfer>>> + Send + 'static,
    {
        let engine = self.engine.clone();
        let notify = self.on_transfers_queued.clone();
        let handle = window.window_handle();

        cx.spawn(async move |_, cx| {
            let message = match plan.await {
                Ok(transfers) if transfers.is_empty() => format!("没有可{what}的文件"),
                Ok(transfers) => {
                    let count = transfers.len();
                    engine.enqueue_all(transfers);
                    format!("已加入 {count} 个{what}任务")
                }
                Err(err) => err.user_message(),
            };

            let _ = handle.update(cx, |_, window, cx| {
                window.push_notification(message, cx);
                if let Some(notify) = notify.as_ref() {
                    notify(window, cx);
                }
            });
        })
        .detach();
    }

    /// List an object's versions and offer to restore or download one.
    fn show_versions(&mut self, entry: DirEntry, window: &mut Window, cx: &mut Context<Self>) {
        let vfs = self.vfs.clone();
        let path = entry.path.clone();
        let handle = window.window_handle();
        let this = cx.weak_entity();

        cx.spawn(async move |_, cx| {
            let listed = vfs.list_versions(&path).await;

            let _ = handle.update(cx, |_, window, cx| match listed {
                Ok(versions) if versions.is_empty() => {
                    window.push_notification("这个对象没有版本记录", cx);
                }
                Ok(versions) => {
                    let _ = this.update(cx, |browser, cx| {
                        browser.open_versions_dialog(entry.clone(), versions, window, cx);
                    });
                }
                Err(err) => window.push_notification(err.user_message(), cx),
            });
        })
        .detach();
    }

    /// Open the version dialog with a supplied list. Public for
    /// `examples/versions_dialog.rs`, which lays it out against the real text
    /// system — including the delete-marker row, which has no size to render.
    pub fn open_versions_dialog_with(
        &mut self,
        entry: DirEntry,
        versions: Vec<ObjectVersion>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_versions_dialog(entry, versions, window, cx);
    }

    fn open_versions_dialog(
        &mut self,
        entry: DirEntry,
        versions: Vec<ObjectVersion>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let this = cx.weak_entity();
        let title: SharedString = format!("版本历史 · {}", entry.name).into();
        let versions = Arc::new(versions);

        window.open_dialog(cx, move |builder, _window, _cx| {
            let versions = versions.clone();
            let entry = entry.clone();
            let this = this.clone();

            builder
                .title(title.clone())
                .w(px(560.))
                .button_props(
                    DialogButtonProps::default()
                        .cancel_text("关闭")
                        .show_cancel(true),
                )
                .child(render_versions(&versions, &entry, this))
        });
    }

    fn restore_version(
        &mut self,
        entry: &DirEntry,
        version: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Restoring writes the old bytes back, which itself becomes a new
        // version — history in these stores is append-only.
        let operation = self.vfs.restore_version(&entry.path, version);
        self.run_mutation("已恢复该版本（作为新版本写入）", operation, window, cx);
    }

    fn confirm_delete(
        &mut self,
        entry: roam_core::DirEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let this = cx.weak_entity();
        let title: SharedString = format!("删除 {}？", entry.name).into();

        // Deleting a directory is recursive, so say so plainly. This is the one
        // action in the menu that cannot be undone.
        let body: SharedString = if entry.is_dir() {
            format!("「{}」及其全部内容将被删除，此操作无法撤销。", entry.name).into()
        } else {
            format!("「{}」将被删除，此操作无法撤销。", entry.name).into()
        };

        window.open_dialog(cx, move |builder, _window, _cx| {
            let this = this.clone();
            let entry = entry.clone();

            builder
                .title(title.clone())
                .w(px(440.))
                // `confirm()` used to bundle these three. Deleting is the one
                // irreversible action here, so it keeps them: no dismissing by
                // clicking the backdrop, and no bare X that reads as "cancel".
                .overlay_closable(false)
                .close_button(false)
                .button_props(
                    DialogButtonProps::default()
                        .ok_text("删除")
                        .ok_variant(ButtonVariant::Danger)
                        .cancel_text("取消")
                        .show_cancel(true),
                )
                .child(div().text_sm().child(body.clone()))
                .on_ok(move |_, window, cx| {
                    this.update(cx, |browser, cx| browser.delete_entry(&entry, window, cx))
                        .is_ok()
                })
        });
    }

    fn go_back(&mut self, cx: &mut Context<Self>) {
        if let Some(previous) = self.back.pop() {
            self.forward.push(self.cwd.clone());
            self.navigate(previous, false, cx);
        }
    }

    fn go_forward(&mut self, cx: &mut Context<Self>) {
        if let Some(next) = self.forward.pop() {
            self.back.push(self.cwd.clone());
            self.navigate(next, false, cx);
        }
    }

    fn go_up(&mut self, cx: &mut Context<Self>) {
        if let Some(parent) = path::parent(&self.cwd) {
            self.navigate(parent.into(), true, cx);
        }
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        self.cache.invalidate(&self.key(&self.cwd.clone()));
        let cwd = self.cwd.clone();
        self.navigate(cwd, false, cx);
    }

    // ── rendering ─────────────────────────────────────────────────────────

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let can_up = path::parent(&self.cwd).is_some();
        let can_create_dir = self.vfs.capability().create_dir;
        let show_hidden = self.show_hidden(cx);

        h_flex()
            .gap_1()
            .items_center()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                Button::new("back")
                    .icon(IconName::ArrowLeft)
                    .ghost()
                    .small()
                    .tooltip("后退")
                    .disabled(self.back.is_empty())
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.go_back(cx))),
            )
            .child(
                Button::new("forward")
                    .icon(IconName::ArrowRight)
                    .ghost()
                    .small()
                    .tooltip("前进")
                    .disabled(self.forward.is_empty())
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.go_forward(cx))),
            )
            .child(
                Button::new("up")
                    .icon(IconName::ArrowUp)
                    .ghost()
                    .small()
                    .tooltip("上一级")
                    .disabled(!can_up)
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.go_up(cx))),
            )
            .child(
                Button::new("reload")
                    .icon(IconName::Redo)
                    .ghost()
                    .small()
                    .tooltip("刷新")
                    .loading(self.loading)
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.reload(cx))),
            )
            .child(div().flex_1().child(self.render_breadcrumb(cx)))
            .child(
                div()
                    .w(px(200.))
                    .flex_none()
                    .child(Input::new(&self.filter).xsmall()),
            )
            .child(
                Button::new("toggle-hidden")
                    .icon(if show_hidden {
                        IconName::Eye
                    } else {
                        IconName::EyeOff
                    })
                    .ghost()
                    .small()
                    .tooltip(if show_hidden {
                        "隐藏点文件（⌘⇧.）"
                    } else {
                        "显示点文件（⌘⇧.）"
                    })
                    .selected(show_hidden)
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.toggle_hidden(cx))),
            )
            .child(
                Button::new("toggle-preview")
                    .icon(IconName::Eye)
                    .ghost()
                    .small()
                    .tooltip("预览（空格）")
                    .selected(self.preview_open)
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.action_toggle_preview(&TogglePreview, window, cx)
                    })),
            )
            .child(
                // Also in the row context menu, but an empty directory has no
                // row to right-click — without this button there would be no way
                // to create the first folder.
                Button::new("new-folder")
                    .icon(IconName::Plus)
                    .ghost()
                    .small()
                    .tooltip(if can_create_dir {
                        SharedString::from("新建文件夹")
                    } else {
                        SharedString::from(format!("{} 不支持新建目录", self.vfs.label()))
                    })
                    .disabled(!can_create_dir)
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.prompt_new_folder(window, cx)
                    })),
            )
    }

    fn render_breadcrumb(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let crumbs = path::crumbs(&self.cwd);

        Breadcrumb::new().children(crumbs.into_iter().map(|(label, target)| {
            let target: Arc<str> = target.into();
            BreadcrumbItem::new(SharedString::from(label)).on_click(cx.listener(
                move |this, _: &ClickEvent, _, cx| {
                    this.navigate(target.clone(), true, cx);
                },
            ))
        }))
    }

    fn render_error(&self, err: &Error, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .gap_2()
            .items_center()
            .px_3()
            .py_2()
            .bg(cx.theme().danger.opacity(0.1))
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                Icon::new(IconName::TriangleAlert)
                    .size_4()
                    .text_color(cx.theme().danger),
            )
            .child(div().flex_1().child(err.user_message()))
            .child(
                Button::new("retry")
                    .label("重试")
                    .outline()
                    .small()
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.reload(cx))),
            )
    }

    fn render_status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.table.read(cx);
        let total = state.delegate().total();
        let shown = state.delegate().shown();
        let filtering = !state.delegate().filter().is_empty();

        h_flex()
            .gap_3()
            .items_center()
            .px_3()
            .py_1()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .border_t_1()
            .border_color(cx.theme().border)
            // While filtering, say both numbers — "3 项" alone would look like
            // the directory only has three things in it.
            .child(if filtering {
                format!("{shown} / {total} 项")
            } else {
                format!("{total} 项")
            })
            .when(self.loading, |el| el.child("正在载入…"))
            .child(div().flex_1())
            .child(SharedString::from(format!("fs · {}", self.vfs.label())))
    }
}

#[cfg(test)]
impl Browser {
    pub(crate) fn can_go_back(&self) -> bool {
        !self.back.is_empty()
    }

    pub(crate) fn navigate_to(&mut self, to: &str, cx: &mut Context<Self>) {
        self.navigate(to.into(), true, cx);
    }

    pub(crate) fn entry_named(&self, name: &str, cx: &gpui::App) -> Option<DirEntry> {
        let state = self.table.read(cx);
        let delegate = state.delegate();
        (0..)
            .map_while(|ix| delegate.entry(ix))
            .find(|entry| &*entry.name == name)
            .cloned()
    }

    pub(crate) fn row_names(&self, cx: &gpui::App) -> Vec<String> {
        let state = self.table.read(cx);
        let delegate = state.delegate();
        (0..)
            .map_while(|ix| delegate.entry(ix))
            .map(|entry| entry.name.to_string())
            .collect()
    }

    fn row_of(&self, name: &str, cx: &gpui::App) -> usize {
        self.row_names(cx)
            .iter()
            .position(|candidate| candidate == name)
            .unwrap_or_else(|| panic!("no row named {name}"))
    }
}

impl gpui::Focusable for Browser {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Browser {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            // Shortcuts live in this context so that typing in the filter box or
            // a dialog cannot trigger navigation. The table binds its own
            // up/down/left/right inside a nested context and keeps them.
            .key_context(BROWSER_CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::action_open))
            .on_action(cx.listener(Self::action_go_up))
            .on_action(cx.listener(Self::action_go_back))
            .on_action(cx.listener(Self::action_go_forward))
            .on_action(cx.listener(Self::action_reload))
            .on_action(cx.listener(Self::action_focus_filter))
            .on_action(cx.listener(Self::action_new_folder))
            .on_action(cx.listener(Self::action_delete))
            .on_action(cx.listener(Self::action_download))
            .on_action(cx.listener(Self::action_toggle_preview))
            .on_action(cx.listener(Self::action_toggle_hidden))
            .child(self.render_toolbar(cx))
            .when_some(self.error.clone(), |el, err| {
                el.child(self.render_error(&err, cx))
            })
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(
                        // min_h_0 keeps the flex child from being sized by its
                        // content, which is what lets the table virtualize
                        // instead of growing.
                        div()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            // Files dragged in from the Finder upload into the
                            // directory currently shown.
                            .on_drop(cx.listener(
                                |this, paths: &gpui::ExternalPaths, window, cx| {
                                    this.upload_dropped(paths.paths().to_vec(), window, cx);
                                },
                            ))
                            .child(DataTable::new(&self.table).stripe(true)),
                    )
                    .when(self.preview_open, |el| el.child(self.preview.clone())),
            )
            .child(self.render_status_bar(cx))
    }
}

/// One row per stored version, newest first.
fn render_versions(
    versions: &Arc<Vec<ObjectVersion>>,
    entry: &DirEntry,
    browser: gpui::WeakEntity<Browser>,
) -> impl IntoElement + use<> {
    let rows: Vec<_> =
        versions
            .iter()
            .enumerate()
            .map(|(ix, version)| {
                let id = version.id.clone();
                let entry = entry.clone();
                let browser = browser.clone();
                let restorable = id.is_some() && !version.is_current && !version.is_delete_marker;

                h_flex()
                    .w_full()
                    .py_1p5()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .w(px(120.))
                            .flex_none()
                            .text_xs()
                            .font_family("ui-monospace")
                            .child(version.short_id()),
                    )
                    .child(
                        div()
                            .w(px(120.))
                            .flex_none()
                            .text_xs()
                            .child(roam_core::fmt::modified(version.modified)),
                    )
                    .child(div().w(px(70.)).flex_none().text_xs().child(
                        if version.is_delete_marker {
                            // A delete marker is an event, not content.
                            "删除标记".to_string()
                        } else {
                            roam_core::fmt::size(version.size)
                        },
                    ))
                    .child(div().flex_1().text_xs().child(if version.is_current {
                        "当前版本"
                    } else if version.is_delete_marker {
                        ""
                    } else {
                        "历史版本"
                    }))
                    .when(restorable, |el| {
                        el.child(
                            Button::new(("restore", ix))
                                .label("恢复")
                                .outline()
                                .xsmall()
                                .on_click(move |_, window, cx| {
                                    let Some(id) = id.clone() else { return };
                                    let _ = browser.update(cx, |browser, cx| {
                                        browser.restore_version(&entry, &id, window, cx);
                                    });
                                }),
                        )
                    })
            })
            .collect();

    v_flex()
        .gap_px()
        .max_h(px(320.))
        .child(
            h_flex()
                .w_full()
                .pb_1()
                .gap_2()
                .text_xs()
                .child(div().w(px(120.)).flex_none().child("版本"))
                .child(div().w(px(120.)).flex_none().child("时间"))
                .child(div().w(px(70.)).flex_none().child("大小"))
                .child(div().flex_1().child("状态")),
        )
        .children(rows)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};
    use roam_core::Rt;
    use std::cell::RefCell;
    use std::time::Duration;

    pub(crate) struct Harness {
        dir: tempfile::TempDir,
        browser: Entity<Browser>,
        cx: VisualTestContext,
    }

    impl Harness {
        pub(crate) fn new(cx: &mut TestAppContext) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            std::fs::create_dir_all(root.join("documents/reports")).unwrap();
            std::fs::create_dir(root.join("media")).unwrap();
            std::fs::write(root.join("README.md"), b"# readme").unwrap();
            std::fs::write(root.join("notes.txt"), b"note").unwrap();
            std::fs::write(root.join("documents/contract.pdf"), b"pdf").unwrap();
            std::fs::write(root.join("documents/reports/q1.xlsx"), b"xlsx").unwrap();

            cx.update(|cx| {
                gpui_component::init(cx);
                crate::actions::init(cx);
            });

            let vfs = Vfs::local(Rt::new().unwrap(), root.to_str().unwrap()).unwrap();

            // Wrapped in `Root` exactly as main.rs does: the dialogs and
            // `push_notification` both reach for the window's root layer and
            // panic without it, so a bare Browser root would not be testing the
            // arrangement that ships.
            let holder: Rc<RefCell<Option<Entity<Browser>>>> = Rc::new(RefCell::new(None));
            let window = {
                let holder = holder.clone();
                cx.add_window(move |window, cx| {
                    let engine = TransferEngine::new(
                        Rt::new().unwrap(),
                        roam_core::transfer::DEFAULT_CONCURRENCY,
                    );
                    let browser = cx.new(|cx| Browser::new(vfs, engine, window, cx));
                    *holder.borrow_mut() = Some(browser.clone());
                    // Wrapped in `Root` on purpose: `push_notification` and the
                    // dialog layer both reach for it, and main.rs has the same
                    // shape. On macOS this currently panics under gpui's test
                    // platform — an upstream defect, see docs/DESIGN.md.
                    gpui_component::Root::new(gpui::AnyView::from(browser), window, cx)
                })
            };
            let browser = holder.borrow().clone().expect("browser was built");
            let visual = VisualTestContext::from_window(window.into(), cx);

            let mut harness = Self {
                dir,
                browser,
                cx: visual,
            };
            harness.settle();
            harness
        }

        /// The listing runs on a real tokio runtime, so the deterministic test
        /// executor cannot know when it is done. Park, then give the IO threads
        /// a moment, until the pane reports it has finished loading.
        pub(crate) fn settle(&mut self) {
            for _ in 0..400 {
                self.cx.run_until_parked();
                let done = self.browser.read_with(&self.cx, |b, _| !b.is_busy());
                if done {
                    self.cx.run_until_parked();
                    return;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            panic!("the listing never settled");
        }

        pub(crate) fn path(&self) -> &std::path::Path {
            self.dir.path()
        }

        /// Drive `create_folder` the way the dialog's OK button does.
        pub(crate) fn create_folder(&mut self, name: &str) -> Result<(), &'static str> {
            let browser = self.browser.clone();
            let name = name.to_string();
            let outcome = self.cx.update(|window, cx| {
                browser.update(cx, |browser, cx| browser.create_folder(&name, window, cx))
            });
            self.settle();
            outcome
        }

        pub(crate) fn rename(&mut self, from: &str, to: &str) -> Result<(), &'static str> {
            let entry = self.entry(from);
            let browser = self.browser.clone();
            let to = to.to_string();
            let outcome = self.cx.update(|window, cx| {
                browser.update(cx, |browser, cx| {
                    browser.rename_entry(&entry, &to, window, cx)
                })
            });
            self.settle();
            outcome
        }

        pub(crate) fn duplicate(&mut self, name: &str) {
            let entry = self.entry(name);
            let browser = self.browser.clone();
            self.cx.update(|window, cx| {
                browser.update(cx, |browser, cx| browser.duplicate(entry, window, cx));
            });
            self.settle();
        }

        pub(crate) fn delete(&mut self, name: &str) {
            let entry = self.entry(name);
            let browser = self.browser.clone();
            self.cx.update(|window, cx| {
                browser.update(cx, |browser, cx| browser.delete_entry(&entry, window, cx));
            });
            self.settle();
        }

        pub(crate) fn download(&mut self, name: &str) {
            let entry = self.entry(name);
            let browser = self.browser.clone();
            self.cx.update(|window, cx| {
                browser.update(cx, |browser, cx| browser.download(entry, window, cx));
            });
        }

        pub(crate) fn drop_files(&mut self, paths: Vec<std::path::PathBuf>) {
            let browser = self.browser.clone();
            self.cx.update(|window, cx| {
                browser.update(cx, |browser, cx| browser.upload_dropped(paths, window, cx));
            });
        }

        pub(crate) fn transfer_snapshot(&mut self) -> Vec<roam_core::TaskSnapshot> {
            self.browser
                .read_with(&self.cx, |browser, _| browser.engine.snapshot())
        }

        /// Planning is async and the engine runs on real tokio threads, so wait
        /// for both: first for tasks to appear, then for them all to finish.
        pub(crate) fn settle_transfers(&mut self) {
            for _ in 0..400 {
                self.cx.run_until_parked();
                let engine = self
                    .browser
                    .read_with(&self.cx, |browser, _| browser.engine.clone());
                if !engine.is_empty() && !engine.is_active() {
                    self.cx.run_until_parked();
                    self.settle();
                    return;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            panic!("transfers never settled: {:?}", self.transfer_snapshot());
        }

        /// Type into the filter box, the way `⌘F` then typing does.
        pub(crate) fn set_filter(&mut self, text: &str) {
            let browser = self.browser.clone();
            let text = text.to_string();
            self.cx.update(|window, cx| {
                let filter = browser.read(cx).filter.clone();
                filter.update(cx, |state, cx| state.set_value(text.clone(), window, cx));
            });
            self.cx.run_until_parked();
        }

        pub(crate) fn select_row(&mut self, name: &str) {
            let row = self.browser.read_with(&self.cx, |b, cx| b.row_of(name, cx));
            let table = self.browser.read_with(&self.cx, |b, _| b.table.clone());
            table.update(&mut self.cx, |state, cx| state.set_selected_row(row, cx));
            self.cx.run_until_parked();
        }

        /// Dispatch a real keystroke through the window's key bindings.
        pub(crate) fn keystroke(&mut self, keys: &str) {
            self.cx.simulate_keystrokes(keys);
            self.settle();
        }

        pub(crate) fn focus_list(&mut self) {
            let browser = self.browser.clone();
            self.cx.update(|window, cx| {
                let handle = browser.read(cx).focus.clone();
                window.focus(&handle, cx);
            });
            self.cx.run_until_parked();
        }

        pub(crate) fn name_dialog_open(&mut self) -> bool {
            self.browser
                .read_with(&self.cx, |b, _| b.name_dialog.is_some())
        }

        pub(crate) fn preview_open(&mut self) -> bool {
            self.browser.read_with(&self.cx, |b, _| b.preview_open())
        }

        /// Add a file to the fixture directory, then re-list.
        pub(crate) fn write_file(&mut self, name: &str, bytes: &[u8]) {
            std::fs::write(self.dir.path().join(name), bytes).unwrap();
        }

        pub(crate) fn reload_now(&mut self) {
            let browser = self.browser.clone();
            self.cx.update(|_, cx| {
                browser.update(cx, |browser, cx| browser.reload(cx));
            });
            self.settle();
        }

        pub(crate) fn preview_state(&mut self) -> &'static str {
            self.browser
                .read_with(&self.cx, |b, cx| b.preview.read(cx).state_label())
        }

        pub(crate) fn preview_body(&mut self) -> Option<String> {
            self.browser
                .read_with(&self.cx, |b, cx| b.preview.read(cx).body_text())
        }

        pub(crate) fn preview_truncated(&mut self) -> bool {
            self.browser
                .read_with(&self.cx, |b, cx| b.preview.read(cx).is_truncated())
        }

        /// Wait for the preview's read to land.
        pub(crate) fn settle_preview(&mut self) {
            for _ in 0..200 {
                self.cx.run_until_parked();
                if self.preview_state() != "loading" {
                    return;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            panic!("the preview never loaded");
        }

        pub(crate) fn menu_for(&mut self, name: &str) -> Vec<roam_core::MenuItem> {
            let entry = self.entry(name);
            self.browser
                .read_with(&self.cx, |browser, _| browser.vfs().entry_menu(&entry))
        }

        fn entry(&mut self, name: &str) -> roam_core::DirEntry {
            let name = name.to_string();
            self.browser.read_with(&self.cx, |browser, cx| {
                let state = browser.table.read(cx);
                let delegate = state.delegate();
                (0..)
                    .map_while(|ix| delegate.entry(ix))
                    .find(|entry| &*entry.name == name.as_str())
                    .unwrap_or_else(|| panic!("no row named {name}"))
                    .clone()
            })
        }

        pub(crate) fn names(&mut self) -> Vec<String> {
            self.browser.read_with(&self.cx, |b, cx| b.row_names(cx))
        }

        pub(crate) fn cwd(&mut self) -> String {
            self.browser.read_with(&self.cx, |b, _| b.cwd().to_string())
        }

        pub(crate) fn double_click(&mut self, name: &str) {
            let row = self.browser.read_with(&self.cx, |b, cx| b.row_of(name, cx));
            let table = self.browser.read_with(&self.cx, |b, _| b.table.clone());
            table.update(&mut self.cx, |_, cx| {
                cx.emit(TableEvent::DoubleClickedRow(row));
            });
            self.settle();
        }

        pub(crate) fn click_back(&mut self) {
            self.browser
                .update(&mut self.cx, |browser, cx| browser.go_back(cx));
            self.settle();
        }

        pub(crate) fn click_forward(&mut self) {
            self.browser
                .update(&mut self.cx, |browser, cx| browser.go_forward(cx));
            self.settle();
        }

        pub(crate) fn click_up(&mut self) {
            self.browser
                .update(&mut self.cx, |browser, cx| browser.go_up(cx));
            self.settle();
        }
    }

    #[gpui::test]
    fn lists_the_root_with_directories_first(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);

        // Directories first, then names compared case-insensitively — so
        // notes.txt precedes README.md, which is the point of folding case.
        assert_eq!(
            h.names(),
            vec!["documents", "media", "notes.txt", "README.md"]
        );
        assert_eq!(h.cwd(), "");
    }

    #[gpui::test]
    fn double_clicking_a_directory_navigates_into_it(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);

        h.double_click("documents");

        assert_eq!(h.cwd(), "documents/");
        assert_eq!(h.names(), vec!["reports", "contract.pdf"]);
    }

    #[gpui::test]
    fn double_clicking_a_file_stays_put(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);

        h.double_click("README.md");

        assert_eq!(h.cwd(), "", "a file is not somewhere to navigate to");
    }

    #[gpui::test]
    fn navigation_nests_two_levels_deep(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);

        h.double_click("documents");
        h.double_click("reports");

        assert_eq!(h.cwd(), "documents/reports/");
        assert_eq!(h.names(), vec!["q1.xlsx"]);
    }

    #[gpui::test]
    fn up_goes_to_the_parent_and_stops_at_the_root(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);

        h.double_click("documents");
        h.double_click("reports");

        h.click_up();
        assert_eq!(h.cwd(), "documents/");

        h.click_up();
        assert_eq!(h.cwd(), "");

        // Already at the root: the button is disabled in the UI, and the
        // handler is a no-op even if it is somehow invoked.
        h.click_up();
        assert_eq!(h.cwd(), "");
    }

    #[gpui::test]
    fn back_and_forward_retrace_the_visited_path(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);

        h.double_click("documents");
        h.double_click("reports");
        assert_eq!(h.cwd(), "documents/reports/");

        h.click_back();
        assert_eq!(h.cwd(), "documents/");
        h.click_back();
        assert_eq!(h.cwd(), "");

        h.click_forward();
        assert_eq!(h.cwd(), "documents/");
        h.click_forward();
        assert_eq!(h.cwd(), "documents/reports/");
    }

    #[gpui::test]
    fn breadcrumbs_follow_the_current_directory(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);

        h.double_click("documents");
        h.double_click("reports");

        let crumbs = path::crumbs(&h.cwd());
        let labels: Vec<&str> = crumbs.iter().map(|(label, _)| label.as_str()).collect();
        assert_eq!(labels, vec!["/", "documents", "reports"]);

        // Clicking the "documents" crumb navigates to exactly that path.
        let target = crumbs[1].1.clone();
        h.browser.update(&mut h.cx, |browser, cx| {
            browser.navigate(target.into(), true, cx)
        });
        h.settle();
        assert_eq!(h.cwd(), "documents/");
    }

    #[gpui::test]
    fn a_stale_batch_cannot_overwrite_a_newer_directory(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);

        h.double_click("documents");
        let before = h.names();

        // A batch from the listing we navigated away from. The generation guard
        // is what stops a slow directory from clobbering the current one.
        h.browser.update(&mut h.cx, |browser, cx| {
            let stale = browser.generation - 1;
            browser.on_batch(stale, Vec::new(), true, cx);
        });

        assert_eq!(h.names(), before, "stale batch was ignored");
    }

    #[gpui::test]
    fn revisiting_a_directory_serves_the_cache_before_the_refresh(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);

        h.double_click("documents");
        h.click_back();

        // Navigate back into "documents": the cache from the first visit paints
        // rows on the same tick, before any listing has completed.
        h.browser.update(&mut h.cx, |browser, cx| {
            browser.navigate("documents/".into(), true, cx)
        });
        let painted_immediately = h.names();
        assert_eq!(
            painted_immediately,
            vec!["reports", "contract.pdf"],
            "cache hit should render without waiting for the backend"
        );

        h.settle();
        assert_eq!(h.names(), vec!["reports", "contract.pdf"]);
    }
}

#[cfg(test)]
mod mutation_tests {
    use super::tests::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn creating_a_folder_shows_it_in_the_listing(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);

        h.create_folder("新文件夹").expect("a valid name");

        assert!(h.names().contains(&"新文件夹".to_string()));
        assert!(h.path().join("新文件夹").is_dir());
    }

    #[gpui::test]
    fn a_folder_name_with_a_slash_is_refused(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);

        // Otherwise OpenDAL would quietly create a nested path instead of the
        // single folder the user asked for.
        let reason = h.create_folder("a/b").expect_err("should be refused");
        assert_eq!(reason, "名称不能包含 /");

        assert!(!h.path().join("a").exists());
    }

    #[gpui::test]
    fn an_empty_folder_name_is_refused(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);
        assert_eq!(h.create_folder("").unwrap_err(), "名称不能为空");
    }

    #[gpui::test]
    fn renaming_replaces_the_row(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);

        h.rename("README.md", "介绍.md").expect("a valid name");

        let names = h.names();
        assert!(names.contains(&"介绍.md".to_string()));
        assert!(!names.contains(&"README.md".to_string()));
    }

    #[gpui::test]
    fn renaming_to_the_same_name_is_a_no_op(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);

        h.rename("README.md", "README.md").expect("not an error");

        assert!(h.names().contains(&"README.md".to_string()));
    }

    #[gpui::test]
    fn renaming_a_directory_moves_its_whole_tree(cx: &mut TestAppContext) {
        use roam_core::EntryAction;

        let mut h = Harness::new(cx);

        // Changed here: a directory rename is now offered, and runs as a move
        // task in the transfer panel rather than a single rename call.
        let items = h.menu_for("documents");
        let rename = items
            .iter()
            .find(|i| i.action == EntryAction::Rename)
            .unwrap();
        assert!(rename.enabled);
        assert_eq!(rename.display_label(), "重命名");

        h.rename("documents", "档案").expect("a valid name");
        h.settle_transfers();

        assert!(h.path().join("档案/contract.pdf").exists());
        assert!(
            h.path().join("档案/reports/q1.xlsx").exists(),
            "nesting is preserved"
        );
        assert!(
            !h.path().join("documents").exists(),
            "the source is removed once every copy landed"
        );
    }

    #[gpui::test]
    fn renaming_a_file_still_uses_the_native_rename(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);

        // Files take the direct path — no transfer task involved.
        h.rename("README.md", "介绍.md").expect("a valid name");

        assert!(h.names().contains(&"介绍.md".to_string()));
        assert!(h.transfer_snapshot().is_empty(), "no move task was needed");
    }

    #[gpui::test]
    fn duplicating_a_file_adds_a_copy_beside_it(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);

        h.duplicate("README.md");

        let names = h.names();
        assert!(names.contains(&"README.md".to_string()), "original stays");
        assert!(names.contains(&"README 副本.md".to_string()));
    }

    #[gpui::test]
    fn duplicating_twice_does_not_overwrite_the_first_copy(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);

        h.duplicate("README.md");
        h.duplicate("README.md");

        let names = h.names();
        assert!(names.contains(&"README 副本.md".to_string()));
        assert!(names.contains(&"README 副本 2.md".to_string()));
    }

    #[gpui::test]
    fn deleting_a_file_removes_the_row(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);

        h.delete("notes.txt");

        assert!(!h.names().contains(&"notes.txt".to_string()));
        assert!(!h.path().join("notes.txt").exists());
    }

    #[gpui::test]
    fn deleting_a_directory_takes_its_contents(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);
        assert!(h.path().join("documents/reports/q1.xlsx").exists());

        h.delete("documents");

        assert!(!h.names().contains(&"documents".to_string()));
        assert!(!h.path().join("documents").exists(), "recursive");
    }

    #[gpui::test]
    fn a_mutation_refreshes_the_listing_rather_than_serving_the_cache(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);

        // Visit a subdirectory and come back so the root listing is cached,
        // then mutate. A stale cache would keep showing the deleted row.
        h.double_click("documents");
        h.click_back();
        assert!(h.names().contains(&"notes.txt".to_string()));

        h.delete("notes.txt");

        assert!(
            !h.names().contains(&"notes.txt".to_string()),
            "the cache was invalidated"
        );
    }

    #[gpui::test]
    fn the_menu_reflects_what_the_local_backend_supports(cx: &mut TestAppContext) {
        use roam_core::EntryAction;

        let mut h = Harness::new(cx);
        let items = h.menu_for("README.md");

        let enabled = |action| {
            items
                .iter()
                .find(|i| i.action == action)
                .unwrap_or_else(|| panic!("no {action:?} item"))
                .enabled
        };

        // Local fs can do everything here except presign.
        assert!(enabled(EntryAction::NewFolder));
        assert!(enabled(EntryAction::Rename));
        assert!(enabled(EntryAction::Duplicate));
        assert!(enabled(EntryAction::Delete));
        assert!(!enabled(EntryAction::CopyShareLink));
    }

    #[gpui::test]
    fn a_directory_can_now_be_duplicated(cx: &mut TestAppContext) {
        use roam_core::EntryAction;

        // Changed in M4: the transfer engine flattens the directory into one
        // server-side copy per file, so this is no longer refused.
        let mut h = Harness::new(cx);
        let items = h.menu_for("documents");
        let duplicate = items
            .iter()
            .find(|i| i.action == EntryAction::Duplicate)
            .unwrap();

        assert!(duplicate.enabled);
        assert_eq!(duplicate.display_label(), "创建副本");
    }

    #[gpui::test]
    fn duplicating_a_directory_copies_its_whole_tree(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);

        h.duplicate("documents");
        h.settle_transfers();

        assert!(h.path().join("documents 副本/contract.pdf").exists());
        assert!(
            h.path().join("documents 副本/reports/q1.xlsx").exists(),
            "nested files come along"
        );
        assert!(
            h.path().join("documents/contract.pdf").exists(),
            "the original is untouched"
        );
    }

    #[gpui::test]
    fn downloading_a_file_queues_a_transfer(cx: &mut TestAppContext) {
        use roam_core::TaskState;

        let mut h = Harness::new(cx);
        h.download("README.md");
        h.settle_transfers();

        let tasks = h.transfer_snapshot();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].state, TaskState::Done);
        assert_eq!(&*tasks[0].label, "README.md");
    }

    #[gpui::test]
    fn dropping_local_files_uploads_them_into_the_current_directory(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);

        let source = tempfile::tempdir().unwrap();
        std::fs::write(source.path().join("dropped.txt"), b"from finder").unwrap();

        h.double_click("documents");
        h.drop_files(vec![source.path().join("dropped.txt")]);
        h.settle_transfers();

        assert_eq!(
            std::fs::read_to_string(h.path().join("documents/dropped.txt")).unwrap(),
            "from finder",
            "uploaded into the directory that was open, not the root"
        );
    }

    #[gpui::test]
    fn dropping_a_folder_preserves_its_structure(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);

        let source = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(source.path().join("bundle/inner")).unwrap();
        std::fs::write(source.path().join("bundle/top.txt"), b"top").unwrap();
        std::fs::write(source.path().join("bundle/inner/deep.txt"), b"deep").unwrap();

        h.drop_files(vec![source.path().join("bundle")]);
        h.settle_transfers();

        assert_eq!(
            std::fs::read_to_string(h.path().join("bundle/top.txt")).unwrap(),
            "top"
        );
        assert_eq!(
            std::fs::read_to_string(h.path().join("bundle/inner/deep.txt")).unwrap(),
            "deep"
        );
    }
}

#[cfg(test)]
mod keyboard_tests {
    use super::tests::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn the_filter_narrows_the_visible_rows(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);
        assert_eq!(h.names().len(), 4);

        h.set_filter("read");

        assert_eq!(h.names(), vec!["README.md"]);
    }

    #[gpui::test]
    fn the_filter_is_case_insensitive(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);
        h.set_filter("REAdme");
        assert_eq!(h.names(), vec!["README.md"]);
    }

    #[gpui::test]
    fn clearing_the_filter_restores_every_row(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);

        h.set_filter("read");
        assert_eq!(h.names().len(), 1);

        h.set_filter("");
        assert_eq!(h.names().len(), 4, "the entries were never discarded");
    }

    #[gpui::test]
    fn the_filter_survives_navigation_reset(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);

        h.set_filter("contract");
        // Nothing in the root matches, so the pane is empty...
        assert!(h.names().is_empty());

        // ...but the directory that does contain it still lists it.
        h.set_filter("");
        h.double_click("documents");
        h.set_filter("contract");
        assert_eq!(h.names(), vec!["contract.pdf"]);
    }

    #[gpui::test]
    fn enter_opens_the_selected_directory(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);
        h.focus_list();
        h.select_row("documents");

        h.keystroke("enter");

        assert_eq!(h.cwd(), "documents/");
    }

    #[gpui::test]
    fn enter_on_a_file_does_nothing(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);
        h.focus_list();
        h.select_row("README.md");

        h.keystroke("enter");

        assert_eq!(h.cwd(), "", "a file is not somewhere to navigate to");
    }

    #[gpui::test]
    fn backspace_goes_up(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);
        h.double_click("documents");
        assert_eq!(h.cwd(), "documents/");

        h.focus_list();
        h.keystroke("backspace");

        assert_eq!(h.cwd(), "");
    }

    #[gpui::test]
    fn cmd_bracket_retraces_history(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);
        h.double_click("documents");
        h.focus_list();

        h.keystroke("cmd-[");
        assert_eq!(h.cwd(), "");

        h.keystroke("cmd-]");
        assert_eq!(h.cwd(), "documents/");
    }

    #[gpui::test]
    fn space_toggles_the_preview_panel(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);
        h.focus_list();
        assert!(!h.preview_open());

        h.keystroke("space");
        assert!(h.preview_open());

        h.keystroke("space");
        assert!(!h.preview_open());
    }

    #[gpui::test]
    fn shortcuts_do_nothing_when_no_row_is_selected(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);
        h.focus_list();

        // Acting on row 0 instead of doing nothing is how a stray keypress
        // deletes the wrong file.
        h.keystroke("enter");
        assert_eq!(h.cwd(), "");
    }

    #[gpui::test]
    fn cmd_shift_n_opens_the_new_folder_prompt(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);
        h.focus_list();

        // The prompt is a dialog, so this also proves the binding reaches the
        // handler and the dialog layer can open from a keystroke.
        h.keystroke("cmd-shift-n");

        assert!(h.name_dialog_open(), "the prompt should be up");
    }
}

#[cfg(test)]
mod preview_tests {
    use super::tests::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn the_panel_starts_empty(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);
        assert_eq!(h.preview_state(), "idle");
    }

    #[gpui::test]
    fn selecting_a_text_file_shows_its_contents(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);

        h.select_row("notes.txt");
        h.settle_preview();

        assert_eq!(h.preview_state(), "text");
        assert_eq!(h.preview_body().unwrap(), "note");
    }

    #[gpui::test]
    fn a_markdown_file_is_rendered_as_markdown(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);

        h.select_row("README.md");
        h.settle_preview();

        assert_eq!(h.preview_state(), "markdown");
        assert_eq!(h.preview_body().unwrap(), "# readme");
    }

    #[gpui::test]
    fn a_directory_says_it_has_no_preview(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);

        h.select_row("documents");
        h.settle_preview();

        assert_eq!(h.preview_state(), "unavailable");
        assert_eq!(h.preview_body().unwrap(), "目录没有预览");
    }

    #[gpui::test]
    fn a_binary_file_is_refused_by_extension(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);
        h.write_file("archive.zip", b"PK\x03\x04not really");
        h.reload_now();

        h.select_row("archive.zip");
        h.settle_preview();

        assert_eq!(h.preview_state(), "unavailable");
        assert_eq!(h.preview_body().unwrap(), "二进制文件，暂不预览");
    }

    #[gpui::test]
    fn an_unlabelled_binary_is_caught_by_its_bytes(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);
        // Unknown extension, so classify() guesses text — the NUL byte is what
        // stops a screen of mojibake.
        h.write_file("mystery.dat", b"header\x00\x01\x02binary");
        h.reload_now();

        h.select_row("mystery.dat");
        h.settle_preview();

        assert_eq!(h.preview_state(), "unavailable");
        assert_eq!(h.preview_body().unwrap(), "二进制文件，暂不预览");
    }

    #[gpui::test]
    fn a_large_text_file_is_truncated_and_says_so(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);
        let big = vec![b'a'; roam_core::preview::TEXT_LIMIT as usize + 1000];
        h.write_file("big.txt", &big);
        h.reload_now();

        h.select_row("big.txt");
        h.settle_preview();

        assert_eq!(h.preview_state(), "text");
        assert_eq!(
            h.preview_body().unwrap().len(),
            roam_core::preview::TEXT_LIMIT as usize,
            "read stops at the limit"
        );
        assert!(h.preview_truncated(), "and the panel says it was cut off");
    }

    #[gpui::test]
    fn a_small_file_is_not_marked_truncated(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);

        h.select_row("notes.txt");
        h.settle_preview();

        assert!(!h.preview_truncated());
    }

    #[gpui::test]
    fn moving_the_selection_replaces_the_preview(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);

        h.select_row("notes.txt");
        h.settle_preview();
        assert_eq!(h.preview_body().unwrap(), "note");

        h.select_row("README.md");
        h.settle_preview();
        assert_eq!(h.preview_body().unwrap(), "# readme");
    }
}
