//! Lays out the version-history dialog against the real platform text system.
//!
//! The unit tests only check which menu items are offered; the dialog itself —
//! including the delete-marker row, which deliberately has no size to show — is
//! only ever rendered for real here.
//!
//!     cargo run -p roam-ui --example versions_dialog

use std::sync::Arc;

use gpui::{AnyView, App, AppContext, Bounds, Window, WindowBounds, WindowOptions, px, size};
use gpui_component::Root;
use roam_core::transfer::DEFAULT_CONCURRENCY;
use roam_core::{DirEntry, EntryKind, ObjectVersion, Rt, TransferEngine, Vfs};
use roam_ui::{Assets, Browser};

fn main() {
    let rt = Rt::new().expect("tokio runtime");
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(dir.path().join("cfg.toml"), b"good = true").expect("write");

    let vfs = Vfs::local(rt.clone(), dir.path().to_str().unwrap()).expect("local session");
    let engine = TransferEngine::new(rt, DEFAULT_CONCURRENCY);

    let entry = DirEntry {
        name: "cfg.toml".into(),
        path: "cfg.toml".into(),
        kind: EntryKind::File,
        size: Some(11),
        modified: None,
        etag: None,
        meta_complete: true,
    };

    // A history with all three row shapes: current, historical, delete marker.
    let now: jiff::Timestamp = "2026-08-15T09:00:00Z".parse().unwrap();
    let versions = vec![
        ObjectVersion {
            id: Some(Arc::from("3f9a1c77e0b24d5e8a11")),
            size: Some(11),
            modified: Some(now),
            is_current: true,
            is_delete_marker: false,
        },
        ObjectVersion {
            id: Some(Arc::from("b7c2d0114fa9")),
            size: None,
            modified: Some(now),
            is_current: false,
            is_delete_marker: true,
        },
        ObjectVersion {
            id: Some(Arc::from("0a1b2c3d4e5f6071")),
            size: Some(6),
            modified: Some(now),
            is_current: false,
            is_delete_marker: false,
        },
    ];

    let _keep = dir;

    gpui_platform::application()
        .with_assets(Assets)
        .run(move |cx: &mut App| {
            gpui_component::init(cx);
            roam_ui::init(cx);

            let bounds = Bounds::centered(None, size(px(900.), px(600.)), cx);

            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                move |window: &mut Window, cx| {
                    let browser = cx.new(|cx| {
                        let browser = Browser::new(vfs.clone(), engine.clone(), window, cx);
                        let entry = entry.clone();
                        let versions = versions.clone();

                        // Deferred for the same reason as the other dialog examples:
                        // `Root` has to be the window's root layer first.
                        cx.defer_in(window, move |browser, window, cx| {
                            browser.open_versions_dialog_with(entry, versions, window, cx);
                        });

                        browser
                    });

                    cx.new(|cx| Root::new(AnyView::from(browser), window, cx))
                },
            )
            .expect("window");

            cx.activate(true);
        });
}
