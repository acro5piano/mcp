use std::collections::BTreeMap;

use anyhow::{anyhow, bail, Context, Result};
use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, CONTENT_TYPE};
use reqwest::StatusCode;
use serde_json::{json, Value};

pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// A 401 from the MCP endpoint, carrying the `WWW-Authenticate` challenge so the
/// caller can follow RFC 9728 discovery from it.
#[derive(Debug)]
pub struct Unauthorized {
    pub www_authenticate: Option<String>,
}

pub enum CallError {
    Unauthorized(Unauthorized),
    Other(anyhow::Error),
}

impl From<anyhow::Error> for CallError {
    fn from(e: anyhow::Error) -> Self {
        CallError::Other(e)
    }
}

impl From<CallError> for anyhow::Error {
    fn from(e: CallError) -> Self {
        match e {
            CallError::Other(e) => e,
            CallError::Unauthorized(_) => anyhow!("unauthorized"),
        }
    }
}

pub struct Client {
    http: reqwest::Client,
    url: String,
    extra_headers: BTreeMap<String, String>,
    token: Option<String>,
    session_id: Option<String>,
    next_id: u64,
    initialized: bool,
}

impl Client {
    pub fn new(
        url: String,
        extra_headers: BTreeMap<String, String>,
        token: Option<String>,
    ) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("mcp-cli/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("building HTTP client")?;
        Ok(Self {
            http,
            url,
            extra_headers,
            token,
            session_id: None,
            next_id: 0,
            initialized: false,
        })
    }

    fn headers(&self) -> Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/json, text/event-stream"),
        );
        if self.initialized {
            headers.insert(
                HeaderName::from_static("mcp-protocol-version"),
                HeaderValue::from_static(PROTOCOL_VERSION),
            );
        }
        if let Some(session) = &self.session_id {
            headers.insert(
                HeaderName::from_static("mcp-session-id"),
                HeaderValue::from_str(session).context("invalid session id")?,
            );
        }
        if let Some(token) = &self.token {
            headers.insert(
                reqwest::header::AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {token}"))
                    .context("invalid access token")?,
            );
        }
        for (k, v) in &self.extra_headers {
            headers.insert(
                HeaderName::from_bytes(k.as_bytes())
                    .with_context(|| format!("invalid header name: {k}"))?,
                HeaderValue::from_str(v)
                    .with_context(|| format!("invalid header value for {k}"))?,
            );
        }
        Ok(headers)
    }

    async fn send(
        &mut self,
        body: Value,
        want_id: Option<u64>,
    ) -> Result<Option<Value>, CallError> {
        let response = self
            .http
            .post(&self.url)
            .headers(self.headers()?)
            .json(&body)
            .send()
            .await
            .context("sending request to MCP server")
            .map_err(CallError::Other)?;

        if response.status() == StatusCode::UNAUTHORIZED {
            let www = response
                .headers()
                .get(reqwest::header::WWW_AUTHENTICATE)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            return Err(CallError::Unauthorized(Unauthorized {
                www_authenticate: www,
            }));
        }

        if let Some(session) = response
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
        {
            self.session_id = Some(session.to_string());
        }

        let status = response.status();
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(CallError::Other(anyhow!(
                "MCP server returned {status}{}",
                if text.is_empty() {
                    String::new()
                } else {
                    format!(": {}", text.trim())
                }
            )));
        }

        let Some(want_id) = want_id else {
            // Notification: nothing to wait for.
            return Ok(None);
        };

        if content_type.starts_with("text/event-stream") {
            read_sse_response(response, want_id)
                .await
                .map(Some)
                .map_err(CallError::Other)
        } else {
            let text = response
                .text()
                .await
                .context("reading MCP response")
                .map_err(CallError::Other)?;
            if text.trim().is_empty() {
                return Err(CallError::Other(anyhow!("empty response from MCP server")));
            }
            let value: Value = serde_json::from_str(&text)
                .with_context(|| format!("parsing MCP response: {}", truncate(&text)))
                .map_err(CallError::Other)?;
            extract_result(value, want_id)
                .map(Some)
                .map_err(CallError::Other)
        }
    }

    pub async fn request(&mut self, method: &str, params: Value) -> Result<Value, CallError> {
        self.next_id += 1;
        let id = self.next_id;
        let body = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let result = self.send(body, Some(id)).await?;
        Ok(result.unwrap_or(Value::Null))
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<(), CallError> {
        let body = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        self.send(body, None).await?;
        Ok(())
    }

    pub async fn initialize(&mut self) -> Result<Value, CallError> {
        let result = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": { "name": "mcp-cli", "version": env!("CARGO_PKG_VERSION") },
                }),
            )
            .await?;
        self.initialized = true;
        self.notify("notifications/initialized", json!({})).await?;
        Ok(result)
    }

    /// Lists every tool, following `nextCursor` pagination.
    pub async fn list_tools(&mut self) -> Result<Vec<Value>, CallError> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = match &cursor {
                Some(c) => json!({ "cursor": c }),
                None => json!({}),
            };
            let result = self.request("tools/list", params).await?;
            if let Some(list) = result.get("tools").and_then(Value::as_array) {
                tools.extend(list.iter().cloned());
            }
            match result.get("nextCursor").and_then(Value::as_str) {
                Some(next) if !next.is_empty() => cursor = Some(next.to_string()),
                _ => break,
            }
        }
        Ok(tools)
    }

    pub async fn call_tool(&mut self, name: &str, arguments: Value) -> Result<Value, CallError> {
        self.request(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        )
        .await
    }
}

/// Reads an SSE stream until the JSON-RPC message with `want_id` arrives.
async fn read_sse_response(response: reqwest::Response, want_id: u64) -> Result<Value> {
    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("reading SSE stream")?;
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        // Events are separated by a blank line; keep the trailing partial event.
        while let Some(index) = find_event_boundary(&buffer) {
            let (raw, rest) = buffer.split_at(index);
            let event = raw.to_string();
            buffer = rest.trim_start_matches(['\r', '\n']).to_string();
            if let Some(data) = sse_data(&event) {
                let value: Value = match serde_json::from_str(&data) {
                    Ok(v) => v,
                    // Ignore keep-alives and anything that is not JSON.
                    Err(_) => continue,
                };
                if matches_id(&value, want_id) {
                    return extract_result(value, want_id);
                }
            }
        }
    }
    bail!("MCP server closed the stream before answering request {want_id}")
}

fn find_event_boundary(buffer: &str) -> Option<usize> {
    let lf = buffer.find("\n\n");
    let crlf = buffer.find("\r\n\r\n");
    match (lf, crlf) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

fn sse_data(event: &str) -> Option<String> {
    let mut data = String::new();
    for line in event.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
        }
    }
    if data.is_empty() {
        None
    } else {
        Some(data)
    }
}

fn matches_id(value: &Value, want_id: u64) -> bool {
    value.get("id").and_then(Value::as_u64) == Some(want_id)
}

fn extract_result(value: Value, want_id: u64) -> Result<Value> {
    if !matches_id(&value, want_id) {
        bail!(
            "unexpected JSON-RPC response: {}",
            truncate(&value.to_string())
        );
    }
    if let Some(error) = value.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
        let data = match error.get("data") {
            Some(d) if !d.is_null() => format!(" ({d})"),
            _ => String::new(),
        };
        bail!("MCP error {code}: {message}{data}");
    }
    Ok(value.get("result").cloned().unwrap_or(Value::Null))
}

fn truncate(s: &str) -> String {
    let s = s.trim();
    if s.chars().count() > 500 {
        format!("{}…", s.chars().take(500).collect::<String>())
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_multi_line_sse_data() {
        let event = "event: message\ndata: {\"a\":\ndata: 1}";
        assert_eq!(sse_data(event).as_deref(), Some("{\"a\":\n1}"));
        assert_eq!(sse_data(": keep-alive"), None);
    }

    #[test]
    fn finds_the_earliest_event_boundary() {
        assert_eq!(find_event_boundary("data: 1\n\ndata: 2"), Some(7));
        assert_eq!(find_event_boundary("data: 1\r\n\r\ndata: 2"), Some(7));
        assert_eq!(find_event_boundary("data: partial"), None);
    }

    #[test]
    fn extract_result_surfaces_jsonrpc_errors() {
        let error =
            json!({"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"Method not found"}});
        let message = extract_result(error, 1).unwrap_err().to_string();
        assert!(message.contains("-32601"), "{message}");
        assert!(message.contains("Method not found"), "{message}");
    }

    #[test]
    fn extract_result_rejects_a_mismatched_id() {
        let other = json!({"jsonrpc":"2.0","id":7,"result":{}});
        assert!(extract_result(other, 1).is_err());
    }

    #[test]
    fn extract_result_returns_the_payload() {
        let ok = json!({"jsonrpc":"2.0","id":3,"result":{"tools":[]}});
        assert_eq!(extract_result(ok, 3).unwrap(), json!({"tools":[]}));
    }
}
