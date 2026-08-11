//! Reading OCI documents, over the `picojson` pull parser.
//!
//! This used to be a hand-rolled scanner that searched for `"key"` and returned
//! the raw slice after it. That is a substring search wearing a parser's name:
//! it matches a key at *any* depth, so an image config's `"config"` and a
//! `"container_config"` sitting next to it are the same lookup, and a brace
//! inside a string could end an object early. `picojson` is a real tokenizer —
//! `no_std`, no allocation of its own, no recursion — and everything here is a
//! path-addressed view on top of it.
//!
//! Values are addressed by the path that leads to them, so
//! `["config", "Cmd", "*"]` reaches only the top-level `config` object's `Cmd`
//! array, never a same-named key nested somewhere else:
//!
//! ```text
//! {"config": {"Cmd": ["/bin/sh"]}}   →  path ["config", "Cmd", 0] = "/bin/sh"
//! ```
//!
//! The parser needs a scratch buffer only to un-escape strings; this module
//! allocates one as large as the document, which is always enough, since an
//! un-escaped string is never longer than its escaped form.

use alloc::string::String;
use alloc::vec::Vec;
use picojson::{Event, PullParser, SliceParser};

pub use picojson::ParseError;

/// One step of the path to a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seg {
    /// An object member name.
    Key(String),
    /// A position in an array.
    Index(usize),
}

impl Seg {
    /// Whether a pattern segment selects this one. `*` selects any segment; a
    /// decimal pattern selects that array position; anything else is a literal
    /// key.
    fn matched_by(&self, pattern: &str) -> bool {
        if pattern == "*" {
            return true;
        }
        match self {
            Self::Key(k) => k == pattern,
            Self::Index(i) => pattern.parse::<usize>().is_ok_and(|p| p == *i),
        }
    }
}

/// Where the value currently being visited sits in the document, outermost
/// first. Empty for a value at the document root.
pub struct Path {
    segs: Vec<Seg>,
}

impl Path {
    /// Whether this path is exactly `pattern` — same length, segment by
    /// segment. `["layers", "*", "digest"]` matches every layer's digest and
    /// nothing else.
    pub fn matches(&self, pattern: &[&str]) -> bool {
        self.segs.len() == pattern.len()
            && self.segs.iter().zip(pattern).all(|(s, p)| s.matched_by(p))
    }

    /// The array position at `depth`, if that segment is one. Used to keep the
    /// fields of the same array element together — `platform.architecture` and
    /// `digest` belong to one manifest only if their paths agree here.
    pub fn index_at(&self, depth: usize) -> Option<usize> {
        match self.segs.get(depth) {
            Some(Seg::Index(i)) => Some(*i),
            _ => None,
        }
    }

    pub fn segments(&self) -> &[Seg] {
        &self.segs
    }
}

/// A JSON value, as seen by a visitor.
///
/// Numbers are integers only: nothing in an OCI manifest or image config is
/// fractional, and the parser is built with float support off (`float-skip`),
/// which reports any float it meets as [`Value::Other`] rather than failing the
/// whole document.
#[derive(Debug, PartialEq)]
pub enum Value<'a> {
    Str(&'a str),
    Int(i64),
    Bool(bool),
    Null,
    /// The `{` of an object or the `[` of an array. Reported so a caller can
    /// tell an empty array apart from a missing one.
    StartObject,
    StartArray,
    /// A number that is not an integer this build can represent.
    Other,
}

/// Which container the walk is currently inside, and whether it has already
/// pushed a segment onto the path for the member being read.
struct Frame {
    array: bool,
    next_index: usize,
    seg_pushed: bool,
}

/// A value inside an array takes the next position; one inside an object was
/// already named by its key, and one at the document root has no segment.
fn enter_array_element(path: &mut Path, stack: &mut [Frame]) {
    if let Some(frame) = stack.last_mut() {
        if frame.array {
            if frame.seg_pushed {
                path.segs.pop();
            }
            path.segs.push(Seg::Index(frame.next_index));
            frame.next_index += 1;
            frame.seg_pushed = true;
        }
    }
}

/// Walk `doc`, calling `visit` for every value with the path that leads to it.
///
/// One pass, in document order, no allocation beyond the path and the parser's
/// scratch buffer. Callers accumulate what they need as it goes by.
pub fn walk<F>(doc: &str, mut visit: F) -> Result<(), ParseError>
where
    F: FnMut(&Path, Value<'_>),
{
    // Only escaped strings are copied here; the parser borrows from `doc`
    // otherwise. Sizing it to the document makes `ScratchBufferFull`
    // unreachable.
    let mut scratch = alloc::vec![0u8; doc.len() + 16];
    let mut parser = SliceParser::with_buffer(doc, &mut scratch);

    let mut path = Path { segs: Vec::new() };
    let mut stack: Vec<Frame> = Vec::new();

    loop {
        let event = parser.next_event()?;

        match event {
            Event::EndDocument => break,

            Event::Key(k) => {
                if let Some(frame) = stack.last_mut() {
                    if frame.seg_pushed {
                        path.segs.pop();
                    }
                    frame.seg_pushed = true;
                }
                path.segs.push(Seg::Key(String::from(k.as_str())));
            }

            Event::StartObject | Event::StartArray => {
                let array = event == Event::StartArray;
                enter_array_element(&mut path, &mut stack);
                visit(
                    &path,
                    if array {
                        Value::StartArray
                    } else {
                        Value::StartObject
                    },
                );
                stack.push(Frame {
                    array,
                    next_index: 0,
                    seg_pushed: false,
                });
            }

            Event::EndObject | Event::EndArray => {
                if let Some(frame) = stack.pop() {
                    if frame.seg_pushed {
                        path.segs.pop();
                    }
                }
            }

            Event::String(s) => {
                enter_array_element(&mut path, &mut stack);
                visit(&path, Value::Str(s.as_str()));
            }
            Event::Number(n) => {
                let value = n.as_int().map_or(Value::Other, Value::Int);
                enter_array_element(&mut path, &mut stack);
                visit(&path, value);
            }
            Event::Bool(b) => {
                enter_array_element(&mut path, &mut stack);
                visit(&path, Value::Bool(b));
            }
            Event::Null => {
                enter_array_element(&mut path, &mut stack);
                visit(&path, Value::Null);
            }
        }
    }

    Ok(())
}

/// The string at `pattern`, or `None` if it is absent or not a string. The
/// first match wins.
pub fn string_at(doc: &str, pattern: &[&str]) -> Option<String> {
    let mut found = None;
    let _ = walk(doc, |path, value| {
        if found.is_none() {
            if let Value::Str(s) = value {
                if path.matches(pattern) {
                    found = Some(String::from(s));
                }
            }
        }
    });
    found
}

/// Every string matching `pattern`, in document order — `["Cmd", "*"]` for an
/// array of strings, `["layers", "*", "digest"]` across array elements.
pub fn strings_at(doc: &str, pattern: &[&str]) -> Vec<String> {
    let mut found = Vec::new();
    let _ = walk(doc, |path, value| {
        if let Value::Str(s) = value {
            if path.matches(pattern) {
                found.push(String::from(s));
            }
        }
    });
    found
}

/// The integer at `pattern`, e.g. a layer's `size`.
pub fn number_at(doc: &str, pattern: &[&str]) -> Option<i64> {
    let mut found = None;
    let _ = walk(doc, |path, value| {
        if found.is_none() {
            if let Value::Int(i) = value {
                if path.matches(pattern) {
                    found = Some(i);
                }
            }
        }
    });
    found
}

/// Whether anything at all sits at `pattern` — including an empty array or
/// object, which the `*_at` accessors cannot report.
pub fn exists(doc: &str, pattern: &[&str]) -> bool {
    let mut seen = false;
    let _ = walk(doc, |path, _| {
        if path.matches(pattern) {
            seen = true;
        }
    });
    seen
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    /// Every value in the document as `path = value`, for asserting on the
    /// shape of a walk rather than one lookup at a time.
    fn trace(doc: &str) -> Vec<String> {
        let mut out = Vec::new();
        walk(doc, |path, value| {
            let mut p = String::new();
            for (i, seg) in path.segments().iter().enumerate() {
                if i > 0 {
                    p.push('.');
                }
                match seg {
                    Seg::Key(k) => p.push_str(k),
                    Seg::Index(n) => p.push_str(&alloc::format!("{}", n)),
                }
            }
            out.push(match value {
                Value::Str(s) => alloc::format!("{}=\"{}\"", p, s),
                Value::Int(i) => alloc::format!("{}={}", p, i),
                Value::Bool(b) => alloc::format!("{}={}", p, b),
                Value::Null => alloc::format!("{}=null", p),
                Value::StartObject => alloc::format!("{}={{", p),
                Value::StartArray => alloc::format!("{}=[", p),
                Value::Other => alloc::format!("{}=?", p),
            });
        })
        .unwrap();
        out
    }

    #[test]
    fn walks_a_flat_object() {
        assert_eq!(
            trace(r#"{"name": "hello", "size": 3, "ok": true, "gone": null}"#),
            ["={", "name=\"hello\"", "size=3", "ok=true", "gone=null"]
        );
    }

    #[test]
    fn walks_nested_objects_and_arrays() {
        assert_eq!(
            trace(r#"{"config": {"Cmd": ["/bin/sh", "-c"]}}"#),
            [
                "={",
                "config={",
                "config.Cmd=[",
                "config.Cmd.0=\"/bin/sh\"",
                "config.Cmd.1=\"-c\"",
            ]
        );
    }

    #[test]
    fn array_indices_advance_per_element() {
        assert_eq!(
            trace(r#"{"l": [{"d": "a"}, {"d": "b"}, {"d": "c"}]}"#),
            [
                "={",
                "l=[",
                "l.0={",
                "l.0.d=\"a\"",
                "l.1={",
                "l.1.d=\"b\"",
                "l.2={",
                "l.2.d=\"c\"",
            ]
        );
    }

    #[test]
    fn sibling_keys_do_not_leak_into_each_others_paths() {
        // The bug this shape guards against: popping the wrong segment when a
        // container closes leaves the next key nested under the previous one.
        assert_eq!(
            trace(r#"{"a": {"x": 1}, "b": 2, "c": {"y": 3}}"#),
            ["={", "a={", "a.x=1", "b=2", "c={", "c.y=3"]
        );
    }

    #[test]
    fn nested_arrays_keep_separate_positions() {
        assert_eq!(
            trace(r#"{"m": [["a", "b"], ["c"]]}"#),
            [
                "={",
                "m=[",
                "m.0=[",
                "m.0.0=\"a\"",
                "m.0.1=\"b\"",
                "m.1=[",
                "m.1.0=\"c\"",
            ]
        );
    }

    #[test]
    fn root_scalar_has_an_empty_path() {
        assert_eq!(trace(r#""bare""#), ["=\"bare\""]);
    }

    #[test]
    fn reports_malformed_documents() {
        assert!(walk(r#"{"a": }"#, |_, _| {}).is_err());
        assert!(walk(r#"{"a": 1"#, |_, _| {}).is_err());
        assert!(walk("not json", |_, _| {}).is_err());
    }

    #[test]
    fn unescapes_strings() {
        let doc = r#"{"path": "foo\/bar", "msg": "he said \"hi\"", "nl": "a\nb"}"#;
        assert_eq!(string_at(doc, &["path"]).unwrap(), "foo/bar");
        assert_eq!(string_at(doc, &["msg"]).unwrap(), "he said \"hi\"");
        assert_eq!(string_at(doc, &["nl"]).unwrap(), "a\nb");
    }

    #[test]
    fn unescapes_unicode() {
        assert_eq!(
            string_at(r#"{"k": "café"}"#, &["k"]).unwrap(),
            "café"
        );
    }

    #[test]
    fn a_key_only_matches_at_its_own_depth() {
        // The whole point of paths: an image config has a `config` object, and
        // a `container_config` beside it with the same member names. A
        // substring search for `"Cmd"` cannot tell them apart.
        let doc = r#"{
            "container_config": {"Cmd": ["wrong"]},
            "config": {"Cmd": ["right"]}
        }"#;
        assert_eq!(strings_at(doc, &["config", "Cmd", "*"]), ["right"]);
        assert_eq!(
            strings_at(doc, &["container_config", "Cmd", "*"]),
            ["wrong"]
        );
        assert!(strings_at(doc, &["Cmd", "*"]).is_empty());
    }

    #[test]
    fn braces_and_brackets_inside_strings_are_not_structure() {
        let doc = r#"{"config": {"Cmd": ["echo }{ ][", "next"]}}"#;
        assert_eq!(
            strings_at(doc, &["config", "Cmd", "*"]),
            ["echo }{ ][", "next"]
        );
    }

    #[test]
    fn wildcards_and_literal_indices_both_select() {
        let doc = r#"{"l": [{"d": "a"}, {"d": "b"}]}"#;
        assert_eq!(strings_at(doc, &["l", "*", "d"]), ["a", "b"]);
        assert_eq!(string_at(doc, &["l", "1", "d"]).unwrap(), "b");
        assert_eq!(string_at(doc, &["l", "2", "d"]), None);
    }

    #[test]
    fn accessors_reject_the_wrong_type() {
        let doc = r#"{"size": 1471, "digest": "sha256:abc"}"#;
        assert_eq!(string_at(doc, &["size"]), None);
        assert_eq!(number_at(doc, &["digest"]), None);
        assert_eq!(number_at(doc, &["size"]), Some(1471));
    }

    #[test]
    fn exists_sees_empty_containers() {
        assert!(exists(r#"{"manifests": []}"#, &["manifests"]));
        assert!(exists(r#"{"config": {}}"#, &["config"]));
        assert!(!exists(r#"{"config": {}}"#, &["manifests"]));
        assert!(strings_at(r#"{"manifests": []}"#, &["manifests", "*"]).is_empty());
    }

    #[test]
    fn index_at_correlates_fields_of_one_element() {
        let doc = r#"{"m": [{"a": "x", "b": "1"}, {"a": "y", "b": "2"}]}"#;
        let mut pairs: Vec<(usize, String)> = vec![];
        walk(doc, |path, value| {
            if let Value::Str(s) = value {
                if path.matches(&["m", "*", "a"]) {
                    pairs.push((path.index_at(1).unwrap(), String::from(s)));
                }
            }
        })
        .unwrap();
        assert_eq!(pairs, [(0, String::from("x")), (1, String::from("y"))]);
    }

    #[test]
    fn floats_do_not_derail_a_document() {
        // Built with `float-skip`: a fractional number anywhere must not fail
        // the parse of the fields that matter.
        let doc = r#"{"weight": 1.5, "digest": "sha256:abc"}"#;
        assert_eq!(string_at(doc, &["digest"]).unwrap(), "sha256:abc");
    }

    #[test]
    fn deep_nesting_stays_within_the_parsers_depth() {
        // 16 levels — half the 32 the default bitstack allows, and far past
        // anything a registry emits.
        let mut doc = String::new();
        for _ in 0..16 {
            doc.push_str(r#"{"a":"#);
        }
        doc.push_str(r#""deep""#);
        for _ in 0..16 {
            doc.push('}');
        }
        let pattern = vec!["a"; 16];
        assert_eq!(string_at(&doc, &pattern).unwrap(), "deep");
    }
}
