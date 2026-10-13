//! Rhythm helpers.

/// Euclidean rhythm (Bjorklund): distribute `pulses` onsets as evenly as
/// possible over `steps` steps, then rotate right by `rotation`.
///
/// `euclidean(8, 3, 0)` → `x..x..x.`; `euclidean(16, 4, 0)` → four-on-the-floor.
pub fn euclidean(steps: usize, pulses: usize, rotation: usize) -> Vec<bool> {
    if steps == 0 {
        return Vec::new();
    }
    let pulses = pulses.min(steps);
    // Bresenham-style bucket fill is equivalent to Bjorklund's algorithm up to
    // rotation, and always lands the first pulse on step 0.
    let raw: Vec<bool> = (0..steps).map(|i| (i * pulses) % steps < pulses).collect();
    let r = rotation % steps;
    let mut out = vec![false; steps];
    for (i, &v) in raw.iter().enumerate() {
        out[(i + r) % steps] = v;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(p: &[bool]) -> String {
        p.iter().map(|&b| if b { 'x' } else { '.' }).collect()
    }

    #[test]
    fn classic_patterns() {
        assert_eq!(s(&euclidean(8, 3, 0)), "x..x..x.");
        assert_eq!(s(&euclidean(8, 5, 0)), "x.xx.xx.");
        assert_eq!(s(&euclidean(16, 4, 0)), "x...x...x...x...");
        assert_eq!(s(&euclidean(4, 4, 0)), "xxxx");
        assert_eq!(s(&euclidean(5, 0, 0)), ".....");
        assert!(euclidean(0, 3, 0).is_empty());
    }

    #[test]
    fn rotation_and_counts() {
        assert_eq!(s(&euclidean(8, 3, 1)), ".x..x..x");
        for steps in 1..20 {
            for pulses in 0..=steps {
                let p = euclidean(steps, pulses, 3);
                assert_eq!(p.iter().filter(|&&b| b).count(), pulses);
            }
        }
        assert_eq!(euclidean(8, 12, 0).iter().filter(|&&b| b).count(), 8);
    }
}
