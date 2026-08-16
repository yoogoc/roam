//! Deciding what, if anything, can be previewed.
//!
//! Kept out of the view layer so the rules are testable and so the size limits
//! live next to the reason for them: a preview must never pull a multi-gigabyte
//! object over the network just because someone tapped Space.

/// Bytes read for a text preview. Enough to see what a file is; small enough
/// that it costs one ranged request.
pub const TEXT_LIMIT: u64 = 128 * 1024;

/// Images have to be fetched whole to decode, so the ceiling is higher but
/// still bounded.
pub const IMAGE_LIMIT: u64 = 8 * 1024 * 1024;

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
    "zip", "gz", "bz2", "xz", "zst", "7z", "rar", "tar", "pdf", "mp3", "mp4", "mov", "avi", "mkv",
    "flac", "wav", "ogg", "webm", "so", "dylib", "dll", "exe", "bin", "dmg", "iso", "parquet",
    "sqlite", "db", "woff", "woff2", "ttf", "otf", "psd", "sketch", "class", "pyc", "o", "a",
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
        _ => TEXT_LIMIT,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        for name in ["a.zip", "movie.mp4", "lib.dylib", "data.parquet", "doc.pdf"] {
            assert_eq!(
                classify(name, Some(10)),
                PreviewKind::None("二进制文件，暂不预览"),
                "{name}"
            );
        }
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
}
