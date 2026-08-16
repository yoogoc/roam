use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    AnyElement, ClickEvent, Context, InteractiveElement, IntoElement, ParentElement, Render,
    SharedString, StatefulInteractiveElement, Styled, Task, Window, div, prelude::FluentBuilder,
    px,
};
use gpui_component::scroll::ScrollableElement;
use gpui_component::{ActiveTheme, Icon, IconName, h_flex, v_flex};
use roam_core::{DirTree, Vfs};

/// Called when a row is clicked, so navigation stays in the workspace.
pub type NavigateHandler = Rc<dyn Fn(Arc<str>, &mut Window, &mut gpui::App)>;

/// The sidebar's directory tree.
///
/// Rows come from [`roam_core::DirTree`], which tracks "expanded" and "loaded"
/// separately so a node can honestly show a spinner. Only expanded directories
/// are ever listed — opening the tree on a large bucket costs one request.
pub struct DirTreeView {
    vfs: Vfs,
    tree: DirTree,
    /// The directory the main pane is showing, highlighted here.
    current: Arc<str>,
    on_navigate: Option<NavigateHandler>,
    tasks: Vec<Task<()>>,
}

impl DirTreeView {
    pub fn new(vfs: Vfs, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            vfs,
            tree: DirTree::new(),
            current: "".into(),
            on_navigate: None,
            tasks: Vec::new(),
        };
        this.load("", cx);
        this
    }

    pub fn on_navigate(
        &mut self,
        handler: impl Fn(Arc<str>, &mut Window, &mut gpui::App) + 'static,
    ) {
        self.on_navigate = Some(Rc::new(handler));
    }

    pub fn set_vfs(&mut self, vfs: Vfs, cx: &mut Context<Self>) {
        self.vfs = vfs;
        self.tree.clear();
        self.current = "".into();
        self.tasks.clear();
        self.load("", cx);
    }

    /// Follow the main pane: expand ancestors so the current directory is
    /// visible, and highlight it.
    pub fn set_current(&mut self, dir: &str, cx: &mut Context<Self>) {
        self.current = dir.into();
        for pending in self.tree.reveal(dir) {
            self.load(&pending, cx);
        }
        cx.notify();
    }

    fn load(&mut self, dir: &str, cx: &mut Context<Self>) {
        if self.tree.is_loading(dir) {
            return;
        }
        self.tree.mark_loading(dir);
        cx.notify();

        let vfs = self.vfs.clone();
        let dir: Arc<str> = dir.into();

        self.tasks.push(cx.spawn(async move |this, cx| {
            // Only the directories matter here; files are the main pane's job.
            let listed = vfs.list_all(&dir).await;

            let _ = this.update(cx, |this, cx| {
                match listed {
                    Ok(entries) => {
                        let dirs = entries
                            .into_iter()
                            .filter(|entry| entry.is_dir())
                            .map(|entry| entry.path.clone())
                            .collect();
                        this.tree.set_children(&dir, dirs);
                    }
                    // Left unrecorded so expanding again retries, rather than
                    // freezing a permission error into an empty node.
                    Err(_) => this.tree.fail(&dir),
                }
                cx.notify();
            });
        }));
    }

    fn toggle(&mut self, dir: Arc<str>, cx: &mut Context<Self>) {
        if self.tree.toggle(&dir) {
            self.load(&dir, cx);
        }
        cx.notify();
    }

    fn render_row(&self, row: &roam_core::TreeRow, cx: &mut Context<Self>) -> AnyElement {
        let path = row.path.clone();
        let is_current = *self.current == *row.path;
        let toggle_path = path.clone();
        let nav_path = path.clone();
        let handler = self.on_navigate.clone();

        h_flex()
            .id(SharedString::from(format!("tree-{}", row.path)))
            .w_full()
            .py(px(2.))
            .pr_1()
            .pl(px(4. + row.depth as f32 * 12.))
            .gap_1()
            .items_center()
            .rounded_sm()
            .cursor_pointer()
            .text_xs()
            .when(is_current, |el| {
                el.bg(cx.theme().sidebar_accent)
                    .text_color(cx.theme().sidebar_accent_foreground)
            })
            .when(!is_current, |el| {
                el.hover(|el| el.bg(cx.theme().sidebar_accent))
            })
            .child(
                // The disclosure triangle is a separate hit area: clicking the
                // name navigates, clicking the triangle only expands.
                div()
                    .id(SharedString::from(format!("tw-{}", row.path)))
                    .w(px(14.))
                    .flex_none()
                    .when(row.loading, |el| {
                        el.child(Icon::new(IconName::LoaderCircle).size_3())
                    })
                    .when(!row.loading && !row.leaf, |el| {
                        el.child(
                            Icon::new(if row.expanded {
                                IconName::ChevronDown
                            } else {
                                IconName::ChevronRight
                            })
                            .size_3(),
                        )
                    })
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.toggle(toggle_path.clone(), cx);
                    })),
            )
            .child(
                Icon::new(if row.expanded {
                    IconName::FolderOpen
                } else {
                    IconName::FolderClosed
                })
                .size_3()
                .flex_none(),
            )
            .child(div().flex_1().min_w_0().child(row.label.to_string()))
            .on_click(cx.listener(move |_, _: &ClickEvent, window, cx| {
                if let Some(handler) = handler.as_ref() {
                    handler(nav_path.clone(), window, cx);
                }
            }))
            .into_any_element()
    }
}

impl Render for DirTreeView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows: Vec<AnyElement> = self
            .tree
            .visible()
            .iter()
            .map(|row| self.render_row(row, cx))
            .collect();

        v_flex()
            .px_1()
            .max_h(px(260.))
            .overflow_y_scrollbar()
            .children(rows)
    }
}

#[cfg(test)]
impl DirTreeView {
    pub(crate) fn row_labels(&self) -> Vec<String> {
        self.tree
            .visible()
            .iter()
            .map(|row| format!("{}{}", "  ".repeat(row.depth), row.label))
            .collect()
    }

    pub(crate) fn is_settled(&self) -> bool {
        !self.tree.visible().iter().any(|row| row.loading)
    }

    pub(crate) fn toggle_for_test(&mut self, dir: &str, cx: &mut Context<Self>) {
        self.toggle(dir.into(), cx);
    }
}
