//! Pure text/colour helpers used by the renderer. Kept free of widgets so
//! they are trivially unit-testable.

use std::collections::BTreeMap;

use ratatui::style::Color;
use sonder_core::{SourceState, Value};

/// Ramp glyphs used for meters and inline bars, lowest to highest.
pub const RAMP: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// Fixed 9-colour layer palette (256-colour safe).
pub const LAYER_PALETTE: [Color; 9] = [
    Color::Indexed(81),  // sky
    Color::Indexed(213), // pink
    Color::Indexed(118), // lime
    Color::Indexed(220), // gold
    Color::Indexed(171), // violet
    Color::Indexed(209), // salmon
    Color::Indexed(45),  // turquoise
    Color::Indexed(198), // magenta
    Color::Indexed(157), // mint
];

/// Colour for layer `index` (wraps around the palette).
pub fn layer_color(index: usize) -> Color {
    LAYER_PALETTE[index % LAYER_PALETTE.len()]
}

/// A dimmer variant of a palette colour, used for trails and inactive notes.
pub fn dim_color(index: usize) -> Color {
    const DIM: [Color; 9] = [
        Color::Indexed(24),
        Color::Indexed(96),
        Color::Indexed(22),
        Color::Indexed(94),
        Color::Indexed(54),
        Color::Indexed(95),
        Color::Indexed(23),
        Color::Indexed(89),
        Color::Indexed(65),
    ];
    DIM[index % DIM.len()]
}

/// Render an RMS level (0..1) as `cells` ramp glyphs. Returns `(lit, unlit)`
/// so the caller can style them separately; `lit.len() + unlit.len() == cells`
/// in chars. The level is square-rooted so quiet material still registers.
pub fn meter_glyphs(rms: f32, cells: usize) -> (String, String) {
    let level = rms.clamp(0.0, 1.0).sqrt();
    let lit = (level * cells as f32).round() as usize;
    let lit = lit.min(cells);
    let lit_s: String = (0..lit)
        .map(|i| RAMP[(i * RAMP.len()) / cells.max(1)])
        .collect();
    let unlit_s: String = std::iter::repeat_n('·', cells - lit).collect();
    (lit_s, unlit_s)
}

/// Meter colour for a level: green, then gold, then red near clipping.
pub fn meter_color(rms: f32) -> Color {
    if rms > 0.8 {
        Color::Indexed(196)
    } else if rms > 0.5 {
        Color::Indexed(220)
    } else {
        Color::Indexed(114)
    }
}

/// A fixed-width horizontal bar of `width` cells for a 0..1 value, e.g. `███░░`.
pub fn mini_gauge(value: f64, width: usize) -> String {
    let v = if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let filled = (v * width as f64).round() as usize;
    let filled = filled.min(width);
    let mut s = String::with_capacity(width * 3);
    s.extend(std::iter::repeat_n('█', filled));
    s.extend(std::iter::repeat_n('░', width - filled));
    s
}

/// Format engine seconds as `m:ss` (or `h:mm:ss` past an hour).
pub fn fmt_time(secs: f64) -> String {
    let total = if secs.is_finite() && secs > 0.0 {
        secs.floor() as u64
    } else {
        0
    };
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Format engine seconds with tenths, e.g. `1:02.3`.
pub fn fmt_time_tenths(secs: f64) -> String {
    let tenths = if secs.is_finite() && secs > 0.0 {
        ((secs * 10.0).floor() as u64) % 10
    } else {
        0
    };
    format!("{}.{}", fmt_time(secs), tenths)
}

/// Compact number formatting: integers stay integers, otherwise two decimals
/// with trailing zeros stripped.
pub fn fmt_num(v: f64) -> String {
    if !v.is_finite() {
        return "nan".into();
    }
    if (v - v.round()).abs() < 1e-9 && v.abs() < 1e12 {
        return format!("{}", v.round() as i64);
    }
    let s = format!("{v:.2}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    s.to_string()
}

/// Format an attribute value for the ticker; text is truncated to `max_text` chars.
pub fn fmt_value(v: &Value, max_text: usize) -> String {
    match v {
        Value::Num(n) => fmt_num(*n),
        Value::Bool(b) => if *b { "yes" } else { "no" }.to_string(),
        Value::Text(t) => truncate(t, max_text),
    }
}

/// Truncate `s` to at most `max` chars, adding `…` if anything was cut.
pub fn truncate(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out
}

/// Summarise the first `n` attributes as `k=v k=v`. Attributes called `lat01` /
/// `lon01` (viz hints) are skipped in favour of anything more interesting.
pub fn summarize_attrs(attrs: &BTreeMap<String, Value>, n: usize) -> String {
    let mut parts: Vec<String> = attrs
        .iter()
        .filter(|(k, _)| !matches!(k.as_str(), "lat01" | "lon01"))
        .take(n)
        .map(|(k, v)| format!("{}={}", truncate(k, 10), fmt_value(v, 12)))
        .collect();
    if parts.len() < n {
        parts.extend(
            attrs
                .iter()
                .filter(|(k, _)| matches!(k.as_str(), "lat01" | "lon01"))
                .take(n - parts.len())
                .map(|(k, v)| format!("{}={}", k, fmt_value(v, 12))),
        );
    }
    parts.join(" ")
}

/// Human label and colour for a source state.
pub fn source_state_style(state: SourceState) -> (&'static str, Color) {
    match state {
        SourceState::Connecting => ("connecting", Color::Yellow),
        SourceState::Live => ("live", Color::Green),
        SourceState::Idle => ("idle", Color::DarkGray),
        SourceState::Reconnecting => ("reconnecting", Color::Indexed(208)),
        SourceState::Failed => ("failed", Color::Red),
    }
}

/// `bar.beat`, 1-based, e.g. `12.3`.
pub fn fmt_bar_beat(bar: u64, beat: u8) -> String {
    format!("{}.{}", bar + 1, beat as u16 + 1)
}

/// Resample `data` to exactly `width` points by taking the max of each bucket
/// (so narrow terminals still show spectral peaks).
pub fn resample_max(data: &[f32], width: usize) -> Vec<f32> {
    if width == 0 || data.is_empty() {
        return Vec::new();
    }
    if data.len() <= width {
        return data.to_vec();
    }
    (0..width)
        .map(|i| {
            let lo = i * data.len() / width;
            let hi = ((i + 1) * data.len() / width).max(lo + 1).min(data.len());
            data[lo..hi].iter().copied().fold(0.0f32, f32::max)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meter_glyphs_fill_and_bound() {
        let (lit, unlit) = meter_glyphs(0.0, 8);
        assert_eq!(lit.chars().count(), 0);
        assert_eq!(unlit.chars().count(), 8);
        let (lit, unlit) = meter_glyphs(1.0, 8);
        assert_eq!(lit.chars().count(), 8);
        assert!(unlit.is_empty());
        assert_eq!(lit.chars().last(), Some('█'));
        let (lit, unlit) = meter_glyphs(0.25, 8); // sqrt → 0.5 → 4 cells
        assert_eq!(lit.chars().count(), 4);
        assert_eq!(unlit.chars().count(), 4);
        let (lit, _) = meter_glyphs(7.0, 8); // over-range clamps
        assert_eq!(lit.chars().count(), 8);
        let (lit, _) = meter_glyphs(f32::NAN, 8);
        assert_eq!(lit.chars().count(), 0);
    }

    #[test]
    fn mini_gauge_widths() {
        assert_eq!(mini_gauge(0.0, 4), "░░░░");
        assert_eq!(mini_gauge(0.5, 4), "██░░");
        assert_eq!(mini_gauge(1.0, 4), "████");
        assert_eq!(mini_gauge(2.0, 4), "████");
        assert_eq!(mini_gauge(f64::NAN, 3), "░░░");
    }

    #[test]
    fn time_formatting() {
        assert_eq!(fmt_time(0.0), "0:00");
        assert_eq!(fmt_time(65.9), "1:05");
        assert_eq!(fmt_time(3661.0), "1:01:01");
        assert_eq!(fmt_time(-3.0), "0:00");
        assert_eq!(fmt_time_tenths(62.34), "1:02.3");
        assert_eq!(fmt_bar_beat(0, 0), "1.1");
        assert_eq!(fmt_bar_beat(11, 2), "12.3");
    }

    #[test]
    fn number_and_value_formatting() {
        assert_eq!(fmt_num(3.0), "3");
        assert_eq!(fmt_num(0.5), "0.5");
        assert_eq!(fmt_num(0.123456), "0.12");
        assert_eq!(fmt_num(-2.0), "-2");
        assert_eq!(fmt_value(&Value::Bool(true), 5), "yes");
        assert_eq!(fmt_value(&Value::Text("hello world".into()), 5), "hell…");
        assert_eq!(truncate("abc", 3), "abc");
        assert_eq!(truncate("abcd", 0), "");
    }

    #[test]
    fn attr_summary_prefers_interesting_keys() {
        let mut attrs = BTreeMap::new();
        attrs.insert("lat01".to_string(), Value::Num(0.5));
        attrs.insert("mag".to_string(), Value::Num(4.25));
        attrs.insert(
            "place".to_string(),
            Value::Text("Off the coast of Oregon".into()),
        );
        attrs.insert("lon01".to_string(), Value::Num(0.25));
        assert_eq!(summarize_attrs(&attrs, 2), "mag=4.25 place=Off the coa…");
        // Falls back to viz hints when nothing else is there.
        let mut only_geo = BTreeMap::new();
        only_geo.insert("lat01".to_string(), Value::Num(0.5));
        assert_eq!(summarize_attrs(&only_geo, 2), "lat01=0.5");
        assert_eq!(summarize_attrs(&BTreeMap::new(), 2), "");
    }

    #[test]
    fn source_states_have_distinct_colors() {
        let states = [
            SourceState::Connecting,
            SourceState::Live,
            SourceState::Idle,
            SourceState::Reconnecting,
            SourceState::Failed,
        ];
        let colors: Vec<Color> = states.iter().map(|s| source_state_style(*s).1).collect();
        for (i, a) in colors.iter().enumerate() {
            for b in &colors[i + 1..] {
                assert_ne!(a, b);
            }
        }
        assert_eq!(source_state_style(SourceState::Live).0, "live");
    }

    #[test]
    fn resampling_keeps_peaks() {
        let data = [0.0, 0.9, 0.1, 0.2, 0.0, 0.0, 0.7, 0.0];
        assert_eq!(resample_max(&data, 4), vec![0.9, 0.2, 0.0, 0.7]);
        assert_eq!(resample_max(&data, 8), data.to_vec());
        assert_eq!(resample_max(&data, 16).len(), 8);
        assert!(resample_max(&data, 0).is_empty());
        assert!(resample_max(&[], 4).is_empty());
    }

    #[test]
    fn palette_wraps() {
        assert_eq!(layer_color(0), layer_color(9));
        assert_ne!(layer_color(0), layer_color(1));
        assert_ne!(layer_color(3), dim_color(3));
    }
}
