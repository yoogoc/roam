//! Renders the transfer panel with live tasks, against the real platform text
//! system.
//!
//! Queues a batch of uploads into a temporary backend so the panel has progress
//! bars, speeds, and a mix of finished and running rows to draw. If any of that
//! fails to lay out, the process aborts instead of staying up — which is what
//! the `TestPlatform`-based unit tests cannot detect.
//!
//!     cargo run -p roam-ui --example transfer_panel

use std::sync::Arc;

use gpui::{
    AnyView, App, AppContext, Application, Bounds, Window, WindowBounds, WindowOptions, px, size,
};
use gpui_component::Root;
use roam_core::transfer::{DEFAULT_CONCURRENCY, Transfer};
use roam_core::{Rt, TransferEngine, Vfs};
use roam_ui::{Assets, TransferPanel};

fn main() {
    let rt = Rt::new().expect("tokio runtime");

    // A scratch backend plus a handful of files to push into it.
    let target = tempfile::tempdir().expect("temp dir");
    let source = tempfile::tempdir().expect("temp dir");
    let vfs = Vfs::local(rt.clone(), target.path().to_str().unwrap()).expect("local session");

    let mut queued = Vec::new();
    for (ix, size) in [4_096usize, 2_000_000, 9_000_000, 512].iter().enumerate() {
        let file = source.path().join(format!("sample-{ix}.bin"));
        std::fs::write(&file, vec![b'x'; *size]).expect("write sample");
        queued.push(Transfer::Upload {
            vfs: vfs.clone(),
            local: file,
            remote: Arc::from(format!("sample-{ix}.bin").as_str()),
        });
    }

    // One doomed task, so the failed row and its retry button get laid out too.
    queued.push(Transfer::Download {
        vfs: vfs.clone(),
        remote: Arc::from("does-not-exist.bin"),
        local: source.path().join("never.bin"),
        size: None,
    });

    let engine = TransferEngine::new(rt, DEFAULT_CONCURRENCY);
    engine.enqueue_all(queued);

    // Keep the temp dirs alive for the life of the process.
    let _keep = (target, source);

    Application::new()
        .with_assets(Assets)
        .run(move |cx: &mut App| {
            gpui_component::init(cx);
            // Installs the keyboard bindings; without it every shortcut is inert.
            roam_ui::init(cx);

            let bounds = Bounds::centered(None, size(px(760.), px(420.)), cx);

            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                move |window: &mut Window, cx| {
                    let panel = cx.new(|cx| {
                        let mut panel = TransferPanel::new(engine.clone(), window, cx);
                        panel.refresh(cx);
                        panel
                    });

                    cx.new(|cx| Root::new(AnyView::from(panel), window, cx))
                },
            )
            .expect("window");

            cx.activate(true);
        });
}
