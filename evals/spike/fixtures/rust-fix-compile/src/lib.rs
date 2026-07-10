//! SPIKE FIXTURE (throwaway). Defect: `add` has a trailing semicolon so it returns `()`
//! instead of `i32` — a type-mismatch compile error. Task: make `cargo test` pass by
//! returning the sum. The fix is a one-character delete; the point is to measure whether the
//! model orients, edits the right line, and re-runs the gate — not difficulty.

/// Return the sum of two integers.
pub fn add(a: i32, b: i32) -> i32 {
    a + b;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds() {
        assert_eq!(add(2, 3), 5);
        assert_eq!(add(-1, 1), 0);
    }
}
