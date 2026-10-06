use quick_xml::events::Event;
use serde_json::Value;

#[derive(Clone, Debug)]
pub struct Node {
    pub label: String,
    pub children: Vec<Node>,
}

#[derive(Clone, Debug)]
pub struct StructuredPreview {
    pub root: Node,
    pub formatted: String,
    pub language: String,
    pub truncated: bool,
}

pub fn parse(name: &str, bytes: &[u8]) -> Result<StructuredPreview, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("文本不是 UTF-8：{e}"))?;
    let ext = name.rsplit('.').next().unwrap_or("").to_lowercase();
    let value = match ext.as_str() {
        "yaml" | "yml" => serde_json::to_value(
            serde_yaml::from_str::<serde_yaml::Value>(text).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?,
        "toml" => {
            serde_json::to_value(toml::from_str::<toml::Value>(text).map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?
        }
        "xml" => {
            let xml = xml(text)?;
            let mut remaining = super::TREE_ENTRY_LIMIT;
            let root = xml_node(&xml, 0, &mut remaining);
            return Ok(StructuredPreview {
                root,
                formatted: text.to_owned(),
                language: ext,
                truncated: remaining == 0,
            });
        }
        _ => serde_json::from_str(text).map_err(|e| e.to_string())?,
    };
    let mut remaining = super::TREE_ENTRY_LIMIT;
    let root = json_node("root", &value, 0, &mut remaining);
    let formatted = match ext.as_str() {
        "yaml" | "yml" => serde_yaml::to_string(&value).map_err(|e| e.to_string())?,
        "toml" => {
            toml::to_string_pretty(&toml::from_str::<toml::Value>(text).map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?
        }
        _ => serde_json::to_string_pretty(&value).map_err(|e| e.to_string())?,
    };
    Ok(StructuredPreview {
        root,
        formatted,
        language: ext,
        truncated: remaining == 0,
    })
}

fn json_node(key: &str, value: &Value, depth: usize, remaining: &mut usize) -> Node {
    *remaining = remaining.saturating_sub(1);
    let (label, children) = match value {
        Value::Object(values) => (
            format!("{key} {{ {} }}", values.len()),
            if depth < 32 {
                values
                    .iter()
                    .take(*remaining)
                    .map(|(k, v)| (k.to_owned(), v))
                    .collect()
            } else {
                Vec::new()
            },
        ),
        Value::Array(values) => (
            format!("{key} [ {} ]", values.len()),
            if depth < 32 {
                values
                    .iter()
                    .enumerate()
                    .take(*remaining)
                    .map(|(i, v)| (i.to_string(), v))
                    .collect()
            } else {
                Vec::new()
            },
        ),
        _ => (
            format!(
                "{key}: {}",
                value.to_string().chars().take(1024).collect::<String>()
            ),
            Vec::new(),
        ),
    };
    let mut nodes = Vec::new();
    for (key, value) in children {
        if *remaining == 0 {
            break;
        }
        nodes.push(json_node(&key, value, depth + 1, remaining));
    }
    Node {
        label,
        children: nodes,
    }
}

#[derive(Default, Debug)]
pub(crate) struct Xml {
    pub name: String,
    pub attrs: std::collections::BTreeMap<String, String>,
    pub children: Vec<Xml>,
    pub text: String,
    content: String,
}

impl Xml {
    pub fn all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Xml> {
        std::iter::once(self)
            .filter(move |node| node.name == name)
            .chain(
                self.children
                    .iter()
                    .flat_map(move |c| Box::new(c.all(name)) as Box<dyn Iterator<Item = &'a Xml>>),
            )
    }
    pub fn plain(&self) -> String {
        if self.name == "tab" {
            return "\t".into();
        }
        if self.name == "br" || self.name == "line-break" {
            return "\n".into();
        }
        self.content.clone()
    }
}

pub(crate) fn xml(text: &str) -> Result<Xml, String> {
    let mut reader = quick_xml::Reader::from_str(text);
    let mut stack = vec![Xml::default()];
    let mut count = 0;
    loop {
        match reader
            .read_event()
            .map_err(|e| format!("XML 格式错误：{e}"))?
        {
            Event::Start(event) | Event::Empty(event) => {
                count += 1;
                if count > 100_000 || stack.len() > 64 {
                    return Err("XML 节点或深度超过预览上限".into());
                }
                let mut node = Xml {
                    name: String::from_utf8_lossy(event.local_name().as_ref()).into_owned(),
                    ..Default::default()
                };
                for attr in event.attributes() {
                    let attr = attr.map_err(|e| e.to_string())?;
                    node.attrs.insert(
                        String::from_utf8_lossy(attr.key.local_name().as_ref()).into_owned(),
                        attr.decode_and_unescape_value(reader.decoder())
                            .map_err(|e| e.to_string())?
                            .into_owned(),
                    );
                }
                // Empty elements do not put a matching closing event in the stream.
                if text.as_bytes().get(reader.buffer_position() as usize - 2) == Some(&b'/') {
                    stack.last_mut().unwrap().content.push_str(&node.plain());
                    stack.last_mut().unwrap().children.push(node);
                } else {
                    stack.push(node);
                }
            }
            Event::End(_) => {
                if stack.len() < 2 {
                    return Err("XML 标签不匹配".into());
                }
                let node = stack.pop().unwrap();
                stack.last_mut().unwrap().content.push_str(&node.plain());
                stack.last_mut().unwrap().children.push(node);
            }
            Event::Text(event) => {
                let text = event.xml_content().map_err(|e| e.to_string())?;
                let node = stack.last_mut().unwrap();
                node.text.push_str(&text);
                node.content.push_str(&text);
            }
            Event::CData(event) => {
                let text = event.decode().map_err(|e| e.to_string())?;
                let node = stack.last_mut().unwrap();
                node.text.push_str(&text);
                node.content.push_str(&text);
            }
            Event::GeneralRef(event) => {
                let reference = event.decode().map_err(|e| e.to_string())?;
                let decoded = quick_xml::escape::unescape(&format!("&{reference};"))
                    .map_err(|e| e.to_string())?
                    .into_owned();
                stack.last_mut().unwrap().text.push_str(&decoded);
                stack.last_mut().unwrap().content.push_str(&decoded);
            }
            Event::DocType(_) => return Err("预览不解析 XML DTD".into()),
            Event::Eof => break,
            _ => {}
        }
    }
    if stack.len() != 1 {
        return Err("XML 标签未闭合".into());
    }
    Ok(stack.pop().unwrap())
}

fn xml_node(xml: &Xml, depth: usize, remaining: &mut usize) -> Node {
    *remaining = remaining.saturating_sub(1);
    let label = format!(
        "{}{}{}",
        if xml.name.is_empty() {
            "XML"
        } else {
            &xml.name
        },
        xml.attrs
            .iter()
            .map(|(k, v)| format!(" {k}=\"{v}\""))
            .collect::<String>(),
        if xml.text.trim().is_empty() {
            String::new()
        } else {
            format!(
                ": {}",
                xml.text.trim().chars().take(1024).collect::<String>()
            )
        }
    );
    let mut children = Vec::new();
    if depth < 32 {
        for child in &xml.children {
            if *remaining == 0 {
                break;
            }
            children.push(xml_node(child, depth + 1, remaining));
        }
    }
    Node { label, children }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn handles_entities_and_empty_elements() {
        let root = xml("<root><empty/><p>A &amp; B</p></root>").unwrap();
        assert_eq!(root.children[0].children[1].plain(), "A & B");
        assert!(xml("<!DOCTYPE root><root/>").is_err());
    }
    #[test]
    fn structured_formats_have_children() {
        for (name, body) in [
            ("a.json", "{\"a\":[1,2]}"),
            ("a.yaml", "a: [1, 2]"),
            ("a.toml", "a = [1, 2]"),
        ] {
            assert_eq!(parse(name, body.as_bytes()).unwrap().root.children.len(), 1);
        }
    }
}
