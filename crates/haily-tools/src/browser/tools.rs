//! The three stealth-browser tools (Phase 13) — behind the `browser` cargo feature.
//!
//! `browser_navigate` (read → markdown), `browser_interact` (multi-step, mutations
//! approval-gated), `browser_session` (cookie list/export/import/clear). All page content is
//! routed through [`crate::security::html_to_text`] so untrusted markup is tag-stripped BEFORE it
//! reaches the model (LOCKED decision 4). Every op runs the shared browser under the P0
//! network-allowed sandbox profile (loopback CDP, credential-scrubbed env, confined profile dir).

use crate::browser::manager::{global, human_mouse_click, human_type};
use crate::browser::session::{map_same_site, SameSite};
use crate::browser::{interact_risk_tier, session_risk_tier};
use crate::{security, RiskTier, Tool, ToolContext};
use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use base64::Engine as _;
use chromiumoxide::cdp::browser_protocol::network::{CookieParam, CookieSameSite};
use chromiumoxide::cdp::browser_protocol::page::{CaptureScreenshotFormat, PrintToPdfParams};
use chromiumoxide::page::{Page, ScreenshotParams};
use serde_json::{json, Value};
use std::collections::HashMap;
use tokio::sync::Mutex;

/// Default / hard cap on characters returned from a rendered page.
const DEFAULT_MAX_CHARS: usize = 30_000;
const MAX_MAX_CHARS: usize = 60_000;

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Render the current page as `# title\n**URL:** url\n\n<stripped text>`, tag-stripped.
async fn page_markdown(page: &Page) -> Result<String> {
    let html = page.content().await.map_err(|e| anyhow!("reading page HTML: {e}"))?;
    let url = page.url().await.ok().flatten().unwrap_or_default();
    let title = page.get_title().await.ok().flatten().unwrap_or_default();
    let md = security::html_to_text(&html);
    Ok(format!("# {title}\n**URL:** {url}\n\n{md}"))
}

fn clamp_chars(s: String, max: usize) -> String {
    s.chars().take(max).collect()
}

/// Navigate + wait per `wait_for` ("load" default, "idle", or a CSS selector). Non-fatal on a
/// missing selector — the page is returned in its current state.
async fn navigate_and_wait(page: &Page, url: &str, _wait_for: &str) -> Result<()> {
    page.goto(url).await.map_err(|e| anyhow!("navigate: {e}"))?;
    page.wait_for_navigation().await.ok();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    Ok(())
}

// ── browser_navigate ─────────────────────────────────────────────────────────

/// Opens a URL in the shared stealth browser and returns the rendered page as markdown.
pub struct BrowserNavigateTool;

#[async_trait]
impl Tool for BrowserNavigateTool {
    fn name(&self) -> &str {
        "browser_navigate"
    }
    fn description(&self) -> &str {
        "Open a URL in a real browser (Chrome/CloakBrowser), run its JavaScript, and return the \
         rendered page as markdown. Use for JS-heavy or bot-walled sites that plain url_fetch \
         cannot read. Login sessions persist across calls. Read-only."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "Full http/https URL to open." },
                "wait_for": { "type": "string", "description": "'load' (default), 'idle', or a CSS selector." },
                "screenshot": { "type": "boolean", "description": "Also return a base64 PNG data URI." },
                "max_chars": { "type": "integer", "description": "Max characters (default 30000, max 60000)." }
            },
            "required": ["url"]
        })
    }
    fn risk_tier(&self, _args: &Value) -> RiskTier {
        RiskTier::Read
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<String> {
        let url = args["url"].as_str().ok_or_else(|| anyhow!("url is required"))?;
        if !url.starts_with("http://") && !url.starts_with("https://") {
            bail!("only http/https URLs are supported");
        }
        let wait_for = args["wait_for"].as_str().unwrap_or("load");
        let want_shot = args["screenshot"].as_bool().unwrap_or(false);
        let max_chars = args["max_chars"]
            .as_u64()
            .map(|v| (v as usize).min(MAX_MAX_CHARS))
            .filter(|v| *v > 0)
            .unwrap_or(DEFAULT_MAX_CHARS);

        let page = { global().lock().await.new_page().await? };
        navigate_and_wait(&page, url, wait_for).await?;

        let mut out = String::new();
        if want_shot {
            let params = ScreenshotParams::builder()
                .format(CaptureScreenshotFormat::Png)
                .build();
            if let Ok(img) = page.screenshot(params).await {
                out.push_str(&format!("[screenshot: data:image/png;base64,{}]\n\n", b64(&img)));
            }
        }
        out.push_str(&page_markdown(&page).await?);
        page.close().await.ok();
        Ok(clamp_chars(out, max_chars))
    }
}

// ── browser_interact ───────────────────────────────────────────────────────────

/// Multi-step interaction on a per-`session_key` page (state preserved across calls).
pub struct BrowserInteractTool {
    pages: Mutex<HashMap<String, Page>>,
}

impl BrowserInteractTool {
    pub fn new() -> Self {
        Self { pages: Mutex::new(HashMap::new()) }
    }

    async fn session_page(&self, key: &str) -> Result<Page> {
        let mut pages = self.pages.lock().await;
        if let Some(p) = pages.get(key) {
            if p.url().await.is_ok() {
                return Ok(p.clone());
            }
            pages.remove(key);
        }
        let page = global().lock().await.new_page().await?;
        pages.insert(key.to_string(), page.clone());
        Ok(page)
    }
}

impl Default for BrowserInteractTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for BrowserInteractTool {
    fn name(&self) -> &str {
        "browser_interact"
    }
    fn description(&self) -> &str {
        "Multi-step browser interaction: navigate, click, fill, scroll, surf, screenshot, pdf, \
         eval, snap (list interactive elements), content, close. Use session_key to keep state \
         across calls. Set human_type:true for realistic keystrokes/mouse on behavioral-\
         fingerprint sites. Mutations (click/fill/eval/pdf) require approval."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["navigate","click","fill","scroll","surf","screenshot","pdf","eval","snap","content","close"] },
                "session_key": { "type": "string", "description": "Session id (default 'default')." },
                "url": { "type": "string" },
                "selector": { "type": "string" },
                "value": { "type": "string", "description": "Text (fill), px (scroll), or JS (eval)." },
                "human_type": { "type": "boolean" },
                "full_page": { "type": "boolean" },
                "landscape": { "type": "boolean" },
                "steps": { "type": "integer" }
            },
            "required": ["action"]
        })
    }
    fn risk_tier(&self, args: &Value) -> RiskTier {
        interact_risk_tier(args["action"].as_str())
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<String> {
        let action = args["action"].as_str().ok_or_else(|| anyhow!("action is required"))?;
        let key = args["session_key"].as_str().unwrap_or("default");

        if action == "close" {
            let mut pages = self.pages.lock().await;
            if let Some(p) = pages.remove(key) {
                p.close().await.ok();
            }
            return Ok(format!("session {key:?} closed"));
        }

        let page = self.session_page(key).await?;
        let human = args["human_type"].as_bool().unwrap_or(false);

        match action {
            "navigate" => {
                let url = args["url"].as_str().ok_or_else(|| anyhow!("url required for navigate"))?;
                navigate_and_wait(&page, url, args["wait_for"].as_str().unwrap_or("load")).await?;
                let title = page.get_title().await.ok().flatten().unwrap_or_default();
                let cur = page.url().await.ok().flatten().unwrap_or_default();
                Ok(format!("navigated to: {cur}\ntitle: {title}"))
            }
            "click" => {
                let sel = args["selector"].as_str().ok_or_else(|| anyhow!("selector required for click"))?;
                let el = page.find_element(sel).await.map_err(|e| anyhow!("element not found {sel:?}: {e}"))?;
                if human {
                    human_mouse_click(&page, &el).await?;
                    Ok(format!("clicked: {sel} (human-mouse)"))
                } else {
                    el.click().await.map_err(|e| anyhow!("click failed: {e}"))?;
                    Ok(format!("clicked: {sel}"))
                }
            }
            "fill" => {
                let sel = args["selector"].as_str().ok_or_else(|| anyhow!("selector required for fill"))?;
                let val = args["value"].as_str().unwrap_or("");
                let el = page.find_element(sel).await.map_err(|e| anyhow!("element not found {sel:?}: {e}"))?;
                if human {
                    human_type(&page, &el, val).await?;
                    Ok(format!("filled {sel:?} ({} chars, human-typed)", val.len()))
                } else {
                    el.focus().await.ok();
                    el.type_str(val).await.map_err(|e| anyhow!("fill failed: {e}"))?;
                    Ok(format!("filled {sel:?} ({} chars)", val.len()))
                }
            }
            "scroll" => {
                let px: i64 = args["value"].as_str().and_then(|s| s.parse().ok()).unwrap_or(500);
                page.evaluate(format!("window.scrollBy(0, {px})"))
                    .await
                    .map_err(|e| anyhow!("scroll failed: {e}"))?;
                Ok(format!("scrolled {px} px"))
            }
            "surf" => {
                let steps = args["steps"].as_u64().map(|v| v.min(20)).unwrap_or(5);
                for i in 1..=steps {
                    let y = i as i64 * 560;
                    page.evaluate(format!("window.scrollTo(0, {y})")).await.ok();
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                }
                Ok(clamp_chars(page_markdown(&page).await?, DEFAULT_MAX_CHARS))
            }
            "screenshot" => {
                let params = ScreenshotParams::builder()
                    .format(CaptureScreenshotFormat::Png)
                    .full_page(args["full_page"].as_bool().unwrap_or(false))
                    .build();
                let img = page.screenshot(params).await.map_err(|e| anyhow!("screenshot failed: {e}"))?;
                Ok(format!("data:image/png;base64,{}", b64(&img)))
            }
            "pdf" => {
                let params = PrintToPdfParams::builder()
                    .landscape(args["landscape"].as_bool().unwrap_or(false))
                    .print_background(true)
                    .build();
                let data = page.pdf(params).await.map_err(|e| anyhow!("PDF failed: {e}"))?;
                Ok(format!("data:application/pdf;base64,{}", b64(&data)))
            }
            "eval" => {
                let expr = args["value"].as_str().ok_or_else(|| anyhow!("value (JS) required for eval"))?;
                let res = page.evaluate(expr).await.map_err(|e| anyhow!("eval failed: {e}"))?;
                let v: Value = res.into_value().unwrap_or(Value::Null);
                Ok(clamp_chars(v.to_string(), DEFAULT_MAX_CHARS))
            }
            "snap" => {
                let res = page.evaluate(SNAP_JS).await.map_err(|e| anyhow!("snap failed: {e}"))?;
                let v: Value = res.into_value().unwrap_or(Value::Null);
                let raw = v.as_str().unwrap_or("").to_string();
                if raw.is_empty() {
                    Ok("no interactive elements found in current viewport".to_string())
                } else {
                    Ok(raw)
                }
            }
            "content" => Ok(clamp_chars(page_markdown(&page).await?, DEFAULT_MAX_CHARS)),
            other => bail!("unknown action {other:?}"),
        }
    }
}

/// Accessibility-tree snapshot JS (ported from the prior `snap` command) — returns interactive
/// elements as `[i] role "name" (x,y) [state]`, capped at 80 entries.
const SNAP_JS: &str = r#"(function(){var sel='a[href], button, input:not([type=hidden]), textarea, select, [role=button], [role=link], [role=textbox], [role=checkbox], [role=combobox], [role=menuitem], [role=option], [aria-label]';var nodes=[];Array.from(document.querySelectorAll(sel)).forEach(function(el){var rect=el.getBoundingClientRect();if(rect.width===0||rect.height===0)return;var cs=window.getComputedStyle(el);if(cs.display==='none'||cs.visibility==='hidden'||cs.opacity==='0')return;var role=el.getAttribute('role')||el.tagName.toLowerCase();if(el.tagName==='INPUT'&&el.type)role='input['+el.type+']';var name=(el.getAttribute('aria-label')||el.getAttribute('placeholder')||el.getAttribute('title')||el.textContent||'').trim().replace(/\s+/g,' ').slice(0,50).replace(/"/g,"'");var cx=Math.round(rect.left+rect.width/2);var cy=Math.round(rect.top+rect.height/2);nodes.push('['+nodes.length+'] '+role+' "'+name+'" ('+cx+','+cy+')');});return nodes.slice(0,80).join('\n');})()"#;

// ── browser_session ─────────────────────────────────────────────────────────────

/// Manage browser cookie sessions (list / export / import / clear) with explicit `SameSite`
/// mapping so `SameSite=None` session cookies survive an import round-trip.
pub struct BrowserSessionTool;

#[async_trait]
impl Tool for BrowserSessionTool {
    fn name(&self) -> &str {
        "browser_session"
    }
    fn description(&self) -> &str {
        "Manage browser login sessions and cookies: list domains, export cookies as JSON, import \
         cookies from JSON, or clear a domain's cookies to force re-login. import/clear require \
         approval."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["list","export","import","clear"] },
                "domain": { "type": "string", "description": "Domain filter (required for export/clear)." },
                "cookies_json": { "type": "string", "description": "JSON array of cookies (for import)." }
            },
            "required": ["action"]
        })
    }
    fn risk_tier(&self, args: &Value) -> RiskTier {
        session_risk_tier(args["action"].as_str())
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<String> {
        let action = args["action"].as_str().ok_or_else(|| anyhow!("action is required"))?;
        let domain = args["domain"].as_str().unwrap_or("");
        let page = { global().lock().await.new_page().await? };

        let result = match action {
            "list" => {
                let cookies = page.get_cookies().await.map_err(|e| anyhow!("get cookies: {e}"))?;
                let mut seen: HashMap<String, usize> = HashMap::new();
                for c in &cookies {
                    let d = c.domain.trim_start_matches('.').to_string();
                    if domain.is_empty() || d.ends_with(domain) {
                        *seen.entry(d).or_insert(0) += 1;
                    }
                }
                if seen.is_empty() {
                    Ok("no saved sessions found".to_string())
                } else {
                    let mut s = String::from("Active sessions (domain → cookie count):\n");
                    for (d, n) in seen {
                        s.push_str(&format!("  {d} → {n} cookies\n"));
                    }
                    Ok(s)
                }
            }
            "export" => {
                if domain.is_empty() {
                    bail!("domain is required for export");
                }
                let cookies = page.get_cookies().await.map_err(|e| anyhow!("get cookies: {e}"))?;
                let filtered: Vec<&_> = cookies
                    .iter()
                    .filter(|c| c.domain.trim_start_matches('.').ends_with(domain))
                    .collect();
                if filtered.is_empty() {
                    Ok(format!("no cookies found for domain {domain:?}"))
                } else {
                    Ok(serde_json::to_string_pretty(&filtered)?)
                }
            }
            "import" => import_cookies(&page, args["cookies_json"].as_str().unwrap_or("")).await,
            "clear" => {
                let cookies = page.get_cookies().await.map_err(|e| anyhow!("get cookies: {e}"))?;
                let mut n = 0;
                for c in cookies {
                    if domain.is_empty() || c.domain.trim_start_matches('.').ends_with(domain) {
                        page.delete_cookies(vec![
                            chromiumoxide::cdp::browser_protocol::network::DeleteCookiesParams::new(
                                c.name.clone(),
                            ),
                        ])
                        .await
                        .ok();
                        n += 1;
                    }
                }
                Ok(format!("cleared {n} cookies"))
            }
            other => bail!("unknown action {other:?}"),
        };
        page.close().await.ok();
        result
    }
}

/// Parse a JSON cookie array and install it via CDP, mapping `sameSite` explicitly (an omitted
/// value stays UNSET rather than defaulting to Lax — see [`crate::browser::session`]).
async fn import_cookies(page: &Page, cookies_json: &str) -> Result<String> {
    if cookies_json.is_empty() {
        bail!("cookies_json is required for import");
    }
    let raw: Vec<Value> =
        serde_json::from_str(cookies_json).map_err(|e| anyhow!("parsing cookies_json: {e}"))?;
    let mut params = Vec::with_capacity(raw.len());
    for c in &raw {
        let name = c["name"].as_str().unwrap_or("").to_string();
        let value = c["value"].as_str().unwrap_or("").to_string();
        let mut p = CookieParam::new(name, value);
        p.domain = c["domain"].as_str().map(str::to_string);
        p.path = c["path"].as_str().map(str::to_string);
        p.secure = c["secure"].as_bool();
        p.http_only = c["httpOnly"].as_bool();
        p.same_site = c["sameSite"].as_str().and_then(map_same_site).map(|s| match s {
            SameSite::Strict => CookieSameSite::Strict,
            SameSite::Lax => CookieSameSite::Lax,
            SameSite::None => CookieSameSite::None,
        });
        params.push(p);
    }
    let count = params.len();
    page.set_cookies(params).await.map_err(|e| anyhow!("setting cookies: {e}"))?;
    Ok(format!("imported {count} cookies"))
}
