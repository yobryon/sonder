//! Source recipes: named kinds that expand to a generic transport spec plus an
//! engine-side mapping. Embedded from `recipes/*.toml`; see `recipes/README.md`.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value as Json;
use sonder_core::SourceSpec;

/// Every recipe file, embedded.
pub const RECIPE_SOURCES: &[&str] = &[
    include_str!("../../../recipes/kraken.trades.toml"),
    include_str!("../../../recipes/bybit.trades.toml"),
    include_str!("../../../recipes/okx.trades.toml"),
    include_str!("../../../recipes/binance.trades.toml"),
    include_str!("../../../recipes/bluesky.firehose.toml"),
    include_str!("../../../recipes/thingspeak.channel.toml"),
    include_str!("../../../recipes/uk.flood.toml"),
    include_str!("../../../recipes/uk.carbon.toml"),
];

#[derive(Debug, Clone, Deserialize)]
pub struct RecipeMeta {
    pub kind: String,
    pub summary: String,
    #[serde(default = "default_true")]
    pub network: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct RecipeParam {
    pub summary: String,
    pub default: Option<Json>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Recipe {
    pub recipe: RecipeMeta,
    #[serde(default)]
    pub params: BTreeMap<String, RecipeParam>,
    pub spec: BTreeMap<String, Json>,
}

/// All recipes, parsed. Malformed files are skipped (and caught by the unit test).
pub fn all() -> Vec<Recipe> {
    RECIPE_SOURCES.iter().filter_map(|src| toml::from_str::<Recipe>(src).ok()).collect()
}

/// Look a recipe up by kind.
pub fn get(kind: &str) -> Option<Recipe> {
    all().into_iter().find(|r| r.recipe.kind == kind)
}

/// Is this kind a recipe?
pub fn is_recipe(kind: &str) -> bool {
    get(kind).is_some()
}

/// Expand a spec whose `kind` names a recipe into the underlying transport spec.
/// Non-recipe specs are returned unchanged.
pub fn expand(spec: &SourceSpec) -> Result<SourceSpec, String> {
    let Some(recipe) = get(&spec.kind) else { return Ok(spec.clone()) };
    // Parameter values: user-supplied, else default.
    let mut values: BTreeMap<String, Json> = BTreeMap::new();
    for (name, p) in &recipe.params {
        match spec.params.get(name).cloned().or_else(|| p.default.clone()) {
            Some(v) => {
                values.insert(name.clone(), v);
            }
            None => return Err(format!("recipe `{}`: parameter `{name}` is required ({})", spec.kind, p.summary)),
        }
    }
    let mut out = SourceSpec::new(
        recipe.spec.get("kind").and_then(|k| k.as_str()).ok_or_else(|| format!("recipe `{}` has no spec.kind", spec.kind))?,
    );
    for (k, v) in &recipe.spec {
        if k == "kind" {
            continue;
        }
        // `caption` is a caption template (`{size} {product}`), not recipe placeholders.
        let v = if k == "caption" { v.clone() } else { substitute(v, &values)? };
        out.params.insert(k.clone(), v);
    }
    // Any extra user params (not recipe parameters) override the transport spec directly,
    // so `ping`, `interval` etc. can still be tuned.
    for (k, v) in &spec.params {
        if !recipe.params.contains_key(k) {
            out.params.insert(k.clone(), v.clone());
        }
    }
    Ok(out)
}

/// Replace `{name}` / `{name:fmt}` / `{name:fmt|sep}` placeholders.
///
/// - A string that is exactly one placeholder takes the parameter's typed value (a list
///   stays a list; a number stays a number). With `:fmt`, each list element is formatted
///   (`%s`) and, if the result parses as JSON, parsed — so `{symbols:{"id":"%s"}}` yields
///   a list of objects. With `|sep` the formatted elements are joined into one string.
/// - Embedded placeholders are stringified (lists joined by `sep`, default `,`).
fn substitute(v: &Json, values: &BTreeMap<String, Json>) -> Result<Json, String> {
    match v {
        Json::String(s) => substitute_str(s, values),
        Json::Array(items) => Ok(Json::Array(items.iter().map(|i| substitute(i, values)).collect::<Result<_, _>>()?)),
        Json::Object(m) => Ok(Json::Object(
            m.iter().map(|(k, v)| substitute(v, values).map(|v| (k.clone(), v))).collect::<Result<_, _>>()?,
        )),
        other => Ok(other.clone()),
    }
}

fn substitute_str(s: &str, values: &BTreeMap<String, Json>) -> Result<Json, String> {
    // Exact single placeholder?
    if let Some((name, fmt, sep)) = parse_placeholder(s) {
        let v = values.get(&name).ok_or_else(|| format!("unknown recipe parameter `{name}`"))?;
        return Ok(match (fmt, sep, v) {
            (None, _, v) => v.clone(),
            (Some(f), None, Json::Array(items)) => {
                Json::Array(items.iter().map(|i| format_item(&f, i)).map(|t| serde_json::from_str(&t).unwrap_or(Json::String(t))).collect())
            }
            (Some(f), Some(sep), Json::Array(items)) => Json::String(items.iter().map(|i| format_item(&f, i)).collect::<Vec<_>>().join(&sep)),
            (Some(f), _, scalar) => {
                let t = format_item(&f, scalar);
                serde_json::from_str(&t).unwrap_or(Json::String(t))
            }
        });
    }
    // Embedded placeholders → string.
    let mut out = String::new();
    let mut rest = s;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let Some(end) = find_matching_brace(&rest[start..]) else {
            out.push_str(&rest[start..]);
            rest = "";
            break;
        };
        let inner = &rest[start + 1..start + end];
        match parse_placeholder(&format!("{{{inner}}}")) {
            Some((name, fmt, sep)) => {
                let v = values.get(&name).ok_or_else(|| format!("unknown recipe parameter `{name}`"))?;
                let sep = sep.unwrap_or_else(|| ",".into());
                let text = match v {
                    Json::Array(items) => items.iter().map(|i| format_item(fmt.as_deref().unwrap_or("%s"), i)).collect::<Vec<_>>().join(&sep),
                    scalar => format_item(fmt.as_deref().unwrap_or("%s"), scalar),
                };
                out.push_str(&text);
            }
            None => out.push_str(&rest[start..start + end + 1]),
        }
        rest = &rest[start + end + 1..];
    }
    out.push_str(rest);
    Ok(Json::String(out))
}

/// `{name}`, `{name:fmt}`, `{name:fmt|sep}` → (name, fmt, sep); only when the whole string
/// is one placeholder.
fn parse_placeholder(s: &str) -> Option<(String, Option<String>, Option<String>)> {
    if !s.starts_with('{') || !s.ends_with('}') || find_matching_brace(s)? != s.len() - 1 {
        return None;
    }
    let inner = &s[1..s.len() - 1];
    let (name, rest) = match inner.find(':') {
        Some(i) => (&inner[..i], Some(&inner[i + 1..])),
        None => (inner, None),
    };
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    let (fmt, sep) = match rest {
        None => (None, None),
        Some(r) => match r.rfind('|') {
            Some(i) if !r[..i].ends_with('\\') => (Some(r[..i].to_string()), Some(r[i + 1..].to_string())),
            _ => (Some(r.to_string()), None),
        },
    };
    Some((name.to_string(), fmt, sep))
}

/// Index of the brace matching the `{` at position 0, honouring nesting.
fn find_matching_brace(s: &str) -> Option<usize> {
    let mut depth = 0usize;
    for (i, c) in s.char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

fn format_item(fmt: &str, v: &Json) -> String {
    let text = match v {
        Json::String(s) => s.clone(),
        other => other.to_string(),
    };
    fmt.replace("%s", &text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_recipe_parses_and_expands_with_defaults() {
        assert_eq!(all().len(), RECIPE_SOURCES.len(), "a recipe file failed to parse");
        for r in all() {
            let spec = SourceSpec::new(&r.recipe.kind);
            let out = expand(&spec).unwrap_or_else(|e| panic!("{}: {e}", r.recipe.kind));
            assert!(matches!(out.kind.as_str(), "websocket" | "sse" | "http-json"), "{}: kind {}", r.recipe.kind, out.kind);
            // Mappings must compile.
            crate::mapping::SourceMapping::from_spec(&out).unwrap_or_else(|e| panic!("{}: {e}", r.recipe.kind));
            let text = serde_json::to_string(&out.params).unwrap();
            assert!(!text.contains("{symbols") && !text.contains("{host"), "{}: unexpanded placeholder in {text}", r.recipe.kind);
            assert!(out.get_str("caption").is_some_and(|c| !c.is_empty()), "{}: recipe has no caption", r.recipe.kind);
        }
    }

    #[test]
    fn placeholders_substitute_typed_values() {
        let spec = SourceSpec::new("kraken.trades").with("symbols", serde_json::json!(["BTC/USD", "ETH/USD"]));
        let out = expand(&spec).unwrap();
        assert_eq!(out.params["subscribe"]["params"]["symbol"], serde_json::json!(["BTC/USD", "ETH/USD"]));

        let spec = SourceSpec::new("bybit.trades").with("symbols", serde_json::json!(["BTCUSDT", "ETHUSDT"]));
        let out = expand(&spec).unwrap();
        assert_eq!(out.params["subscribe"]["args"], serde_json::json!(["publicTrade.BTCUSDT", "publicTrade.ETHUSDT"]));

        let spec = SourceSpec::new("okx.trades");
        let out = expand(&spec).unwrap();
        assert_eq!(out.params["subscribe"]["args"], serde_json::json!([{"channel": "trades", "instId": "BTC-USDT"}]));

        let spec = SourceSpec::new("binance.trades").with("symbols", serde_json::json!(["btcusdt", "ethusdt"]));
        let out = expand(&spec).unwrap();
        assert_eq!(out.params["url"], serde_json::json!("wss://stream.binance.com:9443/stream?streams=btcusdt@trade/ethusdt@trade"));

        let out = expand(&SourceSpec::new("thingspeak.channel")).unwrap();
        assert_eq!(out.params["interval"], serde_json::json!(20));
        assert_eq!(out.params["url"], serde_json::json!("https://api.thingspeak.com/channels/1417/feeds.json?results=1"));

        // Extra params pass through to the transport.
        let out = expand(&SourceSpec::new("kraken.trades").with("ping", 5)).unwrap();
        assert_eq!(out.params["ping"], serde_json::json!(5));
        // Non-recipes are untouched.
        let plain = SourceSpec::new("usgs.earthquakes").with("feed", "all_day");
        assert_eq!(expand(&plain).unwrap(), plain);
    }
}
