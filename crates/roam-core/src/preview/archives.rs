use super::{TREE_ENTRY_LIMIT, TreeNode, TreePreview, finish_tree, slash_components};
use std::io::{Cursor, Read};

pub fn tree(name: &str, bytes: &[u8]) -> Result<TreePreview, String> {
    let lower = name.to_lowercase();
    let mut root = TreeNode::default();
    if lower.ends_with(".7z") {
        let archive =
            sevenz_rust2::Archive::read(&mut Cursor::new(bytes), &sevenz_rust2::Password::empty())
                .map_err(|e| format!("无法读取 7Z 目录（不支持加密目录）：{e}"))?;
        let truncated = archive.files.len() > TREE_ENTRY_LIMIT;
        for file in archive.files.iter().take(TREE_ENTRY_LIMIT) {
            if let Some(parts) = slash_components(file.name()) {
                root.insert(&parts, file.is_directory());
            }
        }
        return Ok(finish_tree(name, root, truncated));
    }
    let reader: Box<dyn Read + '_> = if lower.ends_with(".gz") || lower.ends_with(".tgz") {
        Box::new(flate2::read::GzDecoder::new(bytes))
    } else if lower.ends_with(".bz2") {
        Box::new(bzip2::read::BzDecoder::new(bytes))
    } else if lower.ends_with(".xz") {
        Box::new(xz2::read::XzDecoder::new(bytes))
    } else {
        Box::new(Cursor::new(bytes))
    };
    // TAR traversal skips bodies, but compressed TAR still has to inflate them.
    // Cap that work as well as the downloaded bytes.
    let mut archive = tar::Archive::new(reader.take(128 * 1024 * 1024));
    let entries = archive
        .entries()
        .map_err(|e| format!("无法读取 TAR 目录：{e}"))?;
    let mut truncated = false;
    for (ix, entry) in entries.enumerate() {
        if ix == TREE_ENTRY_LIMIT {
            truncated = true;
            break;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                truncated = true;
                break;
            }
            Err(e) => return Err(format!("无法读取 TAR 目录：{e}")),
        };
        let path = entry.path().map_err(|e| e.to_string())?;
        if path.is_absolute() {
            continue;
        }
        if let Some(parts) = slash_components(&path.to_string_lossy()) {
            root.insert(&parts, entry.header().entry_type().is_dir());
        }
    }
    Ok(finish_tree(name, root, truncated))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn compressed_tar_lists_without_extracting() {
        let mut tar = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(5);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, "docs/test.txt", &b"hello"[..])
            .unwrap();
        let bytes = tar.into_inner().unwrap();
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut gzip, &bytes).unwrap();
        assert!(
            tree("test.tgz", &gzip.finish().unwrap())
                .unwrap()
                .body
                .contains("test.txt")
        );
    }
}
