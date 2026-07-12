//! Path containment + secret deny-glob for every coding tool.
//!
//! The single rule: no read or write may resolve outside the CodingWorkspace root. This is
//! enforced by CANONICALIZING every path argument (resolving symlinks + `..` + Windows case)
//! and rejecting anything whose canonical form is not under the canonical workspace root.
//! Fail-closed: an unparseable/unresolvable path is an error, never a silent pass.

use anyhow::{anyhow, bail, Result};
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

/// Resolve a workspace-relative path argument to an absolute path proven to sit inside
/// `root_canon` (which MUST already be canonicalized by the caller — see
/// [`canonical_root`]).
///
/// Rejects, before touching the filesystem: absolute paths (other-drive/UNC included),
/// any `..` component, and any Windows drive/root prefix. Then canonicalizes the nearest
/// EXISTING ancestor (so a not-yet-created file for `fs_write` still resolves, while any
/// symlink in the existing prefix is followed and re-checked) and re-appends the
/// non-existent tail. The final path must be under `root_canon` or this fails.
///
/// A symlink INSIDE the repo pointing OUTSIDE it is caught here: canonicalizing the
/// existing ancestor resolves the link's real target, which then fails the containment
/// check. Windows case-insensitive bypass (`SRC/MAIN.RS` vs `src/main.rs`) is caught
/// because canonicalization returns the on-disk canonical casing for both root and path.
///
/// # Errors
/// Returns an error for an absolute path, a `..`/prefix component, an unresolvable
/// ancestor, or a resolved path that escapes `root_canon`.
pub fn resolve_in_workspace(root_canon: &Path, rel: &str) -> Result<PathBuf> {
    let rel_path = Path::new(rel);
    if rel_path.as_os_str().is_empty() {
        bail!("empty path argument");
    }
    if rel_path.is_absolute() {
        bail!("absolute path not allowed inside a workspace: {rel}");
    }
    for comp in rel_path.components() {
        match comp {
            Component::ParentDir => bail!("path traversal (`..`) not allowed: {rel}"),
            Component::Prefix(_) | Component::RootDir => {
                bail!("drive/root prefix not allowed inside a workspace: {rel}")
            }
            Component::CurDir | Component::Normal(_) => {}
        }
    }

    let joined = root_canon.join(rel_path);

    // Walk up to the nearest existing ancestor, canonicalizing it (resolves symlinks +
    // case), collecting the non-existent tail so a new file still resolves.
    let mut ancestor: &Path = joined.as_path();
    let mut tail: Vec<OsString> = Vec::new();
    let canon = loop {
        match ancestor.canonicalize() {
            Ok(c) => break c,
            Err(_) => {
                let name = ancestor
                    .file_name()
                    .ok_or_else(|| anyhow!("cannot resolve path within workspace: {rel}"))?;
                tail.push(name.to_os_string());
                ancestor = ancestor
                    .parent()
                    .ok_or_else(|| anyhow!("path has no parent within workspace: {rel}"))?;
            }
        }
    };

    let mut full = canon;
    for name in tail.iter().rev() {
        full.push(name);
    }

    if !full.starts_with(root_canon) {
        bail!("path escapes workspace root: {rel}");
    }
    Ok(full)
}

/// Canonicalize a workspace root once (resolving symlinks + case), for reuse across many
/// [`resolve_in_workspace`] calls.
///
/// # Errors
/// Returns an error if `root` does not exist or is not resolvable.
pub fn canonical_root(root: &Path) -> Result<PathBuf> {
    root.canonicalize()
        .map_err(|e| anyhow!("workspace root not resolvable ({}): {e}", root.display()))
}

/// True if `rel` matches the secret deny-glob (case-folded): `.env*`, `*.pem`, `id_rsa*`,
/// `.git/config`, or any path containing `secret`/`token`. Applied to EVERY read path
/// (fs_read, fs_grep, exemplar selection) so a credential file can never be surfaced to the
/// model, regardless of the platform's filesystem case sensitivity.
pub fn is_secret_path(rel: &str) -> bool {
    let lower = rel.replace('\\', "/").to_ascii_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(lower.as_str());
    name.starts_with(".env")
        || name.ends_with(".pem")
        || name.starts_with("id_rsa")
        || lower == ".git/config"
        || lower.ends_with("/.git/config")
        || name.contains("secret")
        || name.contains("token")
        || name.contains("credential") // cargo/registry credentials, .netrc-style creds
}

/// True if `rel` targets `.git/hooks/*` — a git-hook write is a config-redirection vector
/// (fires arbitrary code on git ops), so coding write tools reject it outright rather than
/// journaling+applying it. Case-folded for Windows.
pub fn is_git_hook_path(rel: &str) -> bool {
    let lower = rel.replace('\\', "/").to_ascii_lowercase();
    lower == ".git/hooks" || lower.starts_with(".git/hooks/") || lower.contains("/.git/hooks/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src").join("main.rs"), "fn main() {}").unwrap();
        dir
    }

    #[test]
    fn accepts_normal_relative_path() {
        let ws = workspace();
        let root = canonical_root(ws.path()).unwrap();
        let p = resolve_in_workspace(&root, "src/main.rs").unwrap();
        assert!(p.starts_with(&root));
    }

    #[test]
    fn accepts_new_file_under_root() {
        // fs_write to a not-yet-existent file must resolve (parent exists).
        let ws = workspace();
        let root = canonical_root(ws.path()).unwrap();
        let p = resolve_in_workspace(&root, "src/new_module.rs").unwrap();
        assert!(p.starts_with(&root));
    }

    #[test]
    fn rejects_parent_traversal() {
        let ws = workspace();
        let root = canonical_root(ws.path()).unwrap();
        assert!(resolve_in_workspace(&root, "../escape.txt").is_err());
        assert!(resolve_in_workspace(&root, "src/../../escape.txt").is_err());
    }

    #[test]
    fn rejects_absolute_path() {
        let ws = workspace();
        let root = canonical_root(ws.path()).unwrap();
        #[cfg(windows)]
        {
            assert!(resolve_in_workspace(&root, "C:\\Windows\\System32\\drivers\\etc\\hosts").is_err());
            assert!(resolve_in_workspace(&root, "\\\\server\\share\\x").is_err());
        }
        #[cfg(not(windows))]
        assert!(resolve_in_workspace(&root, "/etc/passwd").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_pointing_outside_root() {
        use std::os::unix::fs::symlink;
        let ws = workspace();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "leak").unwrap();
        symlink(outside.path(), ws.path().join("escape_link")).unwrap();
        let root = canonical_root(ws.path()).unwrap();
        // A path through the symlink resolves outside the root → rejected.
        assert!(resolve_in_workspace(&root, "escape_link/secret.txt").is_err());
    }

    #[cfg(windows)]
    #[test]
    fn windows_case_variation_still_contained() {
        // A case-varied but in-root path must resolve and stay contained (not a bypass).
        let ws = workspace();
        let root = canonical_root(ws.path()).unwrap();
        let p = resolve_in_workspace(&root, "SRC/MAIN.RS").unwrap();
        assert!(p.starts_with(&root));
    }

    #[test]
    fn secret_deny_glob_is_case_folded() {
        assert!(is_secret_path(".env"));
        assert!(is_secret_path(".env.local"));
        assert!(is_secret_path(".ENV")); // Windows case bypass attempt
        assert!(is_secret_path("keys/server.PEM"));
        assert!(is_secret_path("id_rsa"));
        assert!(is_secret_path("deploy/id_rsa.pub"));
        assert!(is_secret_path(".git/config"));
        assert!(is_secret_path("config/app_secret.json"));
        assert!(is_secret_path("auth/API_TOKEN.txt"));
        assert!(is_secret_path(".cargo/credentials"));
        assert!(is_secret_path("credentials.toml"));
        assert!(!is_secret_path("src/main.rs"));
        assert!(!is_secret_path("README.md"));
    }

    #[test]
    fn git_hook_path_detected_case_folded() {
        assert!(is_git_hook_path(".git/hooks/pre-commit"));
        assert!(is_git_hook_path(".GIT/HOOKS/pre-commit"));
        assert!(!is_git_hook_path("src/hooks.rs"));
    }
}
