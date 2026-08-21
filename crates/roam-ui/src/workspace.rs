use std::sync::Arc;

use gpui::{
    AnyElement, AppContext, ClickEvent, Context, Entity, InteractiveElement, IntoElement,
    ParentElement, Pixels, Render, SharedString, StatefulInteractiveElement, Styled, Window, div,
    prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::dialog::DialogFooter;
use gpui_component::tab::{Tab, TabBar};
use gpui_component::tooltip::Tooltip;
use gpui_component::{
    ActiveTheme, Disableable, Icon, IconName, Root, Sizable, Theme, ThemeMode, WindowExt, h_flex,
    v_flex,
};
use roam_core::transfer::DEFAULT_CONCURRENCY;
use roam_core::{Error, Profile, ProfileId, ProfileStore, Rt, TransferEngine, Vfs};

use crate::actions::{CloseTab, NewTab, NextTab, PrevTab, WORKSPACE_CONTEXT};
use crate::browser::Browser;
use crate::connection_form::ConnectionForm;
use crate::dir_tree::DirTreeView;
use crate::transfer_panel::TransferPanel;

/// Identifies the always-present local session, which has no saved profile.
const LOCAL_SESSION: &str = "";

/// Top-level view: connection sidebar plus the browsing pane.
/// One browsing tab.
///
/// Each tab holds its own session, so one can sit on local disk while another is
/// on S3. That independence is the point: it is what makes moving things between
/// two backends possible without leaving the window.
struct BrowserTab {
    id: usize,
    browser: Entity<Browser>,
    /// `None` means the built-in local session.
    profile: Option<ProfileId>,
}

pub struct Workspace {
    rt: Rt,
    store: Arc<ProfileStore>,
    profiles: Vec<Profile>,
    /// The built-in local session, kept so switching back to it does not depend
    /// on whatever session a browser currently holds.
    local: Vfs,
    /// Shared by every tab: a transfer outlives the pane that started it.
    engine: TransferEngine,

    tabs: Vec<BrowserTab>,
    /// Index into `tabs`.
    active: usize,
    next_tab_id: usize,

    tree: Entity<DirTreeView>,
    /// One engine for the whole app: transfers outlive the pane that started
    /// them and can span two sessions.
    transfers: Entity<TransferPanel>,
    form: Option<Entity<ConnectionForm>>,
    error: Option<Error>,
    shortcuts_open: bool,
}

impl Workspace {
    pub fn new(
        rt: Rt,
        store: Arc<ProfileStore>,
        local: Vfs,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let engine = TransferEngine::new(rt.clone(), DEFAULT_CONCURRENCY);
        let transfers = cx.new(|cx| TransferPanel::new(engine.clone(), window, cx));
        let tree = cx.new(|cx| DirTreeView::new(local.clone(), cx));

        // The tree drives whichever tab is active, so it goes through the
        // workspace rather than holding a pane directly.
        let this = cx.weak_entity();
        tree.update(cx, |tree, _| {
            tree.on_navigate(move |dir, _, cx| {
                let _ = this.update(cx, |workspace, cx| {
                    workspace.navigate_active(&dir, cx);
                });
            });
        });

        // A broken profiles.toml must not stop the app from opening — surface
        // it and carry on with the local session.
        let (profiles, mut error) = match store.load() {
            Ok(profiles) => (profiles, None),
            Err(err) => {
                tracing::warn!(error = %err.full_message(), "failed to load profiles");
                (Vec::new(), Some(err))
            }
        };

        // A connection saved by an older build kept its credentials in the
        // platform keychain, which this version no longer reads. Say so on the
        // banner: the alternative is a connection that looks fine and fails with
        // a permission error whose cause is invisible.
        if error.is_none() {
            let orphaned: Vec<String> = profiles
                .iter()
                .filter(|p| !p.orphaned_by_keychain_removal().is_empty())
                .map(|p| {
                    format!(
                        "{}（{}）",
                        p.name,
                        p.orphaned_by_keychain_removal().join("、")
                    )
                })
                .collect();

            if !orphaned.is_empty() {
                error = Some(Error::Config(format!(
                    "以下连接的凭据原先存放在系统钥匙串中，本版本不再读取，请重新填写：{}",
                    orphaned.join("；")
                )));
            }
        }

        let mut this = Self {
            rt,
            store,
            profiles,
            local: local.clone(),
            engine,
            tabs: Vec::new(),
            active: 0,
            next_tab_id: 1,
            tree,
            transfers,
            form: None,
            error,
            shortcuts_open: false,
        };

        this.open_tab(local, None, window, cx);
        this
    }

    // ── tabs ──────────────────────────────────────────────────────────────

    /// Build a tab on `vfs` and make it active.
    fn open_tab(
        &mut self,
        vfs: Vfs,
        profile: Option<ProfileId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = self.next_tab_id;
        self.next_tab_id += 1;

        let engine = self.engine.clone();
        let browser = cx.new(|cx| Browser::new(vfs, engine, window, cx));

        // The hooks route through the workspace with the tab's id, so a
        // background tab finishing a listing cannot move the sidebar.
        let this = cx.weak_entity();
        let hidden = this.clone();
        let panel = self.transfers.downgrade();
        browser.update(cx, |browser, _| {
            browser.on_directory_changed(move |dir, cx| {
                let _ = this.update(cx, |workspace, cx| {
                    workspace.tab_directory_changed(id, &dir, cx);
                });
            });
            browser.on_hidden_changed(move |show, cx| {
                let _ = hidden.update(cx, |workspace, cx| {
                    workspace.tab_hidden_changed(id, show, cx);
                });
            });
            browser.on_transfers_queued(move |_, cx| {
                let _ = panel.update(cx, |panel, cx| panel.refresh(cx));
            });
        });

        self.tabs.push(BrowserTab {
            id,
            browser,
            profile,
        });
        self.active = self.tabs.len() - 1;
        self.sync_tree(cx);
        cx.notify();
    }

    fn active_tab(&self) -> &BrowserTab {
        // `active` is kept in range by every mutation below, and there is always
        // at least one tab.
        &self.tabs[self.active.min(self.tabs.len() - 1)]
    }

    fn active_browser(&self) -> &Entity<Browser> {
        &self.active_tab().browser
    }

    fn action_new_tab(&mut self, _: &NewTab, window: &mut Window, cx: &mut Context<Self>) {
        // Opens where you already are, like a browser would — a new tab at the
        // root would throw away the navigation you just did.
        let tab = self.active_tab();
        let vfs = tab.browser.read(cx).vfs().clone();
        let profile = tab.profile.clone();
        let cwd = tab.browser.read(cx).cwd().to_string();

        self.open_tab(vfs, profile, window, cx);
        let browser = self.active_browser().clone();
        browser.update(cx, |browser, cx| browser.navigate_to_dir(&cwd, cx));
    }

    fn action_close_tab(&mut self, _: &CloseTab, _: &mut Window, cx: &mut Context<Self>) {
        self.close_tab(self.active, cx);
    }

    fn close_tab(&mut self, ix: usize, cx: &mut Context<Self>) {
        // The last tab stays: an empty window with no way back is not a state
        // worth being able to reach.
        if self.tabs.len() <= 1 || ix >= self.tabs.len() {
            return;
        }

        self.tabs.remove(ix);
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len() - 1;
        } else if self.active > ix {
            self.active -= 1;
        }

        self.sync_tree(cx);
        cx.notify();
    }

    fn select_tab(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix >= self.tabs.len() || ix == self.active {
            return;
        }
        self.active = ix;
        self.sync_tree(cx);
        cx.notify();
    }

    fn action_next_tab(&mut self, _: &NextTab, _: &mut Window, cx: &mut Context<Self>) {
        let next = (self.active + 1) % self.tabs.len();
        self.select_tab(next, cx);
    }

    fn action_prev_tab(&mut self, _: &PrevTab, _: &mut Window, cx: &mut Context<Self>) {
        let prev = (self.active + self.tabs.len() - 1) % self.tabs.len();
        self.select_tab(prev, cx);
    }

    /// Point the sidebar tree at the active tab's session and directory.
    fn sync_tree(&mut self, cx: &mut Context<Self>) {
        let browser = self.active_browser().clone();
        let (vfs, cwd) = {
            let browser = browser.read(cx);
            (browser.vfs().clone(), browser.cwd().to_string())
        };

        let show_hidden = browser.read(cx).show_hidden(cx);

        self.tree.update(cx, |tree, cx| {
            tree.set_vfs(vfs, cx);
            // Before `set_current`, so revealing the current directory does not
            // first draw rows the setting is about to remove.
            tree.set_show_hidden(show_hidden, cx);
            tree.set_current(&cwd, cx);
        });
    }

    /// Each tab carries its own hidden-files setting, and the sidebar shows the
    /// active one.
    fn tab_hidden_changed(&mut self, tab_id: usize, show: bool, cx: &mut Context<Self>) {
        if self.active_tab().id != tab_id {
            return;
        }
        self.tree
            .update(cx, |tree, cx| tree.set_show_hidden(show, cx));
    }

    fn tab_directory_changed(&mut self, tab_id: usize, dir: &str, cx: &mut Context<Self>) {
        // A listing finishing in a background tab must not move the sidebar.
        if self.active_tab().id != tab_id {
            return;
        }
        self.tree.update(cx, |tree, cx| tree.set_current(dir, cx));
    }

    fn navigate_active(&mut self, dir: &str, cx: &mut Context<Self>) {
        let browser = self.active_browser().clone();
        browser.update(cx, |browser, cx| browser.navigate_to_dir(dir, cx));
    }

    fn active_profile(&self) -> Option<&Profile> {
        let id = self.active_tab().profile.as_ref()?;
        self.profiles.iter().find(|p| &p.id == id)
    }

    fn taken_ids(&self) -> Vec<ProfileId> {
        self.profiles.iter().map(|p| p.id.clone()).collect()
    }

    // ── connecting ────────────────────────────────────────────────────────

    fn connect(&mut self, id: ProfileId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(profile) = self.profiles.iter().find(|p| p.id == id).cloned() else {
            return;
        };

        match Vfs::from_profile(self.rt.clone(), &profile) {
            Ok(vfs) => {
                self.error = None;

                // Connecting swaps the *active* tab's session; other tabs keep
                // whatever they were showing.
                let ix = self.active;
                self.tabs[ix].profile = Some(id);
                let browser = self.tabs[ix].browser.clone();
                browser.update(cx, |browser, cx| browser.set_vfs(vfs, cx));
                self.sync_tree(cx);
            }
            Err(err) => {
                // A bad endpoint or a missing credential shows up here, before
                // any request is made. Say exactly what was wrong with it —
                // "连接配置有误" on its own gives nothing to act on.
                window.push_notification(err.full_message(), cx);
                self.error = Some(err);
            }
        }

        cx.notify();
    }

    fn connect_local(&mut self, cx: &mut Context<Self>) {
        self.error = None;

        // Must come from the retained local session, not from `browser.vfs()` —
        // that is whatever session is currently open, which after connecting to
        // a remote profile is the remote one.
        let local = self.local.clone();
        let ix = self.active;
        self.tabs[ix].profile = None;
        let browser = self.tabs[ix].browser.clone();
        browser.update(cx, |browser, cx| browser.set_vfs(local, cx));
        self.sync_tree(cx);
        cx.notify();
    }

    // ── profile editing ───────────────────────────────────────────────────

    /// Open the "new connection" dialog. Also the entry point used by
    /// `examples/connection_dialog.rs`, which exists to exercise the dialog
    /// against the real platform text system.
    pub fn open_new_connection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_form(None, window, cx);
    }

    /// Opens the dialog with a service already chosen. Only the example uses
    /// this, to bring up the tallest form — the one whose height forced the field
    /// list to scroll.
    pub fn open_new_connection_for(
        &mut self,
        scheme: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_form(None, window, cx);
        if let Some(form) = self.form.clone() {
            let scheme = scheme.to_string();
            form.update(cx, |form, cx| form.select_service(&scheme, window, cx));
        }
    }

    fn open_form(&mut self, editing: Option<Profile>, window: &mut Window, cx: &mut Context<Self>) {
        let form = cx.new(|cx| match &editing {
            Some(profile) => ConnectionForm::editing(profile, window, cx),
            None => ConnectionForm::new(window, cx),
        });
        self.form = Some(form.clone());

        let title: SharedString = match &editing {
            Some(profile) => format!("编辑连接 · {}", profile.name).into(),
            None => "新建连接".into(),
        };

        let this = cx.weak_entity();

        window.open_dialog(cx, move |dialog, window, _cx| {
            let form = form.clone();
            let on_save = this.clone();
            let on_enter = this.clone();

            let ceiling = dialog_max_height(window.viewport_size().height);

            dialog
                .title(title.clone())
                .w(px(520.))
                // The one line that makes the form scrollable at all — see
                // `dialog_max_height`.
                .max_h(ceiling)
                .child(form.clone())
                // gpui-component renders `button_props` as actual buttons only
                // for an AlertDialog. A plain Dialog takes them as the Enter and
                // Escape handlers and draws no footer whatsoever, so this dialog
                // had nothing to click: the only way out was Esc or the close
                // cross, and the only way to save was Enter.
                .footer(
                    DialogFooter::new()
                        .child(
                            Button::new("cancel-connection")
                                .label("取消")
                                .outline()
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("save-connection")
                                .label("保存")
                                .primary()
                                .on_click(move |_, window, cx| {
                                    let done = on_save
                                        .update(cx, |workspace, cx| workspace.save_form(window, cx))
                                        .unwrap_or(false);
                                    // False means the form rejected the input and
                                    // said so; leaving the dialog open keeps what
                                    // was typed.
                                    if done {
                                        window.close_dialog(cx);
                                    }
                                }),
                        ),
                )
                // Enter goes through the same check, and the framework closes
                // the dialog when it returns true.
                .on_ok(move |_, window, cx| {
                    on_enter
                        .update(cx, |workspace, cx| workspace.save_form(window, cx))
                        .unwrap_or(false)
                })
        });
    }

    /// Returns true when the dialog should close.
    fn save_form(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(form) = self.form.clone() else {
            return true;
        };

        let taken = self.taken_ids();
        let profile = match form.read(cx).build(&taken, cx) {
            Ok(profile) => profile,
            Err(err) => {
                // Keep the dialog open so the input is not lost.
                window.push_notification(err.full_message(), cx);
                return false;
            }
        };

        let mut profiles = self.profiles.clone();
        match profiles.iter_mut().find(|p| p.id == profile.id) {
            Some(slot) => *slot = profile.clone(),
            None => profiles.push(profile.clone()),
        }

        if let Err(err) = self.store.save(&profiles) {
            window.push_notification(err.full_message(), cx);
            return false;
        }

        self.profiles = profiles;
        self.form = None;
        window.push_notification("连接已保存", cx);

        let id = profile.id.clone();
        self.connect(id, window, cx);
        true
    }

    fn remove_profile(&mut self, id: ProfileId, window: &mut Window, cx: &mut Context<Self>) {
        if !self.profiles.iter().any(|p| p.id == id) {
            return;
        }

        let remaining: Vec<Profile> = self
            .profiles
            .iter()
            .filter(|p| p.id != id)
            .cloned()
            .collect();

        if let Err(err) = self.store.save(&remaining) {
            window.push_notification(err.full_message(), cx);
            return;
        }

        // Nothing to clean up outside this file any more: the credentials went
        // out with the profile when it was dropped from `remaining`.
        self.profiles = remaining;
        // Any tab still pointing at the deleted profile falls back to local.
        if self
            .tabs
            .iter()
            .any(|tab| tab.profile.as_ref() == Some(&id))
        {
            let local = self.local.clone();
            for tab in &mut self.tabs {
                if tab.profile.as_ref() == Some(&id) {
                    tab.profile = None;
                    let browser = tab.browser.clone();
                    browser.update(cx, |browser, cx| browser.set_vfs(local.clone(), cx));
                }
            }
            self.sync_tree(cx);
        }
        window.push_notification("连接已删除", cx);
        cx.notify();
    }

    // ── rendering ─────────────────────────────────────────────────────────

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let local_active = self.active_tab().profile.is_none();

        // Built up in a loop rather than a `map` closure: each row needs `cx`
        // for its listeners, which a closure cannot borrow mutably per item.
        let mut rows: Vec<AnyElement> = Vec::with_capacity(self.profiles.len() + 1);
        rows.push(
            self.render_sidebar_row(LOCAL_SESSION, "本机", "fs", local_active, false, cx)
                .into_any_element(),
        );
        for profile in &self.profiles {
            let active = self.active_tab().profile.as_deref() == Some(profile.id.as_str());
            rows.push(
                self.render_sidebar_row(
                    &profile.id,
                    &profile.name,
                    profile.scheme(),
                    active,
                    true,
                    cx,
                )
                .into_any_element(),
            );
        }

        v_flex()
            .w(px(216.))
            .flex_none()
            .h_full()
            .bg(cx.theme().sidebar)
            .border_r_1()
            .border_color(cx.theme().sidebar_border)
            .child(
                h_flex()
                    .px_3()
                    .py_2()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("连接"),
                    )
                    .child(
                        Button::new("new-connection")
                            .icon(IconName::Plus)
                            .ghost()
                            .small()
                            .tooltip("新建连接")
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.open_form(None, window, cx)
                            })),
                    ),
            )
            .child(v_flex().flex_none().px_2().gap_px().children(rows))
            .child(
                v_flex()
                    .flex_none()
                    .border_t_1()
                    .border_color(cx.theme().sidebar_border)
                    .child(
                        div()
                            .px_3()
                            .py_1()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("目录"),
                    )
                    .child(self.tree.clone()),
            )
            .child(self.render_sidebar_footer(cx))
    }

    fn render_sidebar_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let dark = cx.theme().mode.is_dark();

        v_flex()
            .flex_none()
            .border_t_1()
            .border_color(cx.theme().sidebar_border)
            .child(
                h_flex()
                    .px_2()
                    .py_1p5()
                    .gap_2()
                    .items_center()
                    .child(
                        Button::new("toggle-theme")
                            .icon(if dark { IconName::Sun } else { IconName::Moon })
                            .ghost()
                            .small()
                            .tooltip(if dark {
                                "切换到亮色"
                            } else {
                                "切换到暗色"
                            })
                            .on_click(cx.listener(|_, _: &ClickEvent, window, cx| {
                                let next = if cx.theme().mode.is_dark() {
                                    ThemeMode::Light
                                } else {
                                    ThemeMode::Dark
                                };
                                Theme::change(next, Some(window), cx);
                            })),
                    )
                    .child(
                        Button::new("toggle-shortcuts")
                            .icon(IconName::Info)
                            .ghost()
                            .small()
                            .tooltip("快捷键")
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.shortcuts_open = !this.shortcuts_open;
                                cx.notify();
                            })),
                    ),
            )
            .when(self.shortcuts_open, |el| {
                // Shortcuts are undiscoverable otherwise; this is cheaper than a
                // separate help window and stays next to what it describes.
                el.child(
                    v_flex()
                        .px_2()
                        .pb_2()
                        .gap_px()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .children(crate::actions::SHORTCUT_HINTS.iter().map(|(label, keys)| {
                            h_flex()
                                .justify_between()
                                .gap_2()
                                .child(*label)
                                .child(*keys)
                        })),
                )
            })
    }

    fn render_sidebar_row(
        &self,
        id: &str,
        name: &str,
        scheme: &str,
        active: bool,
        removable: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let id_owned = id.to_string();
        let edit_id = id.to_string();
        let remove_id = id.to_string();

        h_flex()
            .id(SharedString::from(format!("conn-{id}")))
            .group("conn-row")
            .w_full()
            .px_2()
            .py_1p5()
            .gap_2()
            .items_center()
            .rounded_sm()
            .cursor_pointer()
            .when(active, |el| {
                el.bg(cx.theme().sidebar_accent)
                    .text_color(cx.theme().sidebar_accent_foreground)
            })
            .when(!active, |el| {
                el.hover(|el| el.bg(cx.theme().sidebar_accent))
            })
            .on_click(cx.listener(move |this, _, window, cx| {
                if id_owned == LOCAL_SESSION {
                    this.connect_local(cx);
                } else {
                    this.connect(id_owned.clone(), window, cx);
                }
            }))
            .child(
                Icon::new(if scheme == "fs" {
                    IconName::FolderClosed
                } else {
                    IconName::Globe
                })
                .size_4()
                .flex_none(),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(div().text_sm().child(name.to_string()))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(scheme.to_string()),
                    ),
            )
            .when(removable, |el| {
                el.child(
                    Button::new(SharedString::from(format!("edit-{id}")))
                        .icon(IconName::Settings2)
                        .ghost()
                        .xsmall()
                        .tooltip("编辑")
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            if let Some(profile) =
                                this.profiles.iter().find(|p| p.id == edit_id).cloned()
                            {
                                this.open_form(Some(profile), window, cx);
                            }
                        })),
                )
                .child(
                    Button::new(SharedString::from(format!("remove-{id}")))
                        .icon(IconName::Delete)
                        .ghost()
                        .xsmall()
                        .tooltip("删除")
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            this.remove_profile(remove_id.clone(), window, cx)
                        })),
                )
            })
    }

    fn on_tab_clicked(&mut self, ix: &usize, _: &mut Window, cx: &mut Context<Self>) {
        self.select_tab(*ix, cx);
    }

    fn render_tab_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let closable = self.tabs.len() > 1;

        // Each tab is labelled by its session plus where it is, since two tabs
        // on the same backend are otherwise indistinguishable.
        let tabs: Vec<Tab> = self
            .tabs
            .iter()
            .map(|tab| {
                let browser = tab.browser.read(cx);
                let where_ = match roam_core::path::basename(browser.cwd()) {
                    "" => "/".to_string(),
                    name => name.to_string(),
                };
                let label = format!("{} · {}", browser.label(), where_);
                let id = tab.id;

                Tab::new().label(label).suffix(
                    Button::new(SharedString::from(format!("close-tab-{id}")))
                        .icon(IconName::Close)
                        .ghost()
                        .xsmall()
                        .disabled(!closable)
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            if let Some(ix) = this.tabs.iter().position(|t| t.id == id) {
                                this.close_tab(ix, cx);
                            }
                        })),
                )
            })
            .collect();

        h_flex()
            .w_full()
            .items_center()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                TabBar::new("tabs")
                    .underline()
                    .selected_index(self.active)
                    .children(tabs)
                    .on_click(cx.listener(Self::on_tab_clicked)),
            )
            .child(
                Button::new("new-tab")
                    .icon(IconName::Plus)
                    .ghost()
                    .xsmall()
                    .tooltip("新标签（⌘T）")
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.action_new_tab(&NewTab, window, cx)
                    })),
            )
    }

    fn render_capability_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let vfs = self.active_browser().read(cx).vfs().clone();
        let capability = vfs.capability();

        // A compact honest summary of what this backend can do. It is the same
        // data the context menu is built from, surfaced so the difference
        // between backends is visible before you right-click anything.
        let flags = [
            ("重命名", capability.rename),
            ("服务端复制", capability.copy),
            ("分享链接", capability.presign),
            ("递归删除", capability.delete_with_recursive),
            ("版本", capability.list_with_versions),
        ];

        h_flex()
            .gap_2()
            .px_3()
            .py_1()
            .items_center()
            .text_xs()
            .border_b_1()
            .border_color(cx.theme().border)
            .children(flags.into_iter().map(|(label, supported)| {
                div()
                    .px_1p5()
                    .rounded_sm()
                    .bg(if supported {
                        cx.theme().accent
                    } else {
                        cx.theme().muted
                    })
                    .text_color(if supported {
                        cx.theme().accent_foreground
                    } else {
                        cx.theme().muted_foreground
                    })
                    .child(label)
            }))
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title = match self.active_profile() {
            Some(profile) => format!("{} · {}", profile.name, profile.uri),
            None => format!("本机 · {}", self.active_browser().read(cx).label()),
        };

        div()
            .relative()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            // Tab management sits on the window, not the pane: a tab outlives
            // whichever pane has focus.
            .key_context(WORKSPACE_CONTEXT)
            .on_action(cx.listener(Self::action_new_tab))
            .on_action(cx.listener(Self::action_close_tab))
            .on_action(cx.listener(Self::action_next_tab))
            .on_action(cx.listener(Self::action_prev_tab))
            .child(
                h_flex().size_full().child(self.render_sidebar(cx)).child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .child(
                            h_flex()
                                .px_3()
                                .py_1p5()
                                .items_center()
                                .justify_between()
                                .border_b_1()
                                .border_color(cx.theme().border)
                                .child(div().text_sm().child(title))
                                .when_some(self.error.clone(), |el, err| {
                                    // One line shared with the title, so the
                                    // backend's own words go in a tooltip when
                                    // they do not fit.
                                    let full: SharedString = err.full_message().into();
                                    el.child(
                                        div()
                                            .id("connect-error")
                                            .text_xs()
                                            .min_w_0()
                                            .truncate()
                                            .text_color(cx.theme().danger)
                                            .child(full.clone())
                                            .tooltip(move |window, cx| {
                                                Tooltip::new(full.clone()).build(window, cx)
                                            }),
                                    )
                                }),
                        )
                        .child(self.render_tab_bar(cx))
                        .child(self.render_capability_bar(cx))
                        .child(
                            div()
                                .flex_1()
                                .min_h_0()
                                .child(self.active_browser().clone()),
                        )
                        .child(self.transfers.clone()),
                ),
            )
            // Root does not draw these itself; the app's root view has to.
            .children(Root::render_dialog_layer(window, cx))
            .children(Root::render_notification_layer(window, cx))
    }
}

/// How tall the connection dialog may get, given the window's height.
///
/// The dialog has to be the thing that is bounded. gpui-component wraps a
/// dialog's children in a scroll area already, but that area only ever
/// overflows — and so only ever grows a scrollbar — if the popup around it has
/// a ceiling; left alone the popup simply gets taller than the window, taking
/// the buttons with it. The form itself must not carry the cap instead: see the
/// comment in `ConnectionForm::render` for why capping a scrollable clips it.
///
/// The popup is anchored a tenth of the way down the window, so 0.8 leaves the
/// same margin underneath it.
fn dialog_max_height(viewport_height: Pixels) -> Pixels {
    viewport_height * 0.8
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};
    use std::cell::RefCell;
    use std::path::PathBuf;
    use std::rc::Rc;
    use std::time::Duration;

    pub(crate) struct Harness {
        _local_dir: tempfile::TempDir,
        _remote_dir: tempfile::TempDir,
        _config_dir: tempfile::TempDir,
        remote_root: PathBuf,
        config_path: PathBuf,
        workspace: Entity<Workspace>,
        cx: VisualTestContext,
    }

    impl Harness {
        pub(crate) fn new(cx: &mut TestAppContext) -> Self {
            let local_dir = tempfile::tempdir().unwrap();
            std::fs::write(local_dir.path().join("local-only.txt"), b"local").unwrap();

            // A second filesystem root stands in for a remote backend: it is a
            // genuinely different session, without needing the network.
            let remote_dir = tempfile::tempdir().unwrap();
            std::fs::write(remote_dir.path().join("remote-only.txt"), b"remote").unwrap();

            let config_dir = tempfile::tempdir().unwrap();
            let config_path = config_dir.path().join("profiles.toml");

            cx.update(gpui_component::init);

            let rt = Rt::new().unwrap();
            let local = Vfs::local(rt.clone(), local_dir.path().to_str().unwrap()).unwrap();
            let store = Arc::new(ProfileStore::at(&config_path));

            // The window root must be a `gpui_component::Root`, exactly as in
            // main.rs: `window.push_notification` and the dialog layer both
            // reach for it and panic otherwise. Wrapping here keeps the tests
            // exercising the same arrangement the app ships.
            let holder: Rc<RefCell<Option<Entity<Workspace>>>> = Rc::new(RefCell::new(None));
            let window = {
                let holder = holder.clone();
                cx.add_window(move |window, cx| {
                    let workspace = cx.new(|cx| Workspace::new(rt, store, local, window, cx));
                    *holder.borrow_mut() = Some(workspace.clone());
                    Root::new(gpui::AnyView::from(workspace), window, cx)
                })
            };
            let workspace = holder.borrow().clone().expect("workspace was built");
            let visual = VisualTestContext::from_window(window.into(), cx);

            let mut harness = Self {
                remote_root: remote_dir.path().to_path_buf(),
                _local_dir: local_dir,
                _remote_dir: remote_dir,
                _config_dir: config_dir,
                config_path,
                workspace,
                cx: visual,
            };
            harness.settle();
            harness
        }

        pub(crate) fn settle(&mut self) {
            for _ in 0..400 {
                self.cx.run_until_parked();
                let done = self
                    .workspace
                    .read_with(&self.cx, |w, cx| !w.active_browser().read(cx).is_busy());
                if done {
                    self.cx.run_until_parked();
                    return;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            panic!("the listing never settled");
        }

        /// Create directories in the local fixture root.
        pub(crate) fn make_dirs(&mut self, dirs: &[&str]) {
            for dir in dirs {
                std::fs::create_dir_all(self._local_dir.path().join(dir)).unwrap();
            }
        }

        pub(crate) fn write_local_file(&mut self, name: &str) {
            std::fs::write(self._local_dir.path().join(name), b"x").unwrap();
        }

        /// Re-open the local session so the tree lists the new directories.
        pub(crate) fn reconnect_local(&mut self) {
            let workspace = self.workspace.clone();
            self.cx.update(|_, cx| {
                workspace.update(cx, |w, cx| w.connect_local(cx));
            });
            self.settle();
        }

        pub(crate) fn add_profile(&mut self, profile: Profile) {
            self.workspace.update(&mut self.cx, |w, _| {
                w.profiles.push(profile);
            });
        }

        pub(crate) fn error_message(&mut self) -> Option<String> {
            self.workspace
                .read_with(&self.cx, |w, _| w.error.as_ref().map(|e| e.full_message()))
        }

        /// The active pane's own error banner, which is where a failed listing
        /// shows up (as opposed to a failed connect).
        pub(crate) fn pane_error(&mut self) -> Option<String> {
            self.workspace.read_with(&self.cx, |w, cx| {
                w.active_browser().read(cx).error_message()
            })
        }

        pub(crate) fn active_capabilities(&mut self) -> (bool, bool) {
            self.workspace.read_with(&self.cx, |w, cx| {
                let cap = *w.active_browser().read(cx).vfs().capability();
                (cap.presign, cap.rename)
            })
        }

        pub(crate) fn remove_profile(&mut self, id: &str) {
            let workspace = self.workspace.clone();
            let id = id.to_string();
            self.cx.update(|window, cx| {
                workspace.update(cx, |w, cx| w.remove_profile(id.clone(), window, cx));
            });
            self.settle();
        }

        /// The active pane's context menu for a named row.
        pub(crate) fn menu_for_active(&self, name: &str) -> Vec<roam_core::MenuItem> {
            let name = name.to_string();
            self.workspace.read_with(&self.cx, |w, cx| {
                let browser = w.active_browser().read(cx);
                let entry = browser
                    .row_names(cx)
                    .iter()
                    .position(|candidate| candidate == &name)
                    .and_then(|_| browser.entry_named(&name, cx))
                    .unwrap_or_else(|| panic!("no row named {name}"));
                browser.vfs().entry_menu(&entry)
            })
        }

        pub(crate) fn tab_count(&mut self) -> usize {
            self.workspace.read_with(&self.cx, |w, _| w.tabs.len())
        }

        pub(crate) fn active_tab_index(&mut self) -> usize {
            self.workspace.read_with(&self.cx, |w, _| w.active)
        }

        /// Each tab's session label plus its current directory.
        pub(crate) fn tab_summaries(&mut self) -> Vec<String> {
            self.workspace.read_with(&self.cx, |w, cx| {
                w.tabs
                    .iter()
                    .map(|tab| {
                        let browser = tab.browser.read(cx);
                        format!("{}:{}", browser.label(), browser.cwd())
                    })
                    .collect()
            })
        }

        pub(crate) fn new_tab(&mut self) {
            let workspace = self.workspace.clone();
            self.cx.update(|window, cx| {
                workspace.update(cx, |w, cx| {
                    w.action_new_tab(&crate::actions::NewTab, window, cx)
                });
            });
            self.settle();
            self.settle_tree();
        }

        pub(crate) fn close_active_tab(&mut self) {
            let workspace = self.workspace.clone();
            self.cx.update(|window, cx| {
                workspace.update(cx, |w, cx| {
                    w.action_close_tab(&crate::actions::CloseTab, window, cx)
                });
            });
            self.settle();
        }

        pub(crate) fn next_tab(&mut self) {
            let workspace = self.workspace.clone();
            self.cx.update(|window, cx| {
                workspace.update(cx, |w, cx| {
                    w.action_next_tab(&crate::actions::NextTab, window, cx)
                });
            });
            self.settle();
            self.settle_tree();
        }

        pub(crate) fn tree_labels(&mut self) -> Vec<String> {
            self.workspace
                .read_with(&self.cx, |w, cx| w.tree.read(cx).row_labels())
        }

        pub(crate) fn settle_tree(&mut self) {
            for _ in 0..300 {
                self.cx.run_until_parked();
                let settled = self
                    .workspace
                    .read_with(&self.cx, |w, cx| w.tree.read(cx).is_settled());
                if settled {
                    self.cx.run_until_parked();
                    return;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            panic!("the tree never settled: {:?}", self.tree_labels());
        }

        /// Flip the active pane's hidden-files setting, exactly as the toolbar
        /// button does.
        pub(crate) fn toggle_hidden(&mut self) {
            let workspace = self.workspace.clone();
            self.cx.update(|_, cx| {
                workspace.update(cx, |w, cx| {
                    let browser = w.active_browser().clone();
                    browser.update(cx, |browser, cx| browser.toggle_hidden_for_test(cx));
                });
            });
            self.settle();
            self.settle_tree();
        }

        pub(crate) fn toggle_tree(&mut self, dir: &str) {
            let workspace = self.workspace.clone();
            let dir = dir.to_string();
            self.cx.update(|_, cx| {
                workspace.update(cx, |w, cx| {
                    w.tree.update(cx, |tree, cx| tree.toggle_for_test(&dir, cx));
                });
            });
            self.settle_tree();
        }

        pub(crate) fn navigate_pane(&mut self, dir: &str) {
            let workspace = self.workspace.clone();
            let dir = dir.to_string();
            self.cx.update(|_, cx| {
                workspace.update(cx, |w, cx| {
                    let browser = w.active_browser().clone();
                    browser.update(cx, |browser, cx| browser.navigate_to_dir(&dir, cx));
                });
            });
            self.settle();
            self.settle_tree();
        }

        pub(crate) fn rows(&mut self) -> Vec<String> {
            self.workspace
                .read_with(&self.cx, |w, cx| w.active_browser().read(cx).row_names(cx))
        }

        /// The active tab's profile id, or `None` for the local session.
        pub(crate) fn active(&mut self) -> Option<String> {
            self.workspace
                .read_with(&self.cx, |w, _| w.active_tab().profile.clone())
        }

        pub(crate) fn add_remote_profile(&mut self) -> Profile {
            let mut profile = Profile::new("remote", "远端", "fs:///");
            profile.options.insert(
                "root".into(),
                self.remote_root.to_str().unwrap().to_string(),
            );

            self.workspace.update(&mut self.cx, |w, _| {
                w.profiles.push(profile.clone());
            });
            profile
        }

        pub(crate) fn connect(&mut self, id: &str) {
            let workspace = self.workspace.clone();
            let id = id.to_string();
            self.cx.update(|window, cx| {
                workspace.update(cx, |w, cx| w.connect(id.clone(), window, cx));
            });
            self.settle();
        }
    }

    #[gpui::test]
    fn opens_on_the_local_session(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);

        assert_eq!(h.active(), None, "no saved profile is active on first run");
        assert_eq!(h.rows(), vec!["local-only.txt"]);
    }

    #[gpui::test]
    fn a_missing_profiles_file_is_not_an_error(cx: &mut TestAppContext) {
        let h = Harness::new(cx);

        let err = h.workspace.read_with(&h.cx, |w, _| w.error.clone());
        assert!(err.is_none(), "first run should be quiet");
        assert!(!h.config_path.exists());
    }

    /// The buttons live at the bottom of the popup, so a dialog that does not
    /// fit under its own anchor takes them off the screen — which is exactly
    /// how the connection dialog came to have no visible way to save or cancel.
    #[test]
    fn the_dialog_fits_under_its_own_anchor() {
        for height in [px(600.), px(760.), px(1400.)] {
            let bottom = height / 10. + dialog_max_height(height);
            assert!(
                bottom <= height,
                "at {height:?} the dialog ends at {bottom:?}"
            );
        }
    }

    #[gpui::test]
    fn connecting_a_profile_switches_the_backend(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);
        h.add_remote_profile();

        h.connect("remote");

        assert_eq!(h.active().as_deref(), Some("remote"));
        assert_eq!(
            h.rows(),
            vec!["remote-only.txt"],
            "rows come from the new backend"
        );
        assert_eq!(
            h.workspace.read_with(&h.cx, |w, cx| w
                .active_browser()
                .read(cx)
                .label()
                .to_string()),
            "远端"
        );
    }

    #[gpui::test]
    fn switching_sessions_does_not_leak_the_previous_listing(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);
        h.add_remote_profile();

        h.connect("remote");
        assert_eq!(h.rows(), vec!["remote-only.txt"]);

        h.workspace.update(&mut h.cx, |w, cx| w.connect_local(cx));
        h.settle();

        // The cache is per-session and cleared on switch, so nothing from the
        // remote listing survives into the local one.
        assert_eq!(h.rows(), vec!["local-only.txt"]);
        assert_eq!(h.active(), None);
    }

    #[gpui::test]
    fn a_profile_missing_a_required_field_blocks_the_switch(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);
        // An S3 profile with no keys. The schema knows it cannot connect, so this
        // has to fail before the session changes rather than as a 403 later.
        h.add_profile(Profile::new("half", "半个连接", "s3://bucket/"));

        h.connect("half");

        assert_eq!(h.active(), None, "the session must not change");
        assert_eq!(h.rows(), vec!["local-only.txt"], "still showing local");

        let err = h
            .workspace
            .read_with(&h.cx, |w, _| w.error.clone())
            .expect("an error should be reported");
        assert!(
            err.user_message().contains("Access Key ID"),
            "got {}",
            err.user_message()
        );
    }

    #[gpui::test]
    fn history_is_cleared_when_the_session_changes(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);
        std::fs::create_dir(h.remote_root.join("sub")).unwrap();
        h.add_remote_profile();

        h.connect("remote");
        h.workspace.update(&mut h.cx, |w, cx| {
            let browser = w.active_browser().clone();
            browser.update(cx, |browser, cx| browser.navigate_to("sub/", cx))
        });
        h.settle();

        // Switching away and back must not offer "back" into the other
        // backend's path.
        h.workspace.update(&mut h.cx, |w, cx| w.connect_local(cx));
        h.settle();

        let can_go_back = h
            .workspace
            .read_with(&h.cx, |w, cx| w.active_browser().read(cx).can_go_back());
        assert!(!can_go_back, "history from the old session was discarded");
    }

    #[gpui::test]
    fn saving_a_form_writes_the_profile_with_its_credential(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);
        let root = h.remote_root.to_str().unwrap().to_string();

        let workspace = h.workspace.clone();
        h.cx.update(|window, cx| {
            let form = cx.new(|cx| ConnectionForm::new(window, cx));
            form.update(cx, |form, cx| {
                form.set_name("远端", window, cx);
                // fs is the first service, so it is what a fresh form shows.
                form.set_field("root", &root, window, cx);
            });

            workspace.update(cx, |w, cx| {
                w.form = Some(form);
                assert!(w.save_form(window, cx), "the dialog should close");
            });
        });
        h.settle();

        let text = std::fs::read_to_string(&h.config_path).unwrap();
        assert!(text.contains(&root), "the directory is recorded");
        assert!(
            text.contains("fs://"),
            "the URI is composed from the form: {text}"
        );
    }

    #[gpui::test]
    fn a_credential_is_saved_into_the_config_file(cx: &mut TestAppContext) {
        // The inverse of what this file used to assert. Worth an explicit test
        // rather than an absence: the credential really is on disk now, and a
        // reader of these tests should not have to infer that.
        let mut h = Harness::new(cx);

        let workspace = h.workspace.clone();
        h.cx.update(|window, cx| {
            let form = cx.new(|cx| ConnectionForm::new(window, cx));
            form.update(cx, |form, cx| {
                form.choose_scheme("s3", window, cx);
                form.set_name("S3", window, cx);
                form.set_field("bucket", "my-bucket", window, cx);
                form.set_field("access_key_id", "AKIAEXAMPLE", window, cx);
                form.set_field("secret_access_key", "TOP-SECRET", window, cx);
            });

            workspace.update(cx, |w, cx| {
                w.form = Some(form);
                assert!(w.save_form(window, cx), "the dialog should close");
            });
        });
        h.settle();

        let text = std::fs::read_to_string(&h.config_path).unwrap();
        assert!(text.contains("TOP-SECRET"), "the credential is stored here");
        assert!(text.contains("s3://my-bucket/"), "got: {text}");
    }

    #[gpui::test]
    fn a_missing_required_field_keeps_the_dialog_open(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);

        let workspace = h.workspace.clone();
        h.cx.update(|window, cx| {
            let form = cx.new(|cx| ConnectionForm::new(window, cx));
            form.update(cx, |form, cx| {
                form.choose_scheme("s3", window, cx);
                form.set_name("S3", window, cx);
                form.set_field("bucket", "bucket", window, cx);
                // No keys typed: the schema says S3 cannot connect without them.
            });

            workspace.update(cx, |w, cx| {
                w.form = Some(form);
                assert!(
                    !w.save_form(window, cx),
                    "the dialog stays open so the input is not lost"
                );
            });
        });

        assert!(
            !h.config_path.exists(),
            "nothing is written when validation fails"
        );
    }

    #[gpui::test]
    fn switching_service_keeps_the_fields_both_share(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);

        h.cx.update(|window, cx| {
            let form = cx.new(|cx| ConnectionForm::new(window, cx));
            form.update(cx, |form, cx| {
                form.choose_scheme("s3", window, cx);
                form.set_field("bucket", "shared", window, cx);
                form.set_field("endpoint", "http://127.0.0.1:9000", window, cx);
                form.set_field("access_key_id", "k", window, cx);
                form.set_field("secret_access_key", "s", window, cx);

                form.set_name("GCS", window, cx);
                form.choose_scheme("gcs", window, cx);
                assert_eq!(form.scheme(), "gcs");

                // gcs has bucket and endpoint too, so retyping them would be
                // the exact tedium this form removes.
                let err = form
                    .build(&[], cx)
                    .expect_err("gcs still needs its own token");
                assert!(
                    err.user_message().contains("访问令牌"),
                    "got {}",
                    err.user_message()
                );

                form.set_field("token", "t", window, cx);
                let profile = form.build(&[], cx).unwrap();
                assert_eq!(profile.uri, "gcs://shared/");
                assert_eq!(
                    profile.options.get("endpoint").unwrap(),
                    "http://127.0.0.1:9000"
                );
                // And the S3-only key did not follow it over.
                assert!(!profile.options.contains_key("secret_access_key"));
            });
        });
    }

    #[gpui::test]
    fn editing_shows_the_stored_values_including_the_credential(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);

        let mut profile = Profile::new("p", "MinIO", "s3://bucket/data");
        profile.options.insert("region".into(), "us-east-1".into());
        profile
            .options
            .insert("access_key_id".into(), "AKIA".into());
        profile
            .options
            .insert("secret_access_key".into(), "shhh".into());

        h.cx.update(|window, cx| {
            let form = cx.new(|cx| ConnectionForm::editing(&profile, window, cx));
            form.update(cx, |form, cx| {
                assert_eq!(form.scheme(), "s3");
                // Editing must not blank the credential — "change the region"
                // would otherwise delete the password.
                let rebuilt = form.build(&[], cx).unwrap();
                assert_eq!(rebuilt, profile);
            });
        });
    }
}

#[cfg(test)]
mod tree_tests {
    use super::tests::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn the_tree_lists_the_root_on_open(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);
        h.settle_tree();

        // The local fixture has no subdirectories, so just the root.
        assert_eq!(h.tree_labels(), vec!["/"]);
    }

    #[gpui::test]
    fn only_directories_appear_in_the_tree(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);
        h.make_dirs(&["alpha", "beta"]);
        h.write_local_file("loose.txt");
        h.reconnect_local();
        h.settle_tree();

        assert_eq!(h.tree_labels(), vec!["/", "  alpha", "  beta"]);
    }

    #[gpui::test]
    fn a_child_directory_loads_only_when_expanded(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);
        h.make_dirs(&["alpha", "alpha/nested"]);
        h.reconnect_local();
        h.settle_tree();

        // Collapsed: the nested directory has not been listed at all.
        assert_eq!(h.tree_labels(), vec!["/", "  alpha"]);

        h.toggle_tree("alpha/");
        assert_eq!(h.tree_labels(), vec!["/", "  alpha", "    nested"]);
    }

    #[gpui::test]
    fn collapsing_hides_children_without_forgetting_them(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);
        h.make_dirs(&["alpha", "alpha/nested"]);
        h.reconnect_local();
        h.settle_tree();

        h.toggle_tree("alpha/");
        assert_eq!(h.tree_labels().len(), 3);

        h.toggle_tree("alpha/");
        assert_eq!(h.tree_labels().len(), 2);

        // Re-expanding is instant because the children were kept.
        h.toggle_tree("alpha/");
        assert_eq!(h.tree_labels().len(), 3);
    }

    #[gpui::test]
    fn navigating_the_pane_reveals_the_directory_in_the_tree(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);
        h.make_dirs(&["alpha", "alpha/nested"]);
        h.reconnect_local();
        h.settle_tree();

        // Deep navigation expands every ancestor, so you can see where you are.
        h.navigate_pane("alpha/nested/");

        assert_eq!(h.tree_labels(), vec!["/", "  alpha", "    nested"]);
    }

    #[gpui::test]
    fn dot_directories_stay_out_of_the_tree(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);
        h.make_dirs(&[".git", "alpha"]);
        h.reconnect_local();
        h.settle_tree();

        // A repository checkout is mostly `.git` by volume; the sidebar is for
        // the directories someone is actually browsing.
        assert_eq!(h.tree_labels(), vec!["/", "  alpha"]);
    }

    #[gpui::test]
    fn the_hidden_files_toggle_reaches_the_tree(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);
        h.make_dirs(&[".git", "alpha"]);
        h.reconnect_local();
        h.settle_tree();

        // One setting, both halves of the window: a pane listing `.git` beside a
        // tree denying it exists is the confusing case.
        h.toggle_hidden();
        assert_eq!(h.tree_labels(), vec!["/", "  .git", "  alpha"]);

        h.toggle_hidden();
        assert_eq!(h.tree_labels(), vec!["/", "  alpha"]);
    }

    #[gpui::test]
    fn navigating_into_a_dot_directory_still_reveals_it(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);
        h.make_dirs(&[".config", ".config/nvim"]);
        h.reconnect_local();
        h.settle_tree();
        assert_eq!(h.tree_labels(), vec!["/"]);

        // Typed into the pane rather than clicked in the tree. Hiding the row now
        // would leave the sidebar denying where the pane is standing.
        h.navigate_pane(".config/nvim/");

        assert_eq!(h.tree_labels(), vec!["/", "  .config", "    nvim"]);
    }

    #[gpui::test]
    fn switching_sessions_resets_the_tree(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);
        h.make_dirs(&["alpha"]);
        h.reconnect_local();
        h.settle_tree();
        assert_eq!(h.tree_labels(), vec!["/", "  alpha"]);

        h.add_remote_profile();
        h.connect("remote");
        h.settle_tree();

        // The remote fixture has no subdirectories; nothing from the local
        // session may leak in.
        assert_eq!(h.tree_labels(), vec!["/"]);
    }
}

#[cfg(test)]
mod tab_tests {
    use super::tests::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn a_window_opens_with_one_tab(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);

        assert_eq!(h.tab_count(), 1);
        assert_eq!(h.active_tab_index(), 0);
    }

    #[gpui::test]
    fn a_new_tab_opens_where_you_already_are(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);
        h.make_dirs(&["alpha"]);
        h.reconnect_local();
        h.navigate_pane("alpha/");

        h.new_tab();

        // Inheriting the directory: a new tab at the root would throw away the
        // navigation you just did.
        assert_eq!(h.tab_count(), 2);
        assert_eq!(h.active_tab_index(), 1);
        let summaries = h.tab_summaries();
        assert!(summaries[1].ends_with(":alpha/"), "got {summaries:?}");
    }

    #[gpui::test]
    fn tabs_hold_independent_directories(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);
        h.make_dirs(&["alpha", "beta"]);
        h.reconnect_local();

        h.new_tab();
        h.navigate_pane("alpha/");
        h.next_tab(); // wraps back to the first tab
        h.navigate_pane("beta/");

        let summaries = h.tab_summaries();
        assert!(summaries[0].ends_with(":beta/"), "got {summaries:?}");
        assert!(summaries[1].ends_with(":alpha/"), "got {summaries:?}");
    }

    #[gpui::test]
    fn tabs_can_sit_on_different_backends(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);
        h.add_remote_profile();

        // The whole point of independent tabs: local in one, remote in another.
        h.new_tab();
        h.connect("remote");

        assert_eq!(h.active(), Some("remote".to_string()));
        assert_eq!(h.rows(), vec!["remote-only.txt"]);

        h.next_tab();
        assert_eq!(h.active(), None, "the other tab is still local");
        assert_eq!(h.rows(), vec!["local-only.txt"]);
    }

    #[gpui::test]
    fn connecting_only_changes_the_active_tab(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);
        h.add_remote_profile();
        h.new_tab();

        h.connect("remote");

        let summaries = h.tab_summaries();
        assert!(summaries[0].starts_with("本机") || !summaries[0].starts_with("远端"));
        assert!(summaries[1].starts_with("远端"), "got {summaries:?}");
    }

    #[gpui::test]
    fn closing_a_tab_keeps_the_rest(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);
        h.new_tab();
        h.new_tab();
        assert_eq!(h.tab_count(), 3);

        h.close_active_tab();

        assert_eq!(h.tab_count(), 2);
        assert_eq!(h.active_tab_index(), 1, "focus falls back to the neighbour");
    }

    #[gpui::test]
    fn the_last_tab_cannot_be_closed(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);

        // An empty window with no way back is not a state worth reaching.
        h.close_active_tab();

        assert_eq!(h.tab_count(), 1);
    }

    #[gpui::test]
    fn switching_tabs_wraps_around(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);
        h.new_tab();
        assert_eq!(h.active_tab_index(), 1);

        h.next_tab();
        assert_eq!(h.active_tab_index(), 0, "wraps to the start");

        h.next_tab();
        assert_eq!(h.active_tab_index(), 1);
    }

    #[gpui::test]
    fn the_sidebar_tree_follows_the_active_tab(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);
        h.make_dirs(&["alpha", "alpha/nested"]);
        h.reconnect_local();

        h.new_tab();
        h.navigate_pane("alpha/nested/");
        assert_eq!(h.tree_labels(), vec!["/", "  alpha", "    nested"]);

        // Back to the first tab, which never left the root.
        h.next_tab();
        let labels = h.tree_labels();
        assert_eq!(labels[0], "/");
    }

    #[gpui::test]
    fn deleting_a_profile_falls_every_tab_back_to_local(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);
        h.add_remote_profile();
        h.connect("remote");
        h.new_tab();
        assert_eq!(h.active(), Some("remote".to_string()));

        h.remove_profile("remote");

        // Both tabs were on the deleted profile; neither may keep pointing at it.
        assert_eq!(h.active(), None);
        assert!(h.tab_summaries().iter().all(|s| !s.starts_with("远端")));
    }
}

#[cfg(test)]
mod s3_tests {
    //! The one seam the fs-backed tests cannot cover: connecting the UI to a
    //! real S3 server. Skipped unless `ROAM_S3_ENDPOINT` is set — see
    //! `roam-core/tests/s3.rs` for how to start one.
    use super::tests::*;
    use gpui::TestAppContext;
    use roam_core::Profile;

    fn s3_profile() -> Option<Profile> {
        let endpoint = std::env::var("ROAM_S3_ENDPOINT").ok()?;
        let bucket = std::env::var("ROAM_S3_BUCKET").unwrap_or_else(|_| "roam-test".into());

        let mut profile = Profile::new("minio", "MinIO", format!("s3://{bucket}/ui-test/"));
        profile.options.insert("endpoint".into(), endpoint);
        profile.options.insert("region".into(), "us-east-1".into());
        profile
            .options
            .insert("enable_virtual_host_style".into(), "false".into());
        profile.options.insert(
            "access_key_id".into(),
            std::env::var("ROAM_S3_KEY").unwrap_or_else(|_| "roamtest".into()),
        );
        profile.options.insert(
            "secret_access_key".into(),
            std::env::var("ROAM_S3_SECRET").unwrap_or_else(|_| "roamtest-secret".into()),
        );
        Some(profile)
    }

    #[gpui::test]
    fn connecting_to_a_real_s3_server_lists_it(cx: &mut TestAppContext) {
        let Some(profile) = s3_profile() else {
            eprintln!("skipping: ROAM_S3_ENDPOINT is not set");
            return;
        };

        let mut h = Harness::new(cx);
        // The credentials ride in the profile itself now.
        h.add_profile(profile);
        h.connect("minio");

        assert_eq!(h.active().as_deref(), Some("minio"));
        // Listing succeeded, so no error banner. The prefix may be empty on a
        // fresh server; what matters is that the request went through.
        assert!(h.error_message().is_none(), "got {:?}", h.error_message());

        // Capabilities come from the live server, and drive the menus.
        let (presign, rename) = h.active_capabilities();
        assert!(presign, "a real S3 server advertises presign");
        assert!(!rename, "and no native rename");
    }

    #[gpui::test]
    fn a_wrong_secret_shows_the_credential_error_in_the_ui(cx: &mut TestAppContext) {
        let Some(profile) = s3_profile() else {
            eprintln!("skipping: ROAM_S3_ENDPOINT is not set");
            return;
        };

        let mut profile = profile;
        profile
            .options
            .insert("access_key_id".into(), "wrong".into());
        profile
            .options
            .insert("secret_access_key".into(), "alsowrong".into());

        let mut h = Harness::new(cx);
        h.add_profile(profile);
        h.connect("minio");

        // The session still switches — the credentials are only rejected once a
        // request is made — but the pane must surface the failure rather than
        // looking like an empty bucket.
        h.settle();
        let shown = h
            .pane_error()
            .expect("an empty pane with no message is indistinguishable from an empty bucket");
        assert!(
            shown.starts_with("没有访问权限："),
            "the headline should still lead: {shown}"
        );
        // And the server's own words follow it — "没有访问权限" alone does not
        // say which key was rejected.
        assert!(
            shown.len() > "没有访问权限：".len(),
            "no detail from the server: {shown}"
        );
    }
}

#[cfg(test)]
mod version_tests {
    //! Version browsing through the UI, against a versioned bucket.
    use super::tests::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn the_menu_offers_version_history_on_a_versioned_backend(cx: &mut TestAppContext) {
        use roam_core::EntryAction;

        let h = Harness::new(cx);

        // Local fs has no versioning, so the item must be disabled with a
        // reason rather than missing.
        let items = h.menu_for_active("local-only.txt");
        let versions = items
            .iter()
            .find(|i| i.action == EntryAction::ShowVersions)
            .unwrap();
        assert!(!versions.enabled);
        assert_eq!(versions.display_label(), "版本历史（该后端不支持版本历史）");
    }
}
