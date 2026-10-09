//! Minimal blocking client for OpenAI-compatible `POST /chat/completions`.
//!
//! Streams the reply (server-sent events) so cancellation and the output cap
//! take effect between tokens; dropping the response closes the connection,
//! which makes Ollama/llama-server stop generating. Non-streaming JSON replies
//! are accepted too.

use std::io::{BufRead, BufReader, Read};
use std::time::Duration;

use serde_json::{Value, json};

use crate::config::{CleanupConfig, is_loopback_host, split_url};
use crate::transcription::CancelFlag;

/// Upper bound for any response body.
const MAX_BODY: u64 = 4 << 20;
/// Error bodies are only read for their message.
const MAX_ERROR_BODY: u64 = 64 << 10;

/// User-facing errors (shown as the reason for using the raw transcript).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RequestError {
    #[error("cancelled")]
    Cancelled,
    #[error("cannot reach {0} (is the LLM server, e.g. `ollama serve`, running?)")]
    Unavailable(String),
    #[error("timed out")]
    Timeout,
    #[error("server replied {status}: {message}")]
    Status { status: u16, message: String },
    #[error("environment variable {0} (cleanup.api_key_env) is not set")]
    MissingKey(String),
    #[error("invalid reply: {0}")]
    Invalid(String),
    #[error("reply exceeded {0} characters")]
    TooLong(usize),
    #[error("request failed: {0}")]
    Other(String),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Completion {
    pub content: String,
    pub finish_reason: Option<String>,
    /// The server streamed separate reasoning ("thinking") output.
    pub reasoned: bool,
}

pub struct Client {
    agent: ureq::Agent,
    endpoint: String,
    base_url: String,
    model: String,
    api_key_env: Option<String>,
    reasoning_effort: Option<String>,
    temperature: f32,
}

impl Client {
    pub fn new(config: &CleanupConfig) -> Self {
        let base_url = config.base_url.trim().trim_end_matches('/').to_owned();
        let local = split_url(&base_url).is_some_and(|(_, host)| is_loopback_host(host));
        let mut builder = ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_millis(config.connect_timeout_ms)))
            .timeout_global(Some(Duration::from_secs(config.timeout_secs)))
            .http_status_as_error(false)
            // A redirected POST turns into a GET; report it instead.
            .max_redirects(0)
            .user_agent(concat!("wayland-wispr/", env!("CARGO_PKG_VERSION")));
        if local {
            // Never route a local server through an HTTP(S)_PROXY.
            builder = builder.proxy(None);
        }
        Self {
            agent: ureq::Agent::new_with_config(builder.build()),
            endpoint: format!("{base_url}/chat/completions"),
            base_url,
            model: config.model.clone(),
            api_key_env: config.api_key_env.clone(),
            reasoning_effort: config.reasoning_effort.clone(),
            temperature: config.temperature,
        }
    }

    /// Run one chat completion. Blocking; checks `cancel` between chunks and
    /// stops once the content exceeds `max_chars`.
    pub fn complete(
        &self,
        messages: Vec<Value>,
        max_tokens: u32,
        max_chars: usize,
        cancel: &CancelFlag,
    ) -> Result<Completion, RequestError> {
        let mut body = json!({
            "model": self.model,
            "messages": messages,
            "stream": true,
            "temperature": self.temperature,
            "max_tokens": max_tokens,
        });
        if let Some(effort) = &self.reasoning_effort {
            body["reasoning_effort"] = json!(effort);
        }

        let mut request = self
            .agent
            .post(&self.endpoint)
            .header("content-type", "application/json")
            .header("accept", "text/event-stream, application/json");
        if let Some(var) = &self.api_key_env {
            let key = std::env::var(var)
                .ok()
                .filter(|k| !k.trim().is_empty())
                .ok_or_else(|| RequestError::MissingKey(var.clone()))?;
            request = request.header("authorization", format!("Bearer {}", key.trim()));
        }

        let response = request
            .send(body.to_string())
            .map_err(|e| self.map_error(e))?;
        if cancel.is_cancelled() {
            return Err(RequestError::Cancelled);
        }
        let status = response.status().as_u16();
        let streaming = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("text/event-stream"));
        let (_, body) = response.into_parts();

        if !(200..300).contains(&status) {
            let mut text = String::new();
            let _ = body
                .into_with_config()
                .limit(MAX_ERROR_BODY)
                .reader()
                .read_to_string(&mut text);
            return Err(RequestError::Status {
                status,
                message: error_message(&text),
            });
        }

        let reader = body.into_with_config().limit(MAX_BODY).reader();
        if streaming {
            read_stream(reader, max_chars, cancel).map_err(|e| self.map_io(e))
        } else {
            let mut text = String::new();
            BufReader::new(reader)
                .read_to_string(&mut text)
                .map_err(|e| self.map_io(StreamError::Io(e)))?;
            parse_full(&text)
        }
    }

    fn map_error(&self, e: ureq::Error) -> RequestError {
        match e {
            ureq::Error::Timeout(ureq::Timeout::Connect | ureq::Timeout::Resolve) => {
                RequestError::Unavailable(self.base_url.clone())
            }
            ureq::Error::Timeout(_) => RequestError::Timeout,
            ureq::Error::HostNotFound | ureq::Error::ConnectionFailed => {
                RequestError::Unavailable(self.base_url.clone())
            }
            ureq::Error::Io(io) => self.map_io(StreamError::Io(io)),
            other => RequestError::Other(other.to_string()),
        }
    }

    fn map_io(&self, e: StreamError) -> RequestError {
        use std::io::ErrorKind as K;
        match e {
            StreamError::Request(e) => e,
            StreamError::Io(io) => match io.kind() {
                K::ConnectionRefused | K::ConnectionReset | K::NotFound | K::AddrNotAvailable => {
                    RequestError::Unavailable(self.base_url.clone())
                }
                K::TimedOut | K::WouldBlock => RequestError::Timeout,
                _ => match io
                    .get_ref()
                    .and_then(|inner| inner.downcast_ref::<ureq::Error>())
                {
                    Some(ureq::Error::Timeout(_)) => RequestError::Timeout,
                    _ => RequestError::Other(io.to_string()),
                },
            },
        }
    }
}

enum StreamError {
    Io(std::io::Error),
    Request(RequestError),
}

impl From<RequestError> for StreamError {
    fn from(e: RequestError) -> Self {
        Self::Request(e)
    }
}

/// Read `data: {...}` server-sent events until `[DONE]` or end of stream.
fn read_stream(
    reader: impl Read,
    max_chars: usize,
    cancel: &CancelFlag,
) -> Result<Completion, StreamError> {
    let mut out = Completion::default();
    let mut chars = 0usize;
    let mut line = String::new();
    let mut reader = BufReader::new(reader);
    loop {
        if cancel.is_cancelled() {
            return Err(RequestError::Cancelled.into());
        }
        line.clear();
        if reader.read_line(&mut line).map_err(StreamError::Io)? == 0 {
            break;
        }
        let Some(data) = line.trim_end().strip_prefix("data:") else {
            continue; // blank separators, comments, `event:` lines
        };
        let data = data.trim();
        if data == "[DONE]" {
            break;
        }
        let event: Value = serde_json::from_str(data)
            .map_err(|e| RequestError::Invalid(format!("bad stream event: {e}")))?;
        if let Some(err) = event.get("error") {
            let message = error_message(&err.to_string());
            return Err(RequestError::Other(format!("server error: {message}")).into());
        }
        let Some(choice) = event["choices"].get(0) else {
            continue; // e.g. a usage-only chunk
        };
        let delta = &choice["delta"];
        if let Some(piece) = delta["content"].as_str() {
            chars += piece.chars().count();
            out.content.push_str(piece);
            if chars > max_chars {
                if out.content.contains("<think>") {
                    // Inline reasoning (server without a reasoning parser):
                    // report it like a reasoning model that ran out of budget.
                    out.reasoned = true;
                    out.finish_reason = Some("length".into());
                    return Ok(out);
                }
                return Err(RequestError::TooLong(max_chars).into());
            }
        }
        if has_reasoning(delta) {
            out.reasoned = true;
        }
        if let Some(reason) = choice["finish_reason"].as_str() {
            out.finish_reason = Some(reason.to_owned());
        }
    }
    Ok(out)
}

fn parse_full(text: &str) -> Result<Completion, RequestError> {
    let value: Value =
        serde_json::from_str(text).map_err(|e| RequestError::Invalid(format!("not JSON: {e}")))?;
    let choice = value["choices"]
        .get(0)
        .ok_or_else(|| RequestError::Invalid("no choices".into()))?;
    let message = &choice["message"];
    Ok(Completion {
        content: message["content"].as_str().unwrap_or_default().to_owned(),
        finish_reason: choice["finish_reason"].as_str().map(str::to_owned),
        reasoned: has_reasoning(message),
    })
}

/// Ollama streams thinking as `reasoning`, llama-server/vLLM as
/// `reasoning_content`.
fn has_reasoning(v: &Value) -> bool {
    ["reasoning", "reasoning_content"]
        .iter()
        .any(|k| v[k].as_str().is_some_and(|s| !s.is_empty()))
}

/// Extract `error` / `error.message` from an error body; fall back to a
/// truncated body.
fn error_message(body: &str) -> String {
    let parsed: Option<Value> = serde_json::from_str(body).ok();
    let message = parsed.as_ref().and_then(|v| {
        let e = v.get("error").unwrap_or(v);
        e.as_str()
            .map(str::to_owned)
            .or_else(|| e["message"].as_str().map(str::to_owned))
    });
    let message = message.unwrap_or_else(|| body.trim().to_owned());
    let mut short: String = message.chars().take(200).collect();
    if short.is_empty() {
        short = "no details".into();
    }
    short
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;
    use std::thread;

    /// One-shot HTTP server; returns the base URL and a handle yielding the
    /// raw request it received.
    pub fn serve_once(response: String) -> (String, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            let _ = stream.write_all(response.as_bytes());
            request
        });
        (url, handle)
    }

    fn read_request(stream: &mut std::net::TcpStream) -> String {
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut head = String::new();
        let mut length = 0;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                length = v.trim().parse().unwrap();
            }
            head.push_str(&line);
            if line == "\r\n" || line.is_empty() {
                break;
            }
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        head + &String::from_utf8(body).unwrap()
    }

    pub fn sse(chunks: &[&str]) -> String {
        let mut body = String::new();
        for c in chunks {
            let event = json!({ "choices": [{ "index": 0, "delta": { "content": c } }] });
            body += &format!("data: {event}\n\n");
        }
        let last = json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }] });
        body += &format!("data: {last}\n\ndata: [DONE]\n\n");
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{body}"
        )
    }

    pub fn http(status: &str, content_type: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    pub fn config(url: &str) -> CleanupConfig {
        CleanupConfig {
            base_url: url.into(),
            timeout_secs: 5,
            ..CleanupConfig::default()
        }
    }

    fn msgs() -> Vec<Value> {
        vec![json!({ "role": "user", "content": "hi" })]
    }

    #[test]
    fn streams_content_and_sends_expected_request() {
        let (url, server) = serve_once(sse(&["Hal", "lo."]));
        let mut cfg = config(&url);
        cfg.reasoning_effort = Some("none".into());
        let c = Client::new(&cfg)
            .complete(msgs(), 42, 1000, &CancelFlag::default())
            .unwrap();
        assert_eq!(c.content, "Hallo.");
        assert_eq!(c.finish_reason.as_deref(), Some("stop"));
        assert!(!c.reasoned);

        let request = server.join().unwrap();
        assert!(request.starts_with("POST /v1/chat/completions HTTP/1.1"));
        assert!(!request.to_ascii_lowercase().contains("authorization"));
        let body: Value = serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(body["model"], "qwen2.5:7b-instruct");
        assert_eq!(body["stream"], true);
        assert_eq!(body["max_tokens"], 42);
        assert_eq!(body["reasoning_effort"], "none");
        assert_eq!(body["messages"][0]["content"], "hi");
    }

    #[test]
    fn accepts_non_streaming_replies() {
        let body = json!({
            "choices": [{ "message": { "role": "assistant", "content": "Ok." }, "finish_reason": "stop" }]
        });
        let (url, _server) = serve_once(http("200 OK", "application/json", &body.to_string()));
        let c = Client::new(&config(&url))
            .complete(msgs(), 10, 100, &CancelFlag::default())
            .unwrap();
        assert_eq!(c.content, "Ok.");
    }

    #[test]
    fn reports_server_errors() {
        let (url, _server) = serve_once(http(
            "404 Not Found",
            "application/json",
            r#"{"error":{"message":"model \"x\" not found, try pulling it first","type":"api_error"}}"#,
        ));
        let e = Client::new(&config(&url))
            .complete(msgs(), 10, 100, &CancelFlag::default())
            .unwrap_err();
        assert_eq!(
            e,
            RequestError::Status {
                status: 404,
                message: "model \"x\" not found, try pulling it first".into()
            }
        );
    }

    #[test]
    fn unreachable_server_is_unavailable() {
        let port = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let url = format!("http://127.0.0.1:{port}/v1");
        let e = Client::new(&config(&url))
            .complete(msgs(), 10, 100, &CancelFlag::default())
            .unwrap_err();
        assert!(matches!(e, RequestError::Unavailable(_)), "{e:?}");
    }

    #[test]
    fn missing_api_key_is_reported() {
        let mut cfg = config("http://127.0.0.1:9/v1");
        cfg.api_key_env = Some("WAYLAND_WISPR_TEST_SURELY_UNSET".into());
        let e = Client::new(&cfg)
            .complete(msgs(), 10, 100, &CancelFlag::default())
            .unwrap_err();
        assert!(matches!(e, RequestError::MissingKey(_)));
    }

    #[test]
    fn caps_output_and_notices_reasoning() {
        let (url, _server) = serve_once(sse(&["0123456789", "0123456789"]));
        let e = Client::new(&config(&url))
            .complete(msgs(), 10, 15, &CancelFlag::default())
            .unwrap_err();
        assert_eq!(e, RequestError::TooLong(15));

        let (url, _server) = serve_once(sse(&["<think>", "0123456789", "0123456789"]));
        let c = Client::new(&config(&url))
            .complete(msgs(), 10, 15, &CancelFlag::default())
            .unwrap();
        assert!(c.reasoned);
        assert_eq!(c.finish_reason.as_deref(), Some("length"));

        let event = json!({ "choices": [{ "delta": { "reasoning": "let me think" }, "finish_reason": "length" }] });
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\ndata: {event}\n\ndata: [DONE]\n\n"
        );
        let (url, _server) = serve_once(response);
        let c = Client::new(&config(&url))
            .complete(msgs(), 10, 100, &CancelFlag::default())
            .unwrap();
        assert!(c.reasoned);
        assert_eq!(c.finish_reason.as_deref(), Some("length"));
        assert!(c.content.is_empty());
    }

    #[test]
    fn stream_errors_are_reported() {
        let response = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\ndata: {\"error\":{\"message\":\"out of memory\"}}\n\n";
        let (url, _server) = serve_once(response.into());
        let e = Client::new(&config(&url))
            .complete(msgs(), 10, 100, &CancelFlag::default())
            .unwrap_err();
        assert!(e.to_string().contains("out of memory"), "{e}");
    }

    #[test]
    fn error_messages_are_extracted() {
        assert_eq!(error_message(r#"{"error":"boom"}"#), "boom");
        assert_eq!(error_message(r#"{"error":{"message":"bad"}}"#), "bad");
        assert_eq!(error_message("plain text"), "plain text");
        assert_eq!(error_message(""), "no details");
        assert_eq!(error_message(&"x".repeat(500)).len(), 200);
    }
}
