//! A tiny JSON path language: `.a.b[0].c`, `a.b`, `["weird key"].x`, `[2]`.
//!
//! Also [`flatten_numeric`], which turns any JSON tree into dotted numeric channels.

use serde_json::Value;

/// One step of a parsed path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seg {
    Key(String),
    Index(usize),
}

/// Parse a path into segments. Leading dot optional. Supports `.key`, `[0]`,
/// `["quoted key"]`, `['quoted']`, and bare `key.key`. Returns `None` on syntax errors.
pub fn parse(path: &str) -> Option<Vec<Seg>> {
    let s = path.trim();
    let s = s.strip_prefix('$').unwrap_or(s);
    let mut segs = Vec::new();
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '.' => {
                i += 1;
            }
            '[' => {
                i += 1;
                if i >= chars.len() {
                    return None;
                }
                if chars[i] == '"' || chars[i] == '\'' {
                    let q = chars[i];
                    i += 1;
                    let mut key = String::new();
                    loop {
                        if i >= chars.len() {
                            return None;
                        }
                        if chars[i] == '\\' && i + 1 < chars.len() {
                            key.push(chars[i + 1]);
                            i += 2;
                            continue;
                        }
                        if chars[i] == q {
                            i += 1;
                            break;
                        }
                        key.push(chars[i]);
                        i += 1;
                    }
                    if i >= chars.len() || chars[i] != ']' {
                        return None;
                    }
                    i += 1;
                    segs.push(Seg::Key(key));
                } else {
                    let start = i;
                    while i < chars.len() && chars[i] != ']' {
                        i += 1;
                    }
                    if i >= chars.len() {
                        return None;
                    }
                    let inner: String = chars[start..i].iter().collect();
                    i += 1;
                    let inner = inner.trim();
                    match inner.parse::<usize>() {
                        Ok(n) => segs.push(Seg::Index(n)),
                        Err(_) if !inner.is_empty() => segs.push(Seg::Key(inner.to_string())),
                        Err(_) => return None,
                    }
                }
            }
            _ => {
                let start = i;
                while i < chars.len() && chars[i] != '.' && chars[i] != '[' {
                    i += 1;
                }
                let key: String = chars[start..i].iter().collect();
                segs.push(Seg::Key(key));
            }
        }
    }
    Some(segs)
}

/// Extract the value at `path`, or `None` if the path is malformed or absent.
pub fn extract<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
    let segs = parse(path)?;
    let mut cur = v;
    for seg in &segs {
        cur = match seg {
            Seg::Key(k) => match cur {
                Value::Object(m) => m.get(k)?,
                // Allow numeric keys to index arrays: `.items.0.x`.
                Value::Array(a) => a.get(k.parse::<usize>().ok()?)?,
                _ => return None,
            },
            Seg::Index(i) => match cur {
                Value::Array(a) => a.get(*i)?,
                Value::Object(m) => m.get(&i.to_string())?,
                _ => return None,
            },
        };
    }
    Some(cur)
}

/// Extract and coerce to a number (numbers, bools, numeric strings).
pub fn extract_f64(v: &Value, path: &str) -> Option<f64> {
    to_f64(extract(v, path)?)
}

/// Numeric view of a leaf: numbers as-is, bools → 0/1, numeric strings parsed.
pub fn to_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        Value::String(s) => {
            let t = s.trim();
            if t.is_empty() {
                None
            } else {
                t.parse::<f64>().ok().filter(|f| f.is_finite())
            }
        }
        _ => None,
    }
}

/// Flatten every numeric leaf into `(dotted.path, value)`; arrays are indexed
/// (`items[0].x`). `prefix` is prepended (use `""` for none). Bounded to 4096 leaves.
pub fn flatten_numeric(v: &Value, prefix: &str) -> Vec<(String, f64)> {
    let mut out = Vec::new();
    walk(v, prefix.to_string(), &mut out, 0);
    out
}

fn walk(v: &Value, path: String, out: &mut Vec<(String, f64)>, depth: usize) {
    if out.len() >= 4096 || depth > 32 {
        return;
    }
    match v {
        Value::Object(m) => {
            for (k, child) in m {
                let p = if path.is_empty() { k.clone() } else { format!("{path}.{k}") };
                walk(child, p, out, depth + 1);
            }
        }
        Value::Array(a) => {
            for (i, child) in a.iter().enumerate().take(256) {
                walk(child, format!("{path}[{i}]"), out, depth + 1);
            }
        }
        other => {
            if let Some(n) = to_f64(other) {
                if n.is_finite() {
                    out.push((path, n));
                }
            }
        }
    }
}

/// Render segments back into canonical `.a.b[0]` form.
pub fn canonical(segs: &[Seg]) -> String {
    let mut s = String::new();
    for seg in segs {
        match seg {
            Seg::Key(k) => {
                if k.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-') && !k.is_empty() {
                    s.push('.');
                    s.push_str(k);
                } else {
                    s.push_str("[\"");
                    s.push_str(&k.replace('"', "\\\""));
                    s.push_str("\"]");
                }
            }
            Seg::Index(i) => s.push_str(&format!("[{i}]")),
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extracts_nested() {
        let v = json!({"a": {"b": [ {"c": 3}, {"c": 4} ], "weird key": {"x": "5"}}, "top": true});
        assert_eq!(extract(&v, ".a.b[0].c"), Some(&json!(3)));
        assert_eq!(extract(&v, "a.b[1].c"), Some(&json!(4)));
        assert_eq!(extract(&v, "a.b.1.c"), Some(&json!(4)));
        assert_eq!(extract(&v, r#".a["weird key"].x"#), Some(&json!("5")));
        assert_eq!(extract_f64(&v, r#"a['weird key'].x"#), Some(5.0));
        assert_eq!(extract_f64(&v, "top"), Some(1.0));
        assert_eq!(extract(&v, ".a.nope"), None);
        assert_eq!(extract(&v, ".a.b[9]"), None);
        assert_eq!(extract(&v, "a.b[oops"), None);
        assert_eq!(extract(&v, ""), Some(&v));
    }

    #[test]
    fn flattens() {
        let v = json!({"a": 1, "b": {"c": "2.5", "d": true, "e": "text", "f": null}, "arr": [1, {"x": 2}]});
        let f = flatten_numeric(&v, "");
        let m: std::collections::BTreeMap<_, _> = f.into_iter().collect();
        assert_eq!(m["a"], 1.0);
        assert_eq!(m["b.c"], 2.5);
        assert_eq!(m["b.d"], 1.0);
        assert!(!m.contains_key("b.e"));
        assert!(!m.contains_key("b.f"));
        assert_eq!(m["arr[0]"], 1.0);
        assert_eq!(m["arr[1].x"], 2.0);
        let p = flatten_numeric(&json!({"x": 1}), "root");
        assert_eq!(p[0].0, "root.x");
    }

    #[test]
    fn canonical_roundtrip() {
        let segs = parse(r#"a["weird key"][3].b"#).unwrap();
        assert_eq!(canonical(&segs), r#".a["weird key"][3].b"#);
        assert_eq!(parse(&canonical(&segs)).unwrap(), segs);
    }
}
