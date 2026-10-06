//! Iterative RTF content reader: bounded nesting, Unicode/ANSI text, basic
//! emphasis, paragraphs, table cells and PNG/JPEG picture destinations.
use super::documents::{DocumentPage, DocumentPreview, raster};
#[derive(Clone, Copy, Default)]
struct Style {
    bold: bool,
    italic: bool,
    skip: bool,
    uc: usize,
}
pub(super) fn open(bytes: &[u8]) -> Result<DocumentPreview, String> {
    if !bytes.starts_with(b"{\\rtf") {
        return Err("文件不是 RTF 文档".into());
    }
    let mut stack = Vec::new();
    let mut style = Style {
        uc: 1,
        ..Default::default()
    };
    let mut output = String::new();
    let mut images = Vec::new();
    let mut image_bytes = 0;
    let mut ix = 0;
    let mut fallback = 0;
    let mut high = None;
    let mut truncated = false;
    let mut active = "";
    while ix < bytes.len() {
        if output.len() > super::TEXT_LIMIT as usize {
            truncated = true;
            break;
        }
        let mut text = String::new();
        match bytes[ix] {
            b'{' => {
                if stack.len() >= 64 {
                    return Err("RTF 嵌套超过预览上限".into());
                }
                stack.push(style);
                ix += 1;
            }
            b'}' => {
                style = stack.pop().ok_or("RTF 括号不匹配")?;
                ix += 1;
            }
            b'\\' => {
                ix += 1;
                if ix == bytes.len() {
                    return Err("RTF 转义不完整".into());
                }
                match bytes[ix] {
                    b'\\' | b'{' | b'}' => {
                        if fallback > 0 {
                            fallback -= 1;
                        } else {
                            text.push(bytes[ix] as char);
                        }
                        ix += 1;
                    }
                    b'\'' => {
                        let hex = bytes.get(ix + 1..ix + 3).ok_or("RTF 字符编码不完整")?;
                        let value = u8::from_str_radix(
                            std::str::from_utf8(hex).map_err(|e| e.to_string())?,
                            16,
                        )
                        .map_err(|e| e.to_string())?;
                        if fallback > 0 {
                            fallback -= 1;
                        } else {
                            text.push(ansi(value));
                        }
                        ix += 3;
                    }
                    b'*' => {
                        style.skip = true;
                        ix += 1;
                    }
                    b'~' => {
                        text.push('\u{a0}');
                        ix += 1;
                    }
                    b'_' => {
                        text.push('\u{2011}');
                        ix += 1;
                    }
                    b'a'..=b'z' => {
                        let start = ix;
                        while ix < bytes.len() && bytes[ix].is_ascii_alphabetic() {
                            ix += 1;
                        }
                        let word = std::str::from_utf8(&bytes[start..ix]).unwrap();
                        let number_start = ix;
                        if bytes.get(ix) == Some(&b'-') {
                            ix += 1;
                        }
                        while ix < bytes.len() && bytes[ix].is_ascii_digit() {
                            ix += 1;
                        }
                        let number = std::str::from_utf8(&bytes[number_start..ix])
                            .ok()
                            .and_then(|n| n.parse::<i32>().ok());
                        if bytes.get(ix) == Some(&b' ') {
                            ix += 1;
                        }
                        match word {
                            "b" => style.bold = number != Some(0),
                            "i" => style.italic = number != Some(0),
                            "plain" => {
                                style.bold = false;
                                style.italic = false;
                            }
                            "par" | "row" => text.push_str("\n\n"),
                            "line" => text.push('\n'),
                            "tab" => text.push('\t'),
                            "cell" => text.push_str(" | "),
                            "uc" => style.uc = number.unwrap_or(1).clamp(0, 16) as usize,
                            "u" => {
                                let unit = number.unwrap_or(0) as u16;
                                if (0xd800..=0xdbff).contains(&unit) {
                                    high = Some(unit);
                                } else {
                                    let character = if let Some(first) = high.take() {
                                        char::from_u32(
                                            0x10000
                                                + (((u32::from(first) - 0xd800) << 10)
                                                    | (u32::from(unit).saturating_sub(0xdc00))),
                                        )
                                    } else {
                                        char::from_u32(u32::from(unit))
                                    };
                                    text.push(character.unwrap_or('\u{fffd}'));
                                }
                                fallback = style.uc;
                            }
                            "fonttbl" | "colortbl" | "stylesheet" | "info" | "object"
                            | "objdata" | "header" | "footer" | "listtable"
                            | "listoverridetable" => style.skip = true,
                            "pict" => {
                                let start = ix;
                                let mut depth = 1;
                                let mut end = ix;
                                while end < bytes.len() {
                                    match bytes[end] {
                                        b'{' => depth += 1,
                                        b'}' => {
                                            depth -= 1;
                                            if depth == 0 {
                                                break;
                                            }
                                        }
                                        _ => {}
                                    }
                                    end += 1;
                                }
                                if end == bytes.len() {
                                    return Err("RTF 图片数据未闭合".into());
                                }
                                if let Ok(picture) = picture(&bytes[start..end]) {
                                    if images.len() >= 32
                                        || image_bytes + picture.len() > 16 * 1024 * 1024
                                    {
                                        truncated = true;
                                    } else {
                                        image_bytes += picture.len();
                                        if let Ok(png) = raster(&picture) {
                                            images.push(png);
                                        }
                                    }
                                }
                                ix = end;
                            }
                            "bin" => {
                                let length = number.unwrap_or(0).max(0) as usize;
                                ix = ix
                                    .checked_add(length)
                                    .filter(|n| *n <= bytes.len())
                                    .ok_or("RTF 二进制内容不完整")?;
                            }
                            _ => {}
                        }
                    }
                    _ => ix += 1,
                }
            }
            b'\r' | b'\n' => ix += 1,
            value => {
                if fallback > 0 {
                    fallback -= 1;
                } else {
                    text.push(ansi(value));
                }
                ix += 1;
            }
        }
        if !style.skip && !text.is_empty() {
            let marker = if style.bold && style.italic {
                "***"
            } else if style.bold {
                "**"
            } else if style.italic {
                "*"
            } else {
                ""
            };
            if marker != active {
                output.push_str(active);
                output.push_str(marker);
                active = marker;
            }
            for ch in text.chars() {
                if ['*', '_', '[', ']', '\\'].contains(&ch) {
                    output.push('\\');
                }
                if ch == '<' {
                    output.push_str("&lt;");
                } else if ch == '>' {
                    output.push_str("&gt;");
                } else {
                    output.push(ch);
                }
            }
        }
    }
    output.push_str(active);
    if !truncated && !stack.is_empty() {
        return Err("RTF 括号未闭合".into());
    }
    Ok(DocumentPreview {
        pages: vec![DocumentPage {
            markdown: output,
            images,
        }],
        slides: false,
        truncated,
    })
}
fn ansi(value: u8) -> char {
    const SPECIAL: [char; 32] = [
        '€', '\u{fffd}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{fffd}', 'Ž',
        '\u{fffd}', '\u{fffd}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ',
        '\u{fffd}', 'ž', 'Ÿ',
    ];
    if (0x80..0xa0).contains(&value) {
        SPECIAL[(value - 0x80) as usize]
    } else {
        value as char
    }
}
fn picture(bytes: &[u8]) -> Result<Vec<u8>, String> {
    if !bytes.windows(8).any(|w| w == b"\\pngblip") && !bytes.windows(9).any(|w| w == b"\\jpegblip")
    {
        return Err("不支持的 RTF 图片格式".into());
    }
    let mut output = Vec::new();
    let mut high = None;
    let mut ix = 0;
    let mut nested = 0;
    while ix < bytes.len() {
        match bytes[ix] {
            b'{' => {
                nested += 1;
                ix += 1;
            }
            b'}' => {
                nested -= 1;
                ix += 1;
            }
            b'\\' => {
                ix += 1;
                while ix < bytes.len()
                    && !bytes[ix].is_ascii_whitespace()
                    && !matches!(bytes[ix], b'{' | b'}' | b'\\')
                {
                    ix += 1;
                }
            }
            value if nested == 0 && value.is_ascii_hexdigit() => {
                let value = (value as char).to_digit(16).unwrap() as u8;
                if let Some(h) = high.take() {
                    output.push(h * 16 + value);
                } else {
                    high = Some(value);
                }
                ix += 1;
            }
            _ => ix += 1,
        }
        if output.len() > super::IMAGE_LIMIT as usize {
            return Err("RTF 图片超过 8 MiB".into());
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unicode_paragraphs_and_destinations() {
        let doc = open(
            br"{\rtf1\ansi{\fonttbl{\f0 ignored;}}Hello {\b World}\par \uc1\u-10179?\u-8704?}",
        )
        .unwrap();
        assert_eq!(doc.pages[0].markdown, "Hello **World**\n\n😀");
        assert!(open(br"{\rtf1 Hello").is_err());
    }
    #[test]
    fn png_picture_destination_is_decoded() {
        let png=super::super::documents::svg(br#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><rect width="1" height="1" fill="red"/></svg>"#).unwrap();
        let hex = png.iter().map(|b| format!("{b:02x}")).collect::<String>();
        let doc = open(format!("{{\\rtf1 Text {{\\pict\\pngblip\n{hex}}}}}").as_bytes()).unwrap();
        assert_eq!(doc.pages[0].images.len(), 1);
        assert_eq!(doc.pages[0].markdown, "Text ");
    }
}
