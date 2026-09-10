//! Opens the connection dialog immediately, against the real platform text
//! system.
//!
//! The unit tests run on gpui's `TestPlatform`, whose text system is a stub — it
//! cannot reproduce layout panics like the multi-line-placeholder crash in
//! gpui-component 0.5.1 (see `connection_form.rs`). This example can: if the
//! dialog fails to lay out, the process aborts instead of staying up.
//!
//!     cargo run -p roam-ui --example connection_dialog

use std::sync::Arc;

use gpui_kit::component::Root;
use gpui_kit::{AnyView, App, AppContext, Bounds, Window, WindowBounds, WindowOptions, px, size};
use roam_core::{ProfileStore, Rt, Vfs};
use roam_ui::{Assets, Workspace};

fn main() {
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

            let bounds = Bounds::centered(None, size(px(900.), px(640.)), cx);

            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
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
                        cx.defer_in(window, |workspace, window, cx| {
                            // S3 on purpose: seven fields is the tallest form, and the one
                            // whose height showed that the dialog never scrolled.
                            workspace.open_new_connection_for("s3", window, cx);
                        });

                        workspace
                    });

                    cx.new(|cx| Root::new(AnyView::from(workspace), window, cx))
                },
            )
            .expect("window");

            cx.activate(true);
        });
}
