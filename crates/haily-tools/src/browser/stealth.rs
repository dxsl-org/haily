//! Software stealth layer (Phase 13) — the drop-in anti-detection port from haily.go.
//!
//! Feature-INDEPENDENT on purpose: the JS asset, the CloakBrowser binary-discovery seam, the
//! launch-flag set, and the CDP burst-timing jitter are all pure data/logic that the default
//! workspace build compiles and tests. Only the live CDP driver (`manager.rs`) is behind the
//! `browser` cargo feature.
//!
//! # Two-layer stealth model (see `reports/browser-tool-port.md`)
//! 1. Binary layer — CloakBrowser (49 C++ patches: TLS/JA3 + deep fingerprint). When the active
//!    binary is CloakBrowser, the software layer below is SKIPPED (`is_cloak_browser`) — the
//!    binary does it at C++ level. CloakBrowser is an OPTIONAL external binary, pointed at via
//!    `HAILY_BROWSER_PATH` (local) or `HAILY_CDP_URL` (remote).
//! 2. Software layer — applied only when NOT on CloakBrowser: this module.
//!
//! SECURITY NOTE: this is anti-detection for the OWNER's own single interactive session, so the
//! user's normal logged-in browsing is not blocked as "a bot". It is NOT proxy/UA rotation, NOT
//! scale evasion, and carries no multi-target/batch behaviour of any kind.

/// The anti-detection script, injected via `add_script_to_evaluate_on_new_document` BEFORE any
/// page JS runs, in the main world, WITHOUT enabling the CDP `Runtime` domain (the key vector
/// go-rod already avoids). Kept as an updatable ASSET (not a hard-coded string) so Chromium
/// version drift can be tracked without a code change (see Risk Notes in the phase file).
pub const ANTI_DETECTION_SCRIPT: &str = include_str!("anti_detection.js");

/// Environment variable pointing at an explicit browser binary (highest discovery priority).
pub const ENV_BROWSER_PATH: &str = "HAILY_BROWSER_PATH";
/// Environment variable pointing at a remote/external CDP endpoint (e.g. a remote CloakBrowser
/// or a cloud browser). When set, the manager connects to it directly instead of launching.
pub const ENV_CDP_URL: &str = "HAILY_CDP_URL";

/// Loopback CDP jitter window, in milliseconds: after page-create, wait 50–149 ms before the
/// first script injection so the zero-latency `create → inject` burst (an automation
/// fingerprint) is broken. Skipped on CloakBrowser (binary normalizes timing).
pub const CDP_JITTER_MIN_MS: u64 = 50;
pub const CDP_JITTER_MAX_MS: u64 = 149;

/// `true` when `active_path` is the CloakBrowser binary — the signal to SKIP the software
/// stealth layer (the binary handles TLS/JA3 + fingerprint at C++ level). Case-insensitive
/// substring match on the path, mirroring the prior Go `isCloakBrowser`.
pub fn is_cloak_browser(active_path: &str) -> bool {
    active_path.to_lowercase().contains("cloakbrowser")
}

/// Resolve a browser binary path by discovery priority (highest anti-detection first):
///   1. `HAILY_BROWSER_PATH` (explicit override, if it exists on disk)
///   2. CloakBrowser (49 C++ patches — best antibot)
///   3. Google Chrome
///   4. Chromium
///
/// Returns `None` when no binary is found — the caller then reports "no browser found — install
/// Chrome/CloakBrowser or set HAILY_BROWSER_PATH" rather than silently degrading.
///
/// `path_exists` is injected (rather than calling `Path::exists` directly) so the discovery
/// ORDER is unit-testable without a real browser installed.
pub fn find_browser_binary(
    env_override: Option<String>,
    path_exists: impl Fn(&str) -> bool,
) -> Option<String> {
    if let Some(p) = env_override {
        if !p.is_empty() && path_exists(&p) {
            return Some(p);
        }
    }
    for candidate in binary_candidates() {
        if path_exists(candidate) {
            return Some(candidate.to_string());
        }
    }
    None
}

/// Per-OS candidate binary paths, CloakBrowser first. Kept as a function (not a const) so the
/// `cfg!(target_os)` selection is evaluated at call time and the full list is testable.
pub fn binary_candidates() -> &'static [&'static str] {
    #[cfg(target_os = "windows")]
    {
        &[
            r"C:\Program Files\CloakBrowser\Application\cloakbrowser.exe",
            r"C:\Program Files\Google\Chrome\Application\chrome.exe",
            r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
        ]
    }
    #[cfg(target_os = "macos")]
    {
        &[
            "/Applications/CloakBrowser.app/Contents/MacOS/CloakBrowser",
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            "/Applications/Chromium.app/Contents/MacOS/Chromium",
        ]
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        &[
            "/usr/bin/cloakbrowser",
            "/usr/bin/google-chrome",
            "/usr/bin/chromium",
            "/usr/bin/chromium-browser",
            "/snap/bin/chromium",
        ]
    }
}

/// Build the Chrome/Chromium launch flags for a loopback-only CDP endpoint on `port`, confined
/// to `user_data_dir`. Ports the verbatim flag set from haily.go's `buildChromeCmd`:
///   - `--disable-blink-features=AutomationControlled` (the primary `navigator.webdriver` tell)
///   - `--remote-debugging-address=127.0.0.1` (LOOPBACK-ONLY — the debug port is full remote
///     control and must never be exposed off-host)
///   - persistent `--user-data-dir` (login sessions survive across runs)
///
/// When `is_cloak` is true the extra Chrome-only stealth flags are OMITTED — CloakBrowser does
/// the equivalent at binary level. `bind_addr` MUST be the loopback address (see
/// [`crate::exec::sandbox::BROWSER_CDP_BIND_ADDR`]).
pub fn build_launch_flags(
    port: u16,
    user_data_dir: &str,
    is_cloak: bool,
    bind_addr: &str,
) -> Vec<String> {
    let mut flags = vec![
        format!("--remote-debugging-port={port}"),
        format!("--remote-debugging-address={bind_addr}"),
        "--no-first-run".to_string(),
        "--no-default-browser-check".to_string(),
        "--disable-blink-features=AutomationControlled".to_string(),
        "--disable-infobars".to_string(),
        "--disable-extensions".to_string(),
        "--disable-background-networking".to_string(),
        "--disable-sync".to_string(),
        "--password-store=basic".to_string(),
        "--disable-default-apps".to_string(),
        format!("--user-data-dir={user_data_dir}"),
        "--window-size=1280,800".to_string(),
    ];
    if !is_cloak {
        flags.extend(
            [
                "--disable-features=Translate,OptimizationHints,MediaRouter,CalculateNativeWinOcclusion",
                "--metrics-recording-only",
                "--hide-scrollbars",
                "--mute-audio",
                "--disable-client-side-phishing-detection",
                "--disable-component-extensions-with-background-pages",
                "--disable-ipc-flooding-protection",
            ]
            .iter()
            .map(|s| s.to_string()),
        );
    }
    flags.push("about:blank".to_string());
    flags
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::sandbox::BROWSER_CDP_BIND_ADDR;

    #[test]
    fn anti_detection_asset_present_and_covers_key_vectors() {
        let js = ANTI_DETECTION_SCRIPT;
        assert!(!js.is_empty(), "stealth JS asset must be embedded");
        // The load-bearing anti-bot vectors must all be present in the asset.
        assert!(js.contains("navigator, 'webdriver'"), "webdriver override missing");
        assert!(js.contains("return undefined"), "webdriver must resolve undefined");
        assert!(js.contains("getParameter"), "WebGL getParameter patch missing");
        assert!(js.contains("37445") && js.contains("37446"), "WebGL vendor/renderer patch missing");
        assert!(js.contains("toDataURL"), "canvas noise patch missing");
        assert!(js.contains("navigator, 'plugins'"), "plugins override missing");
        assert!(js.contains("window.chrome"), "window.chrome override missing");
        assert!(js.contains("userAgentData"), "userAgentData override missing");
        assert!(js.contains("getChannelData"), "audio fingerprint patch missing");
    }

    #[test]
    fn is_cloak_browser_matches_case_insensitively() {
        assert!(is_cloak_browser(r"C:\Program Files\CloakBrowser\Application\cloakbrowser.exe"));
        assert!(is_cloak_browser("/usr/bin/CLOAKBROWSER"));
        assert!(!is_cloak_browser("/usr/bin/google-chrome"));
        assert!(!is_cloak_browser("/usr/bin/chromium"));
    }

    #[test]
    fn binary_discovery_honors_env_override_first() {
        // The explicit override wins over any installed candidate, but only if it exists.
        let found = find_browser_binary(Some("/opt/custom/chrome".to_string()), |p| {
            p == "/opt/custom/chrome"
        });
        assert_eq!(found.as_deref(), Some("/opt/custom/chrome"));
    }

    #[test]
    fn binary_discovery_skips_missing_override_then_falls_to_candidates() {
        // Override points at a non-existent file → fall through to the candidate list. Here we
        // make only the FIRST candidate (CloakBrowser) "exist" to prove the priority order.
        let cloak = binary_candidates()[0];
        let found = find_browser_binary(Some("/does/not/exist".to_string()), |p| p == cloak);
        assert_eq!(found.as_deref(), Some(cloak));
    }

    #[test]
    fn binary_discovery_returns_none_when_nothing_found() {
        assert!(find_browser_binary(None, |_| false).is_none());
    }

    #[test]
    fn launch_flags_pin_loopback_and_disable_automation_tell() {
        let flags = build_launch_flags(9333, "/tmp/haily-browser", false, BROWSER_CDP_BIND_ADDR);
        assert!(flags.iter().any(|f| f == "--remote-debugging-address=127.0.0.1"),
            "CDP must bind loopback-only");
        assert!(flags.iter().any(|f| f == "--remote-debugging-port=9333"));
        assert!(flags.iter().any(|f| f == "--disable-blink-features=AutomationControlled"));
        assert!(flags.iter().any(|f| f == "--user-data-dir=/tmp/haily-browser"),
            "persistent profile dir must be set");
        assert_eq!(flags.last().map(String::as_str), Some("about:blank"));
    }

    #[test]
    fn launch_flags_omit_extra_stealth_on_cloakbrowser() {
        // CloakBrowser handles the extra stealth at binary level — the software flags are
        // omitted so we don't double-apply (and risk a mismatch the binary already avoids).
        let cloak = build_launch_flags(9333, "/tmp/p", true, BROWSER_CDP_BIND_ADDR);
        let chrome = build_launch_flags(9333, "/tmp/p", false, BROWSER_CDP_BIND_ADDR);
        assert!(!cloak.iter().any(|f| f.contains("--metrics-recording-only")));
        assert!(chrome.iter().any(|f| f.contains("--metrics-recording-only")));
        // Both still pin loopback + automation-tell disable (those are unconditional).
        for flags in [&cloak, &chrome] {
            assert!(flags.iter().any(|f| f == "--remote-debugging-address=127.0.0.1"));
            assert!(flags.iter().any(|f| f == "--disable-blink-features=AutomationControlled"));
        }
    }

    #[test]
    fn cdp_jitter_window_is_the_ported_50_to_149ms() {
        assert_eq!(CDP_JITTER_MIN_MS, 50);
        assert_eq!(CDP_JITTER_MAX_MS, 149);
    }
}
