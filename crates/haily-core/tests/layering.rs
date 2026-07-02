//! Regression guard for the architectural invariant documented in CLAUDE.md:
//! "haily-core must never import from haily-io". They communicate only via
//! tokio mpsc channels carrying `haily-types` DTOs. If this test starts failing,
//! someone re-added a `haily-io` dependency to `haily-core` — route the fix
//! through `haily-types` instead.

const CORE_CARGO_TOML: &str = include_str!("../Cargo.toml");

#[test]
fn haily_core_does_not_depend_on_haily_io() {
    let has_io_dependency = CORE_CARGO_TOML
        .lines()
        .any(|line| line.trim_start().starts_with("haily-io"));

    assert!(
        !has_io_dependency,
        "haily-core/Cargo.toml must not depend on haily-io — \
         haily-core never imports the adapter layer (see CLAUDE.md). \
         Move any shared types into haily-types instead."
    );
}
