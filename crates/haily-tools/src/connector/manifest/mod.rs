//! Connector manifest schema + version diffing, split for focus:
//! [`schema`] defines/parses the manifest document (`Manifest`, `OpSpec`, v2's `AuthSpec` +
//! `ProtocolSpec`); [`diff`] compares two versions against the re-approval whitelist.
mod diff;
mod schema;

pub use diff::{approved_version_pref_key, check_version, manifest_diff, ManifestDiff, OpDiff, VersionCheck};
pub use schema::{
    parse, AuthSpec, FaultRule, Manifest, MethodShape, ModelRequiredFields, OpSpec, ProtocolSpec,
    ReadbackSpec, ResolvedAuthScheme,
};
