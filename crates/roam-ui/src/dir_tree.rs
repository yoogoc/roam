use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme, Icon, IconName, h_flex};
use gpui_kit::{
    AnyElement, ClickEvent, Context, InteractiveElement, IntoElement, ParentElement, Render,
    StatefulInteractiveElement, Styled, Task, Window, div, prelude::FluentBuilder, px,
    uniform_list,
};
use roam_core::{DirTree, TreeRow, Vfs};

/// Every row is this tall, which is what lets the list virtualize: a
/// `uniform_list` has to know an item's height without building it.
const ROW_HEIGHT: gpui_kit::Pixels = px(22.);

/// The tallest the sidebar tree gets before it scrolls.
const PANEL_HEIGHT: gpui_kit::Pixels = px(260.);

/// The list's viewport: the rows it has, capped.
fn visible_height(rows: usize) -> gpui_kit::Pixels {
    let wanted = ROW_HEIGHT * (rows.max(1) as f32);
    if wanted < PANEL_HEIGHT {
        wanted
    } else {
        PANEL_HEIGHT
    }
}

/// How much of the sidebar the name has left once the indent, the disclosure
/// triangle, the folder icon and the gaps are paid for. The sidebar is a fixed
/// 216px, so this can be arithmetic rather than measurement.
const NAME_WIDTH_AT_ROOT: f32 = 216. - 8. - 4. - 14. - 12. - 12.;

/// Each depth level costs another indent step.
const INDENT: f32 = 12.;

/// Whether `label` is wide enough at this depth to be cut off, and so wants a
/// tooltip carrying the whole name.
///
/// An estimate on purpose: gpui measures text during layout, which happens long
/// after the row element is built. Guessing slightly high only costs a tooltip
/// on a name that happened to fit.
fn may_truncate(label: &str, depth: usize) -> bool {
    let width: f32 = label
        .chars()
        // CJK and emoji are full-width; the sidebar is mostly filenames, so this
        // two-bucket guess is closer than any single average would be.
        .map(|c| if c.is_ascii() { 6.5 } else { 12.0 })
        .sum();
    width > NAME_WIDTH_AT_ROOT - depth as f32 * INDENT
}

/// Called when a row is clicked, so navigation stays in the workspace.
pub type NavigateHandler = Rc<dyn Fn(Arc<str>, &mut Window, &mut gpui_kit::App)>;

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
        handler: impl Fn(Arc<str>, &mut Window, &mut gpui_kit::App) + 'static,
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

    /// Show or hide dot-directories, following the main pane's toggle.
    ///
    /// Nothing is re-listed: the children are already recorded, and a directory
    /// hidden here is still worth keeping for the moment it is shown again.
    pub fn set_show_hidden(&mut self, show: bool, cx: &mut Context<Self>) {
        if self.tree.show_hidden() == show {
            return;
        }
        self.tree.set_show_hidden(show);
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
        let label = row.label.to_string();

        h_flex()
            // Indexed rather than named: this used to format the path into a
            // string for every row on every frame. The index is unique within the
            // list, which is all an element id has to be.
            .id(("tree-row", ix))
            .w_full()
            .h(ROW_HEIGHT)
            // The row is exactly one line tall and the list places the next row
            // right below it, so anything that overflows would be drawn on top of
            // its neighbour rather than pushing it down.
            .overflow_hidden()
            .pr_1()
            .pl(px(4. + row.depth as f32 * INDENT))
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
            .child(self.render_twisty(ix, row, toggle_path, cx))
            .child(
                Icon::new(if row.expanded {
                    IconName::FolderOpen
                } else {
                    IconName::FolderClosed
                })
                .size_3()
                .flex_none(),
            )
            .child(
                div()
                    .id(("tree-label", ix))
                    .flex_1()
                    .min_w_0()
                    // Without this a long name wraps onto a second line, which the
                    // fixed row height turns into two rows overlapping.
                    .truncate()
                    .when(may_truncate(&label, row.depth), |el| {
                        let full = label.clone();
                        el.tooltip(move |window, cx| Tooltip::new(full.clone()).build(window, cx))
                    })
                    .child(label),
            )
            .on_click(cx.listener(move |_, _: &ClickEvent, window, cx| {
                if let Some(handler) = handler.as_ref() {
                    handler(nav_path.clone(), window, cx);
                }
            }))
            .into_any_element()
    }

    /// The disclosure triangle: its own hit area, so clicking the name navigates
    /// and clicking the triangle only expands.
    fn render_twisty(
        &self,
        ix: usize,
        row: &TreeRow,
        path: Arc<str>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let spacer = div().w(px(14.)).flex_none();

        if row.loading {
            return spacer
                .child(Icon::new(IconName::LoaderCircle).size_3())
                .into_any_element();
        }

        // Nothing to expand: a plain spacer keeps the names lined up, and stays
        // out of the way of the click that navigates.
        if row.leaf {
            return spacer.into_any_element();
        }

        spacer
            .id(("tree-twisty", ix))
            .child(
                Icon::new(if row.expanded {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .size_3(),
            )
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                // The row underneath navigates on click. Without this, expanding a
                // directory would also walk the main pane into it — which is the
                // one thing a disclosure triangle must not do.
                cx.stop_propagation();
                this.toggle(path.clone(), cx);
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
    /// These layout and click tests need a fixed tree. IO-backed tree behavior
    /// is covered by the Workspace harness, which permits real Tokio wakeups.
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
    use gpui_kit::{AppContext as _, TestAppContext};
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

    fn view(cx: &mut TestAppContext, tree: DirTree) -> gpui_kit::Entity<DirTreeView> {
        // A Vfs is needed to construct the view but never used: nothing here
        // lists, so the directory can go away again immediately. Creating the
        // runtime does not run anything on it.
        let dir = tempfile::tempdir().unwrap();
        let vfs = Vfs::local(Rt::new().unwrap(), dir.path().to_str().unwrap()).unwrap();

        cx.update(|cx| cx.new(|_| DirTreeView::with_tree_for_test(vfs, tree)))
    }

    #[gpui_kit::test]
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

    #[gpui_kit::test]
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

    #[gpui_kit::test]
    fn showing_hidden_directories_refreshes_the_cache(cx: &mut TestAppContext) {
        let tree = tree_with(&[("", &[".git/", "alpha/"])]);
        let view = view(cx, tree);

        view.read_with(cx, |v, _| {
            assert_eq!(v.cached_rows().len(), 2, "root plus alpha");
        });

        cx.update(|cx| view.update(cx, |v, cx| v.set_show_hidden(true, cx)));
        view.read_with(cx, |v, _| {
            assert_eq!(v.cached_rows().len(), 3);
            // A stale cache here would keep drawing rows the setting just
            // removed, or hide ones it just admitted.
            assert_eq!(v.cached_rows(), v.tree.visible().as_slice());
        });

        cx.update(|cx| view.update(cx, |v, cx| v.set_show_hidden(false, cx)));
        view.read_with(cx, |v, _| {
            assert_eq!(v.cached_rows().len(), 2);
            assert_eq!(v.cached_rows(), v.tree.visible().as_slice());
        });
    }

    /// The reason the cache exists: rendering must not depend on how big the tree
    /// is. Ten thousand expanded directories still means one cached list, and the
    /// virtualized list only builds the rows in view.
    #[gpui_kit::test]
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

/// Clicking, for real: the twisty and the name share a row, and which one was
/// hit decides whether the pane moves.
#[cfg(test)]
mod click_tests {
    use super::*;
    use gpui_kit::{Modifiers, TestAppContext, VisualTestContext, point};
    use std::cell::RefCell;

    /// A tree with `alpha/` loaded and collapsed, so its row draws a triangle.
    fn tree_with_a_collapsed_child() -> DirTree {
        let mut tree = DirTree::new();
        tree.set_children("", vec![Arc::from("alpha/")]);
        tree.set_children("alpha/", vec![Arc::from("alpha/nested/")]);
        tree
    }

    /// Where in the row the disclosure triangle sits: the list's own padding,
    /// plus the indent for `depth`, plus half a triangle.
    fn twisty_at(depth: usize, row: usize) -> gpui_kit::Point<gpui_kit::Pixels> {
        point(
            px(4. + 4. + depth as f32 * INDENT + 7.),
            ROW_HEIGHT * (row as f32 + 0.5),
        )
    }

    /// A point on the name, well clear of the triangle.
    fn label_at(row: usize) -> gpui_kit::Point<gpui_kit::Pixels> {
        point(px(120.), ROW_HEIGHT * (row as f32 + 0.5))
    }

    /// Builds a window around the tree and reports every navigation it asks for.
    ///
    /// A focused tree fixture: Workspace and its overlay layers are not needed.
    fn window(
        cx: &mut TestAppContext,
        tree: DirTree,
    ) -> (
        gpui_kit::Entity<DirTreeView>,
        Rc<RefCell<Vec<String>>>,
        VisualTestContext,
    ) {
        // The rows ask the theme for their colours; without this the first paint
        // panics looking for it.
        cx.update(gpui_kit::init);

        let dir = tempfile::tempdir().unwrap();
        let vfs = Vfs::local(roam_core::Rt::new().unwrap(), dir.path().to_str().unwrap()).unwrap();
        let navigated: Rc<RefCell<Vec<String>>> = Rc::default();

        let handle = cx.add_window(|_, _| {
            let mut view = DirTreeView::with_tree_for_test(vfs, tree);
            let seen = navigated.clone();
            view.on_navigate(move |path, _, _| seen.borrow_mut().push(path.to_string()));
            view
        });

        let view = handle.root(cx).unwrap();
        let visual = VisualTestContext::from_window(handle.into(), cx);
        visual.run_until_parked();
        (view, navigated, visual)
    }

    #[gpui_kit::test]
    fn the_disclosure_triangle_expands_without_navigating(cx: &mut TestAppContext) {
        let (view, navigated, mut cx) = window(cx, tree_with_a_collapsed_child());

        cx.simulate_click(twisty_at(1, 1), Modifiers::none());

        view.read_with(&cx, |v, _| {
            assert_eq!(
                v.cached_rows().len(),
                3,
                "the triangle expanded alpha: root, alpha, nested"
            );
        });
        assert!(
            navigated.borrow().is_empty(),
            "expanding a directory must not walk the pane into it"
        );
    }

    #[gpui_kit::test]
    fn clicking_the_name_navigates(cx: &mut TestAppContext) {
        let (view, navigated, mut cx) = window(cx, tree_with_a_collapsed_child());

        cx.simulate_click(label_at(1), Modifiers::none());

        assert_eq!(&*navigated.borrow(), &["alpha/".to_string()]);
        view.read_with(&cx, |v, _| {
            assert_eq!(
                v.cached_rows().len(),
                2,
                "navigation alone does not expand — `set_current` does that"
            );
        });
    }
}

#[cfg(test)]
mod truncation_tests {
    use super::*;

    #[test]
    fn an_ordinary_name_needs_no_tooltip() {
        assert!(!may_truncate("documents", 0));
        assert!(!may_truncate("报告", 0));
    }

    #[test]
    fn a_long_name_gets_one() {
        // The kind of name that used to wrap onto a second line and land on top
        // of the row below it.
        assert!(may_truncate(
            "2024-年度财务报表-最终版-请勿外传.xlsx-backup",
            0
        ));
    }

    #[test]
    fn indentation_eats_into_the_budget() {
        let name = "quarterly-reports-2024";
        assert!(!may_truncate(name, 0));
        // Same name, pushed right by six levels of nesting: now it is cut off.
        assert!(may_truncate(name, 6));
    }
}
