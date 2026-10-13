//! Rhythm helpers.

/// Euclidean rhythm (Bjorklund): distribute `pulses` onsets as evenly as
/// possible over `steps` steps, then rotate right by `rotation`.
///
/// `euclidean(8, 3, 0)` → `x..x..x.`; `euclidean(16, 4, 0)` → four-on-the-floor.
pub fn euclidean(steps: usize, pulses: usize, rotation: usize) -> Vec<bool> {
    if steps == 0 {
        return Vec::new();
    }
    let raw = if pulses == 0 {
        vec![false; steps]
    } else if pulses >= steps {
        vec![true; steps]
    } else {
        bjorklund(steps, pulses)
    };
    let r = rotation % steps;
    let mut out = vec![false; steps];
    for (i, &v) in raw.iter().enumerate() {
        out[(i + r) % steps] = v;
    }
    out
}

/// Bjorklund's algorithm proper (the canonical "x.xx.xx." shapes), with the
/// result rotated so the first pulse lands on step 0.
fn bjorklund(steps: usize, pulses: usize) -> Vec<bool> {
    let mut counts: Vec<usize> = Vec::new();
    let mut remainders: Vec<usize> = vec![pulses];
    let mut divisor = steps - pulses;
    let mut level = 0usize;
    loop {
        counts.push(divisor / remainders[level]);
        remainders.push(divisor % remainders[level]);
        divisor = remainders[level];
        level += 1;
        if remainders[level] <= 1 {
            break;
        }
    }
    counts.push(divisor);

    fn build(level: i32, counts: &[usize], remainders: &[usize], out: &mut Vec<bool>) {
        match level {
            -1 => out.push(false),
            -2 => out.push(true),
            l => {
                let l = l as usize;
                for _ in 0..counts[l] {
                    build(level - 1, counts, remainders, out);
                }
                if remainders[l] != 0 {
                    build(level - 2, counts, remainders, out);
                }
            }
        }
    }
    let mut out = Vec::with_capacity(steps);
    build(level as i32, &counts, &remainders, &mut out);
    out.truncate(steps);
    while out.len() < steps {
        out.push(false);
    }
    if let Some(first) = out.iter().position(|&b| b) {
        out.rotate_left(first);
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
