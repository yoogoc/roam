//! The application's asset source.
//!
//! gpui-component names its icons by relative path — `IconName::ArrowLeft`
//! resolves to `icons/arrow-left.svg` — and an application that supplies no
//! asset source gets *nothing drawn*, silently, in both layers:
//!
//! 1. `Application` installs `()` as the asset source unless told otherwise, and
//!    its `load()` answers every path with `Ok(None)`.
//! 2. gpui's svg renderer treats `Ok(None)` as "nothing to draw" and returns
//!    without a log line or an error.
//!
//! Every icon in the app then renders as empty space. On a `.ghost()` icon-only
//! button that leaves nothing to see until the pointer hovers and paints a
//! background — which is how the bug was originally reported: "the button only
//! appears when the mouse passes over it".
//!
//! The icons used to be vendored here, 86 of them, because the published
//! gpui-component shipped none. GPUI Kit supplies the matching icon set through
//! `gpui_kit::assets`, so this is a re-export — but the tests stay.
//! Nothing checking was the reason the icons went missing in the first place, and
//! that is just as true of someone else's asset crate as of our own.

pub use gpui_kit::assets::Assets;

#[cfg(test)]
mod tests {
    use gpui_kit::AssetSource;
    use gpui_kit::component::{IconName, IconNamed};
    use resvg::usvg;

    use super::*;

    /// Every icon our own views reference, derived from the sources so the test
    /// cannot drift out of date: a view that starts using an icon upstream does
    /// not ship fails here, at the one place that can still explain why. Left to
    /// itself the only symptom is invisible-but-clickable space in a running app.
    ///
    /// Several are reachable only through a conditional — `Sun`/`Moon` on the
    /// theme toggle, `ChevronUp` on a collapsed transfer panel — so reading the
    /// button definitions by eye does not find them all.
    fn icon_paths_referenced_by_our_sources() -> Vec<String> {
        let sources = [
            include_str!("browser.rs"),
            include_str!("connection_form.rs"),
            include_str!("delegate.rs"),
            include_str!("dir_tree.rs"),
            include_str!("name_dialog.rs"),
            include_str!("preview.rs"),
            include_str!("transfer_panel.rs"),
            include_str!("workspace.rs"),
        ];

        let mut paths: Vec<String> = Vec::new();
        for src in sources {
            for (ix, _) in src.match_indices("IconName::") {
                let variant: String = src[ix + "IconName::".len()..]
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric())
                    .collect();
                if variant.is_empty() {
                    continue;
                }

                // `IconName` owns the mapping but can neither be enumerated nor
                // even formatted, so the filename is derived the way
                // gpui-component spells them: every uppercase letter and every
                // digit starts a new kebab segment (`Settings2` → `settings-2`).
                let mut file = String::new();
                for (i, c) in variant.char_indices() {
                    if i > 0 && (c.is_ascii_uppercase() || c.is_ascii_digit()) {
                        file.push('-');
                    }
                    file.push(c.to_ascii_lowercase());
                }

                let path = format!("icons/{file}.svg");
                if !paths.contains(&path) {
                    paths.push(path);
                }
            }
        }
        paths
    }

    #[test]
    fn every_icon_our_views_use_actually_resolves() {
        let paths = icon_paths_referenced_by_our_sources();
        assert!(
            paths.len() >= 25,
            "only found {} icon references — the scan itself broke",
            paths.len()
        );

        for path in paths {
            let loaded = Assets
                .load(&path)
                .unwrap_or_else(|e| panic!("{path} is not shipped upstream: {e}"));
            assert!(
                loaded.is_some_and(|bytes| !bytes.is_empty()),
                "{path} resolved to nothing, so it would render as empty space"
            );
        }
    }

    /// The derivation above has to agree with gpui-component's own spelling, so
    /// pin the two together — including on the name that exercises the digit rule.
    #[test]
    fn the_derived_filename_matches_gpui_components_mapping() {
        assert_eq!(IconName::Settings2.path(), "icons/settings-2.svg");
        assert_eq!(IconName::LoaderCircle.path(), "icons/loader-circle.svg");
        assert_eq!(IconName::TriangleAlert.path(), "icons/triangle-alert.svg");
        assert_eq!(IconName::Plus.path(), "icons/plus.svg");
    }

    /// Upstream's asset source reports a missing path as an **error**, where ours
    /// returned `Ok(None)` and gpui's default still does. That is the better of
    /// the two — `Ok(None)` is exactly the silence this module exists to document
    /// — so it is worth pinning: if it ever softens back to `Ok(None)`, a missing
    /// icon goes quiet again.
    #[test]
    fn a_path_that_does_not_exist_is_loud() {
        assert!(
            Assets.load("icons/not-a-real-icon.svg").is_err(),
            "a missing icon must not resolve quietly"
        );
    }

    /// Resolving a path is not the same as drawing something. gpui rasterises an
    /// svg and keeps only the alpha channel, so a file that parses but covers no
    /// pixels is *still* an invisible button — the very symptom this module
    /// exists to prevent.
    #[test]
    fn every_icon_we_use_rasterises_to_visible_pixels() {
        let options = usvg::Options::default();

        for path in icon_paths_referenced_by_our_sources() {
            let bytes = Assets.load(&path).unwrap().expect("shipped");
            let tree = usvg::Tree::from_data(&bytes, &options)
                .unwrap_or_else(|e| panic!("{path} does not parse as svg: {e}"));

            // 16px is the size these actually render at in the toolbar.
            let scale = 16.0 / tree.size().width();
            let mut pixmap = resvg::tiny_skia::Pixmap::new(16, 16).unwrap();
            resvg::render(
                &tree,
                resvg::tiny_skia::Transform::from_scale(scale, scale),
                &mut pixmap.as_mut(),
            );

            let inked = pixmap.pixels().iter().filter(|p| p.alpha() > 0).count();
            let coverage = inked as f32 / pixmap.pixels().len() as f32;

            assert!(
                coverage > 0.02,
                "{path} rasterises to {:.1}% coverage — it would be invisible",
                coverage * 100.0
            );
            // A glyph that fills its whole box is a solid block, not an icon;
            // usually it means a stray `fill` on the root or a bad viewBox.
            assert!(
                coverage < 0.95,
                "{path} covers {:.1}% of its box — that is a filled square",
                coverage * 100.0
            );
        }
    }
}
