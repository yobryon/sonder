//! `quantize` grid parsing shared by the composer and validators.

use crate::error::ExprError;

/// Parse a rhythmic grid spec into a length in beats (quarter note = 1 beat).
///
/// Accepted forms (case-insensitive, whitespace trimmed):
/// - `"off"`, `"none"`, `""` → `Ok(None)` (no quantization)
/// - `"1/4"` → 1 beat, `"1/8"` → 0.5, `"1/16"` → 0.25, `"1/1"` → 4
/// - triplets with a `t` suffix: `"1/8t"` → 1/3, `"1/4t"` → 2/3
/// - dotted with a `.` suffix: `"1/8."` → 0.75
/// - a plain positive number is taken as beats directly: `"0.5"` → 0.5, `"2"` → 2
pub fn parse_grid(s: &str) -> Result<Option<f64>, ExprError> {
    let raw = s.trim();
    let t = raw.to_ascii_lowercase();
    if t.is_empty() || t == "off" || t == "none" || t == "0" {
        return Ok(None);
    }
    let bad = || {
        ExprError::new(format!("bad grid `{raw}`: expected `off`, `1/4`, `1/8`, `1/16`, `1/8t`, `1/8.` or a number of beats"))
    };
    let (body, mult) = if let Some(b) = t.strip_suffix('t') {
        (b, 2.0 / 3.0)
    } else if let Some(b) = t.strip_suffix('.') {
        (b, 1.5)
    } else {
        (t.as_str(), 1.0)
    };
    let beats = if let Some((num, den)) = body.split_once('/') {
        let num: f64 = num.trim().parse().map_err(|_| bad())?;
        let den: f64 = den.trim().parse().map_err(|_| bad())?;
        if den <= 0.0 || num <= 0.0 {
            return Err(bad());
        }
        // A whole note (1/1) is 4 beats.
        4.0 * num / den
    } else {
        let n: f64 = body.parse().map_err(|_| bad())?;
        if n <= 0.0 || !n.is_finite() {
            return Err(bad());
        }
        n
    };
    Ok(Some(beats * mult))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(s: &str) -> Option<f64> {
        parse_grid(s).unwrap()
    }

    #[test]
    fn parses_common_grids() {
        assert_eq!(g("off"), None);
        assert_eq!(g(""), None);
        assert_eq!(g("NONE"), None);
        assert_eq!(g("1/4"), Some(1.0));
        assert_eq!(g("1/8"), Some(0.5));
        assert_eq!(g("1/16"), Some(0.25));
        assert_eq!(g("1/1"), Some(4.0));
        assert_eq!(g(" 1/2 "), Some(2.0));
        assert!((g("1/8t").unwrap() - 1.0 / 3.0).abs() < 1e-12);
        assert!((g("1/4T").unwrap() - 2.0 / 3.0).abs() < 1e-12);
        assert_eq!(g("1/8."), Some(0.75));
        assert_eq!(g("0.5"), Some(0.5));
        assert_eq!(g("2"), Some(2.0));
    }

    #[test]
    fn rejects_garbage() {
        for s in ["x", "1/0", "-1", "1/", "/4", "1/8x", "nan"] {
            assert!(parse_grid(s).is_err(), "{s} should fail");
        }
        assert!(parse_grid("abc")
            .unwrap_err()
            .to_string()
            .contains("bad grid `abc`"));
    }
}
