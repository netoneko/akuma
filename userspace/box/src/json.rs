use alloc::string::String;
use alloc::vec::Vec;

fn find_value_start(json: &str, key: &str) -> Option<usize> {
    let pattern = alloc::format!("\"{}\"", key);
    let key_pos = json.find(&pattern)?;
    let after_key = &json[key_pos + pattern.len()..];
    let colon_pos = after_key.find(':')?;
    let value_start = key_pos + pattern.len() + colon_pos + 1;
    let rest = &json[value_start..];
    let trimmed = rest.trim_start();
    let offset = rest.len() - trimmed.len();
    Some(value_start + offset)
}

fn find_matching(s: &str, open: u8, close: u8) -> Option<usize> {
    let bytes = s.as_bytes();
    if bytes.is_empty() || bytes[0] != open {
        return None;
    }
    let mut depth = 0i32;
    let mut in_string = false;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            if b == b'\\' {
                i += 2;
                continue;
            }
            if b == b'"' {
                in_string = false;
            }
        } else {
            if b == b'"' {
                in_string = true;
            } else if b == open {
                depth += 1;
            } else if b == close {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
        }
        i += 1;
    }
    None
}

fn find_unescaped_quote(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            i += 2;
            continue;
        }
        if bytes[i] == b'"' {
            return Some(i);
        }
        i += 1;
    }
    None
}

pub fn extract_string(json: &str, key: &str) -> Option<String> {
    let start = find_value_start(json, key)?;
    if json.as_bytes().get(start).copied() != Some(b'"') {
        return None;
    }
    let content = &json[start + 1..];
    let end = find_unescaped_quote(content)?;
    Some(unescape(&content[..end]))
}

pub fn extract_object<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let start = find_value_start(json, key)?;
    if json.as_bytes().get(start).copied() != Some(b'{') {
        return None;
    }
    let end = find_matching(&json[start..], b'{', b'}')?;
    Some(&json[start..start + end + 1])
}

pub fn extract_array<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let start = find_value_start(json, key)?;
    if json.as_bytes().get(start).copied() != Some(b'[') {
        return None;
    }
    let end = find_matching(&json[start..], b'[', b']')?;
    Some(&json[start..start + end + 1])
}

pub fn iter_array_objects(array_json: &str) -> Vec<&str> {
    let inner = array_json.trim();
    if inner.len() < 2 {
        return Vec::new();
    }
    let inner = &inner[1..inner.len() - 1];
    let mut result = Vec::new();
    let mut pos = 0;
    let bytes = inner.as_bytes();

    while pos < bytes.len() {
        while pos < bytes.len() && matches!(bytes[pos], b' ' | b',' | b'\n' | b'\r' | b'\t') {
            pos += 1;
        }
        if pos >= bytes.len() {
            break;
        }
        if bytes[pos] == b'{' {
            if let Some(end) = find_matching(&inner[pos..], b'{', b'}') {
                result.push(&inner[pos..pos + end + 1]);
                pos += end + 1;
            } else {
                break;
            }
        } else {
            pos += 1;
        }
    }
    result
}

pub fn extract_string_array(json: &str, key: &str) -> Option<Vec<String>> {
    let arr = extract_array(json, key)?;
    let inner = arr.trim();
    if inner.len() < 2 {
        return Some(Vec::new());
    }
    let inner = &inner[1..inner.len() - 1];
    let mut result = Vec::new();
    let mut pos = 0;
    let bytes = inner.as_bytes();

    while pos < bytes.len() {
        while pos < bytes.len() && matches!(bytes[pos], b' ' | b',' | b'\n' | b'\r' | b'\t') {
            pos += 1;
        }
        if pos >= bytes.len() {
            break;
        }
        if bytes[pos] == b'"' {
            let content = &inner[pos + 1..];
            if let Some(end) = find_unescaped_quote(content) {
                result.push(unescape(&content[..end]));
                pos += 1 + end + 1;
            } else {
                break;
            }
        } else {
            pos += 1;
        }
    }
    Some(result)
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'"' => out.push('"'),
                b'\\' => out.push('\\'),
                b'/' => out.push('/'),
                b'n' => out.push('\n'),
                b'r' => out.push('\r'),
                b't' => out.push('\t'),
                other => {
                    out.push('\\');
                    out.push(other as char);
                }
            }
            i += 2;
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}
