//! Document content previews. Office files are read as bounded ZIP/XML parts;
//! macros, external relationships and embedded programs are never evaluated.
use super::structured::{Xml, xml};
use std::{
    io::{Cursor, Read},
    sync::{Arc, OnceLock},
};

pub const PAGE_LIMIT: usize = 500;
const PART_LIMIT: u64 = 8 * 1024 * 1024;
const PIXEL_LIMIT: u64 = 16_000_000;

#[derive(Clone)]
pub struct DocumentPage {
    pub markdown: String,
    pub images: Vec<Vec<u8>>,
}
#[derive(Clone)]
pub struct DocumentPreview {
    pub pages: Vec<DocumentPage>,
    pub slides: bool,
    pub truncated: bool,
}

#[derive(Clone)]
pub struct PagedImage {
    bytes: Arc<Vec<u8>>,
    tiff: bool,
    pub count: usize,
    pub truncated: bool,
}
impl PagedImage {
    pub fn open(bytes: Vec<u8>, tiff: bool) -> Result<Self, String> {
        let count = if tiff {
            let mut decoder =
                tiff::decoder::Decoder::new(Cursor::new(&bytes)).map_err(|e| e.to_string())?;
            let mut count = 1;
            while decoder.more_images() && count <= PAGE_LIMIT {
                decoder.next_image().map_err(|e| e.to_string())?;
                count += 1;
            }
            count
        } else {
            let pdf = hayro::hayro_syntax::Pdf::new(bytes.clone())
                .map_err(|e| format!("无法读取 PDF（加密文档需要先解密）：{e:?}"))?;
            pdf.pages().len()
        };
        if count == 0 {
            return Err("文档没有页面".into());
        }
        Ok(Self {
            bytes: Arc::new(bytes),
            tiff,
            count: count.min(PAGE_LIMIT),
            truncated: count > PAGE_LIMIT,
        })
    }
    pub fn render(&self, index: usize, zoom: f32) -> Result<Vec<u8>, String> {
        if index >= self.count {
            return Err("页码超出范围".into());
        }
        if self.tiff {
            return tiff_page(&self.bytes, index);
        }
        let pdf = hayro::hayro_syntax::Pdf::new(self.bytes.as_ref().clone())
            .map_err(|e| format!("PDF 格式错误：{e:?}"))?;
        let page = &pdf.pages()[index];
        let (width, height) = page.render_dimensions();
        if !width.is_finite() || !height.is_finite() || width <= 0. || height <= 0. {
            return Err("PDF 页面尺寸无效".into());
        }
        let scale = (900. * zoom.clamp(0.5, 2.) / width)
            .min(2400. / height)
            .min(2400. / width);
        let settings = hayro::PixmapSettings {
            x_scale: scale,
            y_scale: scale,
            bg_color: hayro::vello_cpu::color::palette::css::WHITE,
        };
        let pixmap = hayro::render(
            page,
            &hayro::RenderCache::new(),
            &Default::default(),
            &Default::default(),
            &settings,
        );
        pixmap.into_png().map_err(|e| e.to_string())
    }
}

fn tiff_page(bytes: &[u8], index: usize) -> Result<Vec<u8>, String> {
    use tiff::{ColorType, decoder::DecodingResult};
    let mut limits = tiff::decoder::Limits::default();
    limits.decoding_buffer_size = 64 * 1024 * 1024;
    limits.intermediate_buffer_size = 16 * 1024 * 1024;
    let mut decoder = tiff::decoder::Decoder::new(Cursor::new(bytes))
        .map_err(|e| e.to_string())?
        .with_limits(limits);
    for _ in 0..index {
        decoder.next_image().map_err(|e| e.to_string())?;
    }
    let (w, h) = decoder.dimensions().map_err(|e| e.to_string())?;
    if u64::from(w) * u64::from(h) > PIXEL_LIMIT {
        return Err("TIFF 页面超过 1600 万像素".into());
    }
    let kind = decoder.colortype().map_err(|e| e.to_string())?;
    let decoded = decoder.read_image().map_err(|e| e.to_string())?;
    let data = match decoded {
        DecodingResult::U8(v) => v,
        DecodingResult::U16(v) => v.into_iter().map(|v| (v >> 8) as u8).collect(),
        _ => return Err("暂不支持这种 TIFF 像素格式".into()),
    };
    let mut rgba = Vec::with_capacity((w as usize) * (h as usize) * 4);
    match kind {
        ColorType::RGB(_) => {
            for pixel in data.as_chunks::<3>().0 {
                rgba.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255]);
            }
        }
        ColorType::RGBA(_) => rgba = data,
        ColorType::Gray(_) => {
            for v in data {
                rgba.extend_from_slice(&[v, v, v, 255]);
            }
        }
        ColorType::GrayA(_) => {
            for p in data.as_chunks::<2>().0 {
                rgba.extend_from_slice(&[p[0], p[0], p[0], p[1]]);
            }
        }
        ColorType::CMYK(_) => {
            for p in data.as_chunks::<4>().0 {
                rgba.extend_from_slice(&[
                    (255 - p[0]).saturating_sub(p[3]),
                    (255 - p[1]).saturating_sub(p[3]),
                    (255 - p[2]).saturating_sub(p[3]),
                    255,
                ]);
            }
        }
        _ => return Err("暂不支持这种 TIFF 色彩格式".into()),
    }
    let image = image::RgbaImage::from_raw(w, h, rgba).ok_or("TIFF 像素数据不完整")?;
    png(image::DynamicImage::ImageRgba8(image))
}

pub fn raster(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let mut reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(64 * 1024 * 1024);
    limits.max_image_width = Some(8000);
    limits.max_image_height = Some(8000);
    reader.limits(limits);
    let image = reader.decode().map_err(|e| e.to_string())?;
    if u64::from(image.width()) * u64::from(image.height()) > PIXEL_LIMIT {
        return Err("图片超过 1600 万像素".into());
    }
    png(image)
}
fn png(image: image::DynamicImage) -> Result<Vec<u8>, String> {
    let mut bytes = Cursor::new(Vec::new());
    image
        .thumbnail(2400, 2400)
        .write_to(&mut bytes, image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    Ok(bytes.into_inner())
}

pub fn svg(bytes: &[u8]) -> Result<Vec<u8>, String> {
    svg_sized(bytes, 1200., 1600.)
}
pub fn thumbnail(bytes: &[u8]) -> Result<Vec<u8>, String> {
    svg_sized(bytes, 160., 100.)
}
fn svg_sized(bytes: &[u8], width: f32, height: f32) -> Result<Vec<u8>, String> {
    static FONTS: OnceLock<Arc<resvg::usvg::fontdb::Database>> = OnceLock::new();
    let fonts = FONTS.get_or_init(|| {
        let mut fonts = resvg::usvg::fontdb::Database::new();
        fonts.load_system_fonts();
        Arc::new(fonts)
    });
    let options = resvg::usvg::Options {
        fontdb: fonts.clone(),
        image_href_resolver: resvg::usvg::ImageHrefResolver {
            resolve_string: Box::new(|_, _| None),
            ..Default::default()
        },
        ..Default::default()
    };
    let tree =
        resvg::usvg::Tree::from_data(bytes, &options).map_err(|e| format!("SVG 格式错误：{e}"))?;
    let size = tree.size();
    let scale = (width / size.width()).min(height / size.height()).min(2.);
    let width = (size.width() * scale).ceil().max(1.) as u32;
    let height = (size.height() * scale).ceil().max(1.) as u32;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(width, height).ok_or("图片尺寸无效")?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    pixmap.encode_png().map_err(|e| e.to_string())
}

fn part(zip: &mut zip::ZipArchive<Cursor<&[u8]>>, path: &str) -> Result<Vec<u8>, String> {
    let file = zip
        .by_name(path)
        .map_err(|e| format!("缺少文档部件 {path}：{e}"))?;
    if file.size() > PART_LIMIT {
        return Err("文档部件超过 8 MiB".into());
    }
    let mut bytes = Vec::new();
    file.take(PART_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > PART_LIMIT {
        return Err("文档解压内容过大".into());
    }
    Ok(bytes)
}
fn xml_part(zip: &mut zip::ZipArchive<Cursor<&[u8]>>, path: &str) -> Result<Xml, String> {
    xml(std::str::from_utf8(&part(zip, path)?).map_err(|e| e.to_string())?)
}
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
fn markdown_escape(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('*', "\\*")
        .replace('_', "\\_")
        .replace('[', "\\[")
        .replace(']', "\\]")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub fn open(name: &str, bytes: &[u8]) -> Result<DocumentPreview, String> {
    let ext = name.rsplit('.').next().unwrap_or("").to_lowercase();
    if ext == "rtf" {
        return super::rtf::open(bytes);
    }
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|e| format!("Office 文档格式错误：{e}"))?;
    if ext == "pptx" {
        return slides(&mut zip);
    }
    let odt = ext == "odt";
    let doc = xml_part(
        &mut zip,
        if odt {
            "content.xml"
        } else {
            "word/document.xml"
        },
    )?;
    let mut markdown = String::new();
    let body = doc
        .all(if odt { "text" } else { "body" })
        .next()
        .ok_or("文档正文不存在")?;
    document_blocks(body, &mut markdown);
    let mut images = Vec::new();
    let mut image_bytes = 0;
    let paths = zip
        .file_names()
        .filter(|name| name.starts_with(if odt { "Pictures/" } else { "word/media/" }))
        .map(str::to_owned)
        .take(32)
        .collect::<Vec<_>>();
    for path in paths {
        if let Ok(bytes) = part(&mut zip, &path) {
            if image_bytes + bytes.len() > 16 * 1024 * 1024 {
                break;
            }
            image_bytes += bytes.len();
            if let Ok(image) = if path.to_lowercase().ends_with(".svg") {
                svg(&bytes)
            } else {
                raster(&bytes)
            } {
                images.push(image);
            }
        }
    }
    let truncated = markdown.len() > super::TEXT_LIMIT as usize;
    if truncated {
        let mut end = super::TEXT_LIMIT as usize;
        while !markdown.is_char_boundary(end) {
            end -= 1;
        }
        markdown.truncate(end);
    }
    Ok(DocumentPreview {
        pages: vec![DocumentPage { markdown, images }],
        slides: false,
        truncated,
    })
}

fn document_blocks(node: &Xml, markdown: &mut String) {
    for child in &node.children {
        match child.name.as_str() {
            "p" | "h" => {
                let heading = if child.name == "h" {
                    child
                        .attrs
                        .get("outline-level")
                        .and_then(|v| v.parse::<usize>().ok())
                } else {
                    child
                        .all("pStyle")
                        .next()
                        .and_then(|s| s.attrs.get("val"))
                        .and_then(|s| s.strip_prefix("Heading"))
                        .and_then(|s| s.parse::<usize>().ok())
                };
                if let Some(level) = heading {
                    markdown.push_str(&"#".repeat(level.clamp(1, 6)));
                    markdown.push(' ');
                }
                markdown.push_str(&markdown_escape(&child.plain()));
                markdown.push_str("\n\n");
            }
            "tbl" | "table" => {
                let rows = child
                    .all(if child.name == "tbl" {
                        "tr"
                    } else {
                        "table-row"
                    })
                    .take(200)
                    .collect::<Vec<_>>();
                for (index, row) in rows.iter().enumerate() {
                    let cells = row
                        .children
                        .iter()
                        .filter(|c| c.name == "tc" || c.name == "table-cell")
                        .take(32)
                        .map(|c| {
                            markdown_escape(&c.plain())
                                .replace('|', "\\|")
                                .replace('\n', " ")
                        })
                        .collect::<Vec<_>>();
                    markdown.push_str(&format!("| {} |\n", cells.join(" | ")));
                    if index == 0 {
                        markdown
                            .push_str(&format!("| {} |\n", vec!["---"; cells.len()].join(" | ")));
                    }
                }
                markdown.push('\n');
            }
            _ => document_blocks(child, markdown),
        }
    }
}

fn slides(zip: &mut zip::ZipArchive<Cursor<&[u8]>>) -> Result<DocumentPreview, String> {
    let presentation = xml_part(zip, "ppt/presentation.xml")?;
    let size = presentation.all("sldSz").next();
    let number = |node: Option<&Xml>, key: &str, default: f32| {
        node.and_then(|n| n.attrs.get(key))
            .and_then(|v| v.parse::<f32>().ok())
            .filter(|n| n.is_finite() && *n > 0.)
            .unwrap_or(default)
    };
    let width = number(size, "cx", 12192000.);
    let height = number(size, "cy", 6858000.);
    let mut paths = zip
        .file_names()
        .filter(|name| {
            name.starts_with("ppt/slides/slide")
                && name.ends_with(".xml")
                && !name.contains("/_rels/")
        })
        .map(str::to_owned)
        .collect::<Vec<_>>();
    paths.sort_by_key(|path| {
        path.trim_start_matches("ppt/slides/slide")
            .trim_end_matches(".xml")
            .parse::<usize>()
            .unwrap_or(usize::MAX)
    });
    let truncated = paths.len() > PAGE_LIMIT;
    let mut pages = Vec::new();
    for path in paths.into_iter().take(PAGE_LIMIT) {
        let doc = xml_part(zip, &path)?;
        let rel_path = path.replace("ppt/slides/", "ppt/slides/_rels/") + ".rels";
        let relationships = xml_part(zip, &rel_path).ok();
        let mut output = format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"1200\" height=\"{}\" viewBox=\"0 0 1200 {}\"><rect width=\"100%\" height=\"100%\" fill=\"white\"/>",
            1200. * height / width,
            1200. * height / width
        );
        let mut markdown = String::new();
        let scale = 1200. / width;
        for shape in doc.all("sp").chain(doc.all("pic")) {
            let transform = shape.all("xfrm").next();
            let off = transform.and_then(|n| n.all("off").next());
            let ext = transform.and_then(|n| n.all("ext").next());
            let coordinate = |n: Option<&Xml>, key: &str| {
                n.and_then(|n| n.attrs.get(key))
                    .and_then(|v| v.parse::<f32>().ok())
                    .filter(|v| v.is_finite())
                    .unwrap_or(0.)
                    * scale
            };
            let x = coordinate(off, "x");
            let y = coordinate(off, "y");
            let w = number(ext, "cx", width) * scale;
            let h = number(ext, "cy", height) * scale;
            if let Some(fill) = shape
                .children
                .iter()
                .find(|c| c.name == "spPr")
                .and_then(|n| n.children.iter().find(|c| c.name == "solidFill"))
                .and_then(|n| n.all("srgbClr").next())
                .and_then(|n| n.attrs.get("val"))
                .filter(|s| s.len() == 6 && s.chars().all(|c| c.is_ascii_hexdigit()))
            {
                output.push_str(&format!(
                    "<rect x=\"{x}\" y=\"{y}\" width=\"{w}\" height=\"{h}\" fill=\"#{fill}\"/>"
                ));
            }
            if shape.name == "pic" {
                let id = shape.all("blip").next().and_then(|n| n.attrs.get("embed"));
                let target = id.and_then(|id| {
                    relationships
                        .as_ref()?
                        .all("Relationship")
                        .find(|r| {
                            r.attrs.get("Id") == Some(id) && !r.attrs.contains_key("TargetMode")
                        })?
                        .attrs
                        .get("Target")
                });
                if let Some(target) = target.and_then(|t| t.strip_prefix("../media/"))
                    && !target.contains('/')
                    && let Ok(bytes) = part(zip, &format!("ppt/media/{target}"))
                    && let Ok(png) = raster(&bytes)
                {
                    use base64::Engine;
                    output.push_str(&format!("<image x=\"{x}\" y=\"{y}\" width=\"{w}\" height=\"{h}\" href=\"data:image/png;base64,{}\"/>", base64::engine::general_purpose::STANDARD.encode(png)));
                }
            }
            let mut baseline = y;
            for paragraph in shape.all("p") {
                let font = paragraph
                    .all("rPr")
                    .next()
                    .and_then(|p| p.attrs.get("sz"))
                    .and_then(|v| v.parse::<f32>().ok())
                    .unwrap_or(1800.)
                    / 100.
                    * 1.25;
                baseline += font * 1.25;
                let text = paragraph.all("t").map(Xml::plain).collect::<String>();
                if text.is_empty() {
                    continue;
                }
                output.push_str(&format!("<text x=\"{x}\" y=\"{baseline}\" font-family=\"sans-serif\" font-size=\"{}\" fill=\"#202124\">{}</text>",font.clamp(8.,160.),escape(&text)));
                markdown.push_str(&markdown_escape(&text));
                markdown.push_str("\n\n");
            }
        }
        output.push_str("</svg>");
        // Rasterize only the selected slide in the preview worker.
        pages.push(DocumentPage {
            markdown,
            images: vec![output.into_bytes()],
        });
    }
    if pages.is_empty() {
        return Err("演示文稿没有幻灯片".into());
    }
    Ok(DocumentPreview {
        pages,
        slides: true,
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    fn package(parts: &[(&str, &str)]) -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, text) in parts {
            zip.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(text.as_bytes()).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }
    #[test]
    fn word_and_odt_keep_headings_and_tables() {
        let bytes = package(&[(
            "word/document.xml",
            r#"<w:document xmlns:w="word"><w:body><w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>Title</w:t></w:r></w:p><w:p><w:r><w:t>First</w:t></w:r><w:r><w:t> Second</w:t></w:r></w:p><w:tbl><w:tr><w:tc><w:p><w:r><w:t>Name</w:t></w:r></w:p></w:tc></w:tr><w:tr><w:tc><w:p><w:r><w:t>Hello</w:t></w:r></w:p></w:tc></w:tr></w:tbl></w:body></w:document>"#,
        )]);
        let doc = open("test.docx", &bytes).unwrap();
        let body = &doc.pages[0].markdown;
        assert!(body.contains("# Title"));
        assert!(body.contains("First Second"));
        assert!(body.contains("| Hello |"));
        let bytes = package(&[(
            "content.xml",
            r#"<office:document-content xmlns:office="office" xmlns:text="text"><office:body><office:text><text:h text:outline-level="2">Title</text:h><text:p>A<text:span>B</text:span>C</text:p></office:text></office:body></office:document-content>"#,
        )]);
        let doc = open("test.odt", &bytes).unwrap();
        assert!(doc.pages[0].markdown.contains("## Title"));
        assert!(doc.pages[0].markdown.contains("ABC"));
    }
    #[test]
    fn powerpoint_slide_order_and_page_rendering() {
        let bytes = package(&[
            (
                "ppt/presentation.xml",
                r#"<p:presentation xmlns:p="p"><p:sldSz cx="12192000" cy="6858000"/></p:presentation>"#,
            ),
            (
                "ppt/slides/slide1.xml",
                r#"<p:sld xmlns:p="p" xmlns:a="a"><p:sp><p:spPr><a:xfrm><a:off x="300000" y="300000"/><a:ext cx="8000000" cy="1000000"/></a:xfrm></p:spPr><p:txBody><a:p><a:r><a:t>Hello</a:t></a:r></a:p></p:txBody></p:sp></p:sld>"#,
            ),
        ]);
        let doc = open("test.pptx", &bytes).unwrap();
        assert!(doc.slides);
        assert_eq!(doc.pages.len(), 1);
        assert!(doc.pages[0].markdown.contains("Hello"));
        assert!(
            svg(&doc.pages[0].images[0])
                .unwrap()
                .starts_with(b"\x89PNG")
        );
    }
    #[test]
    fn pdf_and_multipage_tiff_render_each_page() {
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents 5 0 R >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents 6 0 R >>",
            "<< /Length 29 >>\nstream\n1 0 0 rg 0 0 100 100 re f\nendstream",
            "<< /Length 29 >>\nstream\n0 0 1 rg 0 0 100 100 re f\nendstream",
        ];
        let mut pdf = String::from("%PDF-1.4\n");
        let mut offsets = vec![0];
        for (ix, object) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.push_str(&format!("{} 0 obj\n{object}\nendobj\n", ix + 1));
        }
        let start = pdf.len();
        pdf.push_str("xref\n0 7\n0000000000 65535 f \n");
        for offset in &offsets[1..] {
            pdf.push_str(&format!("{offset:010} 00000 n \n"));
        }
        pdf.push_str(&format!(
            "trailer\n<< /Size 7 /Root 1 0 R >>\nstartxref\n{start}\n%%EOF\n"
        ));
        let pages = PagedImage::open(pdf.into_bytes(), false).unwrap();
        assert_eq!(pages.count, 2);
        assert_ne!(pages.render(0, 1.).unwrap(), pages.render(1, 1.).unwrap());
        assert!(pages.render(2, 1.).is_err());
        let mut bytes = Cursor::new(Vec::new());
        {
            let mut encoder = tiff::encoder::TiffEncoder::new(&mut bytes).unwrap();
            encoder
                .write_image::<tiff::encoder::colortype::RGB8>(1, 1, &[255, 0, 0])
                .unwrap();
            encoder
                .write_image::<tiff::encoder::colortype::RGB8>(1, 1, &[0, 0, 255])
                .unwrap();
        }
        let pages = PagedImage::open(bytes.into_inner(), true).unwrap();
        assert_eq!(pages.count, 2);
        assert_ne!(pages.render(0, 1.).unwrap(), pages.render(1, 1.).unwrap());
    }
    #[test]
    fn svg_does_not_resolve_local_files() {
        let png = svg(br#"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="10"><rect width="20" height="10" fill="red"/><image href="file:///etc/passwd"/></svg>"#).unwrap();
        assert!(png.starts_with(b"\x89PNG"));
    }
    #[test]
    fn invalid_documents_are_errors() {
        assert!(PagedImage::open(b"hello".to_vec(), false).is_err());
        assert!(open("bad.docx", b"hello").is_err());
        assert!(open("bad.rtf", b"hello").is_err());
    }
    #[test]
    fn rtf_preserves_bold_text() {
        let doc = open("a.rtf", br"{\rtf1\ansi Hello {\b World}}").unwrap();
        assert!(doc.pages[0].markdown.contains("**World**"));
    }
}
