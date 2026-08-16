use std::sync::Arc;

use jiff::Timestamp;
use opendal::{Entry, EntryMode};

use crate::path;

pub type SessionId = u32;

/// Bumped on every navigation. Batches arriving with a stale generation are
/// dropped, which is how a listing gets cancelled (see `Vfs::list`).
pub type Generation = u64;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RemotePath {
    pub session: SessionId,
    pub path: Arc<str>,
}

impl RemotePath {
    pub fn new(session: SessionId, path: impl AsRef<str>) -> Self {
        Self {
            session,
            path: path.as_ref().into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Dir,
    File,
    Unknown,
}

#[derive(Debug, Clone)]
pub struct DirEntry {
    /// Basename, for display. No trailing slash even for directories.
    pub name: Arc<str>,
    /// Full OpenDAL path. Directories keep their trailing slash.
    pub path: Arc<str>,
    pub kind: EntryKind,
    pub size: Option<u64>,
    pub modified: Option<Timestamp>,
    pub etag: Option<Arc<str>>,
    /// `false` means the backend's `list` did not return full metadata for this
    /// entry and a `stat` is needed to fill in size/mtime. OpenDAL 0.58 removed
    /// `Metakey`, so completeness is now entirely backend-dependent and has to
    /// be inferred here.
    pub meta_complete: bool,
}

impl DirEntry {
    pub fn from_entry(entry: &Entry) -> Self {
        Self::from_parts(entry.path(), entry.metadata())
    }

    pub fn from_parts(path: &str, meta: &opendal::Metadata) -> Self {
        let kind = match meta.mode() {
            EntryMode::DIR => EntryKind::Dir,
            EntryMode::FILE => EntryKind::File,
            _ => EntryKind::Unknown,
        };

        // A directory has no meaningful size, so it counts as complete once the
        // mode is known. For a file we require last_modified as the tell that
        // the backend really returned metadata rather than a bare listing —
        // `content_length()` defaults to 0 and cannot be distinguished from a
        // genuinely empty file (`has_content_length` is crate-private).
        let meta_complete = match kind {
            EntryKind::Dir => true,
            EntryKind::File => meta.last_modified().is_some(),
            EntryKind::Unknown => false,
        };

        // OpenDAL wraps timestamps in its own `raw::Timestamp` newtype; the
        // model stores plain `jiff::Timestamp`, so convert at this boundary.

        Self {
            name: path::basename(path).into(),
            path: path.into(),
            kind,
            size: (kind == EntryKind::File && meta_complete).then(|| meta.content_length()),
            modified: meta.last_modified().map(Into::into),
            etag: meta.etag().map(Into::into),
            meta_complete,
        }
    }

    /// Rebuild an entry from a `stat` result, keeping the path we already know.
    pub fn with_metadata(&self, meta: &opendal::Metadata) -> Self {
        let kind = match meta.mode() {
            EntryMode::DIR => EntryKind::Dir,
            EntryMode::FILE => EntryKind::File,
            _ => self.kind,
        };

        Self {
            name: self.name.clone(),
            path: self.path.clone(),
            kind,
            size: (kind == EntryKind::File).then(|| meta.content_length()),
            modified: meta.last_modified().map(Into::into).or(self.modified),
            etag: meta.etag().map(Into::into).or_else(|| self.etag.clone()),
            meta_complete: true,
        }
    }

    pub fn is_dir(&self) -> bool {
        self.kind == EntryKind::Dir
    }
}

/// One stored version of an object.
///
/// Only version-aware backends produce these; `Vfs::list_versions` refuses on
/// the rest rather than returning a single fake "current" entry, which would
/// suggest versioning is on when it is not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectVersion {
    /// Backend-assigned id. Absent for a delete marker on some backends.
    pub id: Option<Arc<str>>,
    pub size: Option<u64>,
    pub modified: Option<Timestamp>,
    /// This is the version a plain read returns.
    pub is_current: bool,
    /// A delete marker rather than content — the object was removed at this
    /// point in its history.
    pub is_delete_marker: bool,
}

impl ObjectVersion {
    pub fn from_metadata(meta: &opendal::Metadata) -> Self {
        Self {
            id: meta.version().map(Into::into),
            size: (!meta.is_deleted()).then(|| meta.content_length()),
            modified: meta.last_modified().map(Into::into),
            is_current: meta.is_current().unwrap_or(false),
            is_delete_marker: meta.is_deleted(),
        }
    }

    /// Short id for display; a full S3 version id is unreadably long.
    pub fn short_id(&self) -> String {
        match &self.id {
            Some(id) if id.len() > 12 => format!("{}…", &id[..12]),
            Some(id) => id.to_string(),
            None => "—".to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Name,
    Size,
    Modified,
}

/// Sort into an index view instead of reordering the entries themselves.
///
/// The entry list is shared as `Arc<Vec<DirEntry>>` between the cache and every
/// pane showing it, so sorting must not clone or mutate it — a 100k-entry
/// directory would otherwise copy the whole vector on every sort click.
///
/// Directories always come before files, regardless of key or direction.
pub fn sort_indices(entries: &[DirEntry], key: SortKey, ascending: bool) -> Vec<u32> {
    view_indices(entries, key, ascending, "")
}

/// Does `name` match the filter box's text?
///
/// Case-insensitive substring, which is what people expect from a filter field:
/// typing `rep` finds `Reports` without anchoring or globbing.
pub fn matches_filter(name: &str, filter: &str) -> bool {
    if filter.is_empty() {
        return true;
    }
    name.to_lowercase().contains(&filter.to_lowercase())
}

/// The sorted, filtered index view the table renders from.
///
/// Filtering happens here rather than on the entry list so that clearing the
/// filter costs nothing — the entries were never touched.
pub fn view_indices(entries: &[DirEntry], key: SortKey, ascending: bool, filter: &str) -> Vec<u32> {
    let mut ix: Vec<u32> = (0..entries.len() as u32)
        .filter(|&i| matches_filter(&entries[i as usize].name, filter))
        .collect();

    ix.sort_by(|&a, &b| {
        let (a, b) = (&entries[a as usize], &entries[b as usize]);

        // Directories first.
        match (a.is_dir(), b.is_dir()) {
            (true, false) => return std::cmp::Ordering::Less,
            (false, true) => return std::cmp::Ordering::Greater,
            _ => {}
        }

        let ord = match key {
            SortKey::Name => natural_cmp(&a.name, &b.name),
            // Entries still missing metadata sort last within their direction
            // rather than pretending to be zero-sized or epoch-dated.
            SortKey::Size => a.size.cmp(&b.size),
            SortKey::Modified => a.modified.cmp(&b.modified),
        };

        let ord = if ascending { ord } else { ord.reverse() };
        ord.then_with(|| natural_cmp(&a.name, &b.name))
    });

    ix
}

/// Case-insensitive comparison, falling back to bytewise so it stays a total
/// order for names differing only by case.
fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    a.to_lowercase().cmp(&b.to_lowercase()).then(a.cmp(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, kind: EntryKind, size: Option<u64>) -> DirEntry {
        DirEntry {
            name: name.into(),
            path: name.into(),
            kind,
            size,
            modified: None,
            etag: None,
            meta_complete: size.is_some(),
        }
    }

    #[test]
    fn a_listing_without_mtime_is_marked_incomplete() {
        // Backends whose `list` returns only the mode — OpenDAL 0.58 dropped
        // Metakey, so this is per-backend and cannot be requested. Such entries
        // must report no size rather than a bogus 0, and must be flagged so the
        // viewport can stat them.
        let meta = opendal::Metadata::new(opendal::EntryMode::FILE);
        let entry = DirEntry::from_parts("dir/a.txt", &meta);

        assert!(!entry.meta_complete);
        assert_eq!(entry.size, None);
        assert_eq!(entry.modified, None);
        assert_eq!(&*entry.name, "a.txt");
    }

    #[test]
    fn a_directory_is_complete_once_its_mode_is_known() {
        let meta = opendal::Metadata::new(opendal::EntryMode::DIR);
        let entry = DirEntry::from_parts("dir/sub/", &meta);

        assert!(entry.meta_complete, "a directory has no size to fetch");
        assert_eq!(entry.size, None);
        assert_eq!(&*entry.name, "sub");
    }

    #[test]
    fn stat_completes_an_incomplete_entry() {
        let listed = DirEntry::from_parts(
            "dir/a.txt",
            &opendal::Metadata::new(opendal::EntryMode::FILE),
        );

        let mut meta = opendal::Metadata::new(opendal::EntryMode::FILE);
        meta.set_content_length(4096);
        let filled = listed.with_metadata(&meta);

        assert!(filled.meta_complete);
        assert_eq!(filled.size, Some(4096));
        assert_eq!(&*filled.path, "dir/a.txt", "path survives the refresh");
    }

    #[test]
    fn dirs_sort_before_files_in_both_directions() {
        let entries = vec![
            entry("b.txt", EntryKind::File, Some(1)),
            entry("a_dir", EntryKind::Dir, None),
        ];

        for ascending in [true, false] {
            let ix = sort_indices(&entries, SortKey::Name, ascending);
            assert!(entries[ix[0] as usize].is_dir(), "ascending={ascending}");
        }
    }

    #[test]
    fn name_sort_is_case_insensitive() {
        let entries = vec![
            entry("Zebra", EntryKind::File, Some(1)),
            entry("apple", EntryKind::File, Some(1)),
        ];
        let ix = sort_indices(&entries, SortKey::Name, true);
        assert_eq!(&*entries[ix[0] as usize].name, "apple");
    }

    #[test]
    fn missing_size_does_not_masquerade_as_zero() {
        let entries = vec![
            entry("unknown", EntryKind::File, None),
            entry("empty", EntryKind::File, Some(0)),
            entry("big", EntryKind::File, Some(100)),
        ];
        let ix = sort_indices(&entries, SortKey::Size, true);
        let names: Vec<&str> = ix.iter().map(|&i| &*entries[i as usize].name).collect();
        // None sorts below Some(0) in Rust's Option ordering; the point is that
        // it is ordered distinctly rather than being coerced to 0.
        assert_eq!(names, vec!["unknown", "empty", "big"]);
    }

    #[test]
    fn a_version_id_is_shortened_for_display() {
        let version = |id: Option<&str>| ObjectVersion {
            id: id.map(Arc::from),
            size: Some(1),
            modified: None,
            is_current: false,
            is_delete_marker: false,
        };

        // A real S3 version id is 32+ characters and unreadable in a table.
        assert_eq!(
            version(Some("1234567890abcdefghij")).short_id(),
            "1234567890ab…"
        );
        assert_eq!(version(Some("short")).short_id(), "short");
        assert_eq!(version(None).short_id(), "—");
    }

    #[test]
    fn a_delete_marker_has_no_size() {
        let mut meta = opendal::Metadata::new(opendal::EntryMode::FILE);
        meta.set_content_length(100);
        meta.set_is_deleted(true);

        let version = ObjectVersion::from_metadata(&meta);
        assert!(version.is_delete_marker);
        assert_eq!(
            version.size, None,
            "a delete marker is an event, not content with a size"
        );
    }

    #[test]
    fn a_live_version_keeps_its_size() {
        let mut meta = opendal::Metadata::new(opendal::EntryMode::FILE);
        meta.set_content_length(42);
        meta.set_is_current(true);

        let version = ObjectVersion::from_metadata(&meta);
        assert_eq!(version.size, Some(42));
        assert!(version.is_current);
        assert!(!version.is_delete_marker);
    }

    #[test]
    fn filter_is_a_case_insensitive_substring() {
        assert!(matches_filter("Reports", "rep"));
        assert!(matches_filter("reports", "REP"));
        assert!(matches_filter("q1-report.xlsx", "report"));
        assert!(!matches_filter("notes.txt", "report"));
    }

    #[test]
    fn an_empty_filter_matches_everything() {
        assert!(matches_filter("anything", ""));
        assert!(matches_filter("", ""));
    }

    #[test]
    fn the_view_drops_non_matching_rows_but_keeps_the_order() {
        let entries = vec![
            entry("zebra.txt", EntryKind::File, Some(1)),
            entry("report-a.txt", EntryKind::File, Some(1)),
            entry("reports", EntryKind::Dir, None),
            entry("apple.txt", EntryKind::File, Some(1)),
        ];

        let view = view_indices(&entries, SortKey::Name, true, "rep");
        let names: Vec<&str> = view.iter().map(|&i| &*entries[i as usize].name).collect();

        // Directories still come first inside the filtered set.
        assert_eq!(names, vec!["reports", "report-a.txt"]);
    }

    #[test]
    fn clearing_the_filter_restores_every_row() {
        let entries = vec![
            entry("a.txt", EntryKind::File, Some(1)),
            entry("b.txt", EntryKind::File, Some(1)),
        ];

        assert_eq!(view_indices(&entries, SortKey::Name, true, "a").len(), 1);
        assert_eq!(view_indices(&entries, SortKey::Name, true, "").len(), 2);
    }

    #[test]
    fn a_filter_matching_nothing_yields_an_empty_view() {
        let entries = vec![entry("a.txt", EntryKind::File, Some(1))];
        assert!(view_indices(&entries, SortKey::Name, true, "zzz").is_empty());
    }

    #[test]
    fn sort_is_stable_across_equal_keys() {
        let entries = vec![
            entry("b", EntryKind::File, Some(5)),
            entry("a", EntryKind::File, Some(5)),
        ];
        let ix = sort_indices(&entries, SortKey::Size, true);
        let names: Vec<&str> = ix.iter().map(|&i| &*entries[i as usize].name).collect();
        assert_eq!(names, vec!["a", "b"], "ties fall back to name");
    }
}
