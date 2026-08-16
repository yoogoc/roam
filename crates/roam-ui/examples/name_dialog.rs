//! Opens the new-folder prompt against the real platform text system.
//!
//! Same reason as `connection_dialog.rs`: the unit tests run on gpui's
//! `TestPlatform`, whose text system is a stub and cannot reproduce layout
//! panics such as the multi-line-placeholder crash in gpui-component 0.5.1 (see
//! `placeholders.rs`). If this dialog fails to lay out, the process aborts
//! instead of staying up.
//!
//!     cargo run -p roam-ui --example name_dialog

use gpui::{
    AnyView, App, AppContext, Application, Bounds, Window, WindowBounds, WindowOptions, px, size,
};
use gpui_component::Root;
use roam_core::transfer::DEFAULT_CONCURRENCY;
use roam_core::{Rt, TransferEngine, Vfs};
use roam_ui::{Assets, Browser};

fn main() {
    let rt = Rt::new().expect("tokio runtime");
    let temp = std::env::temp_dir();
    let vfs = Vfs::local(rt.clone(), temp.to_str().unwrap()).expect("local session");
    let engine = TransferEngine::new(rt, DEFAULT_CONCURRENCY);

    Application::new()
        .with_assets(Assets)
        .run(move |cx: &mut App| {
            gpui_component::init(cx);
            // Installs the keyboard bindings; without it every shortcut is inert.
            roam_ui::init(cx);

            let bounds = Bounds::centered(None, size(px(820.), px(560.)), cx);

            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                move |window: &mut Window, cx| {
                    let browser = cx.new(|cx| {
                        let browser = Browser::new(vfs.clone(), engine.clone(), window, cx);

                        // Deferred: `open_dialog` needs the `Root` below to already
                        // be the window's root layer, and must not run inside a
                        // `Root` update.
                        cx.defer_in(window, |browser, window, cx| {
                            browser.open_new_folder_prompt(window, cx);
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
