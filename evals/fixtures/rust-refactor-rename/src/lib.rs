//! Planted defect (the task): this declares `pub mod math;`, which Rust resolves to `src/math.rs`
//! — but the module file is currently `src/old_math.rs`, so the crate does not compile. Rename
//! `src/old_math.rs` to `src/math.rs` (a file move) to fix it. Do NOT change the test.

pub mod math;

#[cfg(test)]
mod tests {
    #[test]
    fn uses_the_renamed_module() {
        assert_eq!(crate::math::add(2, 2), 4);
    }
}
