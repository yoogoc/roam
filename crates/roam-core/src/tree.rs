//! The sidebar's lazily-loaded directory tree.
//!
//! Only directories are tracked, and only the ones that have been expanded are
//! ever listed: opening the tree on an S3 bucket must not walk it.
//!
//! The model is flat — a map of directory to its child directories, plus a set
//! of expanded paths — and [`DirTree::visible`] flattens it into `(path, depth)`
//! rows. That shape is what the view wants anyway, and it keeps "loaded" and
//! "expanded" as separate facts, which a nested node structure tends to blur.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::path;

/// A row the sidebar draws.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeRow {
    /// OpenDAL directory path, `""` for the root.
    pub path: Arc<str>,
    /// Display name; the root renders as `/`.
    pub label: Arc<str>,
    pub depth: usize,
    pub expanded: bool,
    /// Children have not been fetched yet, so the row shows a spinner.
    pub loading: bool,
    /// Known to have no subdirectories, so no disclosure triangle.
    pub leaf: bool,
}

#[derive(Debug, Default)]
pub struct DirTree {
    /// Child directories per directory. Absent means "never listed".
    children: BTreeMap<Arc<str>, Vec<Arc<str>>>,
    expanded: BTreeSet<Arc<str>>,
    /// Requests in flight, so a row can say so and a second click cannot queue
    /// the same listing twice.
    loading: BTreeSet<Arc<str>>,
    /// Whether dot-directories are drawn. Mirrors the main pane's setting, so
    /// the two halves of the window agree on what exists.
    show_hidden: bool,
}

impl DirTree {
    pub fn new() -> Self {
        let mut tree = Self::default();
        // The root is expanded from the start; a tree that opens collapsed makes
        // the panel look broken.
        tree.expanded.insert("".into());
        tree
    }

    /// Are dot-directories drawn?
    pub fn show_hidden(&self) -> bool {
        self.show_hidden
    }

    /// Show or hide dot-directories. Nothing is re-listed: the children are
    /// already here, and hiding is a display decision.
    pub fn set_show_hidden(&mut self, show: bool) {
        self.show_hidden = show;
    }

    /// Should this child be drawn?
    ///
    /// An expanded dot-directory stays visible even while hidden files are off:
    /// it is only ever expanded because someone navigated into it, and dropping
    /// the row would leave the main pane pointing somewhere the tree denies.
    fn is_visible(&self, dir: &str) -> bool {
        self.show_hidden || !path::basename(dir).starts_with('.') || self.is_expanded(dir)
    }

    pub fn is_loaded(&self, dir: &str) -> bool {
        self.children.contains_key(dir)
    }

    pub fn is_expanded(&self, dir: &str) -> bool {
        self.expanded.contains(dir)
    }

    pub fn is_loading(&self, dir: &str) -> bool {
        self.loading.contains(dir)
    }

    pub fn mark_loading(&mut self, dir: &str) {
        self.loading.insert(dir.into());
    }

    /// Record a directory's children. Clears the loading flag.
    pub fn set_children(&mut self, dir: &str, mut dirs: Vec<Arc<str>>) {
        dirs.sort_by_key(|dir| path::basename(dir).to_lowercase());
        self.children.insert(dir.into(), dirs);
        self.loading.remove(dir);
    }

    /// A listing that failed: drop the loading flag but do not record children,
    /// so expanding again retries instead of showing a permanently empty node.
    pub fn fail(&mut self, dir: &str) {
        self.loading.remove(dir);
    }

    /// Toggle a directory. Returns `true` when the caller should fetch its
    /// children.
    pub fn toggle(&mut self, dir: &str) -> bool {
        if self.expanded.contains(dir) {
            self.expanded.remove(dir);
            return false;
        }

        self.expanded.insert(dir.into());
        !self.is_loaded(dir) && !self.is_loading(dir)
    }

    /// Expand every ancestor of `dir` so it is visible, returning the paths that
    /// still need fetching.
    ///
    /// Used to keep the tree in step with the main pane: navigating by
    /// double-click should reveal where you landed.
    pub fn reveal(&mut self, dir: &str) -> Vec<Arc<str>> {
        let mut to_load = Vec::new();
        let mut current = path::as_dir(dir);

        loop {
            self.expanded.insert(current.as_str().into());
            if !self.is_loaded(&current) && !self.is_loading(&current) {
                to_load.push(Arc::from(current.as_str()));
            }
            match path::parent(&current) {
                Some(parent) => current = parent,
                None => break,
            }
        }

        to_load.reverse();
        to_load
    }

    /// Reset to a fresh root, keeping the display settings — a new session is a
    /// new tree, not a new set of preferences.
    pub fn clear(&mut self) {
        let show_hidden = self.show_hidden;
        *self = Self::new();
        self.show_hidden = show_hidden;
    }

    /// The rows to draw, depth-first, parents before children.
    pub fn visible(&self) -> Vec<TreeRow> {
        let mut rows = Vec::new();
        self.push_rows("", 0, &mut rows);
        rows
    }

    fn push_rows(&self, dir: &str, depth: usize, rows: &mut Vec<TreeRow>) {
        let expanded = self.is_expanded(dir);
        let label: Arc<str> = if dir.is_empty() {
            "/".into()
        } else {
            path::basename(dir).into()
        };

        rows.push(TreeRow {
            path: dir.into(),
            label,
            depth,
            expanded,
            loading: self.is_loading(dir),
            // Only claim leafness once the directory has actually been listed —
            // otherwise every unexpanded node would look childless. Hidden
            // children do not count: a folder holding nothing but `.git` draws a
            // triangle that expands to nothing otherwise.
            leaf: self
                .children
                .get(dir)
                .map(|kids| !kids.iter().any(|kid| self.is_visible(kid)))
                .unwrap_or(false),
        });

        if !expanded {
            return;
        }

        for child in self.children.get(dir).into_iter().flatten() {
            if !self.is_visible(child) {
                continue;
            }
            self.push_rows(child, depth + 1, rows);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dirs(paths: &[&str]) -> Vec<Arc<str>> {
        paths.iter().map(|p| Arc::from(*p)).collect()
    }

    #[test]
    fn a_new_tree_shows_only_the_root() {
        let tree = DirTree::new();
        let rows = tree.visible();

        assert_eq!(rows.len(), 1);
        assert_eq!(&*rows[0].label, "/");
        assert_eq!(rows[0].depth, 0);
        assert!(rows[0].expanded, "opening collapsed looks broken");
    }

    #[test]
    fn children_appear_once_recorded() {
        let mut tree = DirTree::new();
        tree.set_children("", dirs(&["docs/", "media/"]));

        let labels: Vec<String> = tree.visible().iter().map(|r| r.label.to_string()).collect();
        assert_eq!(labels, vec!["/", "docs", "media"]);
    }

    #[test]
    fn children_are_sorted_case_insensitively() {
        let mut tree = DirTree::new();
        tree.set_children("", dirs(&["Zebra/", "apple/", "Mango/"]));

        let labels: Vec<String> = tree
            .visible()
            .iter()
            .skip(1)
            .map(|r| r.label.to_string())
            .collect();
        assert_eq!(labels, vec!["apple", "Mango", "Zebra"]);
    }

    #[test]
    fn collapsed_directories_hide_their_children() {
        let mut tree = DirTree::new();
        tree.set_children("", dirs(&["docs/"]));
        tree.set_children("docs/", dirs(&["docs/reports/"]));

        // docs/ was never expanded, so its child stays hidden.
        assert_eq!(tree.visible().len(), 2);

        assert!(!tree.toggle("docs/"), "already loaded, nothing to fetch");
        assert_eq!(tree.visible().len(), 3);
    }

    #[test]
    fn toggling_an_unloaded_directory_asks_for_a_fetch() {
        let mut tree = DirTree::new();
        tree.set_children("", dirs(&["docs/"]));

        assert!(tree.toggle("docs/"), "children are unknown, so fetch them");
        tree.mark_loading("docs/");

        // A second toggle collapses rather than queueing a duplicate listing.
        assert!(!tree.toggle("docs/"));
    }

    #[test]
    fn a_loading_directory_says_so() {
        let mut tree = DirTree::new();
        tree.mark_loading("");
        assert!(tree.visible()[0].loading);

        tree.set_children("", dirs(&[]));
        assert!(!tree.visible()[0].loading);
    }

    #[test]
    fn leafness_is_only_claimed_after_listing() {
        let mut tree = DirTree::new();
        tree.set_children("", dirs(&["docs/"]));

        let docs = |tree: &DirTree| {
            tree.visible()
                .into_iter()
                .find(|r| &*r.path == "docs/")
                .unwrap()
        };

        // Unlisted: must not look childless, or every collapsed node would lose
        // its disclosure triangle.
        assert!(!docs(&tree).leaf);

        tree.set_children("docs/", dirs(&[]));
        assert!(docs(&tree).leaf, "now known to have no subdirectories");
    }

    #[test]
    fn a_failed_listing_can_be_retried() {
        let mut tree = DirTree::new();
        tree.set_children("", dirs(&["docs/"]));

        assert!(tree.toggle("docs/"));
        tree.mark_loading("docs/");
        tree.fail("docs/");

        // Collapse, then expand again: still asks to fetch, because nothing was
        // ever recorded.
        tree.toggle("docs/");
        assert!(
            tree.toggle("docs/"),
            "a failure must not become an empty node"
        );
    }

    #[test]
    fn reveal_expands_every_ancestor() {
        let mut tree = DirTree::new();

        let to_load = tree.reveal("a/b/c/");

        assert!(tree.is_expanded("a/b/c/"));
        assert!(tree.is_expanded("a/b/"));
        assert!(tree.is_expanded("a/"));
        assert!(tree.is_expanded(""));
        // Outermost first, so the tree fills in top-down.
        assert_eq!(
            to_load,
            dirs(&["", "a/", "a/b/", "a/b/c/"]),
            "ancestors are fetched from the root down"
        );
    }

    #[test]
    fn reveal_skips_directories_already_known() {
        let mut tree = DirTree::new();
        tree.set_children("", dirs(&["a/"]));
        tree.set_children("a/", dirs(&["a/b/"]));

        assert_eq!(tree.reveal("a/b/"), dirs(&["a/b/"]));
    }

    #[test]
    fn dot_directories_stay_out_of_the_way() {
        let mut tree = DirTree::new();
        tree.set_children("", dirs(&[".git/", "docs/"]));

        let labels = |tree: &DirTree| -> Vec<String> {
            tree.visible().iter().map(|r| r.label.to_string()).collect()
        };
        assert_eq!(labels(&tree), vec!["/", "docs"]);

        tree.set_show_hidden(true);
        assert_eq!(labels(&tree), vec!["/", ".git", "docs"]);
    }

    #[test]
    fn hiding_is_about_the_name_not_the_path() {
        let mut tree = DirTree::new();
        // The dot is on an ancestor, not on the child itself.
        tree.set_children("", dirs(&[".config/"]));
        tree.set_children(".config/", dirs(&[".config/nvim/"]));
        tree.reveal(".config/");

        // Revealed, so it stays on screen even with hidden files off — the main
        // pane is standing in it.
        let labels: Vec<String> = tree.visible().iter().map(|r| r.label.to_string()).collect();
        assert_eq!(labels, vec!["/", ".config", "nvim"]);
    }

    #[test]
    fn a_directory_of_only_dotfiles_is_a_leaf() {
        let mut tree = DirTree::new();
        tree.set_children("", dirs(&["project/"]));
        tree.set_children("project/", dirs(&["project/.git/"]));

        let project = |tree: &DirTree| {
            tree.visible()
                .into_iter()
                .find(|r| &*r.path == "project/")
                .unwrap()
        };

        // A triangle here would expand to nothing at all.
        assert!(project(&tree).leaf);

        tree.set_show_hidden(true);
        assert!(!project(&tree).leaf);
    }

    #[test]
    fn clearing_keeps_the_display_setting() {
        let mut tree = DirTree::new();
        tree.set_show_hidden(true);
        tree.clear();

        assert!(
            tree.show_hidden(),
            "a new session is a new tree, not a new preference"
        );
    }

    #[test]
    fn clearing_resets_to_a_fresh_root() {
        let mut tree = DirTree::new();
        tree.set_children("", dirs(&["docs/"]));
        tree.clear();

        assert_eq!(tree.visible().len(), 1);
        assert!(!tree.is_loaded(""), "a new session listed nothing yet");
    }
}
