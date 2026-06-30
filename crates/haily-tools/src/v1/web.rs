use crate::{Tool, ToolClass, ToolContext};
use anyhow::{bail, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// WebSearchTool
// ---------------------------------------------------------------------------
pub struct WebSearchTool;

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str { "web_search" }
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
    fn approval_class(&self) -> ToolClass { ToolClass::AutoApprove }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<String> {
        let query = args["query"].as_str().ok_or_else(|| anyhow::anyhow!("query is required"))?;
        // SSRF guard on the base URL — DDG is a known safe external host.
        crate::security::ssrf_guard("https://api.duckduckgo.com/").await?;

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .user_agent("Haily/1.0")
            .build()?;
        let resp = client
            .get("https://api.duckduckgo.com/")
            .query(&[("q", query), ("format", "json"), ("no_html", "1"), ("skip_disambig", "1")])
            .send()
            .await?
            .text()
            .await?;
        let parsed: Value = serde_json::from_str(&resp).unwrap_or_default();

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
                if let (Some(text), Some(url)) = (topic["Text"].as_str(), topic["FirstURL"].as_str()) {
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

// ---------------------------------------------------------------------------
// UrlFetchTool
// ---------------------------------------------------------------------------
pub struct UrlFetchTool;

#[async_trait]
impl Tool for UrlFetchTool {
    fn name(&self) -> &str { "url_fetch" }
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
    fn approval_class(&self) -> ToolClass { ToolClass::AutoApprove }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<String> {
        let url = args["url"].as_str().ok_or_else(|| anyhow::anyhow!("url is required"))?;
        let max_chars = args["max_chars"].as_u64().unwrap_or(4000) as usize;

        crate::security::ssrf_guard(url).await?;

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .user_agent("Haily/1.0")
            .build()?;
        let resp = client.get(url).send().await?;
        let content_type = resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let body = resp.text().await?;

        let text = if content_type.contains("text/html") {
            crate::security::html_to_text(&body)
        } else {
            body
        };

        let trimmed: String = text.chars().take(max_chars).collect();
        Ok(trimmed)
    }
}

// ---------------------------------------------------------------------------
// HttpRequestTool
// ---------------------------------------------------------------------------
pub struct HttpRequestTool;

#[async_trait]
impl Tool for HttpRequestTool {
    fn name(&self) -> &str { "http_request" }
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
    fn approval_class(&self) -> ToolClass { ToolClass::RequireApproval }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<String> {
        let method = args["method"].as_str().unwrap_or("GET").to_uppercase();
        let url = args["url"].as_str().ok_or_else(|| anyhow::anyhow!("url is required"))?;

        crate::security::ssrf_guard(url).await?;

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .user_agent("Haily/1.0")
            .build()?;

        let method = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|_| anyhow::anyhow!("invalid HTTP method"))?;
        let mut req = client.request(method, url);

        if let Some(headers) = args["headers"].as_object() {
            for (k, v) in headers {
                if let Some(val) = v.as_str() {
                    req = req.header(k.as_str(), val);
                }
            }
        }
        if let Some(body) = args["body"].as_str() {
            req = req.body(body.to_string());
        }

        let resp = req.send().await?;
        let status = resp.status().as_u16();
        let body = resp.text().await?;
        let truncated: String = body.chars().take(4000).collect();

        if status >= 400 {
            bail!("HTTP {status}: {truncated}");
        }
        Ok(format!("HTTP {status}\n{truncated}"))
    }
}
