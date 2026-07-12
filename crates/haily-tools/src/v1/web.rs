use crate::security;
use crate::{browser, RiskTier, Tool, ToolContext};
use anyhow::{bail, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::time::Duration;

/// Parse the raw DuckDuckGo Instant Answer API response body.
///
/// Extracted as a standalone function (rather than inlined `unwrap_or_default()`) so a
/// malformed/unexpected response body surfaces as an explicit, testable error instead of
/// silently degrading to `Value::Null` — which downstream code could not distinguish from
/// "no instant answer found".
fn parse_ddg_response(body: &str) -> Result<Value> {
    serde_json::from_str(body)
        .map_err(|e| anyhow::anyhow!("web_search: failed to parse DuckDuckGo response: {e}"))
}

// ---------------------------------------------------------------------------
// WebSearchTool
// ---------------------------------------------------------------------------
pub struct WebSearchTool;

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }
    fn description(&self) -> &str {
        "Tìm kiếm web và trả về kết quả. Dùng khi cần thông tin mới nhất hoặc không có trong memory."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Câu truy vấn tìm kiếm" }
            },
            "required": ["query"]
        })
    }
    fn risk_tier(&self, _args: &Value) -> RiskTier {
        RiskTier::Read
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<String> {
        let query = args["query"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("query is required"))?;

        let base_url = format!(
            "https://api.duckduckgo.com/?q={}&format=json&no_html=1&skip_disambig=1",
            urlencoding_query(query)
        );
        let resp = security::follow_redirects_with_guard(
            &base_url,
            Duration::from_secs(10),
            |client, url| client.get(url),
        )
        .await?;
        let resp = resp
            .error_for_status()
            .map_err(|e| anyhow::anyhow!("web_search: DuckDuckGo request failed: {e}"))?;
        let body = resp.text().await?;
        let parsed: Value = parse_ddg_response(&body)?;

        let mut results = Vec::new();

        // Abstract (Wikipedia-style instant answer)
        if let Some(text) = parsed["AbstractText"].as_str() {
            if !text.is_empty() {
                results.push(json!({
                    "title": parsed["Heading"].as_str().unwrap_or("Abstract"),
                    "url": parsed["AbstractURL"].as_str().unwrap_or(""),
                    "snippet": text
                }));
            }
        }

        // Related topics
        if let Some(topics) = parsed["RelatedTopics"].as_array() {
            for topic in topics.iter().take(5) {
                if let (Some(text), Some(url)) =
                    (topic["Text"].as_str(), topic["FirstURL"].as_str())
                {
                    results.push(json!({ "title": text.chars().take(80).collect::<String>(), "url": url, "snippet": text }));
                }
            }
        }

        if results.is_empty() {
            Ok(format!("Không tìm thấy kết quả instant answer cho: {query}. Thử url_fetch với URL cụ thể hơn."))
        } else {
            Ok(serde_json::to_string_pretty(&results)?)
        }
    }
}

/// Minimal query-string percent-encoding for the single `q` param DDG needs.
/// Avoids pulling in a full URL-encoding crate for one call site — `reqwest`'s
/// `.query(&[...])` builder would normally do this, but the redirect-walker takes a
/// fully-formed URL string up front (it must re-parse/re-join the URL on every
/// redirect hop, which `.query()` doesn't compose with).
fn urlencoding_query(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// UrlFetchTool
// ---------------------------------------------------------------------------
pub struct UrlFetchTool;

#[async_trait]
impl Tool for UrlFetchTool {
    fn name(&self) -> &str {
        "url_fetch"
    }
    fn description(&self) -> &str {
        "Tải nội dung từ một URL và trả về dạng text. Dùng khi cần đọc trang web cụ thể."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "URL cần tải" },
                "max_chars": { "type": "integer", "description": "Số ký tự tối đa trả về (default 4000)" }
            },
            "required": ["url"]
        })
    }
    fn risk_tier(&self, _args: &Value) -> RiskTier {
        RiskTier::Read
    }

    // 3-tier `fetch_strategy` wrapper (Phase 13): plain fetch → on a bot signal, escalate to the
    // stealth browser (feature-gated) → on a captcha/login wall, return `AWAITING_USER`. Without
    // the `browser` feature the middle tier is unavailable, so a bot signal goes STRAIGHT to
    // `AWAITING_USER` (documented). Stays `RiskTier::Read` end-to-end: `browser_navigate` is also
    // Read, so no approval gate is bypassed by this escalation.
    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<String> {
        let url = args["url"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("url is required"))?;
        let max_chars = args["max_chars"].as_u64().unwrap_or(4000) as usize;

        // Tier 1: plain fetch. A transport failure whose message itself carries a bot signal
        // still escalates; any other transport failure propagates unchanged.
        let (status, text) = match plain_fetch(url, max_chars).await {
            Ok(v) => v,
            Err(e) => {
                if browser::fetch_strategy::detect_bot_signal(&e.to_string()) {
                    return escalate_fetch(url, max_chars, _ctx).await;
                }
                return Err(e);
            }
        };

        // Prefix the status onto the bot-signal check only for the block codes, so a 403/429/503
        // challenge with an empty body still trips the signal (parity with the prior Go path that
        // checked `err.Error()` for the status).
        let check = if matches!(status, 403 | 429 | 503) {
            format!("HTTP {status} {text}")
        } else {
            text.clone()
        };
        match browser::fetch_strategy::after_plain(&check) {
            browser::fetch_strategy::Escalation::Done => Ok(text),
            browser::fetch_strategy::Escalation::Browser => escalate_fetch(url, max_chars, _ctx).await,
            // `after_plain` never returns AwaitingUser — the browser tier decides that.
            browser::fetch_strategy::Escalation::AwaitingUser => {
                Ok(browser::fetch_strategy::awaiting_user_json(url, false))
            }
        }
    }
}

/// Tier-1 plain fetch: returns `(http_status, rendered_text)`. HTML bodies are tag-stripped.
async fn plain_fetch(url: &str, max_chars: usize) -> Result<(u16, String)> {
    let resp =
        security::follow_redirects_with_guard(url, Duration::from_secs(15), |client, url| {
            client.get(url)
        })
        .await?;
    let status = resp.status().as_u16();
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let body = resp
        .text()
        .await
        .map_err(|e| anyhow::anyhow!("url_fetch: failed to read response body: {e}"))?;
    let text = if content_type.contains("text/html") {
        security::html_to_text(&body)
    } else {
        body
    };
    Ok((status, text.chars().take(max_chars).collect()))
}

/// Tier 2/3: escalate a bot-walled fetch. With the `browser` feature, render via the stealth
/// browser and, on a captcha/login wall, return `AWAITING_USER` with the browser held open for
/// the HUMAN to solve (never auto-solved). Without the feature, the browser tier is unavailable,
/// so return `AWAITING_USER` immediately (the user must open the URL manually).
#[cfg(feature = "browser")]
async fn escalate_fetch(url: &str, max_chars: usize, ctx: &ToolContext) -> Result<String> {
    let nav = browser::BrowserNavigateTool;
    let rendered = nav
        .execute(serde_json::json!({ "url": url, "max_chars": max_chars }), ctx)
        .await?;
    match browser::fetch_strategy::after_browser(&rendered) {
        browser::fetch_strategy::Escalation::AwaitingUser => {
            Ok(browser::fetch_strategy::awaiting_user_json(url, true))
        }
        _ => Ok(rendered),
    }
}

/// No-`browser`-feature escalation: the stealth-browser tier is unavailable, so a bot signal
/// resolves straight to `AWAITING_USER` (LOCKED decision 6).
#[cfg(not(feature = "browser"))]
async fn escalate_fetch(url: &str, _max_chars: usize, _ctx: &ToolContext) -> Result<String> {
    Ok(browser::fetch_strategy::awaiting_user_json(url, false))
}

// ---------------------------------------------------------------------------
// HttpRequestTool
// ---------------------------------------------------------------------------
pub struct HttpRequestTool;

#[async_trait]
impl Tool for HttpRequestTool {
    fn name(&self) -> &str {
        "http_request"
    }
    fn description(&self) -> &str {
        "Gửi HTTP request tùy chỉnh. Dùng cho API calls cần headers hoặc method cụ thể."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "method": { "type": "string", "enum": ["GET","POST","PUT","PATCH","DELETE"] },
                "url": { "type": "string" },
                "headers": { "type": "object", "description": "HTTP headers tùy chỉnh" },
                "body": { "type": "string", "description": "Request body (JSON hoặc text)" }
            },
            "required": ["method", "url"]
        })
    }
    fn risk_tier(&self, _args: &Value) -> RiskTier {
        RiskTier::IrreversibleWrite
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<String> {
        let method_str = args["method"].as_str().unwrap_or("GET").to_uppercase();
        let url = args["url"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("url is required"))?;
        let method = reqwest::Method::from_bytes(method_str.as_bytes())
            .map_err(|_| anyhow::anyhow!("invalid HTTP method"))?;

        // Reject headers that could defeat the SSRF pin (Host override) or enable
        // request smuggling — see `security::is_denied_header` for the full
        // rationale per header. Checked before any network activity.
        let headers: Vec<(String, String)> = if let Some(obj) = args["headers"].as_object() {
            let mut collected = Vec::with_capacity(obj.len());
            for (k, v) in obj {
                if security::is_denied_header(k) {
                    bail!("http_request: header '{k}' is not permitted (would defeat SSRF protections or enable request smuggling)");
                }
                if let Some(val) = v.as_str() {
                    collected.push((k.clone(), val.to_string()));
                }
            }
            collected
        } else {
            Vec::new()
        };
        let body = args["body"].as_str().map(str::to_string);

        let resp = security::follow_redirects_with_guard(
            url,
            Duration::from_secs(30),
            move |client, url| {
                let mut req = client.request(method.clone(), url);
                for (k, v) in &headers {
                    req = req.header(k.as_str(), v.as_str());
                }
                if let Some(b) = &body {
                    req = req.body(b.clone());
                }
                req
            },
        )
        .await?;

        let status = resp.status().as_u16();
        let body = resp
            .text()
            .await
            .map_err(|e| anyhow::anyhow!("http_request: failed to read response body: {e}"))?;
        let truncated: String = body.chars().take(4000).collect();

        if status >= 400 {
            bail!("HTTP {status}: {truncated}");
        }
        Ok(format!("HTTP {status}\n{truncated}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ddg_response_accepts_valid_json() {
        let body = r#"{"AbstractText":"hello","Heading":"H","AbstractURL":"u","RelatedTopics":[]}"#;
        let parsed = parse_ddg_response(body).unwrap();
        assert_eq!(parsed["AbstractText"].as_str(), Some("hello"));
    }

    #[test]
    fn parse_ddg_response_errors_explicitly_on_malformed_body() {
        // Previously `unwrap_or_default()` silently degraded this to `Value::Null`,
        // making a malformed upstream response indistinguishable from "nothing found".
        let result = parse_ddg_response("not valid json {{{");
        assert!(
            result.is_err(),
            "malformed body must surface as an explicit error"
        );
        assert!(result.unwrap_err().to_string().contains("failed to parse"));
    }

    #[test]
    fn parse_ddg_response_errors_on_empty_body() {
        assert!(parse_ddg_response("").is_err());
    }

    #[test]
    fn urlencoding_query_percent_encodes_reserved_chars() {
        assert_eq!(urlencoding_query("a b"), "a%20b");
        assert_eq!(urlencoding_query("rust&go"), "rust%26go");
        assert_eq!(urlencoding_query("safe-chars_1.0~x"), "safe-chars_1.0~x");
    }

    #[test]
    fn http_request_tool_is_permanently_require_approval() {
        // Authorization/Cookie headers remain allowed on this tool ONLY because it
        // is gated by human approval on every call — if this tool were ever
        // downgraded to auto-approve, the header allowlist decision above would
        // need to be revisited (this test is the tripwire for that).
        let tool = HttpRequestTool;
        assert_eq!(tool.risk_tier(&json!({})), RiskTier::IrreversibleWrite);
    }
}
