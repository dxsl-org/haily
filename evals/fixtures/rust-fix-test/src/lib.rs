//! Planted defect (the task): `rolling_max` computes a running *minimum* instead of a running
//! maximum, so it returns wrong values for a decreasing prefix. Fix the code so the tests pass —
//! do NOT change the tests.

/// The running maximum of `xs`: element `i` of the output is `max(xs[0..=i])`.
pub fn rolling_max(xs: &[i64]) -> Vec<i64> {
    let mut out = Vec::with_capacity(xs.len());
    let mut best: Option<i64> = None;
    for &x in xs {
        best = Some(match best {
            // BUG: this should keep the LARGER of the two, not the smaller.
            Some(b) => b.min(x),
            None => x,
        });
        out.push(best.unwrap());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_the_running_maximum() {
        assert_eq!(rolling_max(&[1, 3, 2, 5, 4]), vec![1, 3, 3, 5, 5]);
    }

    #[test]
    fn handles_a_decreasing_prefix_and_empty_input() {
        assert_eq!(rolling_max(&[5, 4, 3, 6]), vec![5, 5, 5, 6]);
        assert_eq!(rolling_max(&[]), Vec::<i64>::new());
    }
}
