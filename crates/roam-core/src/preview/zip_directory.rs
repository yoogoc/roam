//! ZIP metadata reads only: HEAD, bounded tail, then a bounded central-directory
//! prefix. Building the tree needs no per-entry local headers or decompression;
//! payloads are never fetched in full.
//!
//! Layout: https://pkware.cachefly.net/webdocs/casestudies/APPNOTE.TXT §4.3.
//! S3 contract: https://docs.aws.amazon.com/AmazonS3/latest/API/API_GetObject.html

use std::io::Cursor;
use std::ops::Range;

use futures::TryStreamExt;
use opendal::Operator;

use super::{TREE_ENTRY_LIMIT, TreeNode, TreePreview, finish_tree, slash_components};
use crate::{Error, Result, path};

// EOCD + maximum comment + ZIP64 locator + fixed ZIP64 EOCD. Including the
// latter avoids another GET for ordinary ZIP64 files, even with a long comment.
const TAIL_LIMIT: u64 = 22 + 65_535 + 20 + 56;
const DIRECTORY_LIMIT: u64 = 8 * 1024 * 1024;
const INVALID: &str = "无法读取 ZIP 目录：文件损坏或目录记录不完整";
const MULTI_DISK: &str = "暂不支持分卷 ZIP 目录预览";

fn invalid() -> String {
    INVALID.into()
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap())
}
fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}
fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

#[derive(Debug)]
struct Directory {
    offset: u64,
    size: u64,
    entries: u64,
}

impl Directory {
    fn validate(self, before: u64) -> std::result::Result<Self, String> {
        if self
            .offset
            .checked_add(self.size)
            .is_none_or(|end| end > before)
            || (self.entries > 0 && self.entries > self.size / 46)
            || (self.entries == 0 && self.size != 0)
        {
            return Err(invalid());
        }
        Ok(self)
    }
    fn range(&self) -> Range<u64> {
        self.offset..self.offset + self.size.min(DIRECTORY_LIMIT)
    }
}

#[derive(Debug)]
enum End {
    Classic(Directory),
    Zip64 { offset: u64, locator: u64 },
}

fn end_record(tail: &[u8], size: u64) -> std::result::Result<End, String> {
    let start = size.checked_sub(tail.len() as u64).ok_or_else(invalid)?;
    let at = (0..tail.len().saturating_sub(21))
        .rev()
        .find(|&at| {
            tail[at..].starts_with(b"PK\x05\x06")
                && at + 22 + usize::from(u16_at(tail, at + 20)) == tail.len()
        })
        .ok_or_else(invalid)?;
    let record = &tail[at..];
    let position = start + at as u64;
    if at >= 20 && tail[at - 20..].starts_with(b"PK\x06\x07") {
        let locator = &tail[at - 20..at];
        if u32_at(locator, 4) != 0 || u32_at(locator, 16) != 1 {
            return Err(MULTI_DISK.into());
        }
        let offset = u64_at(locator, 8);
        if offset.checked_add(56).is_none_or(|end| end > position - 20) {
            return Err(invalid());
        }
        return Ok(End::Zip64 {
            offset,
            locator: position - 20,
        });
    }
    if u16_at(record, 4) != 0 || u16_at(record, 6) != 0 || u16_at(record, 8) != u16_at(record, 10) {
        return Err(MULTI_DISK.into());
    }
    let directory = Directory {
        entries: u64::from(u16_at(record, 10)),
        size: u64::from(u32_at(record, 12)),
        offset: u64::from(u32_at(record, 16)),
    };
    if directory.entries == u64::from(u16::MAX)
        || directory.size == u64::from(u32::MAX)
        || directory.offset == u64::from(u32::MAX)
    {
        return Err(invalid());
    }
    let mut directory = directory.validate(position)?;
    // Central-directory offsets in prepended/self-extracting ZIPs can be
    // relative to the original ZIP rather than the containing object.
    directory.offset = position - directory.size;
    Ok(End::Classic(directory))
}

fn zip64_record(bytes: &[u8], offset: u64, locator: u64) -> std::result::Result<Directory, String> {
    if bytes.len() < 56 || !bytes.starts_with(b"PK\x06\x06") {
        return Err(invalid());
    }
    let record_size = u64_at(bytes, 4);
    if record_size < 44
        || offset
            .checked_add(12)
            .and_then(|at| at.checked_add(record_size))
            != Some(locator)
    {
        return Err(invalid());
    }
    if u32_at(bytes, 16) != 0 || u32_at(bytes, 20) != 0 || u64_at(bytes, 24) != u64_at(bytes, 32) {
        return Err(MULTI_DISK.into());
    }
    Directory {
        entries: u64_at(bytes, 32),
        size: u64_at(bytes, 40),
        offset: u64_at(bytes, 48),
    }
    .validate(offset)
}

fn cached(tail: &[u8], size: u64, range: &Range<u64>) -> Option<Vec<u8>> {
    let start = size - tail.len() as u64;
    if range.start >= start && range.end <= size {
        Some(tail[(range.start - start) as usize..(range.end - start) as usize].to_vec())
    } else {
        None
    }
}

async fn read_range(
    op: &Operator,
    path: &str,
    range: Range<u64>,
    etag: Option<&str>,
) -> Result<Vec<u8>> {
    if range.is_empty() {
        return Ok(Vec::new());
    }
    let expected = range.end - range.start;
    let mut request = op.reader_with(path);
    if let Some(etag) = etag {
        request = request.if_match(etag);
    }
    let mut stream = request.await?.into_bytes_stream(range).await?;
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.try_next().await.map_err(crate::vfs::stream_err)? {
        // Bound allocations even if a server ignores Range and sends the body.
        if bytes.len() as u64 + chunk.len() as u64 > expected {
            return Err(Error::Config("ZIP 服务器返回的范围数据不正确".into()));
        }
        bytes.extend_from_slice(&chunk);
    }
    if bytes.len() as u64 != expected {
        return Err(Error::Config(
            "ZIP 文件已变化或服务器未返回完整的范围数据，请刷新后重试".into(),
        ));
    }
    Ok(bytes)
}

pub(crate) async fn read(op: Operator, path: String) -> Result<TreePreview> {
    let meta = op.stat(&path).await?;
    let size = meta.content_length();
    let etag = meta.etag().filter(|etag| {
        op.info().capability().read_with_if_match
            && !etag.starts_with("W/")
            && !etag.starts_with("w/")
    });
    let tail = read_range(&op, &path, size.saturating_sub(TAIL_LIMIT)..size, etag).await?;
    let directory = match end_record(&tail, size).map_err(Error::Config)? {
        End::Classic(directory) => directory,
        End::Zip64 { offset, locator } => {
            let range = offset..offset + 56;
            let bytes = match cached(&tail, size, &range) {
                Some(bytes) => bytes,
                None => read_range(&op, &path, range, etag).await?,
            };
            zip64_record(&bytes, offset, locator).map_err(Error::Config)?
        }
    };
    let range = directory.range();
    let bytes = match cached(&tail, size, &range) {
        Some(bytes) => bytes,
        None => read_range(&op, &path, range, etag).await?,
    };
    tokio::task::spawn_blocking(move || tree(path::basename(&path), &bytes, &directory))
        .await?
        .map_err(Error::Config)
}

/// Parse just a bounded prefix of central records with the existing ZIP crate.
/// A small synthetic EOCD lets it retain filename/Unicode/ZIP64-extra decoding
/// without seeking to local headers or allocating for an untrusted entry count.
fn tree(
    name: &str,
    bytes: &[u8],
    directory: &Directory,
) -> std::result::Result<TreePreview, String> {
    let mut position = 0;
    let mut entries = 0;
    while entries < directory.entries.min(TREE_ENTRY_LIMIT as u64) {
        let Some(header) = bytes.get(position..position + 46) else {
            if (bytes.len() as u64) < directory.size {
                break;
            }
            return Err(invalid());
        };
        if !header.starts_with(b"PK\x01\x02") {
            return Err("无法读取 ZIP 目录：目录已加密、压缩或损坏".into());
        }
        if u16_at(header, 34) != 0 {
            return Err(MULTI_DISK.into());
        }
        let length = 46
            + usize::from(u16_at(header, 28))
            + usize::from(u16_at(header, 30))
            + usize::from(u16_at(header, 32));
        if position + length > bytes.len() {
            if (bytes.len() as u64) < directory.size {
                break;
            }
            return Err(invalid());
        }
        position += length;
        entries += 1;
    }
    let mut metadata = bytes[..position].to_vec();
    metadata.extend_from_slice(b"PK\x05\x06\0\0\0\0");
    metadata.extend_from_slice(&(entries as u16).to_le_bytes());
    metadata.extend_from_slice(&(entries as u16).to_le_bytes());
    metadata.extend_from_slice(&(position as u32).to_le_bytes());
    metadata.extend_from_slice(&0u32.to_le_bytes());
    metadata.extend_from_slice(&0u16.to_le_bytes());
    let archive = zip::ZipArchive::new(Cursor::new(metadata))
        .map_err(|error| format!("无法读取 ZIP 目录：{error}"))?;
    let mut root = TreeNode::default();
    for name in archive.file_names() {
        // Only names are displayed; reject absolute paths, NULs and traversal.
        if name.starts_with(['/', '\\'])
            || name.contains('\0')
            || std::path::Path::new(name).is_absolute()
        {
            continue;
        }
        if let Some(components) = slash_components(name) {
            if components.len() > 128 {
                return Err("ZIP 目录层级过深，暂不预览".into());
            }
            root.insert(&components, name.ends_with('/'));
        }
    }
    Ok(finish_tree(name, root, entries < directory.entries))
}

pub(super) fn from_bytes(name: &str, bytes: &[u8]) -> std::result::Result<TreePreview, String> {
    let size = bytes.len() as u64;
    let tail = &bytes[size.saturating_sub(TAIL_LIMIT) as usize..];
    let directory = match end_record(tail, size)? {
        End::Classic(directory) => directory,
        End::Zip64 { offset, locator } => zip64_record(
            &bytes[offset as usize..offset as usize + 56],
            offset,
            locator,
        )?,
    };
    let range = directory.range();
    tree(
        name,
        &bytes[range.start as usize..range.end as usize],
        &directory,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn archive(names: &[&str], comment: &str) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for name in names {
            writer.start_file(*name, options).unwrap();
            writer.write_all(b"payload").unwrap();
        }
        writer.set_comment(comment).unwrap();
        writer.finish().unwrap().into_inner()
    }

    #[test]
    fn metadata_does_not_need_local_headers_or_payload() {
        let mut bytes = archive(&["目录/你好.txt", "empty/", "root.txt"], "");
        bytes[..4].fill(0); // Local header is deliberately unreadable.
        let tree = from_bytes("test.zip", &bytes).unwrap();
        assert!(tree.body.contains("你好.txt"));
        assert!(tree.body.contains("empty/"));
        assert!(tree.body.contains("root.txt"));
        assert!(!tree.truncated);
    }

    #[test]
    fn maximum_comment_and_false_end_signature_are_handled() {
        let mut comment = "x".repeat(65_535);
        comment.replace_range(65_500..65_504, "PK\x05\x06");
        let bytes = archive(&["hello.txt"], &comment);
        assert!(
            from_bytes("comment.zip", &bytes)
                .unwrap()
                .body
                .contains("hello.txt")
        );
    }

    #[test]
    fn prepended_zip_keeps_working_with_relative_offsets() {
        let original = archive(&["inside.txt"], "");
        let bytes = [b"some preamble".as_slice(), &original].concat();
        assert!(
            from_bytes("prefixed.zip", &bytes)
                .unwrap()
                .body
                .contains("inside.txt")
        );
    }

    #[test]
    fn legacy_cp437_names_use_the_zip_decoder() {
        let mut bytes = archive(&["e.txt"], "");
        let End::Classic(dir) = end_record(&bytes, bytes.len() as u64).unwrap() else {
            panic!()
        };
        bytes[dir.offset as usize + 46] = 0x82; // CP437 é.
        assert!(
            from_bytes("legacy.zip", &bytes)
                .unwrap()
                .body
                .contains("é.txt")
        );
    }

    #[test]
    fn empty_zip_has_an_empty_tree() {
        let bytes = archive(&[], "");
        assert_eq!(
            from_bytes("empty.zip", &bytes).unwrap().body,
            "empty.zip/\n└── （空）\n"
        );
    }

    #[test]
    fn trees_stop_after_two_thousand_central_entries() {
        let names: Vec<_> = (0..TREE_ENTRY_LIMIT + 1)
            .map(|n| format!("file-{n:04}.txt"))
            .collect();
        let refs: Vec<_> = names.iter().map(String::as_str).collect();
        let bytes = archive(&refs, "");
        let tree = from_bytes("many.zip", &bytes).unwrap();
        assert!(tree.truncated);
        assert!(tree.body.contains("file-1999.txt"));
        assert!(!tree.body.contains("file-2000.txt"));
    }

    #[test]
    fn metadata_budget_can_truncate_at_a_record_boundary() {
        let bytes = archive(&["first.txt", "second.txt"], "");
        let End::Classic(dir) = end_record(&bytes, bytes.len() as u64).unwrap() else {
            panic!()
        };
        let start = dir.offset as usize;
        // Simulates the byte budget ending inside the second entry's header.
        let prefix = &bytes[start..start + 46 + "first.txt".len() + 10];
        let tree = tree("budget.zip", prefix, &dir).unwrap();
        assert!(tree.truncated);
        assert!(tree.body.contains("first.txt"));
        assert!(!tree.body.contains("second.txt"));
        assert_eq!(
            Directory {
                offset: 1,
                size: u64::MAX - 1,
                entries: 1
            }
            .range(),
            1..1 + DIRECTORY_LIMIT
        );
    }

    #[test]
    fn invalid_sizes_counts_and_multi_disk_records_are_rejected() {
        let original = archive(&["one.txt"], "");
        let at = original.len() - 22;
        for (field, value) in [(4, 1u16), (8, 2), (10, 1000)] {
            let mut bytes = original.clone();
            bytes[at + field..at + field + 2].copy_from_slice(&value.to_le_bytes());
            assert!(from_bytes("broken.zip", &bytes).is_err());
        }
        let mut bytes = original;
        bytes[at + 12..at + 16].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(from_bytes("broken.zip", &bytes).is_err());
        assert!(
            Directory {
                offset: u64::MAX - 1,
                size: 4,
                entries: 0
            }
            .validate(u64::MAX)
            .is_err()
        );
    }

    #[test]
    fn damaged_central_records_are_rejected() {
        let mut bytes = archive(&["one.txt"], "");
        let End::Classic(dir) = end_record(&bytes, bytes.len() as u64).unwrap() else {
            panic!()
        };
        bytes[dir.offset as usize] = 0;
        assert!(
            from_bytes("broken.zip", &bytes)
                .unwrap_err()
                .starts_with("无法读取 ZIP 目录：")
        );
    }

    #[test]
    fn traversal_names_are_omitted_and_deep_paths_are_bounded() {
        let bytes = archive(&["../bad.txt", "/root.txt", "ok.txt"], "");
        let tree = from_bytes("names.zip", &bytes).unwrap();
        assert!(tree.body.contains("ok.txt"));
        assert!(!tree.body.contains("bad.txt"));
        assert!(!tree.body.contains("root.txt"));
        let deep = format!("{}file", "nested/".repeat(129));
        assert!(from_bytes("deep.zip", &archive(&[&deep], "")).is_err());
    }

    #[test]
    fn zip64_offsets_remain_64_bit_and_need_no_large_buffer() {
        let offset: u64 = 5 * 1024 * 1024 * 1024;
        let mut record = vec![0; 56];
        record[..4].copy_from_slice(b"PK\x06\x06");
        record[4..12].copy_from_slice(&44u64.to_le_bytes());
        record[24..32].copy_from_slice(&1u64.to_le_bytes());
        record[32..40].copy_from_slice(&1u64.to_le_bytes());
        record[40..48].copy_from_slice(&64u64.to_le_bytes());
        record[48..56].copy_from_slice(&offset.to_le_bytes());
        let position = offset + 64;
        let directory = zip64_record(&record, position, position + 56).unwrap();
        assert_eq!(directory.range(), offset..offset + 64);
        let mut locator = vec![0; 20];
        locator[..4].copy_from_slice(b"PK\x06\x07");
        locator[8..16].copy_from_slice(&position.to_le_bytes());
        locator[16..20].copy_from_slice(&1u32.to_le_bytes());
        let mut eocd = vec![0; 22];
        eocd[..4].copy_from_slice(b"PK\x05\x06");
        eocd[8..12].fill(0xff);
        eocd[12..20].fill(0xff);
        let tail = [record, locator, eocd].concat();
        assert!(
            matches!(end_record(&tail, position + tail.len() as u64).unwrap(), End::Zip64 { offset: n, .. } if n == position)
        );
        assert!(zip64_record(&tail[..56], position, position + 55).is_err());
    }

    #[tokio::test]
    async fn local_zip_uses_the_same_range_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("tree.zip"),
            archive(&["nested/file.txt"], ""),
        )
        .unwrap();
        let vfs = crate::Vfs::local(
            crate::Rt::from_current().unwrap(),
            dir.path().to_str().unwrap(),
        )
        .unwrap();
        let tree = vfs.read_zip_tree("tree.zip").await.unwrap();
        assert_eq!(tree.body, "tree.zip/\n└── nested/\n    └── file.txt\n");
    }
}
