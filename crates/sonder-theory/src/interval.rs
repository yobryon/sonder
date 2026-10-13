//! Interval names.

/// Name an interval in semitones ("major 3rd", "tritone", "octave"). Negative
/// intervals are named by their size. Compound intervals are named up to two
/// octaves; anything wider is "compound".
pub fn interval_name(semitones: i32) -> &'static str {
    const SIMPLE: [&str; 13] = [
        "unison",
        "minor 2nd",
        "major 2nd",
        "minor 3rd",
        "major 3rd",
        "perfect 4th",
        "tritone",
        "perfect 5th",
        "minor 6th",
        "major 6th",
        "minor 7th",
        "major 7th",
        "octave",
    ];
    const COMPOUND: [&str; 12] = [
        "minor 9th",
        "major 9th",
        "minor 10th",
        "major 10th",
        "perfect 11th",
        "augmented 11th",
        "perfect 12th",
        "minor 13th",
        "major 13th",
        "minor 14th",
        "major 14th",
        "two octaves",
    ];
    let n = semitones.unsigned_abs() as usize;
    match n {
        0..=12 => SIMPLE[n],
        13..=24 => COMPOUND[n - 13],
        _ => "compound",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn names() {
        assert_eq!(interval_name(0), "unison");
        assert_eq!(interval_name(-7), "perfect 5th");
        assert_eq!(interval_name(6), "tritone");
        assert_eq!(interval_name(12), "octave");
        assert_eq!(interval_name(14), "major 9th");
        assert_eq!(interval_name(24), "two octaves");
        assert_eq!(interval_name(40), "compound");
    }
}
