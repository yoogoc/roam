//! Opens the connection dialog immediately, against the real platform text
//! system.
//!
//! The unit tests run on gpui's `TestPlatform`, whose text system is a stub — it
//! cannot reproduce layout panics like the multi-line-placeholder crash in
//! gpui-component 0.5.1 (see `connection_form.rs`). This example can: if the
//! dialog fails to lay out, the process aborts instead of staying up.
//!
//!     cargo run -p roam-ui --example connection_dialog
//!     cargo run -p roam-ui --example connection_dialog -- --compact
//!     cargo run -p roam-ui --features profiler --example connection_dialog
//!     cargo run -p roam-ui --example connection_dialog -- --service=sharepoint
//!
//! With `profiler`, frame timings are written to a temporary file every second
//! while you interact with the dialog. Set ROAM_DIALOG_PROFILE to its path.

use std::sync::Arc;

use gpui_kit::component::{Root, TitleBar};
use gpui_kit::{AnyView, App, AppContext, Bounds, Window, WindowBounds, WindowOptions, px, size};
use roam_core::{ProfileStore, Rt, Vfs};
use roam_ui::{Assets, Workspace};

fn main() {
    let width = if std::env::args().any(|arg| arg == "--compact") {
        640.
    } else {
        900.
    };
    let initial_service = std::env::args()
        .find_map(|arg| arg.strip_prefix("--service=").map(str::to_owned))
        .unwrap_or_else(|| "s3".into());
    let rt = Rt::new().expect("tokio runtime");
    let temp = std::env::temp_dir();
    let local = Vfs::local(rt.clone(), temp.to_str().unwrap()).expect("local session");
    let store = Arc::new(ProfileStore::at(temp.join("roam-example-profiles.toml")));

    gpui_kit::application()
        .with_assets(Assets)
        .run(move |cx: &mut App| {
            gpui_kit::init(cx);
            // Installs the keyboard bindings; without it every shortcut is inert.
            roam_ui::init(cx);

            let bounds = Bounds::centered(None, size(px(width), px(640.)), cx);

            let handle = cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..TitleBar::window_options()
                },
                move |window: &mut Window, cx| {
                    let workspace = cx.new(|cx| {
                        let workspace =
                            Workspace::new(rt.clone(), store.clone(), local.clone(), window, cx);

                        // Deferred on purpose. `open_dialog` reaches for the
                        // window's root layer, so it cannot run before `Root` is
                        // installed below — and it cannot run inside a `Root`
                        // update either, or gpui panics with "cannot update Root
                        // while it is already being updated".
                        cx.defer_in(window, move |workspace, window, cx| {
                            // S3 remains the default to exercise its tall field list.
                            workspace.open_new_connection_for(&initial_service, window, cx);
                        });

                        workspace
                    });

                    cx.new(|cx| Root::new(AnyView::from(workspace), window, cx))
                },
            )
            .expect("window");

            #[cfg(feature = "profiler")]
            cx.spawn(async move |cx| {
                let path = std::env::var_os("ROAM_DIALOG_PROFILE")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| std::env::temp_dir().join("roam-dialog-profile.txt"));
                loop {
                    cx.background_executor()
                        .timer(std::time::Duration::from_secs(1))
                        .await;
                    let Ok((draw, input)) = handle.update(cx, |_, window, _| {
                        (
                            window.frame_duration_snapshot().draw_duration_histogram,
                            window.input_latency_snapshot().latency_histogram,
                        )
                    }) else {
                        break;
                    };
                    let report = format!(
                        "draw: frames={} p50={:.2}ms p95={:.2}ms max={:.2}ms\ninput: samples={} p50={:.2}ms p95={:.2}ms max={:.2}ms\n",
                        draw.len(), draw.value_at_quantile(0.5) as f64 / 1e6,
                        draw.value_at_quantile(0.95) as f64 / 1e6, draw.max() as f64 / 1e6,
                        input.len(), input.value_at_quantile(0.5) as f64 / 1e6,
                        input.value_at_quantile(0.95) as f64 / 1e6, input.max() as f64 / 1e6,
                    );
                    let path = path.clone();
                    cx.background_executor().spawn(async move {
                        let _ = std::fs::write(path, report);
                    }).await;
                }
            }).detach();
            #[cfg(not(feature = "profiler"))]
            let _ = handle;

            cx.activate(true);
        });
}
