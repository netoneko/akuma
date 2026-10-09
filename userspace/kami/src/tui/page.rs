//! A `DOMSnapshot.captureSnapshot` reply, reduced to what a terminal can paint.
//!
//! The snapshot is Chromium's own layout: every layout object with its box in
//! document coordinates (CSS px, scroll included, `position:fixed` ones too),
//! one entry per line box of text, the computed styles asked for in
//! [`SNAPSHOT_STYLES`], and the paint order. This module keeps the visible
//! text fragments, form fields and images as [`Item`]s and the background
//! boxes as [`Fill`]s, each tagged with the [`Layer`] it scrolls with. It does
//! no geometry of its own; `grid.rs` turns px into cells.
//!
//! Only the top document is read: same-origin iframes come as further
//! documents whose boxes are in their own coordinates, and are not placed yet.

use serde_json::Value;

// Positions in `machine::SNAPSHOT_STYLES` (checked by a test).
const COLOR: usize = 0;
const BACKGROUND: usize = 1;
const WEIGHT: usize = 2;
const STYLE: usize = 3;
const DECORATION: usize = 4;
const VISIBILITY: usize = 5;
const OPACITY: usize = 6;
const POSITION: usize = 7;
const FONT_SIZE: usize = 8;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// What an item scrolls with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layer {
    /// The document: scrolls with the view.
    Flow,
    /// `position:fixed` (or inside one): stays put on the screen.
    Fixed,
    /// kami's own link-hint labels (`hints.js`, `#__kami_hints`): fixed in the
    /// page, but drawn on the rows of the things they label.
    Hint,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Text,
    /// An `<img>`/`<video>`; `text` is its alt text.
    Image,
    /// A text input, textarea or select; `text` is its value (or placeholder).
    Field,
    /// A checkbox or radio button, already drawn (`[x]`, `( )`).
    Check,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub rect: Rect,
    pub text: String,
    pub kind: Kind,
    pub layer: Layer,
    pub fg: Option<Rgb>,
    /// Only set for hint labels: the label's own background.
    pub bg: Option<Rgb>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    /// Inside a link (`<a href>`).
    pub link: bool,
    /// Placeholder text, not a value.
    pub dim: bool,
    /// Font size in CSS px.
    pub size: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Fill {
    pub rect: Rect,
    pub bg: Rgb,
    /// 0..=1.
    pub alpha: f64,
    pub layer: Layer,
    /// Chromium's paint order: later fills paint over earlier ones.
    pub order: i64,
}

#[derive(Clone, Debug, Default)]
pub struct Page {
    /// The page's vertical scroll offset when the snapshot was taken (CSS px).
    pub scroll_y: f64,
    /// The canvas colour: the root element's background, else the body's
    /// (CSS propagates either to the whole canvas). `None` is the browser's
    /// default, white.
    pub canvas: Option<Rgb>,
    pub items: Vec<Item>,
    pub fills: Vec<Fill>,
}

/// `rgb(…)`/`rgba(…)` as Chromium serializes computed colours. Anything else
/// (`color(srgb …)`, `oklch(…)`, an empty string) is "no colour".
pub fn parse_color(s: &str) -> Option<(Rgb, f64)> {
    let inner = s.strip_prefix("rgba(").or_else(|| s.strip_prefix("rgb("))?.strip_suffix(')')?;
    let mut it = inner.split(',').map(|p| p.trim().parse::<f64>());
    let r = it.next()?.ok()?;
    let g = it.next()?.ok()?;
    let b = it.next()?.ok()?;
    let a = match it.next() {
        Some(a) => a.ok()?,
        None => 1.0,
    };
    let c = |v: f64| v.clamp(0.0, 255.0).round() as u8;
    Some((Rgb(c(r), c(g), c(b)), a.clamp(0.0, 1.0)))
}

fn ints(v: &Value) -> Vec<i64> {
    v.as_array().map(|a| a.iter().map(|x| x.as_i64().unwrap_or(-1)).collect()).unwrap_or_default()
}

fn rect(v: &Value) -> Rect {
    let n = |i: usize| v.get(i).and_then(Value::as_f64).unwrap_or(0.0);
    Rect { x: n(0), y: n(1), w: n(2), h: n(3) }
}

/// `RareStringData` / `RareIntegerData`: `{index:[node…], value:[…]}`, as a
/// per-node table.
fn rare_values(v: &Value, n: usize) -> Vec<i64> {
    let mut out = vec![-1; n];
    for (i, val) in ints(&v["index"]).into_iter().zip(ints(&v["value"])) {
        if let Some(slot) = usize::try_from(i).ok().and_then(|i| out.get_mut(i)) {
            *slot = val;
        }
    }
    out
}

/// `RareBooleanData`: `{index:[node…]}`, the nodes for which it is true.
fn rare_flags(v: &Value, n: usize) -> Vec<bool> {
    let mut out = vec![false; n];
    for i in ints(&v["index"]) {
        if let Some(slot) = usize::try_from(i).ok().and_then(|i| out.get_mut(i)) {
            *slot = true;
        }
    }
    out
}

/// The substring of `s` from UTF-16 offset `start`, `len` code units long
/// (Blink counts text-box offsets in UTF-16).
fn utf16_slice(s: &str, start: usize, len: usize) -> &str {
    let (mut pos, mut from, mut to) = (0usize, None, s.len());
    for (b, ch) in s.char_indices() {
        if from.is_none() && pos >= start {
            from = Some(b);
        }
        if pos >= start + len {
            to = b;
            break;
        }
        pos += ch.len_utf16();
    }
    match from {
        Some(f) if f <= to => &s[f..to],
        _ => "",
    }
}

pub fn parse(reply: &[u8]) -> Result<Page, String> {
    let v: Value = serde_json::from_slice(reply).map_err(|e| format!("snapshot json: {e}"))?;
    let r = &v["result"];
    let strings: Vec<&str> = r["strings"]
        .as_array()
        .ok_or("snapshot without strings")?
        .iter()
        .map(|s| s.as_str().unwrap_or(""))
        .collect();
    let s = |i: i64| usize::try_from(i).ok().and_then(|i| strings.get(i).copied()).unwrap_or("");
    let doc = &r["documents"][0];
    if doc.is_null() {
        return Err("snapshot without documents".into());
    }

    let nodes = &doc["nodes"];
    let parent = ints(&nodes["parentIndex"]);
    let n = parent.len();
    let name: Vec<&str> = ints(&nodes["nodeName"]).into_iter().map(s).collect();
    let node_type = ints(&nodes["nodeType"]);
    let node_value = ints(&nodes["nodeValue"]);
    let attrs: Vec<Vec<i64>> = nodes["attributes"].as_array().map(|a| a.iter().map(ints).collect()).unwrap_or_default();
    let attr = |i: usize, key: &str| -> Option<&str> {
        let a = attrs.get(i)?;
        a.chunks(2).find(|kv| kv.len() == 2 && s(kv[0]).eq_ignore_ascii_case(key)).map(|kv| s(kv[1]))
    };
    let input_value = rare_values(&nodes["inputValue"], n);
    let text_value = rare_values(&nodes["textValue"], n);
    let checked = rare_flags(&nodes["inputChecked"], n);
    let selected = rare_flags(&nodes["optionSelected"], n);

    let layout = &doc["layout"];
    let node_index = ints(&layout["nodeIndex"]);
    let styles: Vec<Vec<i64>> = layout["styles"].as_array().map(|a| a.iter().map(ints).collect()).unwrap_or_default();
    let bounds: Vec<Rect> = layout["bounds"].as_array().map(|a| a.iter().map(rect).collect()).unwrap_or_default();
    let layout_text = ints(&layout["text"]);
    let paint = ints(&layout["paintOrders"]);
    let style = |li: usize, k: usize| styles.get(li).and_then(|st| st.get(k)).map(|&i| s(i)).unwrap_or("");

    // Per node: its first layout object, the layout object whose computed
    // style applies (text nodes have none of their own: their parent's),
    // which layer it is on, whether an ancestor made it invisible, and
    // whether it is inside a link. Nodes come in document order, so a parent
    // is always seen before its children.
    let mut node_layout = vec![None; n];
    for (li, &ni) in node_index.iter().enumerate() {
        if let Some(slot) = usize::try_from(ni).ok().and_then(|i| node_layout.get_mut(i)) {
            slot.get_or_insert(li);
        }
    }
    let mut style_of: Vec<Option<usize>> = vec![None; n];
    let mut layer = vec![Layer::Flow; n];
    let mut gone = vec![false; n];
    let mut link = vec![false; n];
    for i in 0..n {
        let p = usize::try_from(parent[i]).ok().filter(|&p| p < i);
        let own = node_layout[i].filter(|&li| styles.get(li).is_some_and(|st| !st.is_empty()));
        style_of[i] = own.or(p.and_then(|p| style_of[p]));
        layer[i] = p.map_or(Layer::Flow, |p| layer[p]);
        if attr(i, "id") == Some("__kami_hints") {
            layer[i] = Layer::Hint;
        } else if layer[i] == Layer::Flow && own.is_some_and(|li| style(li, POSITION) == "fixed") {
            layer[i] = Layer::Fixed;
        }
        gone[i] = p.is_some_and(|p| gone[p]) || own.is_some_and(|li| style(li, OPACITY) == "0");
        link[i] = p.is_some_and(|p| link[p]) || (name[i] == "A" && attr(i, "href").is_some());
    }

    // The selected option's text, for each <select>.
    let mut select_text: Vec<Option<String>> = vec![None; n];
    for i in 0..n {
        if node_type.get(i) != Some(&3) {
            continue;
        }
        let Some(opt) = usize::try_from(parent[i]).ok().filter(|&p| name[p] == "OPTION" && selected[p]) else { continue };
        let mut up = usize::try_from(parent[opt]).ok();
        while let Some(u) = up {
            if name[u] == "SELECT" {
                select_text[u].get_or_insert_with(|| s(node_value[i]).trim().to_string());
                break;
            }
            up = usize::try_from(parent[u]).ok().filter(|_| name[u] == "OPTGROUP");
        }
    }

    let mut page = Page { scroll_y: doc["scrollOffsetY"].as_f64().unwrap_or(0.0), ..Page::default() };

    let base = |ni: usize, rect: Rect, text: String, kind: Kind| -> Option<Item> {
        let li = style_of[ni]?;
        if gone[ni] || style(li, VISIBILITY) != "visible" {
            return None;
        }
        let weight = style(li, WEIGHT);
        let deco = style(li, DECORATION);
        Some(Item {
            rect,
            text,
            kind,
            layer: layer[ni],
            fg: parse_color(style(li, COLOR)).filter(|c| c.1 > 0.0).map(|c| c.0),
            bg: None,
            bold: weight == "bold" || weight == "bolder" || weight.parse::<u32>().is_ok_and(|w| w >= 600),
            italic: style(li, STYLE) == "italic" || style(li, STYLE) == "oblique",
            underline: deco.contains("underline"),
            link: link[ni],
            dim: false,
            size: style(li, FONT_SIZE).trim_end_matches("px").parse().unwrap_or(16.0),
        })
    };

    // Text: one item per line box.
    let tb = &doc["textBoxes"];
    let tb_layout = ints(&tb["layoutIndex"]);
    let tb_bounds: Vec<Rect> = tb["bounds"].as_array().map(|a| a.iter().map(rect).collect()).unwrap_or_default();
    let tb_start = ints(&tb["start"]);
    let tb_len = ints(&tb["length"]);
    for (k, &li) in tb_layout.iter().enumerate() {
        let Some(li) = usize::try_from(li).ok() else { continue };
        let (Some(&r), Some(&ni)) = (tb_bounds.get(k), node_index.get(li)) else { continue };
        let Ok(ni) = usize::try_from(ni) else { continue };
        if r.w <= 0.0 || r.h <= 0.0 {
            continue;
        }
        let full = s(layout_text.get(li).copied().unwrap_or(-1));
        let start = tb_start.get(k).copied().unwrap_or(0).max(0) as usize;
        let len = tb_len.get(k).copied().unwrap_or(0).max(0) as usize;
        let text = utf16_slice(full, start, len);
        if text.trim().is_empty() && layer[ni] != Layer::Hint {
            continue;
        }
        // Screen-reader-only text: a 1px clipped element whose text overflows
        // it (Wikipedia's "Jump to content", its "Toggle ... subsection"
        // button labels). The element's box, not the text node's: a text
        // node can carry a layout entry and styles of its own.
        let element = usize::try_from(parent[ni]).ok().and_then(|p| node_layout[p]).and_then(|li| bounds.get(li));
        if element.is_some_and(|b| b.w <= 2.0 && b.h <= 2.0) {
            continue;
        }
        let Some(mut item) = base(ni, r, text.to_string(), Kind::Text) else { continue };
        if item.layer == Layer::Hint {
            item.bg = style_of[ni].and_then(|li| parse_color(style(li, BACKGROUND))).filter(|c| c.1 > 0.5).map(|c| c.0);
        }
        page.items.push(item);
    }

    // Replaced and form elements: their content is not in the text boxes.
    for (li, &ni) in node_index.iter().enumerate() {
        let Ok(ni) = usize::try_from(ni) else { continue };
        let Some(&r) = bounds.get(li) else { continue };
        if r.w < 4.0 || r.h < 4.0 {
            continue;
        }
        let item = match name[ni] {
            "IMG" | "VIDEO" if r.w >= 16.0 && r.h >= 16.0 => {
                let alt = attr(ni, "alt").or(attr(ni, "title")).unwrap_or("").trim();
                let what = if name[ni] == "VIDEO" { "video" } else { "img" };
                let label = if alt.is_empty() { format!("[{what}]") } else { format!("[{alt}]") };
                base(ni, r, label, Kind::Image)
            }
            "INPUT" => {
                let ty = attr(ni, "type").unwrap_or("text").to_ascii_lowercase();
                let value = s(input_value[ni]);
                match ty.as_str() {
                    "hidden" | "image" => None,
                    "checkbox" => base(ni, r, (if checked[ni] { "[x]" } else { "[ ]" }).into(), Kind::Check),
                    "radio" => base(ni, r, (if checked[ni] { "(*)" } else { "( )" }).into(), Kind::Check),
                    // A button's label lives in its user-agent shadow tree,
                    // which the snapshot leaves out.
                    "submit" | "button" | "reset" => {
                        let label = attr(ni, "value").filter(|v| !v.is_empty()).unwrap_or(if ty == "reset" { "Reset" } else { "Submit" });
                        base(ni, r, format!("[{label}]"), Kind::Text)
                    }
                    _ => field(base(ni, r, String::new(), Kind::Field), value, attr(ni, "placeholder"), ty == "password"),
                }
            }
            "TEXTAREA" => field(base(ni, r, String::new(), Kind::Field), s(text_value[ni]), attr(ni, "placeholder"), false),
            "SELECT" => {
                let t = select_text[ni].clone().unwrap_or_default();
                base(ni, r, format!("{t} ▾"), Kind::Field)
            }
            _ => None,
        };
        page.items.extend(item);
    }

    // Backgrounds.
    for (li, &ni) in node_index.iter().enumerate() {
        let Ok(ni) = usize::try_from(ni) else { continue };
        if styles.get(li).is_none_or(|st| st.is_empty()) || gone[ni] || style(li, VISIBILITY) != "visible" {
            continue;
        }
        let Some((bg, alpha)) = parse_color(style(li, BACKGROUND)) else { continue };
        let Some(&r) = bounds.get(li) else { continue };
        if alpha < 0.05 || r.w < 1.0 || r.h < 1.0 {
            continue;
        }
        match name[ni] {
            "HTML" if alpha > 0.5 => page.canvas = Some(bg),
            "BODY" if alpha > 0.5 && page.canvas.is_none() => page.canvas = Some(bg),
            _ => {}
        }
        page.fills.push(Fill { rect: r, bg, alpha, layer: layer[ni], order: paint.get(li).copied().unwrap_or(0) });
    }
    Ok(page)
}

/// A text field's item: its value, else its placeholder (dimmed).
fn field(item: Option<Item>, value: &str, placeholder: Option<&str>, password: bool) -> Option<Item> {
    let mut item = item?;
    if !value.is_empty() {
        item.text = if password { "*".repeat(value.chars().count()) } else { value.replace('\n', " ") };
    } else if let Some(p) = placeholder.filter(|p| !p.is_empty()) {
        item.text = p.to_string();
        item.dim = true;
    }
    Some(item)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::machine::SNAPSHOT_STYLES;

    #[test]
    fn style_positions_match_the_request() {
        let want = [
            (COLOR, "color"),
            (BACKGROUND, "background-color"),
            (WEIGHT, "font-weight"),
            (STYLE, "font-style"),
            (DECORATION, "text-decoration-line"),
            (VISIBILITY, "visibility"),
            (OPACITY, "opacity"),
            (POSITION, "position"),
            (FONT_SIZE, "font-size"),
        ];
        for (i, name) in want {
            assert_eq!(SNAPSHOT_STYLES[i], name);
        }
    }

    #[test]
    fn colors() {
        assert_eq!(parse_color("rgb(34, 51, 68)"), Some((Rgb(34, 51, 68), 1.0)));
        assert_eq!(parse_color("rgba(0, 0, 0, 0)"), Some((Rgb(0, 0, 0), 0.0)));
        assert_eq!(parse_color("rgba(255, 0, 0, 0.5)"), Some((Rgb(255, 0, 0), 0.5)));
        assert_eq!(parse_color("color(srgb 1 0 0)"), None);
        assert_eq!(parse_color(""), None);
    }

    #[test]
    fn utf16_offsets() {
        assert_eq!(utf16_slice("hello world", 6, 5), "world");
        assert_eq!(utf16_slice("a😀b", 3, 1), "b", "the emoji is two UTF-16 units");
        assert_eq!(utf16_slice("a😀b", 1, 2), "😀");
        assert_eq!(utf16_slice("abc", 5, 1), "");
    }

    /// The local test page (testdata/tui/test.html) as Chrome on macOS
    /// snapshotted it for `kami tui` in a 100x40 terminal (cells.js on), saved
    /// with KAMI_SNAPSHOT_DUMP after the page's script changed its last line.
    #[test]
    fn reads_the_test_page() {
        let p = parse(include_bytes!("../../testdata/tui/test-snapshot.json")).unwrap();
        let texts: Vec<&str> = p.items.iter().filter(|i| i.kind == Kind::Text).map(|i| i.text.as_str()).collect();
        assert!(texts.contains(&"Hello from the layout tree"));
        // In cells the link wraps: "... with a" / "link in the middle".
        let link = p.items.iter().find(|i| i.text == "link in the middle").unwrap();
        assert!(link.link && link.layer == Layer::Flow);
        let header = p.items.iter().find(|i| i.text == "kami").unwrap();
        assert_eq!(header.layer, Layer::Fixed, "inside the position:fixed header");
        assert!(header.bold);
        let input = p.items.iter().find(|i| i.kind == Kind::Field).unwrap();
        assert_eq!(input.text, "typed text");
        let img = p.items.iter().find(|i| i.kind == Kind::Image).unwrap();
        assert_eq!(img.text, "[a picture]");
        assert!(p.fills.iter().any(|f| f.bg == Rgb(34, 51, 68) && f.layer == Layer::Fixed), "the header's background");
        assert_eq!(p.canvas, Some(Rgb(0xfd, 0xfd, 0xf8)), "the test page sets body background #fdfdf8");
    }
}
