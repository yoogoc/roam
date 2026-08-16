//! OpenDAL path helpers.
//!
//! OpenDAL's convention: paths are relative to the operator root, never start
//! with `/`, and a directory path *must* end with `/`. The root itself is `""`
//! (which `list`/`stat` accept as `/`). Getting this wrong is the single most
//! common source of `NotFound` against object stores, so it lives in one place
//! with tests rather than being re-derived at each call site.

/// Normalize a path to OpenDAL's directory form: no leading slash, exactly one
/// trailing slash. The root becomes `""`.
pub fn as_dir(path: &str) -> String {
    let trimmed = path.trim_matches('/');
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("{trimmed}/")
    }
}

/// Normalize a path to OpenDAL's file form: no leading or trailing slash.
pub fn as_file(path: &str) -> String {
    path.trim_matches('/').to_string()
}

/// The parent directory of a path, or `None` if it is already the root.
pub fn parent(path: &str) -> Option<String> {
    let trimmed = path.trim_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    match trimmed.rsplit_once('/') {
        Some((head, _)) => Some(as_dir(head)),
        None => Some(String::new()),
    }
}

/// The basename used for display. Directories keep no trailing slash here.
pub fn basename(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rsplit_once('/') {
        Some((_, tail)) => tail,
        None => trimmed,
    }
}

/// Breadcrumb segments for `path`, from root to the path itself.
///
/// Returns `(label, path)` pairs where each `path` is in OpenDAL directory
/// form, so clicking a crumb can navigate straight to it.
pub fn crumbs(path: &str) -> Vec<(String, String)> {
    let mut out = vec![("/".to_string(), String::new())];
    let trimmed = path.trim_matches('/');
    if trimmed.is_empty() {
        return out;
    }

    let mut acc = String::new();
    for seg in trimmed.split('/') {
        acc.push_str(seg);
        acc.push('/');
        out.push((seg.to_string(), acc.clone()));
    }
    out
}

/// Join a directory and a child name into OpenDAL directory form.
pub fn join_dir(dir: &str, name: &str) -> String {
    as_dir(&format!(
        "{}/{}",
        dir.trim_matches('/'),
        name.trim_matches('/')
    ))
}

/// Join a directory and a child name into OpenDAL file form.
pub fn join_file(dir: &str, name: &str) -> String {
    let dir = dir.trim_matches('/');
    let name = name.trim_matches('/');
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

/// The path of a sibling of `path` with a new basename, preserving whether the
/// original was a directory (trailing slash).
pub fn sibling(path: &str, new_name: &str) -> String {
    let parent = parent(path).unwrap_or_default();
    if path.ends_with('/') {
        join_dir(&parent, new_name)
    } else {
        join_file(&parent, new_name)
    }
}

/// Reject names that cannot be a single path segment.
///
/// `/` is the killer: OpenDAL would silently create a nested path instead of the
/// name the user typed, so a rename to `a/b` would move the entry rather than
/// rename it.
pub fn validate_name(name: &str) -> Result<(), &'static str> {
    if name.is_empty() {
        return Err("名称不能为空");
    }
    if name != name.trim() {
        return Err("名称不能以空格开头或结尾");
    }
    if name.contains('/') {
        return Err("名称不能包含 /");
    }
    if name.contains('\0') {
        return Err("名称不能包含空字符");
    }
    if name == "." || name == ".." {
        return Err("名称不能是 . 或 ..");
    }
    Ok(())
}

/// Split a basename into (stem, extension-with-dot).
///
/// A leading dot belongs to the stem, so `.gitignore` has no extension rather
/// than being an empty name with a `.gitignore` extension.
fn split_extension(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(0) | None => (name, ""),
        Some(ix) => (&name[..ix], &name[ix..]),
    }
}

/// A free name for a copy of `name`, avoiding everything in `taken`.
pub fn duplicate_name(name: &str, taken: &[&str]) -> String {
    let (stem, ext) = split_extension(name);

    let first = format!("{stem} 副本{ext}");
    if !taken.contains(&first.as_str()) {
        return first;
    }

    (2..)
        .map(|n| format!("{stem} 副本 {n}{ext}"))
        .find(|candidate| !taken.contains(&candidate.as_str()))
        .expect("an unbounded range always yields a free name")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dir_form_is_canonical() {
        assert_eq!(as_dir(""), "");
        assert_eq!(as_dir("/"), "");
        assert_eq!(as_dir("a"), "a/");
        assert_eq!(as_dir("/a/b"), "a/b/");
        assert_eq!(as_dir("a/b/"), "a/b/");
        assert_eq!(as_dir("//a//"), "a/");
    }

    #[test]
    fn file_form_strips_both_ends() {
        assert_eq!(as_file("/a/b.txt"), "a/b.txt");
        assert_eq!(as_file("a/b.txt/"), "a/b.txt");
    }

    #[test]
    fn parent_walks_up_to_root_then_stops() {
        assert_eq!(parent("a/b/c/"), Some("a/b/".into()));
        assert_eq!(parent("a/b/"), Some("a/".into()));
        assert_eq!(parent("a/"), Some(String::new()));
        assert_eq!(parent(""), None);
        assert_eq!(parent("/"), None);
    }

    #[test]
    fn basename_ignores_trailing_slash() {
        assert_eq!(basename("a/b/"), "b");
        assert_eq!(basename("a/b.txt"), "b.txt");
        assert_eq!(basename("a"), "a");
        assert_eq!(basename(""), "");
    }

    #[test]
    fn crumbs_are_navigable() {
        assert_eq!(crumbs(""), vec![("/".into(), "".into())]);
        assert_eq!(
            crumbs("a/b/"),
            vec![
                ("/".to_string(), "".to_string()),
                ("a".to_string(), "a/".to_string()),
                ("b".to_string(), "a/b/".to_string()),
            ]
        );
    }

    #[test]
    fn join_dir_handles_root() {
        assert_eq!(join_dir("", "a"), "a/");
        assert_eq!(join_dir("a/", "b"), "a/b/");
        assert_eq!(join_dir("a", "/b/"), "a/b/");
    }

    #[test]
    fn join_file_handles_root() {
        assert_eq!(join_file("", "a.txt"), "a.txt");
        assert_eq!(join_file("a/", "b.txt"), "a/b.txt");
    }

    #[test]
    fn sibling_preserves_the_entry_kind() {
        assert_eq!(sibling("a/b.txt", "c.txt"), "a/c.txt");
        assert_eq!(
            sibling("a/b/", "c"),
            "a/c/",
            "a directory stays a directory"
        );
        assert_eq!(sibling("b.txt", "c.txt"), "c.txt", "at the root");
        assert_eq!(sibling("b/", "c"), "c/");
    }

    #[test]
    fn names_containing_a_slash_are_rejected() {
        // Otherwise a "rename" would quietly move the entry into a subpath.
        assert!(validate_name("a/b").is_err());
        assert!(validate_name("../escape").is_err());
    }

    #[test]
    fn empty_and_dot_names_are_rejected() {
        assert!(validate_name("").is_err());
        assert!(validate_name(".").is_err());
        assert!(validate_name("..").is_err());
        assert!(validate_name(" leading").is_err());
        assert!(validate_name("trailing ").is_err());
    }

    #[test]
    fn ordinary_names_are_accepted() {
        for name in ["a.txt", "报告 2026.xlsx", ".gitignore", "a-b_c.tar.gz"] {
            assert!(validate_name(name).is_ok(), "{name} should be allowed");
        }
    }

    #[test]
    fn duplicate_name_keeps_the_extension() {
        assert_eq!(duplicate_name("a.txt", &[]), "a 副本.txt");
        assert_eq!(duplicate_name("a.tar.gz", &[]), "a.tar 副本.gz");
        assert_eq!(duplicate_name("noext", &[]), "noext 副本");
    }

    #[test]
    fn a_dotfile_has_no_extension_to_preserve() {
        assert_eq!(duplicate_name(".gitignore", &[]), ".gitignore 副本");
    }

    #[test]
    fn duplicate_name_counts_up_past_collisions() {
        let taken = ["a 副本.txt", "a 副本 2.txt"];
        assert_eq!(duplicate_name("a.txt", &taken), "a 副本 3.txt");
    }
}
