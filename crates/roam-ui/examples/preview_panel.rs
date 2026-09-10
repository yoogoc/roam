//! Shows the preview panel against the real platform text and image stacks.
//!
//! The unit tests can check *which* preview state is chosen, but they run on
//! gpui's `TestPlatform` and never decode or lay out anything. Image decoding
//! and markdown layout only happen for real here — if either fails, the process
//! aborts instead of staying up.
//!
//! Pass a file to preview, or leave it out for the built-in PNG:
//!
//!     cargo run -p roam-ui --example preview_panel [path]

use gpui_kit::component::Root;
use gpui_kit::{AnyView, App, AppContext, Bounds, Window, WindowBounds, WindowOptions, px, size};
use roam_core::{DirEntry, EntryKind, Rt, Vfs};
use roam_ui::{Assets, PreviewPanel};

/// A tiny valid PNG, so the example needs no fixture on disk.
fn swatch_png() -> Vec<u8> {
    fn chunk(kind: &[u8], data: &[u8]) -> Vec<u8> {
        let mut body = kind.to_vec();
        body.extend_from_slice(data);
        let mut out = (data.len() as u32).to_be_bytes().to_vec();
        out.extend_from_slice(&body);
        out.extend_from_slice(&crc32(&body).to_be_bytes());
        out
    }

    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for &byte in bytes {
            crc ^= byte as u32;
            for _ in 0..8 {
                let mask = (crc & 1).wrapping_neg();
                crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
            }
        }
        !crc
    }

    let (w, h) = (32u32, 32u32);
    let mut raw = Vec::new();
    for y in 0..h {
        raw.push(0); // filter byte
        for x in 0..w {
            raw.extend_from_slice(&[(x * 8) as u8, (y * 8) as u8, 160]);
        }
    }

    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // 8-bit RGB

    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    png.extend_from_slice(&chunk(b"IHDR", &ihdr));
    png.extend_from_slice(&chunk(b"IDAT", &deflate_stored(&raw)));
    png.extend_from_slice(&chunk(b"IEND", b""));
    png
}

/// zlib stream using stored (uncompressed) deflate blocks — enough for a decoder
/// to accept, and it avoids pulling in a compression dependency.
fn deflate_stored(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01]; // zlib header, no preset dictionary

    for (ix, block) in data.chunks(65_535).enumerate() {
        let last = (ix + 1) * 65_535 >= data.len();
        out.push(if last { 1 } else { 0 });
        out.extend_from_slice(&(block.len() as u16).to_le_bytes());
        out.extend_from_slice(&(!(block.len() as u16)).to_le_bytes());
        out.extend_from_slice(block);
    }

    // Adler-32 of the uncompressed data.
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in data {
        a = (a + byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    out.extend_from_slice(&((b << 16) | a).to_be_bytes());
    out
}

fn main() {
    let rt = Rt::new().expect("tokio runtime");

    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(dir.path().join("swatch.png"), swatch_png()).expect("write png");
    std::fs::write(
        dir.path().join("README.md"),
        "# Roam\n\n预览面板把这段渲染成 **Markdown**。\n\n- 一\n- 二\n",
    )
    .expect("write md");

    // Default to the markdown file, since it exercises the most layout.
    let target = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "README.md".into());
    let vfs = Vfs::local(rt, dir.path().to_str().unwrap()).expect("local session");

    let size_on_disk = std::fs::metadata(dir.path().join(&target))
        .ok()
        .map(|m| m.len());
    let entry = DirEntry {
        name: target.clone().into(),
        path: target.clone().into(),
        kind: EntryKind::File,
        size: size_on_disk,
        modified: None,
        etag: None,
        meta_complete: true,
    };

    let _keep = dir;

    gpui_kit::application()
        .with_assets(Assets)
        .run(move |cx: &mut App| {
            gpui_kit::init(cx);
            roam_ui::init(cx);

            let bounds = Bounds::centered(None, size(px(420.), px(560.)), cx);

            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                move |window: &mut Window, cx| {
                    let panel = cx.new(|cx| {
                        let mut panel = PreviewPanel::new(vfs.clone());
                        panel.set_entry(Some(entry.clone()), cx);
                        panel
                    });

                    cx.new(|cx| Root::new(AnyView::from(panel), window, cx))
                },
            )
            .expect("window");

            cx.activate(true);
        });
}
