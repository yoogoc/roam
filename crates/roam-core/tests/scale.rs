//! Does the design's headline performance claim actually hold?
//!
//! `docs/DESIGN.md` says a 100k-entry directory stays usable because the entry
//! list is never cloned or reordered — sorting and filtering build a `Vec<u32>`
//! index view over it. That claim was never measured. These tests measure it.
//!
//! The thresholds are deliberately loose: the point is to catch a regression that
//! changes the shape of the cost (an accidental clone per row, a quadratic
//! rebuild), not to police a few milliseconds on a busy machine. Measurements are
//! printed either way, so `--nocapture` shows the real numbers.
//!
//! The disk-backed test is opt-in, because creating 100k files takes a while:
//!
//! ```text
//! ROAM_SCALE_FS=1 cargo test -p roam-core --test scale -- --nocapture
//! ```

use std::time::{Duration, Instant};

use roam_core::{DirEntry, EntryKind, Rt, SortKey, Vfs, view_indices};

const ENTRIES: usize = 100_000;

fn synthetic(count: usize) -> Vec<DirEntry> {
    (0..count)
        .map(|i| {
            let dir = i % 20 == 0;
            let name = if dir {
                format!("dir-{i:06}")
            } else {
                format!("file-{i:06}.txt")
            };
            DirEntry {
                name: name.as_str().into(),
                path: name.as_str().into(),
                kind: if dir { EntryKind::Dir } else { EntryKind::File },
                size: Some((i * 7 % 100_000) as u64),
                modified: None,
                etag: None,
                meta_complete: true,
            }
        })
        .collect()
}

fn timed<T>(label: &str, f: impl FnOnce() -> T) -> (T, Duration) {
    let start = Instant::now();
    let out = f();
    let elapsed = start.elapsed();
    eprintln!("  {label}: {elapsed:?}");
    (out, elapsed)
}

#[test]
fn sorting_a_hundred_thousand_entries_is_an_index_sort() {
    let entries = synthetic(ENTRIES);
    eprintln!("sorting {ENTRIES} entries");

    let (by_name, name_time) = timed("by name", || {
        view_indices(&entries, SortKey::Name, true, "")
    });
    let (by_size, size_time) = timed("by size", || {
        view_indices(&entries, SortKey::Size, false, "")
    });

    assert_eq!(by_name.len(), ENTRIES);
    assert_eq!(by_size.len(), ENTRIES);

    // Directories first, still, at this size.
    assert!(entries[by_name[0] as usize].is_dir());

    // An index sort of 100k is well under a second even in a debug build; a
    // breach here means the cost changed shape, not that the machine is busy.
    assert!(
        name_time < Duration::from_secs(5),
        "name sort took {name_time:?}"
    );
    assert!(
        size_time < Duration::from_secs(5),
        "size sort took {size_time:?}"
    );
}

#[test]
fn filtering_a_hundred_thousand_entries_does_not_touch_the_entries() {
    let entries = synthetic(ENTRIES);
    eprintln!("filtering {ENTRIES} entries");

    let (matched, filter_time) = timed("filter \"file-0001\"", || {
        view_indices(&entries, SortKey::Name, true, "file-0001")
    });

    // Every name is unique, so this is a small, predictable subset.
    assert!(!matched.is_empty());
    assert!(matched.len() < ENTRIES / 10);
    assert!(
        filter_time < Duration::from_secs(5),
        "filtering took {filter_time:?}"
    );

    // The whole claim: clearing the filter is free because the entry list was
    // never modified in the first place.
    let (all, clear_time) = timed("clear the filter", || {
        view_indices(&entries, SortKey::Name, true, "")
    });
    assert_eq!(all.len(), ENTRIES);
    assert!(clear_time < Duration::from_secs(5));
}

#[test]
fn an_index_view_costs_four_bytes_per_row() {
    let entries = synthetic(ENTRIES);
    let view = view_indices(&entries, SortKey::Name, true, "");

    // The design's reason for using `Vec<u32>`: the view is a rounding error next
    // to the entries themselves, so re-deriving it per sort click is cheap.
    let view_bytes = view.len() * std::mem::size_of::<u32>();
    let entry_bytes = entries.len() * std::mem::size_of::<DirEntry>();
    eprintln!(
        "index view: {} KB vs entries: {} KB (excluding string data)",
        view_bytes / 1024,
        entry_bytes / 1024
    );

    assert!(
        view_bytes * 4 < entry_bytes,
        "an index view should be far smaller than the rows it points at"
    );
}

#[test]
fn repeated_sorting_does_not_accumulate_cost() {
    let entries = synthetic(ENTRIES);
    eprintln!("re-sorting {ENTRIES} entries ten times");

    let mut worst = Duration::ZERO;
    for i in 0..10 {
        let ascending = i % 2 == 0;
        let start = Instant::now();
        let view = view_indices(&entries, SortKey::Name, ascending, "");
        worst = worst.max(start.elapsed());
        assert_eq!(view.len(), ENTRIES);
    }
    eprintln!("  worst single sort: {worst:?}");

    // Clicking a column header repeatedly is a real thing people do. If each
    // sort mutated or cloned the entries, this would degrade.
    assert!(worst < Duration::from_secs(5), "worst sort was {worst:?}");
}

#[tokio::test]
async fn listing_a_hundred_thousand_files_streams_in_batches() {
    if std::env::var("ROAM_SCALE_FS").is_err() {
        eprintln!("skipping: set ROAM_SCALE_FS=1 (this creates {ENTRIES} files)");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    eprintln!("creating {ENTRIES} files in {}", dir.path().display());
    let (_, create_time) = timed("create", || {
        for i in 0..ENTRIES {
            std::fs::write(dir.path().join(format!("file-{i:06}.txt")), b"x").unwrap();
        }
    });
    assert!(create_time < Duration::from_secs(600), "fixture setup");

    let vfs = Vfs::local(Rt::from_current().unwrap(), dir.path().to_str().unwrap()).unwrap();

    // The claim under test: rows arrive in batches, so the pane paints before the
    // whole directory has been read.
    let mut listing = vfs.list("");
    let start = Instant::now();
    let mut batches = 0usize;
    let mut total = 0usize;
    let mut first_batch_at = None;

    while let Some(batch) = listing.next_batch().await {
        let batch = batch.unwrap();
        if first_batch_at.is_none() {
            first_batch_at = Some(start.elapsed());
        }
        batches += 1;
        total += batch.len();
    }

    let elapsed = start.elapsed();
    let first = first_batch_at.expect("at least one batch");
    eprintln!("  first batch after {first:?}, {batches} batches, {total} entries in {elapsed:?}");

    assert_eq!(total, ENTRIES);
    assert!(
        batches > 1,
        "a directory this size must arrive in more than one batch"
    );
    // What matters for the UI is that the first rows land quickly, not that the
    // whole listing is fast.
    assert!(
        first < elapsed / 2,
        "the first batch ({first:?}) should arrive well before the last ({elapsed:?})"
    );
}

#[tokio::test]
async fn a_dropped_listing_stops_reading_a_huge_directory() {
    if std::env::var("ROAM_SCALE_FS").is_err() {
        eprintln!("skipping: set ROAM_SCALE_FS=1");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    for i in 0..20_000 {
        std::fs::write(dir.path().join(format!("file-{i:06}.txt")), b"x").unwrap();
    }
    let vfs = Vfs::local(Rt::from_current().unwrap(), dir.path().to_str().unwrap()).unwrap();

    // Navigating away from a large directory must stop paying for it: dropping
    // the Listing aborts the task, which drops the lister mid-scan.
    let mut listing = vfs.list("");
    let first = listing.next_batch().await.unwrap().unwrap();
    assert!(!first.is_empty());

    let handle = listing.abort_handle_for_test();
    drop(listing);
    tokio::task::yield_now().await;
    assert!(handle.is_finished(), "the scan should have been aborted");
}

#[test]
fn sorting_scales_linearly_enough_to_rule_out_quadratic_cost() {
    // A shape check rather than a timing check: ten times the rows should not
    // cost a hundred times as much. This is what would break if a sort ever
    // started cloning or re-deriving per row.
    let small = synthetic(10_000);
    let large = synthetic(100_000);

    let (_, small_time) = timed("10k", || view_indices(&small, SortKey::Name, true, ""));
    let (_, large_time) = timed("100k", || view_indices(&large, SortKey::Name, true, ""));

    let ratio = large_time.as_secs_f64() / small_time.as_secs_f64().max(1e-9);
    eprintln!("  100k/10k ratio: {ratio:.1}x (n log n predicts ~12x)");

    assert!(
        ratio < 40.0,
        "cost grew {ratio:.1}x for 10x the rows, which looks superlinear"
    );
}

/// What the sidebar tree costs to draw.
///
/// `DirTreeView::render` calls `visible()` once per frame, so this is per-frame
/// work — not a one-off. The panel is 260px tall, about fifteen rows, so anything
/// proportional to the *whole* expanded tree is spent on rows nobody can see.
#[test]
fn the_tree_row_list_is_rebuilt_every_frame() {
    use roam_core::DirTree;

    for count in [1_000usize, 10_000, 50_000] {
        let mut tree = DirTree::new();
        let kids: Vec<std::sync::Arc<str>> =
            (0..count).map(|i| format!("dir-{i:06}/").into()).collect();
        tree.set_children("", kids);

        let started = std::time::Instant::now();
        let rows = tree.visible();
        let once = started.elapsed();

        // Ten frames' worth, i.e. a sixth of a second of drawing.
        let started = std::time::Instant::now();
        for _ in 0..10 {
            let _ = tree.visible();
        }
        let ten = started.elapsed();

        println!(
            "tree {count:>6} nodes: visible() {once:>10.2?} once, {ten:>10.2?} for 10 frames, \
             {} rows built (panel shows ~15)",
            rows.len()
        );
    }
}

/// Expanding a sidebar node in a directory that is mostly files.
///
/// The tree only wants subdirectories. `list_all` collected every entry first,
/// so a folder of 50k files cost 50k `DirEntry` allocations to find a handful of
/// directories; `list_dirs` discards files as they stream.
#[test]
fn listing_only_directories_skips_the_files() {
    if std::env::var("ROAM_SCALE_FS").is_err() {
        eprintln!("skipping: set ROAM_SCALE_FS=1 (this one writes 50k files)");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    for i in 0..50_000 {
        std::fs::write(dir.path().join(format!("file-{i:06}.txt")), b"x").unwrap();
    }
    for i in 0..20 {
        std::fs::create_dir(dir.path().join(format!("sub-{i:02}"))).unwrap();
    }

    let rt = roam_core::Rt::new().unwrap();
    let vfs = roam_core::Vfs::local(rt.clone(), dir.path().to_str().unwrap()).unwrap();

    let all = std::time::Instant::now();
    let entries = rt
        .handle()
        .block_on(async { vfs.list_all("").await.unwrap() });
    let all = all.elapsed();

    let dirs_only = std::time::Instant::now();
    let dirs = rt
        .handle()
        .block_on(async { vfs.list_dirs("").await.unwrap() });
    let dirs_only = dirs_only.elapsed();

    println!(
        "list_all: {:>10.2?} for {} entries  |  list_dirs: {:>10.2?} for {} dirs",
        all,
        entries.len(),
        dirs_only,
        dirs.len()
    );

    assert_eq!(dirs.len(), 20);
    // What matters is what is held, not the wall clock: one keeps 50k entries
    // alive, the other keeps 20.
    assert!(entries.len() > 50_000);
}

/// What browsing a lot of large directories costs in memory.
///
/// `ListingCache` had no bound at all: every directory ever visited stayed
/// resident for the life of the session. This is the measurement that says
/// whether that matters.
#[test]
fn the_listing_cache_is_bounded() {
    use roam_core::{DirEntry, EntryKind, ListingCache, RemotePath};

    fn listing(n: usize) -> Vec<DirEntry> {
        (0..n)
            .map(|i| DirEntry {
                name: format!("file-{i:06}.txt").into(),
                path: format!("dir/file-{i:06}.txt").into(),
                kind: EntryKind::File,
                size: Some(i as u64),
                modified: None,
                etag: None,
                meta_complete: true,
            })
            .collect()
    }

    // Roughly the per-entry footprint, so the numbers below mean something.
    let per_entry = std::mem::size_of::<DirEntry>();
    let cache = ListingCache::new();

    for d in 0..60 {
        cache.put(
            RemotePath::new(1, format!("dir-{d}/")),
            listing(10_000),
            true,
        );
    }

    let held: usize = cache.total_entries();
    println!(
        "60 directories x 10k entries: cache holds {held} entries \
         (~{} MB of DirEntry alone, {per_entry} B each), {} directories retained",
        held * per_entry / 1_048_576,
        cache.len()
    );

    // The point of the bound: browsing does not grow without limit.
    assert!(
        held <= ListingCache::MAX_ENTRIES,
        "cache holds {held}, over the {} cap",
        ListingCache::MAX_ENTRIES
    );
}

/// Navigating into a directory clones its whole listing — twice, once to serve
/// the cache and once to store it. This says whether that is worth avoiding.
#[test]
fn cloning_a_large_listing() {
    use roam_core::{DirEntry, EntryKind};

    let entries: Vec<DirEntry> = (0..100_000)
        .map(|i| DirEntry {
            name: format!("file-{i:06}.txt").into(),
            path: format!("prefix/file-{i:06}.txt").into(),
            kind: EntryKind::File,
            size: Some(i as u64),
            modified: None,
            etag: Some("\"abc123\"".into()),
            meta_complete: true,
        })
        .collect();

    let started = std::time::Instant::now();
    let cloned = entries.clone();
    let once = started.elapsed();

    println!(
        "cloning 100k entries: {once:>10.2?} ({} bytes each)",
        std::mem::size_of::<DirEntry>()
    );
    assert_eq!(cloned.len(), entries.len());
}

/// What the transfer panel's 10 Hz poll costs when a big folder was dropped in.
///
/// `plan_upload` makes one task per file, so a folder of 100k files becomes 100k
/// tasks. `snapshot()` clones every record and takes three locks per task, and the
/// panel called it ten times a second.
#[test]
fn the_transfer_snapshot_scales_with_task_count() {
    use roam_core::transfer::Transfer;
    use roam_core::{Rt, TransferEngine, Vfs};

    let dir = tempfile::tempdir().unwrap();
    let rt = Rt::new().unwrap();
    let vfs = Vfs::local(rt.clone(), dir.path().to_str().unwrap()).unwrap();
    // Concurrency 1 so almost everything stays queued and the measurement is of
    // the snapshot itself, not of contention with running transfers.
    let engine = TransferEngine::new(rt, 1);

    for count in [1_000usize, 20_000] {
        let engine = engine.clone();
        engine.enqueue_all((0..count).map(|i| Transfer::Upload {
            vfs: vfs.clone(),
            local: dir.path().join(format!("missing-{i}.bin")),
            remote: format!("missing-{i}.bin").into(),
        }));

        let started = std::time::Instant::now();
        let snap = engine.snapshot();
        let full = started.elapsed();

        let started = std::time::Instant::now();
        let overview = engine.overview(50);
        let cheap = started.elapsed();

        println!(
            "{:>6} tasks: snapshot() {full:>10.2?} for {} records  |  overview(50) {cheap:>10.2?} \
             for {} rows (+{} total/{} active counted)",
            snap.len(),
            snap.len(),
            overview.recent.len(),
            overview.total,
            overview.active,
        );

        // Ten polls a second is the panel's rate; the cheap path has to stay flat.
        assert!(overview.recent.len() <= 50);
        assert_eq!(overview.total, snap.len());
    }
}
