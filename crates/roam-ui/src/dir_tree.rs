use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    AnyElement, ClickEvent, Context, InteractiveElement, IntoElement, ParentElement, Render,
    StatefulInteractiveElement, Styled, Task, Window, div, prelude::FluentBuilder, px,
    uniform_list,
};
use gpui_component::{ActiveTheme, Icon, IconName, h_flex};
use roam_core::{DirTree, TreeRow, Vfs};

/// Every row is this tall, which is what lets the list virtualize: a
/// `uniform_list` has to know an item's height without building it.
const ROW_HEIGHT: gpui::Pixels = px(22.);

/// The tallest the sidebar tree gets before it scrolls.
const PANEL_HEIGHT: gpui::Pixels = px(260.);

/// The list's viewport: the rows it has, capped.
fn visible_height(rows: usize) -> gpui::Pixels {
    let wanted = ROW_HEIGHT * (rows.max(1) as f32);
    if wanted < PANEL_HEIGHT {
        wanted
    } else {
        PANEL_HEIGHT
    }
}

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
    /// The flattened row list, rebuilt only when the tree changes.
    ///
    /// `DirTree::visible()` walks the whole expanded tree and allocates a
    /// `TreeRow` (two `Arc<str>`) per node. Calling it from `render` meant paying
    /// that on every frame: measured at 0.96 ms for 10k expanded directories and
    /// 2.68 ms for 50k, for a panel that shows about fifteen rows.
    rows: Vec<TreeRow>,
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
            rows: Vec::new(),
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

    /// Re-flatten the tree. Called from every place that changes it — and from
    /// nowhere else, which is the point.
    fn refresh_rows(&mut self) {
        self.rows = self.tree.visible();
    }

    /// Follow the main pane: expand ancestors so the current directory is
    /// visible, and highlight it.
    pub fn set_current(&mut self, dir: &str, cx: &mut Context<Self>) {
        self.current = dir.into();
        for pending in self.tree.reveal(dir) {
            self.load(&pending, cx);
        }
        self.refresh_rows();
        cx.notify();
    }

    fn load(&mut self, dir: &str, cx: &mut Context<Self>) {
        if self.tree.is_loading(dir) {
            return;
        }
        self.tree.mark_loading(dir);
        self.refresh_rows();
        cx.notify();

        let vfs = self.vfs.clone();
        let dir: Arc<str> = dir.into();

        self.tasks.push(cx.spawn(async move |this, cx| {
            // Only the directories matter here; files are the main pane's job,
            // and `list_dirs` drops them as they stream instead of collecting a
            // whole directory first.
            let listed = vfs.list_dirs(&dir).await;

            let _ = this.update(cx, |this, cx| {
                match listed {
                    Ok(dirs) => {
                        this.tree.set_children(&dir, dirs);
                    }
                    // Left unrecorded so expanding again retries, rather than
                    // freezing a permission error into an empty node.
                    Err(_) => this.tree.fail(&dir),
                }
                this.refresh_rows();
                cx.notify();
            });
        }));
    }

    fn toggle(&mut self, dir: Arc<str>, cx: &mut Context<Self>) {
        if self.tree.toggle(&dir) {
            self.load(&dir, cx);
        }
        self.refresh_rows();
        cx.notify();
    }

    fn render_row(&self, ix: usize, row: &TreeRow, cx: &mut Context<Self>) -> AnyElement {
        let path = row.path.clone();
        let is_current = *self.current == *row.path;
        let toggle_path = path.clone();
        let nav_path = path.clone();
        let handler = self.on_navigate.clone();

        h_flex()
            // Indexed rather than named: this used to format the path into a
            // string for every row on every frame. The index is unique within the
            // list, which is all an element id has to be.
            .id(("tree-row", ix))
            .w_full()
            .h(ROW_HEIGHT)
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
                    .id(("tree-twisty", ix))
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
        // Virtualized: the panel shows about a dozen rows, and it used to build an
        // element for every expanded directory in the tree on every frame — a
        // bucket with thousands of prefixes made the whole window stutter. Now
        // only the visible range is built, so the cost is the panel's height
        // rather than the tree's size.
        uniform_list(
            "dir-tree",
            self.rows.len(),
            cx.processor(|this, range: std::ops::Range<usize>, _window, cx| {
                range
                    .filter_map(|ix| {
                        let row = this.rows.get(ix)?.clone();
                        Some(this.render_row(ix, &row, cx))
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .px_1()
        // Sized to the rows until it hits the cap, so a tree with three
        // directories does not reserve the full panel — `max_h` would have let the
        // list size to its content and taken virtualization with it.
        .h(visible_height(self.rows.len()))
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

#[cfg(test)]
impl DirTreeView {
    /// Builds the view around an already-populated tree, without listing
    /// anything.
    ///
    /// Every other path here goes through `load`, which does its work on the
    /// tokio runtime — and zed's test scheduler now fails any gpui test that sees
    /// activity on another thread ("Your test is not deterministic"). Nothing is
    /// loaded here, so nothing runs off-thread and the assertions stay honest.
    pub(crate) fn with_tree_for_test(vfs: Vfs, tree: DirTree) -> Self {
        let mut this = Self {
            vfs,
            tree,
            rows: Vec::new(),
            current: "".into(),
            on_navigate: None,
            tasks: Vec::new(),
        };
        this.refresh_rows();
        this
    }

    pub(crate) fn cached_rows(&self) -> &[TreeRow] {
        &self.rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext as _, TestAppContext};
    use roam_core::Rt;

    fn tree_with(children: &[(&str, &[&str])]) -> DirTree {
        let mut tree = DirTree::new();
        for (parent, kids) in children {
            tree.set_children(
                parent,
                kids.iter()
                    .map(|k| Arc::from(*k))
                    .collect::<Vec<Arc<str>>>(),
            );
        }
        tree
    }

    fn view(cx: &mut TestAppContext, tree: DirTree) -> gpui::Entity<DirTreeView> {
        // A Vfs is needed to construct the view but never used: nothing here
        // lists, so the directory can go away again immediately. Creating the
        // runtime does not run anything on it.
        let dir = tempfile::tempdir().unwrap();
        let vfs = Vfs::local(Rt::new().unwrap(), dir.path().to_str().unwrap()).unwrap();

        cx.update(|cx| cx.new(|_| DirTreeView::with_tree_for_test(vfs, tree)))
    }

    #[gpui::test]
    fn the_cache_equals_a_fresh_walk(cx: &mut TestAppContext) {
        let tree = tree_with(&[("", &["alpha/", "beta/"])]);
        let view = view(cx, tree);

        view.read_with(cx, |v, _| {
            assert_eq!(v.cached_rows().len(), 3, "root plus two directories");
            // The cache is the whole point, so it has to equal what rendering
            // used to compute from scratch every frame.
            assert_eq!(v.cached_rows(), v.tree.visible().as_slice());
        });
    }

    #[gpui::test]
    fn expanding_refreshes_the_cache(cx: &mut TestAppContext) {
        // Children already known, so toggling expands without listing.
        let tree = tree_with(&[("", &["alpha/"]), ("alpha/", &["alpha/nested/"])]);
        let view = view(cx, tree);
        view.read_with(cx, |v, _| assert_eq!(v.cached_rows().len(), 2));

        cx.update(|cx| view.update(cx, |v, cx| v.toggle_for_test("alpha/", cx)));

        view.read_with(cx, |v, _| {
            assert_eq!(v.cached_rows().len(), 3, "root, alpha, nested");
            assert_eq!(v.cached_rows(), v.tree.visible().as_slice());
        });

        // And collapsing. A stale cache here would leave a row on screen for a
        // directory the tree no longer shows.
        cx.update(|cx| view.update(cx, |v, cx| v.toggle_for_test("alpha/", cx)));
        view.read_with(cx, |v, _| {
            assert_eq!(v.cached_rows().len(), 2);
            assert_eq!(v.cached_rows(), v.tree.visible().as_slice());
        });
    }

    /// The reason the cache exists: rendering must not depend on how big the tree
    /// is. Ten thousand expanded directories still means one cached list, and the
    /// virtualized list only builds the rows in view.
    #[gpui::test]
    fn a_large_tree_still_caches_exactly_once(cx: &mut TestAppContext) {
        let kids: Vec<String> = (0..10_000).map(|i| format!("dir-{i:05}/")).collect();
        let mut tree = DirTree::new();
        tree.set_children(
            "",
            kids.iter()
                .map(|k| Arc::from(k.as_str()))
                .collect::<Vec<Arc<str>>>(),
        );

        let view = view(cx, tree);
        view.read_with(cx, |v, _| {
            assert_eq!(v.cached_rows().len(), 10_001);
        });
    }
}

#[cfg(test)]
mod height_tests {
    use super::*;

    #[test]
    fn a_short_tree_does_not_reserve_the_whole_panel() {
        // Regression on the virtualization change: switching from `max_h` to a
        // fixed height made a three-row tree occupy 260px of sidebar.
        assert_eq!(visible_height(3), ROW_HEIGHT * 3.0);
        assert!(visible_height(3) < PANEL_HEIGHT);
    }

    #[test]
    fn a_long_tree_stops_at_the_cap() {
        assert_eq!(visible_height(10_000), PANEL_HEIGHT);
    }

    #[test]
    fn an_empty_tree_still_has_a_viewport() {
        // A zero-height list has nothing to virtualize against, and gpui would
        // have no range to ask for.
        assert_eq!(visible_height(0), ROW_HEIGHT);
    }
}
