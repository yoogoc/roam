//! Deciding what, if anything, can be previewed.
//!
//! Kept out of the view layer so the rules are testable and so the size limits
//! live next to the reason for them: a preview must never pull a multi-gigabyte
//! object over the network just because someone tapped Space.

use std::collections::BTreeMap;
use std::io::Cursor;
use std::path::Component;

use crate::{DirEntry, path};

/// Bytes read for a text preview. Enough to see what a file is; small enough
/// that it costs one ranged request.
pub const TEXT_LIMIT: u64 = 128 * 1024;

/// Images have to be fetched whole to decode, so the ceiling is higher but
/// still bounded.
pub const IMAGE_LIMIT: u64 = 8 * 1024 * 1024;

/// A ZIP needs its central directory at the end of the file, so it cannot be
/// previewed with a small ranged read. Keep the whole-archive download bounded.
pub const ZIP_LIMIT: u64 = 32 * 1024 * 1024;

/// Directory and archive trees stop here so a selection cannot allocate or
/// render hundreds of thousands of rows.
pub const TREE_ENTRY_LIMIT: usize = 2_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageKind {
    Png,
    Jpeg,
    Gif,
    Webp,
    Bmp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviewKind {
    /// Plain text, rendered as-is.
    Text,
    /// Markdown, rendered with formatting.
    Markdown,
    Image(ImageKind),
    /// ZIP archive, rendered as a tree of central-directory entries.
    Zip,
    /// Nothing useful to show. Carries the reason, which is displayed.
    None(&'static str),
}

const IMAGE_EXTENSIONS: &[(&str, ImageKind)] = &[
    ("png", ImageKind::Png),
    ("jpg", ImageKind::Jpeg),
    ("jpeg", ImageKind::Jpeg),
    ("gif", ImageKind::Gif),
    ("webp", ImageKind::Webp),
    ("bmp", ImageKind::Bmp),
];

/// Extensions worth reading as text. Anything not listed and not a known binary
/// is still attempted as text, with a NUL-byte check as the backstop.
const TEXT_EXTENSIONS: &[&str] = &[
    "txt",
    "log",
    "json",
    "yaml",
    "yml",
    "toml",
    "ini",
    "conf",
    "cfg",
    "csv",
    "tsv",
    "xml",
    "html",
    "htm",
    "css",
    "js",
    "ts",
    "jsx",
    "tsx",
    "rs",
    "go",
    "py",
    "rb",
    "java",
    "kt",
    "swift",
    "c",
    "h",
    "cpp",
    "hpp",
    "cs",
    "sh",
    "bash",
    "zsh",
    "fish",
    "sql",
    "graphql",
    "proto",
    "lock",
    "gitignore",
    "dockerfile",
    "makefile",
    "env",
];

/// Extensions that are definitely not worth rendering as text.
const BINARY_EXTENSIONS: &[&str] = &[
    "gz", "bz2", "xz", "zst", "7z", "rar", "tar", "pdf", "mp3", "mp4", "mov", "avi", "mkv", "flac",
    "wav", "ogg", "webm", "so", "dylib", "dll", "exe", "bin", "dmg", "iso", "parquet", "sqlite",
    "db", "woff", "woff2", "ttf", "otf", "psd", "sketch", "class", "pyc", "o", "a",
];

fn extension(name: &str) -> String {
    match name.rfind('.') {
        // A leading dot is part of the name, not an extension separator, so
        // `.gitignore` has no extension — it is matched by name below instead.
        Some(0) | None => String::new(),
        Some(ix) => name[ix + 1..].to_lowercase(),
    }
}

/// What to show for `name`, given its size when known.
pub fn classify(name: &str, size: Option<u64>) -> PreviewKind {
    let ext = extension(name);
    let lower = name.to_lowercase();

    if let Some((_, kind)) = IMAGE_EXTENSIONS.iter().find(|(e, _)| *e == ext) {
        return match size {
            Some(bytes) if bytes > IMAGE_LIMIT => PreviewKind::None("图片过大，暂不预览"),
            _ => PreviewKind::Image(*kind),
        };
    }

    if ext == "md" || ext == "markdown" {
        return PreviewKind::Markdown;
    }

    if ext == "zip" {
        return match size {
            Some(bytes) if bytes > ZIP_LIMIT => PreviewKind::None("ZIP 文件过大，暂不预览"),
            _ => PreviewKind::Zip,
        };
    }

    if BINARY_EXTENSIONS.contains(&ext.as_str()) {
        return PreviewKind::None("二进制文件，暂不预览");
    }

    // Dotfiles and extensionless names like `Makefile` are text in practice.
    if ext.is_empty() {
        let stem = lower.trim_start_matches('.');
        if TEXT_EXTENSIONS.contains(&stem) || lower.starts_with('.') {
            return PreviewKind::Text;
        }
    }

    if ext.is_empty() || TEXT_EXTENSIONS.contains(&ext.as_str()) {
        return PreviewKind::Text;
    }

    // Unknown extension: try it as text. `looks_binary` catches the mistake once
    // the bytes arrive, which beats refusing to preview anything unfamiliar.
    PreviewKind::Text
}

/// A NUL byte in the first block is the classic "this is not text" signal.
///
/// The backstop for `classify` guessing Text on an unknown extension: better to
/// say "binary" after looking than to render a screen of mojibake.
pub fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|&b| b == 0)
}

/// How many bytes to request for `kind`.
pub fn read_limit(kind: &PreviewKind) -> u64 {
    match kind {
        PreviewKind::Image(_) => IMAGE_LIMIT,
        // One extra byte distinguishes a complete archive at the limit from an
        // unknown-size archive that exceeds it.
        PreviewKind::Zip => ZIP_LIMIT + 1,
        _ => TEXT_LIMIT,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreePreview {
    pub body: String,
    pub truncated: bool,
}

#[derive(Default)]
struct TreeNode {
    directory: bool,
    children: BTreeMap<String, TreeNode>,
}

impl TreeNode {
    fn insert(&mut self, components: &[String], directory: bool) {
        let mut node = self;
        for (ix, component) in components.iter().enumerate() {
            node = node.children.entry(component.clone()).or_default();
            if ix + 1 < components.len() || directory {
                node.directory = true;
            }
        }
    }
}

fn clean_component(component: &str) -> String {
    component
        .chars()
        .map(|ch| if ch.is_control() { '�' } else { ch })
        .collect()
}

fn slash_components(value: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    for component in value.split('/').filter(|part| !part.is_empty()) {
        match component {
            "." => continue,
            ".." => return None,
            _ => out.push(clean_component(component)),
        }
    }
    (!out.is_empty()).then_some(out)
}

fn render_children(node: &TreeNode, prefix: &str, out: &mut String) {
    let mut children: Vec<_> = node.children.iter().collect();
    children.sort_by(|(left_name, left), (right_name, right)| {
        right
            .directory
            .cmp(&left.directory)
            .then_with(|| left_name.to_lowercase().cmp(&right_name.to_lowercase()))
            .then_with(|| left_name.cmp(right_name))
    });

    let last_ix = children.len().saturating_sub(1);
    for (ix, (name, child)) in children.into_iter().enumerate() {
        let last = ix == last_ix;
        out.push_str(prefix);
        out.push_str(if last { "└── " } else { "├── " });
        out.push_str(name);
        if child.directory {
            out.push('/');
        }
        out.push('\n');

        if child.directory {
            let next_prefix = format!("{prefix}{}", if last { "    " } else { "│   " });
            render_children(child, &next_prefix, out);
        }
    }
}

fn finish_tree(root_name: &str, root: TreeNode, truncated: bool) -> TreePreview {
    let mut body = format!("{}/\n", clean_component(root_name));
    if root.children.is_empty() {
        body.push_str("└── （空）\n");
    } else {
        render_children(&root, "", &mut body);
    }

    TreePreview { body, truncated }
}

/// Format entries below a selected directory as a compact tree.
pub fn directory_tree(
    root_name: &str,
    root_path: &str,
    entries: Vec<DirEntry>,
    truncated: bool,
) -> TreePreview {
    let prefix = path::as_dir(root_path);
    let mut root = TreeNode::default();

    for entry in entries {
        let Some(relative) = entry.path.strip_prefix(&prefix) else {
            continue;
        };
        if let Some(components) = slash_components(relative) {
            root.insert(&components, entry.is_dir());
        }
    }

    finish_tree(root_name, root, truncated)
}

/// Read only ZIP metadata and format it as a tree. File bodies are never
/// decompressed, so unsupported compression methods do not prevent previewing.
pub fn zip_tree(name: &str, bytes: &[u8]) -> Result<TreePreview, String> {
    if bytes.len() as u64 > ZIP_LIMIT {
        return Err("ZIP 文件过大，暂不预览".into());
    }

    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|error| format!("无法读取 ZIP 目录：{error}"))?;
    let mut root = TreeNode::default();
    let mut accepted = 0usize;
    let mut truncated = false;

    for ix in 0..archive.len() {
        let file = archive
            .by_index_raw(ix)
            .map_err(|error| format!("无法读取 ZIP 目录：{error}"))?;
        let Some(enclosed) = file.enclosed_name() else {
            continue;
        };
        let components: Vec<String> = enclosed
            .components()
            .filter_map(|component| match component {
                Component::Normal(value) => Some(clean_component(&value.to_string_lossy())),
                _ => None,
            })
            .collect();
        if components.is_empty() {
            continue;
        }
        if accepted == TREE_ENTRY_LIMIT {
            truncated = true;
            break;
        }

        root.insert(&components, file.is_dir());
        accepted += 1;
    }

    Ok(finish_tree(name, root, truncated))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn images_are_recognised_by_extension() {
        assert_eq!(
            classify("photo.PNG", Some(1024)),
            PreviewKind::Image(ImageKind::Png)
        );
        assert_eq!(
            classify("photo.jpeg", Some(1024)),
            PreviewKind::Image(ImageKind::Jpeg)
        );
    }

    #[test]
    fn an_oversized_image_is_refused_with_a_reason() {
        // Fetching 40 MB over the network because someone tapped Space is not a
        // preview, it is a download.
        assert_eq!(
            classify("huge.png", Some(IMAGE_LIMIT + 1)),
            PreviewKind::None("图片过大，暂不预览")
        );
    }

    #[test]
    fn an_image_of_unknown_size_is_still_attempted() {
        // Backends that omit size from a listing are common; refusing to preview
        // there would make the panel useless on those.
        assert_eq!(
            classify("photo.png", None),
            PreviewKind::Image(ImageKind::Png)
        );
    }

    #[test]
    fn markdown_is_distinguished_from_plain_text() {
        assert_eq!(classify("README.md", Some(10)), PreviewKind::Markdown);
        assert_eq!(classify("notes.txt", Some(10)), PreviewKind::Text);
    }

    #[test]
    fn known_binaries_are_refused() {
        for name in ["movie.mp4", "lib.dylib", "data.parquet", "doc.pdf"] {
            assert_eq!(
                classify(name, Some(10)),
                PreviewKind::None("二进制文件，暂不预览"),
                "{name}"
            );
        }
    }

    #[test]
    fn zip_files_are_previewable_up_to_the_download_limit() {
        assert_eq!(classify("files.ZIP", Some(1024)), PreviewKind::Zip);
        assert_eq!(
            classify("huge.zip", Some(ZIP_LIMIT + 1)),
            PreviewKind::None("ZIP 文件过大，暂不预览")
        );
        assert_eq!(read_limit(&PreviewKind::Zip), ZIP_LIMIT + 1);
    }

    #[test]
    fn dotfiles_and_extensionless_names_are_text() {
        assert_eq!(classify(".gitignore", Some(10)), PreviewKind::Text);
        assert_eq!(classify("Makefile", Some(10)), PreviewKind::Text);
        assert_eq!(classify("LICENSE", Some(10)), PreviewKind::Text);
    }

    #[test]
    fn an_unknown_extension_is_attempted_as_text() {
        assert_eq!(classify("data.wat", Some(10)), PreviewKind::Text);
    }

    #[test]
    fn nul_bytes_mark_content_as_binary() {
        assert!(looks_binary(b"abc\0def"));
        assert!(!looks_binary(b"plain text\n"));
        assert!(!looks_binary(&[]));
    }

    #[test]
    fn a_nul_past_the_sniff_window_is_ignored() {
        // Only the first block is checked, so this stays cheap on a large read.
        let mut bytes = vec![b'a'; 9000];
        bytes.push(0);
        assert!(!looks_binary(&bytes));
    }

    #[test]
    fn read_limits_differ_by_kind() {
        assert_eq!(read_limit(&PreviewKind::Text), TEXT_LIMIT);
        assert_eq!(read_limit(&PreviewKind::Image(ImageKind::Png)), IMAGE_LIMIT);
    }

    #[test]
    fn a_directory_is_formatted_with_directories_first() {
        let entries = vec![
            DirEntry {
                name: "notes.txt".into(),
                path: "docs/notes.txt".into(),
                kind: crate::EntryKind::File,
                size: Some(1),
                modified: None,
                etag: None,
                meta_complete: true,
            },
            DirEntry {
                name: "guides".into(),
                path: "docs/guides/".into(),
                kind: crate::EntryKind::Dir,
                size: None,
                modified: None,
                etag: None,
                meta_complete: true,
            },
            DirEntry {
                name: "start.md".into(),
                path: "docs/guides/start.md".into(),
                kind: crate::EntryKind::File,
                size: Some(1),
                modified: None,
                etag: None,
                meta_complete: true,
            },
        ];

        let tree = directory_tree("docs", "docs/", entries, false);
        assert_eq!(
            tree.body,
            "docs/\n├── guides/\n│   └── start.md\n└── notes.txt\n"
        );
        assert!(!tree.truncated);
    }

    #[test]
    fn a_zip_is_formatted_without_decompressing_file_bodies() {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        writer.add_directory("docs/", options).unwrap();
        writer.start_file("docs/readme.txt", options).unwrap();
        writer.write_all(b"hello").unwrap();
        writer.start_file("root.txt", options).unwrap();
        writer.write_all(b"root").unwrap();
        let bytes = writer.finish().unwrap().into_inner();

        let tree = zip_tree("sample.zip", &bytes).unwrap();
        assert_eq!(
            tree.body,
            "sample.zip/\n├── docs/\n│   └── readme.txt\n└── root.txt\n"
        );
        assert!(!tree.truncated);
    }

    #[test]
    fn a_broken_zip_returns_a_readable_error() {
        let error = zip_tree("broken.zip", b"not a zip").unwrap_err();
        assert!(error.starts_with("无法读取 ZIP 目录："), "{error}");
    }
}
