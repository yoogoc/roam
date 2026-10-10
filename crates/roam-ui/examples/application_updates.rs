//! Native update-settings layout, with isolated configuration and no automatic network checks.
use gpui_kit::component::{Root, TitleBar};
use gpui_kit::{AnyView, App, AppContext, Bounds, WindowBounds, WindowOptions, px, size};
use roam_core::{ProfileStore, Rt, Vfs};
use roam_ui::{Assets, Workspace};
use std::sync::Arc;

fn main() {
    let directory = tempfile::tempdir().unwrap();
    roam_updater::Preferences {
        auto_check: false,
        ..Default::default()
    }
    .save(&directory.path().join("updates.toml"))
    .unwrap();
    let config = directory.path().to_path_buf();
    let rt = Rt::new().unwrap();
    let local = Vfs::local(rt.clone(), config.to_str().unwrap()).unwrap();
    let store = Arc::new(ProfileStore::at(config.join("profiles.toml")));
    let width = if std::env::args().any(|arg| arg == "--compact") {
        640.
    } else {
        1000.
    };
    gpui_kit::application()
        .with_assets(Assets)
        .run(move |cx: &mut App| {
            gpui_kit::init(cx);
            roam_ui::init(cx);
            roam_ui::init_updater(rt.clone(), config.clone(), cx);
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                        None,
                        size(px(width), px(700.)),
                        cx,
                    ))),
                    ..TitleBar::window_options()
                },
                move |window, cx| {
                    let workspace = cx.new(|cx| {
                        cx.defer_in(window, |_, window, cx| {
                            roam_ui::open_update_settings(window, cx)
                        });
                        Workspace::new(rt, store, local, window, cx)
                    });
                    cx.new(|cx| Root::new(AnyView::from(workspace), window, cx))
                },
            )
            .unwrap();
            cx.activate(true);
        });
}
