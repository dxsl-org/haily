//! Planted defect (the task): `add` mixes `i32` and `i64`, so the crate does not compile.
//! Fix the body so it builds and the test passes — do NOT change the test.

pub fn add(a: i32, b: i32) -> i32 {
    // BUG: `b as i64` makes this an i64 expression, which cannot be returned as i32.
    a + b as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_two_numbers() {
        assert_eq!(add(2, 3), 5);
        assert_eq!(add(-1, 1), 0);
    }
}
