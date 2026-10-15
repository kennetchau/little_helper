//! OpenAI-compatible chat/completions client: streaming + one-shot.
//!
//! Exactly one error type exists here: [`BackendError`] means the turn
//! aborts (the session survives). Missing usage stats, odd SSE lines and
//! the like all degrade instead of raising.
//!
//! Concurrency is tokio by design (plan §2): [`BackendClient::stream_chat`]
//! returns an async [`SseStream`]; the agent loop drives it under a
//! `tokio::select!` against a `CancellationToken`, so an in-flight stream
//! stops promptly (Python: Esc -> per-line event check -> resp.close()).

use std::collections::VecDeque;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use futures_util::Stream;
use serde_json::{json, Value};
use thiserror::Error;

use crate::config::Config;

const RETRY_BACKOFF: Duration = Duration::from_secs(2);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Idle deadline for the RESPONSE BODY: max gap between received chunks.
/// Crucially this is NOT a total deadline — reqwest's `.read_timeout()`
/// resets on every byte received, so a stream that keeps flowing for hours
/// is never killed, while a backend that goes silent (thinking stall,
/// wedged connection) fails after 10 minutes of no data instead of
/// hanging forever.
///
/// History: this client previously used `.timeout(300s)` (a TOTAL deadline
/// covering headers + entire body). Long generations on slow models
/// (multi-minute streams) were killed mid-body and surfaced as
/// `error decoding response body`; the server saw the cancelled stream as
/// "Connection handling cancelled". The idle-only policy fixes that.
/// `pub` so the agent loop's stall diagnostics can name it in logs.
pub const READ_TIMEOUT: Duration = Duration::from_secs(600);

/// Backend failure: the loop turns this into an error event + turn abort.
#[derive(Debug, Error)]
#[error("{0}")]
pub struct BackendError(pub String);

// ---------- stream events (mirror Python's dataclasses) ----------

#[derive(Debug, Clone, PartialEq)]
pub enum SseEvent {
    Text(String),
    Reasoning(String),
    /// `arguments` is a partial JSON fragment; the loop concatenates per
    /// index (Python `ToolCallDelta` contract).
    ToolCall {
        index: u64,
        id: Option<String>,
        name: Option<String>,
        arguments: String,
    },
    /// Live token counters from the backend mid-stream: the per-chunk
    /// `timings` object (llama.cpp server with `timings_per_token`) or a
    /// non-final `usage` object. Both are running totals for the CURRENT
    /// request (completion resets each request), unlike the terminal
    /// `Done` usage which is authoritative for the whole request.
    Usage(Usage),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

impl Usage {
    /// Top-level `usage` object (OpenAI shape: `prompt_tokens` /
    /// `completion_tokens`).
    fn from_wire(u: &serde_json::Value) -> Usage {
        Usage {
            prompt_tokens: u
                .get("prompt_tokens")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
            completion_tokens: u
                .get("completion_tokens")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
        }
    }

    /// The llama.cpp server's per-chunk `timings` object
    /// (`timings_per_token: true`): `predicted_n` is the completion token
    /// count so far; `prompt_n` (processed) + `cache_n` (cached) is the
    /// prompt size (grows during prefill, then settles).
    fn from_timings(t: &serde_json::Value) -> Option<Usage> {
        let completion = t.get("predicted_n")?.as_u64()?;
        let prompt = t
            .get("prompt_n")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
            + t.get("cache_n").and_then(serde_json::Value::as_u64).unwrap_or(0);
        Some(Usage {
            prompt_tokens: prompt,
            completion_tokens: completion,
        })
    }
}

/// Lifecycle of a model on the backend (llama.cpp router fork state
/// diagram, `tools/server/server-models.h`:
/// UNLOADED → LOADING → LOADED ⇄ SLEEPING, plus DOWNLOADING/DOWNLOADED
/// while the router fetches a GGUF). `Unknown` is a plain OpenAI backend
/// that reports no status field — such backends have no load/unload
/// concept and always serve every listed model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ModelStatus {
    #[default]
    Unknown,
    Loaded,
    Loading,
    Unloaded,
    Sleeping,
    Downloading,
    Downloaded,
}

impl ModelStatus {
    pub fn from_wire(s: &str) -> Self {
        match s {
            "loaded" => Self::Loaded,
            "loading" => Self::Loading,
            "unloaded" => Self::Unloaded,
            "sleeping" => Self::Sleeping,
            "downloading" => Self::Downloading,
            "downloaded" => Self::Downloaded,
            _ => Self::Unknown,
        }
    }

    /// `true` while the backend is still transitioning (the TUI keeps
    /// re-polling /models until this settles).
    pub fn is_transitional(&self) -> bool {
        matches!(self, Self::Loading | Self::Downloading)
    }

    /// Whether a model in this state can serve requests right away
    /// (sleeping models wake on the first request, unknown backends
    /// always serve).
    pub fn is_serving(&self) -> bool {
        matches!(self, Self::Unknown | Self::Loaded | Self::Sleeping)
    }

    /// Whether a model in this state counts as "running" on the server
    /// (unload is only legal on running instances — the server itself
    /// enforces this with 400 "model is not running").
    pub fn is_running(&self) -> bool {
        matches!(self, Self::Loaded | Self::Loading | Self::Sleeping)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Unknown => "remote",
            Self::Loaded => "loaded",
            Self::Loading => "loading",
            Self::Unloaded => "unloaded",
            Self::Sleeping => "sleeping",
            Self::Downloading => "downloading",
            Self::Downloaded => "downloaded",
        }
    }
}

/// One entry of `GET /models`. Plain OpenAI backends only fill `id`
/// (status stays `Unknown`); the llama.cpp fork fills the rest.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelInfo {
    pub id: String,
    pub status: ModelStatus,
    /// `status.value` was `unloaded` with a nonzero child exit code —
    /// the last load attempt crashed.
    pub failed: bool,
    /// "preset" / "models_dir" / "cache" (fork only).
    pub source: Option<String>,
    /// `context_length` from the GGUF metadata (fork only).
    pub context_length: Option<u64>,
}

/// Parse one `data[]` entry. Tolerant: missing `status` (plain OpenAI
/// backends) → `Unknown`; unrecognised values degrade the same way.
fn model_info(id: &str, item: &serde_json::Value) -> ModelInfo {
    let status_obj = item.get("status");
    let value = status_obj
        .and_then(|s| s.get("value"))
        .and_then(serde_json::Value::as_str)
        .map(|s| ModelStatus::from_wire(s))
        .unwrap_or_default();
    ModelInfo {
        id: id.to_string(),
        status: value,
        failed: status_obj
            .and_then(|s| s.get("failed"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        source: item
            .get("source")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        context_length: item
            .get("context_length")
            .and_then(serde_json::Value::as_u64),
    }
}

impl ModelInfo {
    pub fn is_serving(&self) -> bool {
        self.status.is_serving()
    }
    pub fn is_running(&self) -> bool {
        self.status.is_running()
    }
    pub fn is_transitional(&self) -> bool {
        self.status.is_transitional()
    }
}

/// One item of the chat stream: a content delta, or the terminal Done.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamItem {
    Event(SseEvent),
    /// Always last. `usage` is the best count seen for the request — the
    /// final `usage` chunk when the backend sends one, else the last live
    /// `timings` counter; None only when the backend reported neither.
    Done {
        finish_reason: String,
        usage: Option<Usage>,
    },
}

impl StreamItem {
    pub fn is_done(&self) -> bool {
        matches!(self, StreamItem::Done { .. })
    }
}

/// Char count of a message `content`: plain string, or a list of parts
/// (vision tool results use `[{"type":"text",...},{"type":"image_url",...}]`
/// — the base64 payload itself is not tokenized as text; only the small
/// text parts count).
fn content_chars(v: &serde_json::Value) -> usize {
    match v {
        serde_json::Value::String(s) => s.len(),
        serde_json::Value::Array(parts) => parts
            .iter()
            .filter(|p| p.get("type").and_then(serde_json::Value::as_str) == Some("text"))
            .map(|p| p.get("text").and_then(serde_json::Value::as_str).unwrap_or("").len())
            .sum(),
        _ => 0,
    }
}

/// Fallback when the backend doesn't report usage (~4 chars/token).
pub fn estimate_prompt_tokens(messages: &[serde_json::Value]) -> u64 {
    let mut total = 0usize;
    for m in messages {
        total += m.get("content").map(content_chars).unwrap_or(0);
        // reasoning_content (llama.cpp thinking replay) is real prompt text.
        total += m
            .get("reasoning_content")
            .and_then(serde_json::Value::as_str)
            .map(str::len)
            .unwrap_or(0);
        if let Some(tcs) = m.get("tool_calls").and_then(serde_json::Value::as_array) {
            for tc in tcs {
                total += tc
                    .get("function")
                    .and_then(|f| f.get("arguments"))
                    .and_then(serde_json::Value::as_str)
                    .map(str::len)
                    .unwrap_or(0);
            }
        }
    }
    // Python: max(1, total // 4)
    (total / 4).max(1) as u64
}

pub struct BackendClient {
    http: reqwest::Client,
    base: String,
    model: std::sync::Arc<std::sync::Mutex<String>>,
    max_tokens: u64,
}

impl BackendClient {
    pub fn new(cfg: &Config) -> Self {
        let verify = matches!(
            std::env::var("HARNESS_TLS_VERIFY")
                .unwrap_or_default()
                .trim()
                .to_lowercase()
                .as_str(),
            "0" | "false" | "no" | "off"
        );
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            "application/json".parse().unwrap(),
        );
        if cfg.backend.api_key != "" && cfg.backend.api_key != "none" {
            let _ = headers.insert(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {}", cfg.backend.api_key)
                    .parse()
                    .unwrap_or_else(|_| "Bearer none".parse().unwrap()),
            );
        }
        let http = reqwest::Client::builder()
            .default_headers(headers)
            // Python: verify = ca_bundle or (not verify_off). rustls has no
            // per-client CA path in this API surface, so HARNESS_CA_BUNDLE
            // is honored by operators via the standard env; in this env the
            // documented mechanism is HARNESS_TLS_VERIFY=0.
            .danger_accept_invalid_certs(verify)
            // Idle-only timeout: resets on every received chunk, so a
            // healthy long stream is never cut (see READ_TIMEOUT doc).
            .read_timeout(READ_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .expect("valid client config");
        Self {
            http,
            base: cfg.backend.base_url.trim_end_matches('/').to_string(),
            model: std::sync::Arc::new(std::sync::Mutex::new(cfg.backend.model.clone())),
            max_tokens: cfg.backend.max_tokens,
        }
    }

    /// Payload shape matches the Python reference, including the
    /// `reasoning_effort: medium` quirk and the disabled temperature /
    /// thinking-budget fields (see `client.py` — they are commented out
    /// upstream; kept commented here for parity).
    fn payload(
        &self,
        messages: &[serde_json::Value],
        tools: Option<&[serde_json::Value]>,
        max_tokens: u64,
        stream: bool,
    ) -> serde_json::Value {
        // (Python sets temperature on the client but comments it out of the
        // payload; thinking_budget likewise — not sent.)
        let mut p = json!({
            "model": *self.model.lock().unwrap_or_else(|e| e.into_inner()),
            "messages": messages,
            "max_tokens": max_tokens,
            "stream": stream,
            "reasoning_effort": "medium",
        });
        if let Some(tools) = tools {
            if !tools.is_empty() {
                // Some backends 400 on an empty tools array.
                p["tools"] = serde_json::Value::Array(tools.to_vec());
            }
        }
        if stream {
            p["stream_options"] = json!({ "include_usage": true });
            // The llama.cpp server streams LIVE token counters per chunk
            // (`timings`: predicted_n / prompt_n / cache_n) plus prompt-
            // processing progress pings when asked for them — see
            // reference/llama.cpp tools/server/server-task.cpp
            // (to_json_oaicompat_chat partials) and the production payload
            // in reference/llama_cpp_completion_with_thinking_example.
            // Without these the harness only learns the counts from the
            // final usage chunk, after the stream is already over.
            // Standard OpenAI backends ignore the extra fields.
            p["timings_per_token"] = json!(true);
            p["return_progress"] = json!(true);
        }
        p
    }

    /// One logical request with the Python retry policy: 5xx or
    /// connection error -> one retry after backoff; 4xx raises immediately.
    async fn send(
        &self,
        url: &str,
        method: reqwest::Method,
        body: Option<serde_json::Value>,
    ) -> Result<reqwest::Response, BackendError> {
        let mut last = String::new();
        for attempt in 1..=2u32 {
            let mut req = self.http.request(method.clone(), url);
            if let Some(b) = &body {
                req = req.json(b);
            }
            match req.send().await {
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    if status >= 500 {
                        let text = resp.text().await.unwrap_or_default();
                        last = format!("HTTP {status}: {text}");
                        if attempt == 2 {
                            return Err(BackendError(last));
                        }
                        tokio::time::sleep(RETRY_BACKOFF).await;
                    } else if status >= 400 {
                        let text = resp.text().await.unwrap_or_default();
                        let text: String = text.chars().take(200).collect();
                        return Err(BackendError(format!("HTTP {status}: {text}")));
                    } else {
                        return Ok(resp);
                    }
                }
                Err(e) => {
                    last = format!("connection error: {e}");
                    if attempt == 2 {
                        return Err(BackendError(last));
                    }
                    tokio::time::sleep(RETRY_BACKOFF).await;
                }
            }
        }
        Err(BackendError(last))
    }

    /// Stream one completion. The returned stream always ends with a
    /// `StreamItem::Done`; cancellation is done by the consumer dropping
    /// the stream mid-way (loop discards the partial, like Python's Esc).
    /// (Inherent method; the [`Backend`] trait impl below wraps it. Inherent
    /// methods win method-resolution over trait methods, so the impl can
    /// call `self.stream_chat(...)` unambiguously.)
    pub async fn stream_chat(
        &self,
        messages: &[serde_json::Value],
        tools: Option<&[serde_json::Value]>,
    ) -> Result<SseStream, BackendError> {
        let payload = self.payload(messages, tools, self.max_tokens, true);
        let resp = self
            .send(
                &format!("{}/chat/completions", self.base),
                reqwest::Method::POST,
                Some(payload),
            )
            .await?;
        Ok(SseStream::new(resp))
    }

    /// One-shot completion (compaction pass). Returns the content string.
    /// (Inherent method; see `stream_chat` for why the inherent/trait pair
    /// keeps the same names.)
    pub async fn complete(
        &self,
        messages: &[serde_json::Value],
        max_tokens: u64,
    ) -> Result<String, BackendError> {
        let payload = self.payload(messages, None, max_tokens, false);
        let resp = self
            .send(
                &format!("{}/chat/completions", self.base),
                reqwest::Method::POST,
                Some(payload),
            )
            .await?;
        let data: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| BackendError(format!("backend returned non-JSON response: {e}")))?;
        let Some(choices) = data
            .get("choices")
            .and_then(serde_json::Value::as_array)
            .filter(|c| !c.is_empty())
        else {
            return Err(BackendError("backend returned no choices".into()));
        };
        Ok(choices[0]
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string())
    }

    pub fn model(&self) -> String {
        self.model.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn set_model(&self, value: &str) {
        *self.model.lock().unwrap_or_else(|e| e.into_inner()) = value.to_string();
    }

    /// GET /models: the models the backend can serve, with per-model
    /// status (for the TUI picker). The llama.cpp router fork
    /// (reference/llama.cpp, `tools/server/server-models.cpp`) annotates
    /// each entry with `status.value` (loaded / loading / unloaded /
    /// sleeping / downloading / downloaded), an optional `failed` flag
    /// and source/can_remove/context_length; a plain OpenAI backend
    /// sends bare `{id, ...}` entries, which parse to `Unknown` status.
    pub async fn list_models(&self) -> Result<Vec<ModelInfo>, BackendError> {
        self.list_models_raw(false).await
    }

    /// GET /models?reload=1: like `list_models` but first re-scans the
    /// model sources (a new .gguf / preset change shows up).
    pub async fn refresh_models(&self) -> Result<Vec<ModelInfo>, BackendError> {
        self.list_models_raw(true).await
    }

    async fn list_models_raw(&self, reload: bool) -> Result<Vec<ModelInfo>, BackendError> {
        // ?reload=1 is a MANAGEMENT route: it (and load/unload, see
        // admin_base) lives at the server root, not under the /v1
        // OAI-compat prefix (verified live: /v1/models/load -> 404
        // "File Not Found" from the HTTP router, /models/load -> the
        // handler's 400/404 JSON). Plain GET /models works on both.
        let url = if reload {
            format!("{}/models?reload=1", self.admin_base())
        } else {
            format!("{}/models", self.base)
        };
        let resp = self.send(&url, reqwest::Method::GET, None).await?;
        let data: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| BackendError(format!("backend returned non-JSON response: {e}")))?;
        let mut out = Vec::new();
        if let Some(items) = data.get("data").and_then(serde_json::Value::as_array) {
            for item in items {
                if let Some(id) = item.get("id").and_then(serde_json::Value::as_str) {
                    out.push(model_info(id, item));
                }
            }
        }
        Ok(out)
    }

    /// POST /models/load: ask the backend to load a model into memory
    /// (llama.cpp router fork; the request itself auto-loads on first
    /// use anyway — this makes it explicit and immediate).
    /// 404 "model is not found", 400 "model is already running".
    pub async fn load_model(&self, model: &str) -> Result<(), BackendError> {
        self.model_op("load", model).await
    }

    /// POST /models/unload: drop a running model from memory.
    /// 404 "model is not found", 400 "model is not running".
    pub async fn unload_model(&self, model: &str) -> Result<(), BackendError> {
        self.model_op("unload", model).await
    }

    /// Server root for the MANAGEMENT routes (see `list_models_raw`):
    /// the llama.cpp router exposes `/models/load` & `/models/unload`
    /// only at the root, while `base_url` in this env is the `/v1`
    /// OAI-compat prefix. Strip a trailing `/v1` (or `/api`) segment;
    /// otherwise the base already IS the root.
    pub fn admin_base(&self) -> String {
        let trimmed = self.base.trim_end_matches('/');
        if let Some((prefix, seg)) = trimmed.rsplit_once('/') {
            if seg == "v1" || seg == "api" {
                return prefix.to_string();
            }
        }
        trimmed.to_string()
    }

    async fn model_op(&self, op: &str, model: &str) -> Result<(), BackendError> {
        let resp = self
            .send(
                &format!("{}/models/{op}", self.admin_base()),
                reqwest::Method::POST,
                Some(json!({ "model": model })),
            )
            .await?;
        // the body is {"success": true}; any error already surfaced above
        let _ = resp.bytes().await;
        Ok(())
    }
}

/// SSE body reader: `data:` lines, `[DONE]`, usage-only chunks,
/// reasoning_content deltas, tool-call deltas. Odd lines and non-JSON
/// payloads are skipped — the loop owns turn-level parsing.
///
/// Wraps `Response::bytes_stream()` (which owns the body, hence 'static
/// and Send) in a type-erased box; dropping the stream drops the body,
/// closing the connection (Python: resp.close() in finally).
/// The pure SSE state machine: feed raw bytes, get parsed events. Kept
/// separate from networking so the parse contract is unit-testable with
/// plain strings (the loop owns turn-level tool-call accumulation).
pub struct SseParser {
    buf: String,
    pending: VecDeque<StreamItem>,
    finish_reason: String,
    usage: Option<Usage>,
    finished: bool,
}

impl SseParser {
    pub fn new() -> Self {
        Self {
            buf: String::new(),
            pending: VecDeque::new(),
            finish_reason: String::new(),
            usage: None,
            finished: false,
        }
    }

    /// Append raw bytes; returns true once the stream is logically
    /// finished ([DONE] seen or the caller signalled EOF).
    pub fn feed(&mut self, chunk: &[u8], eof: bool) -> bool {
        self.buf.push_str(&String::from_utf8_lossy(chunk));
        self.drain_lines();
        if eof {
            self.finished = true;
        }
        self.finished
    }

    /// Take the next parsed item: a delta event, or the terminal Done
    /// once finished and all earlier events are drained.
    pub fn next_item(&mut self) -> Option<StreamItem> {
        if let Some(item) = self.pending.pop_front() {
            return Some(item);
        }
        if self.finished {
            Some(StreamItem::Done {
                finish_reason: std::mem::take(&mut self.finish_reason),
                usage: self.usage,
            })
        } else {
            None
        }
    }

    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// Parse one `data:` payload. Returns true on `[DONE]`.
    fn parse(&mut self, data: &str) -> bool {
        if data == "[DONE]" {
            self.finished = true;
            return true;
        }
        let Ok(chunk) = serde_json::from_str::<serde_json::Value>(data) else {
            return false; // keep going
        };
        // Live counters (see SseEvent::Usage): the top-level `usage`
        // object wins (OpenAI convention — the terminal usage chunk, but
        // some backends also send it progressively), else the llama.cpp
        // per-chunk `timings` object. Both are running totals for the
        // current request; last write wins, so when the stream ends
        // `self.usage` is the authoritative per-request count.
        let live = chunk
            .get("usage")
            .map(Usage::from_wire)
            .or_else(|| chunk.get("timings").and_then(Usage::from_timings));
        if let Some(u) = live {
            self.usage = Some(u);
            self.pending
                .push_back(StreamItem::Event(SseEvent::Usage(u)));
        }
        let Some(choices) =
            chunk.get("choices").and_then(serde_json::Value::as_array)
        else {
            return false;
        };
        if choices.is_empty() {
            return false; // usage-only chunk
        }
        if let Some(fr) = choices[0]
            .get("finish_reason")
            .and_then(serde_json::Value::as_str)
        {
            self.finish_reason = fr.to_string();
        }
        let Some(delta) = choices[0].get("delta") else {
            return false;
        };
        if let Some(r) = delta
            .get("reasoning_content")
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.is_empty())
        {
            self.pending.push_back(StreamItem::Event(SseEvent::Reasoning(
                r.to_string(),
            )));
        }
        if let Some(c) = delta
            .get("content")
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.is_empty())
        {
            self.pending.push_back(StreamItem::Event(SseEvent::Text(c.to_string())));
        }
        if let Some(tcs) = delta.get("tool_calls").and_then(serde_json::Value::as_array) {
            for tc in tcs {
                let fn_ = tc.get("function");
                self.pending.push_back(StreamItem::Event(SseEvent::ToolCall {
                    index: tc
                        .get("index")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0),
                    id: tc
                        .get("id")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string),
                    name: fn_
                        .and_then(|f| f.get("name"))
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string),
                    arguments: fn_
                        .and_then(|f| f.get("arguments"))
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                }));
            }
        }
        false
    }

    /// Pull every complete line out of the buffer.
    fn drain_lines(&mut self) {
        while let Some(pos) = self.buf.find('\n') {
            let line: String = self.buf[..pos].to_string();
            self.buf.drain(..=pos);
            let line = line.trim();
            if let Some(data) = line.strip_prefix("data:") {
                let data = data.trim();
                if data.is_empty() {
                    continue;
                }
                self.parse(data);
            }
            // non-data lines (comments, `event:`, `id:`) are ignored
        }
    }
}

/// Streaming wrapper: owns the response body and feeds [`SseParser`].
/// Dropping the stream drops the body, closing the connection (Python:
/// resp.close() in finally).
pub struct SseStream {
    inner: Pin<Box<dyn Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send>>,
    parser: SseParser,
    done_emitted: bool,
}

impl SseStream {
    fn new(resp: reqwest::Response) -> Self {
        Self {
            inner: Box::pin(resp.bytes_stream()),
            parser: SseParser::new(),
            done_emitted: false,
        }
    }
}

impl Stream for SseStream {
    type Item = Result<StreamItem, BackendError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            // 1) hand out parsed items
            if let Some(item) = self.parser.next_item() {
                if self.parser.is_finished() && item.is_done() {
                    self.done_emitted = true;
                    return Poll::Ready(Some(Ok(item)));
                }
                return Poll::Ready(Some(Ok(item)));
            }
            // 2) finished but parser drained -> terminal
            if self.parser.is_finished() {
                self.done_emitted = true;
                return Poll::Ready(None);
            }
            if self.done_emitted {
                return Poll::Ready(None);
            }
            // 3) need more bytes. Cancellation is the consumer's concern
            // (loop tokio::select!s against the CancellationToken, exactly
            // like Python checks the event per line).
            match Pin::new(&mut self.inner).poll_next(cx) {
                Poll::Ready(Some(Ok(b))) => {
                    self.parser.feed(&b, false);
                }
                Poll::Ready(Some(Err(e))) => {
                    return Poll::Ready(Some(Err(BackendError(format!(
                        "stream interrupted: {e}"
                    )))));
                }
                Poll::Ready(None) => {
                    self.parser.feed(&[], true); // EOF without [DONE]
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}


// ---------- the Backend trait (agent-facing seam) ----------

/// The boxed chat stream returned by [`Backend::stream_chat`]. `pub` so
/// fakes (unit tests, `tests/compaction.rs`) can name the return type.
pub type ChatStream<'a> = Pin<
    Box<dyn std::future::Future<
            Output = Result<
                Pin<Box<dyn Stream<Item = Result<StreamItem, BackendError>> + Send>>,
                BackendError,
            >,
        > + Send
        + 'a>,
>;
/// The boxed one-shot completion future returned by [`Backend::complete`].
pub type CompleteFut<'a> = Pin<Box<dyn std::future::Future<Output = Result<String, BackendError>> + Send + 'a>>;

/// The slice of the backend the agent loop and compaction consume.
///
/// `BackendClient` is the real HTTP implementation; tests substitute a
/// deterministic fake (this is Python's `FakeClient`/`ChainClient`). Keeping
/// the seam as a dyn-safe trait means the loop and compaction logic are
/// tested against fakes with zero networking, exactly like the reference
/// test suite.
pub trait Backend: Send + Sync {
    /// Model name the backend is currently configured for (diagnostics
    /// only; fakes may report their placeholder).
    fn model(&self) -> String {
        "fake".to_string()
    }

    /// Stream one completion. The returned stream always ends with a
    /// `StreamItem::Done`; the consumer stops it early by dropping it.
    fn stream_chat<'a>(
        &'a self,
        messages: &'a [Value],
        tools: Option<&'a [Value]>,
    ) -> ChatStream<'a>;

    /// One-shot completion (the compaction summarizer).
    fn complete<'a>(
        &'a self,
        messages: &'a [Value],
        max_tokens: u64,
    ) -> CompleteFut<'a>;
}

impl Backend for BackendClient {
    fn model(&self) -> String {
        self.model()
    }

    fn stream_chat<'a>(
        &'a self,
        messages: &'a [Value],
        tools: Option<&'a [Value]>,
    ) -> ChatStream<'a> {
        // Inherent `stream_chat` wins method resolution over this trait's
        // method (inherent impls are tried first) — no recursion.
        Box::pin(async move {
            let s = self.stream_chat(messages, tools).await?;
            Ok(Box::pin(s) as Pin<Box<dyn Stream<Item = Result<StreamItem, BackendError>> + Send>>)
        })
    }

    fn complete<'a>(
        &'a self,
        messages: &'a [Value],
        max_tokens: u64,
    ) -> CompleteFut<'a> {
        Box::pin(self.complete(messages, max_tokens))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_estimate_matches_python() {
        let msgs = vec![
            json!({"role": "user", "content": "abcd"}),
            json!({"role": "assistant", "content": "", "tool_calls": [
                {"function": {"arguments": "12345678"}}
            ]}),
        ];
        // 4 + 8 = 12 chars -> 12 // 4 = 3
        assert_eq!(estimate_prompt_tokens(&msgs), 3);
        assert_eq!(estimate_prompt_tokens(&[]), 1); // max(1, 0//4)
    }

    #[test]
    fn token_estimate_counts_vision_content_parts() {
        // Vision tool results (CR_01/8) use a parts array: only the text
        // parts count; the base64 image payload must not inflate the
        // estimate.
        let msgs = vec![
            json!({"role": "tool", "content": [
                {"type": "text", "text": "abcd"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,ZZZZZZZZ"}},
            ]}),
        ];
        // 4 chars of text -> 4 // 4 = 1 (not the base64 blob)
        assert_eq!(estimate_prompt_tokens(&msgs), 1);
        // missing content stays 0 (max(1,...) floor at the end)
        let msgs = vec![json!({"role": "assistant"})];
        assert_eq!(estimate_prompt_tokens(&msgs), 1);
    }

    #[test]
    fn model_status_parse_and_predicates() {
        for (wire, st) in [
            ("loaded", ModelStatus::Loaded),
            ("loading", ModelStatus::Loading),
            ("unloaded", ModelStatus::Unloaded),
            ("sleeping", ModelStatus::Sleeping),
            ("downloading", ModelStatus::Downloading),
            ("downloaded", ModelStatus::Downloaded),
        ] {
            assert_eq!(ModelStatus::from_wire(wire), st);
            assert_eq!(ModelStatus::from_wire("garbage"), ModelStatus::Unknown);
        }
        assert!(!ModelStatus::Unknown.is_transitional());
        assert!(ModelStatus::Loading.is_transitional());
        assert!(ModelStatus::Downloading.is_transitional());
        // serving = unknown (plain OpenAI) / loaded / sleeping (wakes on
        // request)
        assert!(ModelStatus::Unknown.is_serving());
        assert!(ModelStatus::Loaded.is_serving());
        assert!(ModelStatus::Sleeping.is_serving());
        assert!(!ModelStatus::Unloaded.is_serving());
        assert!(!ModelStatus::Loading.is_serving());
        // running = what the server accepts for unload (is_running)
        assert!(ModelStatus::Loaded.is_running());
        assert!(ModelStatus::Loading.is_running());
        assert!(ModelStatus::Sleeping.is_running());
        assert!(!ModelStatus::Unloaded.is_running());
    }

    #[test]
    fn model_info_parses_fork_and_openai_shapes() {
        // llama.cpp router fork entry: status object + source + ctx
        let fork = json!({
            "id": "qwen_3_8_27B",
            "status": {"value": "loading", "args": ["--n-gpu-layers", "99"]},
            "source": "models_dir",
            "can_remove": false,
            "context_length": 262144
        });
        let m = model_info("qwen_3_8_27B", &fork);
        assert_eq!(m.status, ModelStatus::Loading);
        assert!(!m.failed);
        assert_eq!(m.source.as_deref(), Some("models_dir"));
        assert_eq!(m.context_length, Some(262144));

        // failed load: unloaded + failed + exit_code
        let failed = json!({
            "id": "broken",
            "status": {"value": "unloaded", "failed": true, "exit_code": 137}
        });
        let m = model_info("broken", &failed);
        assert_eq!(m.status, ModelStatus::Unloaded);
        assert!(m.failed);
        assert_eq!(m.source, None);
        assert_eq!(m.context_length, None);

        // plain OpenAI backend: bare id, no status -> Unknown
        let plain = json!({"id": "gpt-x", "object": "model", "owned_by": "openai"});
        let m = model_info("gpt-x", &plain);
        assert_eq!(m.status, ModelStatus::Unknown);
        assert!(!m.failed);
        assert!(m.status.is_serving());
    }

    #[test]
    fn admin_base_strips_the_oai_prefix() {
        let mut mk = |url: &str| {
            let mut cfg = Config::default();
            cfg.backend.base_url = url.into();
            BackendClient::new(&cfg)
        };
        // /v1 prefix (this env's production shape)
        assert_eq!(mk("https://llm.example.lan/v1").admin_base(), "https://llm.example.lan");
        // trailing slash
        assert_eq!(
            mk("https://llm.example.lan/v1/").admin_base(),
            "https://llm.example.lan"
        );
        // nested prefix
        assert_eq!(
            mk("https://host:443/proxy/v1").admin_base(),
            "https://host:443/proxy"
        );
        // /api prefix (some proxies)
        assert_eq!(mk("http://h/api").admin_base(), "http://h");
        // a root base is already the root — unchanged
        assert_eq!(mk("http://127.0.0.1:9999").admin_base(), "http://127.0.0.1:9999");
        // a path that merely CONTAINS v1 but doesn't end in it
        assert_eq!(
            mk("http://127.0.0.1:9999/v1x").admin_base(),
            "http://127.0.0.1:9999/v1x"
        );
    }

    #[tokio::test]
    async fn payload_shape_and_stream_options() {
        let cfg = Config::default();
        let c = BackendClient::new(&cfg);
        let p = c.payload(
            &[json!({"role": "user", "content": "hi"})],
            Some(&[]),
            100,
            true,
        );
        assert_eq!(p["model"], "default");
        assert_eq!(p["max_tokens"], 100);
        assert_eq!(p["stream"], true);
        assert_eq!(p["reasoning_effort"], "medium");
        assert_eq!(p["stream_options"]["include_usage"], true);
        // live per-chunk counters from the llama.cpp server (see payload())
        assert_eq!(p["timings_per_token"], true);
        assert_eq!(p["return_progress"], true);
        assert!(p.get("tools").is_none(), "empty tools array is omitted");
        let p = c.payload(&[], Some(&[json!({"type": "function"})]), 100, false);
        assert!(p.get("tools").is_some());
        assert!(p.get("stream_options").is_none());
        // the progress fields are stream-only
        assert!(p.get("timings_per_token").is_none());
        assert!(p.get("return_progress").is_none());
    }

    /// Feed a raw SSE string through the parser to verify the full event
    /// contract (multi-chunk tool-call accumulation happens in the loop;
    /// here we check the deltas and the terminal Done).
    #[test]
    fn sse_parse_contract() {
        let body = "\
data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"hmm \"}}]}

data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}

data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"list_dir\",\"arguments\":\"{\"}}]}}]}

data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"path\\\": \\\".\\\"}\"}}]}}]}

not-json garbage

: a comment line

data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":7}}

data: [DONE]
";
        let mut p = SseParser::new();
        // feed in two halves to prove line framing survives chunking
        let half = body.len() / 2;
        assert!(!p.feed(body[..half].as_bytes(), false));
        assert!(p.feed(body[half..].as_bytes(), false));
        let mut items: Vec<StreamItem> = Vec::new();
        while let Some(it) = p.next_item() {
            let is_done = it.is_done();
            items.push(it);
            if is_done {
                break;
            }
        }
        let texts: Vec<&str> = items.iter().filter_map(|i| match i {
            StreamItem::Event(SseEvent::Text(t)) => Some(t.as_str()),
            _ => None,
        }).collect();
        assert_eq!(texts, vec!["Hel"]);
        let reasoning: Vec<&str> = items.iter().filter_map(|i| match i {
            StreamItem::Event(SseEvent::Reasoning(t)) => Some(t.as_str()),
            _ => None,
        }).collect();
        assert_eq!(reasoning, vec!["hmm "]);
        let tool_calls: Vec<(&u64, Option<&str>, Option<&str>, &str)> = items.iter().filter_map(|i| match i {
            StreamItem::Event(SseEvent::ToolCall { index, id, name, arguments }) => {
                Some((index, id.as_deref(), name.as_deref(), arguments.as_str()))
            }
            _ => None,
        }).collect();
        assert_eq!(tool_calls, vec![
            (&0, Some("call_1"), Some("list_dir"), "{"),
            (&0, None, None, "\"path\": \".\"}"),
        ]);
        let last = items.last().unwrap();
        assert!(
            matches!(last, StreamItem::Done { finish_reason, usage: Some(u) }
                if finish_reason == "tool_calls" && u.prompt_tokens == 11 && u.completion_tokens == 7),
            "terminal Done mismatch: {items:?}"
        );
        // the final usage chunk also surfaced as a live event before Done
        let live: Vec<Usage> = items.iter().filter_map(|i| match i {
            StreamItem::Event(SseEvent::Usage(u)) => Some(*u),
            _ => None,
        }).collect();
        assert_eq!(live, vec![Usage { prompt_tokens: 11, completion_tokens: 7 }]);
    }

    /// llama.cpp server with `timings_per_token`: every chunk carries a
    /// `timings` object (predicted_n = completion so far, prompt_n +
    /// cache_n = prompt size) and no separate usage chunk. The live
    /// counters must surface as `SseEvent::Usage` and the LAST one must
    /// end up in the terminal Done.
    #[test]
    fn sse_live_usage_from_timings() {
        let body = "\
data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":null}}],\"timings\":{\"cache_n\":8,\"prompt_n\":4,\"predicted_n\":0}}

data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}],\"timings\":{\"cache_n\":8,\"prompt_n\":4,\"predicted_n\":1}}

data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}],\"timings\":{\"cache_n\":8,\"prompt_n\":4,\"predicted_n\":2}}

data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}
";
        let mut p = SseParser::new();
        assert!(!p.feed(body.as_bytes(), false));
        assert!(p.feed(b"", true)); // EOF without [DONE]
        let mut items: Vec<StreamItem> = Vec::new();
        while let Some(it) = p.next_item() {
            let done = it.is_done();
            items.push(it);
            if done {
                break;
            }
        }
        let live: Vec<Usage> = items.iter().filter_map(|i| match i {
            StreamItem::Event(SseEvent::Usage(u)) => Some(*u),
            _ => None,
        }).collect();
        assert_eq!(live, vec![
            Usage { prompt_tokens: 12, completion_tokens: 0 },
            Usage { prompt_tokens: 12, completion_tokens: 1 },
            Usage { prompt_tokens: 12, completion_tokens: 2 },
        ]);
        let texts: Vec<&str> = items.iter().filter_map(|i| match i {
            StreamItem::Event(SseEvent::Text(t)) => Some(t.as_str()),
            _ => None,
        }).collect();
        assert_eq!(texts, vec!["Hel", "lo"]);
        // last live counter becomes the terminal usage
        let last = items.last().unwrap();
        assert!(
            matches!(last, StreamItem::Done { finish_reason, usage: Some(u) }
                if finish_reason == "stop" && u.prompt_tokens == 12 && u.completion_tokens == 2),
            "terminal Done mismatch: {items:?}"
        );
    }
}
