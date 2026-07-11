//! Live CDP browser driver (Phase 13) — behind the `browser` cargo feature.
//!
//! Singleton [`BrowserManager`]: one shared Chromium/CloakBrowser process for every browser tool
//! (mirrors the prior go-rod `globalBrowser`). Binary discovery priority and the `isCloakBrowser`
//! gate come from [`crate::browser::stealth`]; the network-allowed sandbox profile + loopback CDP
//! bind come from [`crate::exec::sandbox`].
//!
//! DEFERRED (LOCKED decision 7): the live CDP smoke (render a JS page, verify stealth against a
//! fingerprint test page, with and without CloakBrowser) is a manual step — this driver is
//! verified only to COMPILE under the feature here. Xvfb virtual-display support (a Linux-server
//! nicety for realistic Mesa WebGL) is deferred to a follow-up; the `--headless=new` fallback is
//! used when no display is present (see Deviation Log).

use crate::browser::stealth;
use crate::exec::sandbox::BROWSER_CDP_BIND_ADDR;
use anyhow::{anyhow, Context, Result};
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::page::Page;
use futures::StreamExt;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::sync::Mutex;

/// Wall-clock ceiling for launching/connecting to the browser.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(20);

/// Process-wide shared browser. Lazily launched on first use; reused across every tool call and
/// session so login state (the persistent `--user-data-dir`) survives.
static GLOBAL: OnceLock<Mutex<BrowserManager>> = OnceLock::new();

/// Accessor for the shared [`BrowserManager`].
pub fn global() -> &'static Mutex<BrowserManager> {
    GLOBAL.get_or_init(|| Mutex::new(BrowserManager::new()))
}

/// Holds the shared browser connection and the active binary path (for the CloakBrowser gate).
pub struct BrowserManager {
    browser: Option<Browser>,
    active_path: String,
}

impl BrowserManager {
    fn new() -> Self {
        Self { browser: None, active_path: String::new() }
    }

    /// `true` when the active binary is CloakBrowser — the signal to SKIP the software stealth
    /// (the binary does TLS/JA3 + fingerprint at C++ level).
    pub fn is_cloak(&self) -> bool {
        stealth::is_cloak_browser(&self.active_path)
    }

    /// Open a fresh page, apply the software stealth layer (unless on CloakBrowser), and return
    /// it. The stealth script is injected via `evaluate_on_new_document` (main world, BEFORE any
    /// page JS, WITHOUT enabling the CDP `Runtime` domain — the key vector).
    pub async fn new_page(&mut self) -> Result<Page> {
        let is_cloak = self.is_cloak_after_acquire().await?;
        let browser = self
            .browser
            .as_ref()
            .ok_or_else(|| anyhow!("browser not acquired"))?;
        let page = browser
            .new_page("about:blank")
            .await
            .context("creating browser page")?;

        if !is_cloak {
            // Break the zero-latency create→inject CDP burst (an automation fingerprint).
            let jitter = stealth::CDP_JITTER_MIN_MS
                + (rand::random::<f64>()
                    * (stealth::CDP_JITTER_MAX_MS - stealth::CDP_JITTER_MIN_MS) as f64)
                    as u64;
            tokio::time::sleep(Duration::from_millis(jitter)).await;
            page.evaluate_on_new_document(stealth::ANTI_DETECTION_SCRIPT)
                .await
                .context("injecting anti-detection script")?;
        }
        Ok(page)
    }

    /// Ensure a live browser is connected, launching one if needed, and report the CloakBrowser
    /// gate for the freshly-acquired connection.
    async fn is_cloak_after_acquire(&mut self) -> Result<bool> {
        if self.browser.is_none() {
            self.launch().await?;
        }
        Ok(self.is_cloak())
    }

    /// Connect to `HAILY_CDP_URL` if set (remote/external CloakBrowser or cloud browser), else
    /// discover a local binary and launch it with the stealth flags + persistent profile dir.
    async fn launch(&mut self) -> Result<()> {
        if let Ok(cdp_url) = std::env::var(stealth::ENV_CDP_URL) {
            if !cdp_url.is_empty() {
                let (browser, mut handler) =
                    tokio::time::timeout(LAUNCH_TIMEOUT, Browser::connect(cdp_url.clone()))
                        .await
                        .context("timed out connecting to HAILY_CDP_URL")?
                        .with_context(|| format!("connecting to HAILY_CDP_URL {cdp_url:?}"))?;
                spawn_handler(async move { while handler.next().await.is_some() {} });
                // A remote CDP endpoint is treated as CloakBrowser when the URL names it.
                self.active_path = cdp_url;
                self.browser = Some(browser);
                return Ok(());
            }
        }

        let env_override = std::env::var(stealth::ENV_BROWSER_PATH).ok();
        let path = stealth::find_browser_binary(env_override, |p| Path::new(p).exists())
            .ok_or_else(|| {
                anyhow!("no browser found — install Chrome/CloakBrowser or set HAILY_BROWSER_PATH")
            })?;
        let is_cloak = stealth::is_cloak_browser(&path);

        let user_data_dir = profile_dir();
        std::fs::create_dir_all(&user_data_dir).ok();
        let port = free_port()?;
        // The stealth flag set is canonical (tested in `stealth`); chromiumoxide owns the port +
        // user-data-dir + executable via its builder, so pass only the remaining flags as args.
        let flags: Vec<String> =
            stealth::build_launch_flags(port, &user_data_dir, is_cloak, BROWSER_CDP_BIND_ADDR)
                .into_iter()
                .filter(|f| {
                    !f.starts_with("--remote-debugging-port=")
                        && !f.starts_with("--user-data-dir=")
                        && f != "about:blank"
                })
                .collect();

        let mut builder = BrowserConfig::builder()
            .chrome_executable(&path)
            .user_data_dir(&user_data_dir)
            .port(port)
            .args(flags);
        // No real display (headless server) → --headless=new. A visible display gets a headful
        // window (more realistic fingerprint).
        if headless_required() {
            builder = builder.new_headless_mode();
        } else {
            builder = builder.with_head();
        }
        let config = builder
            .build()
            .map_err(|e| anyhow!("browser config build failed: {e}"))?;

        let (browser, mut handler) = tokio::time::timeout(LAUNCH_TIMEOUT, Browser::launch(config))
            .await
            .context("timed out launching browser")?
            .context("launching browser")?;
        spawn_handler(async move { while handler.next().await.is_some() {} });
        self.active_path = path;
        self.browser = Some(browser);
        Ok(())
    }
}

/// Spawn the CDP handler-polling task. The `Handler` stream MUST be driven for the connection to
/// make progress; it runs until the browser closes (the stream ends).
fn spawn_handler<F>(fut: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    tokio::task::spawn(fut);
}

/// `true` when no real display is available (a headless server) — use `--headless=new`. On
/// Windows/macOS a display is always present.
fn headless_required() -> bool {
    #[cfg(target_os = "linux")]
    {
        std::env::var("DISPLAY").map(|d| d.is_empty()).unwrap_or(true)
            && std::env::var("WAYLAND_DISPLAY").map(|d| d.is_empty()).unwrap_or(true)
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// The persistent browser profile dir (login sessions survive across runs). Confined per the P0
/// network-allowed browser sandbox profile.
fn profile_dir() -> String {
    let base = std::env::var("APPDATA")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_else(|_| ".".to_string());
    Path::new(&base)
        .join("haily-browser")
        .to_string_lossy()
        .into_owned()
}

/// Bind an ephemeral loopback port, then release it for the browser to claim (matches the prior
/// `freePort`). Loopback-only, so the CDP endpoint is never exposed off-host.
fn free_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind((BROWSER_CDP_BIND_ADDR, 0))
        .context("binding a free loopback port for CDP")?;
    let port = listener.local_addr()?.port();
    Ok(port)
}

// ── human behavior (CDP dispatch of the pure math in `super::human`) ─────────────

use chromiumoxide::cdp::browser_protocol::input::{
    DispatchMouseEventParams, DispatchMouseEventType, InsertTextParams, MouseButton,
};
use chromiumoxide::element::Element;

/// Type `text` into a focused `el` character-by-character with realistic per-rune jitter and an
/// occasional typo→backspace→fix, generating real keydown/keyup CDP events (via `type_str`).
/// Non-ASCII (Vietnamese/CJK/emoji) has no single keymap entry, so it is inserted via
/// `Input.insertText`. Cancellation-free bounded loop (each rune waits < 200 ms).
pub async fn human_type(page: &Page, el: &Element, text: &str) -> Result<()> {
    el.focus().await.context("focusing element for human typing")?;
    for ch in text.chars() {
        tokio::time::sleep(Duration::from_millis(super::human::type_delay_ms(
            rand::random::<f64>(),
        )))
        .await;
        if super::human::should_typo(rand::random::<f64>()) {
            let wrong = (b'a' + (rand::random::<f64>() * 26.0) as u8) as char;
            el.type_str(wrong.to_string()).await.ok();
            tokio::time::sleep(Duration::from_millis(120)).await;
            el.press_key("Backspace").await.ok();
            tokio::time::sleep(Duration::from_millis(60)).await;
        }
        if super::human::is_ascii_printable(ch) {
            el.type_str(ch.to_string())
                .await
                .context("typing character")?;
        } else {
            page.execute(InsertTextParams::new(ch.to_string()))
                .await
                .context("inserting non-ASCII text")?;
        }
    }
    Ok(())
}

/// Move the mouse to `el` along a quadratic Bézier curve (defeats teleport-then-click detectors),
/// hover briefly, then click. Sample count + curve come from the pure math in `super::human`.
pub async fn human_mouse_click(page: &Page, el: &Element) -> Result<()> {
    let target = el
        .clickable_point()
        .await
        .context("resolving element click point")?;
    let end = (target.x, target.y);
    // Start from the viewport origin (the manager does not track a live cursor position).
    let start = (0.0, 0.0);
    let ctrl = super::human::bezier_control(start, end, rand::random::<f64>() * 2.0 - 1.0);
    let steps = super::human::mouse_samples(rand::random::<f64>());
    for i in 1..=steps {
        let t = i as f64 / steps as f64;
        let (x, y) = super::human::bezier_point(start, ctrl, end, t);
        page.execute(DispatchMouseEventParams::new(
            DispatchMouseEventType::MouseMoved,
            x,
            y,
        ))
        .await
        .context("dispatching mouse move")?;
        tokio::time::sleep(Duration::from_millis(12)).await;
    }
    tokio::time::sleep(Duration::from_millis(80)).await; // hover pause
    // Press + release at the target for a real click.
    let mut press = DispatchMouseEventParams::new(DispatchMouseEventType::MousePressed, end.0, end.1);
    press.button = Some(MouseButton::Left);
    press.click_count = Some(1);
    page.execute(press).await.context("mouse press")?;
    let mut release =
        DispatchMouseEventParams::new(DispatchMouseEventType::MouseReleased, end.0, end.1);
    release.button = Some(MouseButton::Left);
    release.click_count = Some(1);
    page.execute(release).await.context("mouse release")?;
    Ok(())
}
