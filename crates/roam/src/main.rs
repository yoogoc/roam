use std::sync::Arc;

use anyhow::{Context as _, Result};
use gpui_kit::component::Root;
use gpui_kit::{
    AnyView, App, AppContext, Bounds, TitlebarOptions, Window, WindowBounds, WindowOptions, px,
    size,
};
use roam_core::{ProfileStore, Rt, Vfs};
use roam_ui::{Assets, Workspace};

/// The always-available local session. Saved connections come from
/// `profiles.toml`; this one needs no configuration so the app is useful on
/// first run.
fn local_root() -> Result<String> {
    if let Some(arg) = std::env::args().nth(1) {
        return Ok(arg);
    }

    // Not `$HOME`: there is no such variable on Windows. See `roam_core::dirs`.
    let home = roam_core::dirs::home().context("cannot determine the home directory")?;
    Ok(home.to_string_lossy().into_owned())
}

/// Where connection profiles live. `ROAM_CONFIG` overrides the platform config
/// directory, which keeps test runs out of the real one.
fn profile_store() -> Result<ProfileStore> {
    match std::env::var_os("ROAM_CONFIG") {
        Some(path) => Ok(ProfileStore::at(path)),
        None => Ok(ProfileStore::default_store()?),
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "roam=info,roam_core=info,roam_ui=info".into()),
        )
        .init();

    let root = local_root()?;
    // Built before the GPUI application so a bad runtime or root fails at the
    // terminal rather than behind an empty window.
    let rt = Rt::new()?;
    let local = Vfs::local(rt.clone(), &root)?;
    let store = Arc::new(profile_store()?);

    tracing::info!(root = %root, profiles = %store.path().display(), "opening roam");

    gpui_kit::application()
        .with_assets(Assets)
        .run(move |cx: &mut App| {
            gpui_kit::init(cx);
            // Installs the keyboard bindings; without it every shortcut is inert.
            roam_ui::init(cx);

            let bounds = Bounds::centered(None, size(px(1180.), px(760.)), cx);

            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions {
                        title: Some("Roam".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                |window: &mut Window, cx| {
                    let workspace = cx.new(|cx| {
                        Workspace::new(rt.clone(), store.clone(), local.clone(), window, cx)
                    });
                    cx.new(|cx| Root::new(AnyView::from(workspace), window, cx))
                },
            )
            .expect("failed to open the roam window");

            cx.activate(true);
        });

    Ok(())
}
