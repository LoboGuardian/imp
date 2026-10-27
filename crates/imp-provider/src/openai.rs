//! OpenAI-compatible Chat Completions client.
//!
//! Hand-rolled rather than built on an SDK so that `base_url` handling, SSE
//! parsing, and provider quirk flags stay under our control and testable.

use std::time::Duration;

use futures::StreamExt;
use futures::stream::BoxStream;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use imp_core::error::{Error, Result};
use imp_core::provider::{ChatEvent, ChatRequest, FinishReason, Provider, Usage, sanitize_schema};

use crate::sse::SseDecoder;

/// Idle timeout applied while waiting for the next stream chunk.
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// Connect timeout for the initial request.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Upper bound on exponential backoff between retries.
const MAX_BACKOFF: Duration = Duration::from_secs(8);

/// An API key that never renders itself in logs or `Debug` output.
#[derive(Clone)]
pub struct ApiKey(String);

impl ApiKey {
    /// Wrap a key.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Whether no key was configured, as for a local backend.
    fn is_empty(&self) -> bool {
        self.0.trim().is_empty()
    }

    /// Borrow the secret. Call sites are expected to be few and obvious.
    fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ApiKey(***)")
    }
}

/// A client for any endpoint implementing `/chat/completions`.
#[derive(Debug, Clone)]
pub struct OpenAiProvider {
    client: reqwest::Client,
    base_url: String,
    api_key: ApiKey,
    max_retries: u32,
    request_timeout: Option<Duration>,
    supports_usage_in_stream: bool,
    strict_tool_arguments: bool,
    headers: Vec<(String, String)>,
    session_id: Option<String>,
    stream: bool,
    stream_idle_timeout: Duration,
    sanitize_schemas: bool,
    omit_parallel_tool_calls: bool,
    omit_tool_choice: bool,
    empty_assistant_content: bool,
    extra_body: serde_json::Map<String, serde_json::Value>,
}

impl OpenAiProvider {
    /// Build a provider against `base_url` (without `/chat/completions`).
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .expect("reqwest client construction cannot fail with these settings");
        Self {
            client,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: ApiKey::new(api_key),
            max_retries: 3,
            request_timeout: Some(Duration::from_secs(120)),
            supports_usage_in_stream: true,
            strict_tool_arguments: false,
            headers: Vec::new(),
            session_id: None,
            stream: true,
            stream_idle_timeout: STREAM_IDLE_TIMEOUT,
            sanitize_schemas: true,
            omit_parallel_tool_calls: false,
            omit_tool_choice: false,
            empty_assistant_content: false,
            extra_body: serde_json::Map::new(),
        }
    }

    /// Use the streaming or the whole-response endpoint.
    ///
    /// A backend with a broken SSE implementation can be driven non-streaming;
    /// the events are synthesized from the single JSON response.
    pub fn with_stream(mut self, stream: bool) -> Self {
        self.stream = stream;
        self
    }

    /// Set the idle budget between stream chunks.
    pub fn with_stream_idle_timeout(mut self, timeout: Duration) -> Self {
        self.stream_idle_timeout = timeout;
        self
    }

    /// Strip non-essential keywords from the advertised tool schemas.
    pub fn with_sanitize_schemas(mut self, sanitize: bool) -> Self {
        self.sanitize_schemas = sanitize;
        self
    }

    /// Omit `parallel_tool_calls` even when the loop asks for it.
    pub fn with_omit_parallel_tool_calls(mut self, omit: bool) -> Self {
        self.omit_parallel_tool_calls = omit;
        self
    }

    /// Omit `tool_choice` entirely.
    pub fn with_omit_tool_choice(mut self, omit: bool) -> Self {
        self.omit_tool_choice = omit;
        self
    }

    /// Send `"content": null` on assistant tool-call messages.
    pub fn with_empty_assistant_content(mut self, empty: bool) -> Self {
        self.empty_assistant_content = empty;
        self
    }

    /// Merge extra keys into every request body (backend-specific sampling).
    pub fn with_extra_body(mut self, extra: serde_json::Map<String, serde_json::Value>) -> Self {
        self.extra_body = extra;
        self
    }

    /// Set the retry budget for pre-stream failures.
    pub fn with_max_retries(mut self, max_retries: u32) -> Self {
        self.max_retries = max_retries;
        self
    }

    /// Set the total request budget. `None` disables the total timeout.
    pub fn with_request_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.request_timeout = timeout;
        self
    }

    /// Tell the client whether the provider reports usage on the last chunk.
    ///
    /// When false, `stream_options` is omitted entirely: several compatible
    /// providers reject unknown request fields.
    pub fn with_usage_in_stream(mut self, supported: bool) -> Self {
        self.supports_usage_in_stream = supported;
        self
    }

    /// Ask the backend to constrain tool arguments to their JSON schema.
    ///
    /// A provider quirk (M10.3). When the backend understands `strict` — OpenAI
    /// function calling, Ollama's structured outputs, a llama.cpp built with
    /// `--jinja` and a grammar — each advertised function carries it. When the
    /// backend does not, the field is simply absent from the request, which is
    /// what "unchanged behaviour" means here.
    pub fn with_strict_tool_arguments(mut self, strict: bool) -> Self {
        self.strict_tool_arguments = strict;
        self
    }

    /// Add headers sent on every request.
    ///
    /// A value may contain `${session}`, replaced by the conversation id set
    /// with [`with_session_id`](Self::with_session_id). Headers whose value is
    /// empty after substitution are dropped rather than sent blank.
    pub fn with_headers(mut self, headers: impl IntoIterator<Item = (String, String)>) -> Self {
        self.headers = headers.into_iter().collect();
        self
    }

    /// Set the stable identifier for the conversation this client serves.
    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    /// Headers with `${session}` expanded, skipping any that resolve to nothing.
    fn resolve_headers(&self) -> Vec<(String, String)> {
        let session = self.session_id.as_deref().unwrap_or_default();
        self.headers
            .iter()
            .map(|(name, value)| (name.clone(), value.replace("${session}", session)))
            .filter(|(name, value)| {
                let keep = !name.trim().is_empty() && !value.trim().is_empty();
                if !keep {
                    tracing::debug!(header = %name, "dropping header with an empty value");
                }
                keep
            })
            .collect()
    }

    /// Apply authentication and configured headers to a request.
    fn decorate(&self, mut request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if !self.api_key.is_empty() {
            request = request.bearer_auth(self.api_key.expose());
        }
        for (name, value) in self.resolve_headers() {
            request = request.header(name, value);
        }
        request
    }

    /// The endpoint root, without a trailing slash.
    pub(crate) fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Fetch JSON from `url`, giving up after `timeout` and swallowing every
    /// failure.
    ///
    /// Only the context probe uses this: discovery is opportunistic, and a
    /// backend that does not answer must degrade to configuration rather than
    /// fail a session.
    pub(crate) async fn probe_json(
        &self,
        url: &str,
        timeout: Duration,
    ) -> Option<serde_json::Value> {
        let request = self.decorate(self.client.get(url));
        let response = tokio::time::timeout(timeout, request.send())
            .await
            .ok()?
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        tokio::time::timeout(timeout, response.json())
            .await
            .ok()?
            .ok()
    }

    /// List the models the backend advertises, via `GET {base_url}/models`.
    ///
    /// Used by `imp init --check` and model discovery. Endpoints that do not
    /// implement `/models` return an error; callers treat that as non-fatal.
    pub async fn list_models(&self) -> Result<Vec<String>> {
        let url = format!("{}/models", self.base_url);
        let request = self.decorate(self.client.get(&url));
        let response = request
            .send()
            .await
            .map_err(|err| Error::Provider(err.to_string()))?;

        let status = response.status();
        if !status.is_success() {
            let detail = response.text().await.unwrap_or_default();
            return Err(classify(status.as_u16(), detail, None));
        }

        let body: serde_json::Value = response
            .json()
            .await
            .map_err(|err| Error::Provider(format!("unreadable /models response: {err}")))?;

        Ok(body
            .get("data")
            .and_then(serde_json::Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|entry| {
                        entry
                            .get("id")
                            .and_then(|id| id.as_str())
                            .map(str::to_string)
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    /// The request body, built explicitly so unsupported fields can be omitted.
    fn body(&self, request: &ChatRequest) -> serde_json::Value {
        let mut body = serde_json::json!({
            "model": request.model,
            "messages": self.wire_messages(request),
            "stream": self.stream,
        });
        let object = body.as_object_mut().expect("body is an object");

        if let Some(temperature) = request.temperature {
            object.insert("temperature".to_string(), serde_json::json!(temperature));
        }
        if let Some(max_tokens) = request.max_tokens {
            object.insert("max_tokens".to_string(), serde_json::json!(max_tokens));
        }
        if !request.tools.is_empty() {
            // Each entry must be the nested `{type, function}` shape, not the
            // flattened struct: providers reject the flat form with a 400.
            let tools: Vec<serde_json::Value> = request
                .tools
                .iter()
                .map(|tool| {
                    let mut wire = tool.to_wire_strict(self.strict_tool_arguments);
                    if self.sanitize_schemas
                        && let Some(parameters) = wire
                            .get_mut("function")
                            .and_then(|function| function.get_mut("parameters"))
                    {
                        sanitize_schema(parameters);
                    }
                    wire
                })
                .collect();
            object.insert("tools".to_string(), serde_json::json!(tools));
            if !self.omit_tool_choice {
                object.insert("tool_choice".to_string(), serde_json::json!("auto"));
            }
        }
        if let Some(parallel) = request.parallel_tool_calls
            && !self.omit_parallel_tool_calls
        {
            object.insert(
                "parallel_tool_calls".to_string(),
                serde_json::json!(parallel),
            );
        }
        if request.include_usage && self.supports_usage_in_stream {
            object.insert(
                "stream_options".to_string(),
                serde_json::json!({ "include_usage": true }),
            );
        }
        // Backend-specific sampling goes last. Collisions with the core keys are
        // rejected by `Config::validate`, so this only ever adds or overrides
        // sampling fields such as `temperature`.
        for (key, value) in &self.extra_body {
            object.insert(key.clone(), value.clone());
        }
        body
    }

    /// Serialize the messages, optionally forcing `"content": null` on an
    /// assistant tool-call message for templates that require the key.
    fn wire_messages(&self, request: &ChatRequest) -> serde_json::Value {
        let mut messages =
            serde_json::to_value(&request.messages).unwrap_or_else(|_| serde_json::json!([]));
        if !self.empty_assistant_content {
            return messages;
        }
        if let Some(list) = messages.as_array_mut() {
            for message in list {
                let is_tool_call_assistant = message.get("role").and_then(|role| role.as_str())
                    == Some("assistant")
                    && message.get("tool_calls").is_some()
                    && message.get("content").is_none();
                if is_tool_call_assistant && let Some(object) = message.as_object_mut() {
                    object.insert("content".to_string(), serde_json::Value::Null);
                }
            }
        }
        messages
    }
}

impl Provider for OpenAiProvider {
    fn stream(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> BoxStream<'static, Result<ChatEvent>> {
        let this = self.clone();
        Box::pin(async_stream::stream! {
            let url = format!("{}/chat/completions", this.base_url);
            let body = this.body(&request);

            let response = match this.open(&url, &body, &cancel).await {
                Ok(response) => response,
                Err(err) => {
                    yield Err(err);
                    return;
                }
            };

            // A non-streaming backend returns one JSON body; synthesize the
            // same event sequence so the agent loop cannot tell the difference.
            if !this.stream {
                let payload = match response.text().await {
                    Ok(text) => text,
                    Err(err) => {
                        yield Err(Error::Provider(err.to_string()));
                        return;
                    }
                };
                for event in parse_completion(&payload) {
                    yield event;
                }
                return;
            }

            let mut bytes = response.bytes_stream();
            let mut decoder = SseDecoder::new();
            let mut finish_reason = None;
            let mut saw_event = false;

            loop {
                if cancel.is_cancelled() {
                    yield Err(Error::Cancelled);
                    return;
                }

                let raced = tokio::select! {
                    _ = cancel.cancelled() => None,
                    outcome = tokio::time::timeout(this.stream_idle_timeout, bytes.next()) => Some(outcome),
                };

                let Some(outcome) = raced else {
                    yield Err(Error::Cancelled);
                    return;
                };

                let chunk = match outcome {
                    // Clean end of stream.
                    Ok(None) => break,
                    Ok(Some(Ok(chunk))) => chunk,
                    Ok(Some(Err(err))) => {
                        yield Err(Error::Provider(err.to_string()));
                        return;
                    }
                    // Idle timeout: the provider stalled mid-stream. The budget
                    // is configurable because local prefill can be slow.
                    Err(_) => {
                        yield Err(Error::Provider(format!(
                            "stream stalled for more than {}s",
                            this.stream_idle_timeout.as_secs()
                        )));
                        return;
                    }
                };

                let text = String::from_utf8_lossy(&chunk);
                let mut saw_done = false;

                for payload in decoder.push(&text) {
                    if payload.trim() == "[DONE]" {
                        saw_done = true;
                        break;
                    }
                    let parsed: Chunk = match serde_json::from_str(&payload) {
                        Ok(parsed) => parsed,
                        Err(err) => {
                            // One non-conforming frame is not worth ending a
                            // turn over; note it and keep reading. If nothing
                            // valid arrives at all, the check below fails.
                            tracing::warn!(error = %err, "skipping an unparseable stream chunk");
                            continue;
                        }
                    };

                    if let Some(wire) = parsed.usage {
                        saw_event = true;
                        yield Ok(ChatEvent::Usage(Usage {
                            prompt_tokens: wire.prompt_tokens.unwrap_or(0),
                            completion_tokens: wire.completion_tokens.unwrap_or(0),
                            total_tokens: wire.total_tokens.unwrap_or(0),
                        }));
                    }

                    for choice in parsed.choices {
                        if let Some(content) = choice.delta.content
                            && !content.is_empty()
                        {
                            saw_event = true;
                            yield Ok(ChatEvent::TextDelta(content));
                        }
                        for call in choice.delta.tool_calls.unwrap_or_default() {
                            let (name, arguments) = match call.function {
                                Some(function) => (function.name, function.arguments),
                                None => (None, None),
                            };
                            saw_event = true;
                            yield Ok(ChatEvent::ToolCallDelta {
                                index: call.index,
                                id: call.id,
                                name,
                                arguments: arguments.map(stringify_arguments).unwrap_or_default(),
                            });
                        }
                        if let Some(reason) = choice.finish_reason {
                            saw_event = true;
                            finish_reason = Some(FinishReason::from_wire(&reason));
                        }
                    }
                }

                if saw_done {
                    break;
                }
            }

            if !saw_event {
                yield Err(Error::Provider(
                    "the stream ended without any events; is this an OpenAI-compatible endpoint?"
                        .to_string(),
                ));
                return;
            }

            yield Ok(ChatEvent::Done {
                finish_reason: finish_reason.unwrap_or(FinishReason::Other),
            });
        })
    }
}

/// Render a tool-call `arguments` value as the string the layer above expects.
///
/// The wire type is normally a string, but a non-conforming backend may send an
/// object or an array; stringifying it is cheaper than failing the whole chunk.
fn stringify_arguments(value: serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text,
        other => other.to_string(),
    }
}

/// Turn one whole (non-streaming) completion into the streaming event sequence.
fn parse_completion(payload: &str) -> Vec<Result<ChatEvent>> {
    let completion: Completion = match serde_json::from_str(payload) {
        Ok(completion) => completion,
        Err(err) => {
            return vec![Err(Error::Provider(format!(
                "unreadable completion body: {err}"
            )))];
        }
    };

    let mut events = Vec::new();
    if let Some(wire) = completion.usage {
        events.push(Ok(ChatEvent::Usage(Usage {
            prompt_tokens: wire.prompt_tokens.unwrap_or(0),
            completion_tokens: wire.completion_tokens.unwrap_or(0),
            total_tokens: wire.total_tokens.unwrap_or(0),
        })));
    }

    let mut finish_reason = FinishReason::Other;
    for choice in completion.choices {
        if let Some(content) = choice.message.content
            && !content.is_empty()
        {
            events.push(Ok(ChatEvent::TextDelta(content)));
        }
        for (index, call) in choice
            .message
            .tool_calls
            .unwrap_or_default()
            .into_iter()
            .enumerate()
        {
            let (name, arguments) = match call.function {
                Some(function) => (function.name, function.arguments),
                None => (None, None),
            };
            events.push(Ok(ChatEvent::ToolCallDelta {
                index,
                id: call.id,
                name,
                arguments: arguments.map(stringify_arguments).unwrap_or_default(),
            }));
        }
        if let Some(reason) = choice.finish_reason {
            finish_reason = FinishReason::from_wire(&reason);
        }
    }

    events.push(Ok(ChatEvent::Done { finish_reason }));
    events
}

impl OpenAiProvider {
    /// Send the request, retrying transient failures.
    ///
    /// Retries happen only here, before any event reaches the caller, so a
    /// retry can never duplicate text or re-run a tool.
    async fn open(
        &self,
        url: &str,
        body: &serde_json::Value,
        cancel: &CancellationToken,
    ) -> Result<reqwest::Response> {
        let mut attempt = 0u32;
        loop {
            let attempt_result = tokio::select! {
                _ = cancel.cancelled() => Err(Error::Cancelled),
                result = self.send_once(url, body) => result,
            };

            match attempt_result {
                Ok(response) if response.status().is_success() => return Ok(response),
                Ok(response) => {
                    let status = response.status();
                    let retry_after = retry_after(&response);
                    let detail = response.text().await.unwrap_or_default();
                    let retryable = status.as_u16() == 429 || status.is_server_error();

                    if retryable && attempt < self.max_retries {
                        attempt += 1;
                        let delay = backoff(attempt, retry_after);
                        tracing::warn!(
                            status = status.as_u16(),
                            attempt,
                            delay_ms = delay.as_millis() as u64,
                            "provider request failed, retrying"
                        );
                        sleep_or_cancel(delay, cancel).await?;
                        continue;
                    }

                    return Err(classify(status.as_u16(), detail, retry_after));
                }
                Err(err) => {
                    if attempt < self.max_retries {
                        attempt += 1;
                        let delay = backoff(attempt, None);
                        tracing::warn!(error = %err, attempt, "transport failure, retrying");
                        sleep_or_cancel(delay, cancel).await?;
                        continue;
                    }
                    return Err(err);
                }
            }
        }
    }

    /// One HTTP attempt, bounded by the configured total request budget.
    async fn send_once(&self, url: &str, body: &serde_json::Value) -> Result<reqwest::Response> {
        let request = self.decorate(self.client.post(url).json(body)).send();

        match self.request_timeout {
            Some(budget) => match tokio::time::timeout(budget, request).await {
                Ok(result) => result.map_err(|err| Error::Provider(err.to_string())),
                Err(_) => Err(Error::Provider(format!(
                    "request exceeded its {}s budget",
                    budget.as_secs()
                ))),
            },
            None => request
                .await
                .map_err(|err| Error::Provider(err.to_string())),
        }
    }
}

/// Sleep, unless the turn is cancelled first.
async fn sleep_or_cancel(delay: Duration, cancel: &CancellationToken) -> Result<()> {
    tokio::select! {
        _ = cancel.cancelled() => Err(Error::Cancelled),
        _ = tokio::time::sleep(delay) => Ok(()),
    }
}

/// Exponential backoff with a ceiling, honouring a provider's `Retry-After`.
fn backoff(attempt: u32, retry_after: Option<Duration>) -> Duration {
    if let Some(delay) = retry_after {
        return delay.min(MAX_BACKOFF);
    }
    let exponential = Duration::from_millis(250) * 2u32.saturating_pow(attempt.min(6));
    exponential.min(MAX_BACKOFF)
}

/// Read and parse a `Retry-After` header expressed in seconds.
fn retry_after(response: &reqwest::Response) -> Option<Duration> {
    response
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

/// Map an HTTP failure onto a typed error.
fn classify(status: u16, detail: String, retry_after: Option<Duration>) -> Error {
    match status {
        401 | 403 => Error::Auth(detail),
        429 => Error::RateLimit { retry_after },
        400..=499 => Error::BadRequest {
            status,
            message: detail,
        },
        _ => Error::Provider(format!("HTTP {status}: {detail}")),
    }
}

/// One `chat.completion.chunk`.
#[derive(Debug, Deserialize)]
struct Chunk {
    #[serde(default)]
    choices: Vec<ChunkChoice>,
    #[serde(default)]
    usage: Option<WireUsage>,
}

#[derive(Debug, Deserialize)]
struct ChunkChoice {
    #[serde(default)]
    delta: Delta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<DeltaToolCall>>,
}

#[derive(Debug, Deserialize)]
struct DeltaToolCall {
    #[serde(default)]
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<DeltaFunction>,
}

#[derive(Debug, Deserialize)]
struct DeltaFunction {
    #[serde(default)]
    name: Option<String>,
    /// A string on a conforming backend; a `Value` so an object or array does
    /// not fail the whole chunk.
    #[serde(default)]
    arguments: Option<serde_json::Value>,
}

/// One whole `chat.completion` response, for the non-streaming path.
#[derive(Debug, Deserialize)]
struct Completion {
    #[serde(default)]
    choices: Vec<CompletionChoice>,
    #[serde(default)]
    usage: Option<WireUsage>,
}

#[derive(Debug, Deserialize)]
struct CompletionChoice {
    #[serde(default)]
    message: CompletionMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct CompletionMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<CompleteToolCall>>,
}

#[derive(Debug, Deserialize)]
struct CompleteToolCall {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<DeltaFunction>,
}

#[derive(Debug, Deserialize)]
struct WireUsage {
    #[serde(default)]
    prompt_tokens: Option<u32>,
    #[serde(default)]
    completion_tokens: Option<u32>,
    #[serde(default)]
    total_tokens: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use imp_core::message::Message;

    #[test]
    fn api_key_is_redacted_in_debug_output() {
        let key = ApiKey::new("sk-super-secret");
        let rendered = format!("{key:?}");
        assert!(!rendered.contains("super-secret"), "key leaked: {rendered}");
    }

    #[test]
    fn base_url_trailing_slash_is_normalized() {
        let provider = OpenAiProvider::new("http://localhost:11434/v1/", "k");
        assert_eq!(provider.base_url, "http://localhost:11434/v1");
    }

    #[test]
    fn usage_option_is_omitted_when_unsupported() {
        let provider = OpenAiProvider::new("http://localhost/v1", "k").with_usage_in_stream(false);
        let request = ChatRequest {
            model: "m".to_string(),
            messages: vec![Message::user("hi")],
            tools: Vec::new(),
            temperature: None,
            max_tokens: None,
            parallel_tool_calls: None,
            include_usage: true,
        };

        let body = provider.body(&request);

        assert!(body.get("stream_options").is_none());
    }

    #[test]
    fn tools_are_only_advertised_when_present() {
        let provider = OpenAiProvider::new("http://localhost/v1", "k");
        let mut request = ChatRequest {
            model: "m".to_string(),
            messages: vec![Message::user("hi")],
            tools: Vec::new(),
            temperature: None,
            max_tokens: None,
            parallel_tool_calls: None,
            include_usage: false,
        };

        assert!(provider.body(&request).get("tools").is_none());

        request.tools.push(imp_core::provider::ToolSchema {
            name: "read_file".to_string(),
            description: "read".to_string(),
            parameters: serde_json::json!({"type": "object"}),
        });
        let body = provider.body(&request);
        assert!(body.get("tools").is_some());
        assert_eq!(body.get("tool_choice").unwrap(), "auto");
    }

    #[test]
    fn each_tool_is_wrapped_in_the_function_envelope() {
        // The flat `{name, description, parameters}` form is rejected with a 400
        // by compliant providers, so the nesting is asserted explicitly rather
        // than left to whatever a permissive test double happens to accept.
        let provider = OpenAiProvider::new("http://localhost/v1", "k");
        let request = ChatRequest {
            model: "m".to_string(),
            messages: vec![Message::user("hi")],
            tools: vec![imp_core::provider::ToolSchema {
                name: "read_file".to_string(),
                description: "read".to_string(),
                parameters: serde_json::json!({"type": "object"}),
            }],
            temperature: None,
            max_tokens: None,
            parallel_tool_calls: None,
            include_usage: false,
        };

        let body = provider.body(&request);
        let entry = &body["tools"][0];

        assert_eq!(entry["type"], "function");
        assert_eq!(entry["function"]["name"], "read_file");
        assert_eq!(entry["function"]["description"], "read");
        assert_eq!(entry["function"]["parameters"]["type"], "object");
        assert!(
            entry.get("name").is_none(),
            "the flat form must not leak alongside the envelope"
        );
    }

    #[test]
    fn strict_tool_arguments_are_off_by_default() {
        let provider = OpenAiProvider::new("http://localhost/v1", "k");
        let request = ChatRequest {
            model: "m".to_string(),
            messages: vec![Message::user("hi")],
            tools: vec![imp_core::provider::ToolSchema {
                name: "read_file".to_string(),
                description: "read".to_string(),
                parameters: serde_json::json!({ "type": "object" }),
            }],
            temperature: None,
            max_tokens: None,
            parallel_tool_calls: None,
            include_usage: false,
        };

        let body = provider.body(&request);

        assert!(
            body["tools"][0]["function"].get("strict").is_none(),
            "the default request must be unchanged: {body}"
        );
    }

    #[test]
    fn the_strict_quirk_marks_every_function() {
        let provider =
            OpenAiProvider::new("http://localhost/v1", "k").with_strict_tool_arguments(true);
        let request = ChatRequest {
            model: "m".to_string(),
            messages: vec![Message::user("hi")],
            tools: vec![imp_core::provider::ToolSchema {
                name: "read_file".to_string(),
                description: "read".to_string(),
                parameters: serde_json::json!({ "type": "object" }),
            }],
            temperature: None,
            max_tokens: None,
            parallel_tool_calls: None,
            include_usage: false,
        };

        let body = provider.body(&request);

        assert_eq!(
            body["tools"][0]["function"]["strict"],
            serde_json::json!(true)
        );
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "read_file");
    }

    #[test]
    fn retry_after_wins_over_exponential_backoff() {
        let delay = backoff(1, Some(Duration::from_secs(3)));
        assert_eq!(delay, Duration::from_secs(3));
    }

    #[test]
    fn backoff_is_capped() {
        assert!(backoff(20, None) <= MAX_BACKOFF);
    }

    #[test]
    fn status_codes_map_to_typed_errors() {
        assert!(matches!(classify(401, "nope".into(), None), Error::Auth(_)));
        assert!(matches!(
            classify(429, "slow".into(), None),
            Error::RateLimit { .. }
        ));
        assert!(matches!(
            classify(404, "missing".into(), None),
            Error::BadRequest { .. }
        ));
        assert!(matches!(
            classify(503, "down".into(), None),
            Error::Provider(_)
        ));
    }

    fn headers(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn the_session_placeholder_expands_to_the_conversation_id() {
        let provider = OpenAiProvider::new("http://localhost/v1", "k")
            .with_headers(headers(&[("x-opencode-session", "${session}")]))
            .with_session_id("0192-abc");

        assert_eq!(
            provider.resolve_headers(),
            headers(&[("x-opencode-session", "0192-abc")])
        );
    }

    #[test]
    fn a_header_needing_a_missing_session_is_dropped_rather_than_sent_blank() {
        let provider = OpenAiProvider::new("http://localhost/v1", "k")
            .with_headers(headers(&[("x-opencode-session", "${session}")]));

        assert!(provider.resolve_headers().is_empty());
    }

    #[test]
    fn static_headers_pass_through_untouched() {
        let provider = OpenAiProvider::new("http://localhost/v1", "k")
            .with_headers(headers(&[("x-title", "imp"), ("x-session", "fixed")]))
            .with_session_id("ignored");

        let resolved = provider.resolve_headers();

        assert_eq!(resolved.len(), 2);
        assert!(resolved.contains(&("x-title".to_string(), "imp".to_string())));
        assert!(resolved.contains(&("x-session".to_string(), "fixed".to_string())));
    }

    #[test]
    fn an_empty_api_key_means_no_authorization_header() {
        let provider = OpenAiProvider::new("http://localhost:11434/v1", "");

        assert!(provider.api_key.is_empty());
    }

    fn request_with_tool() -> ChatRequest {
        ChatRequest {
            model: "m".to_string(),
            messages: vec![Message::user("hi")],
            tools: vec![imp_core::provider::ToolSchema {
                name: "read_file".to_string(),
                description: "read".to_string(),
                parameters: serde_json::json!({
                    "$schema": "http://json-schema.org/draft-07/schema#",
                    "title": "ReadFileArgs",
                    "type": "object",
                    "properties": { "path": { "type": "string" } },
                    "required": ["path"],
                }),
            }],
            temperature: None,
            max_tokens: None,
            parallel_tool_calls: Some(true),
            include_usage: false,
        }
    }

    #[test]
    fn schemas_are_sanitized_on_the_wire_by_default() {
        let provider = OpenAiProvider::new("http://localhost/v1", "k");

        let body = provider.body(&request_with_tool());
        let parameters = &body["tools"][0]["function"]["parameters"];

        assert!(parameters.get("$schema").is_none());
        assert!(parameters.get("title").is_none());
        assert_eq!(parameters["additionalProperties"], serde_json::json!(false));
        assert_eq!(parameters["properties"]["path"]["type"], "string");
    }

    #[test]
    fn sanitizing_can_be_turned_off() {
        let provider = OpenAiProvider::new("http://localhost/v1", "k").with_sanitize_schemas(false);

        let body = provider.body(&request_with_tool());

        assert!(
            body["tools"][0]["function"]["parameters"]
                .get("$schema")
                .is_some()
        );
    }

    #[test]
    fn tool_choice_can_be_omitted() {
        let provider = OpenAiProvider::new("http://localhost/v1", "k").with_omit_tool_choice(true);

        let body = provider.body(&request_with_tool());

        assert!(body.get("tool_choice").is_none());
    }

    #[test]
    fn parallel_tool_calls_can_be_omitted() {
        let provider =
            OpenAiProvider::new("http://localhost/v1", "k").with_omit_parallel_tool_calls(true);

        let body = provider.body(&request_with_tool());

        assert!(body.get("parallel_tool_calls").is_none());
    }

    #[test]
    fn extra_body_is_merged_into_the_request() {
        let mut extra = serde_json::Map::new();
        extra.insert("top_p".to_string(), serde_json::json!(0.9));
        extra.insert("repeat_penalty".to_string(), serde_json::json!(1.05));
        let provider = OpenAiProvider::new("http://localhost/v1", "k").with_extra_body(extra);

        let body = provider.body(&request_with_tool());

        assert_eq!(body["top_p"], serde_json::json!(0.9));
        assert_eq!(body["repeat_penalty"], serde_json::json!(1.05));
    }

    #[test]
    fn the_stream_flag_reaches_the_request_body() {
        let provider = OpenAiProvider::new("http://localhost/v1", "k").with_stream(false);

        assert_eq!(provider.body(&request_with_tool())["stream"], false);
    }

    #[test]
    fn an_object_arguments_payload_is_stringified_not_rejected() {
        let chunk = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1",
            "function":{"name":"read_file","arguments":{"path":"a.rs"}}}]}}]}"#;
        let parsed: Chunk = serde_json::from_str(chunk).expect("object arguments parse");
        let function = parsed.choices[0].delta.tool_calls.as_ref().unwrap()[0]
            .function
            .as_ref()
            .unwrap();
        let rendered = function.arguments.clone().map(stringify_arguments).unwrap();
        assert!(rendered.contains("\"path\""));
    }

    #[test]
    fn a_non_streaming_completion_becomes_the_same_events() {
        let payload = r#"{
            "choices": [{
                "message": {
                    "content": "hello",
                    "tool_calls": [{"id":"c1","function":{"name":"read_file","arguments":"{\"path\":\"a.rs\"}"}}]
                },
                "finish_reason": "tool_calls"
            }]
        }"#;

        let events: Vec<ChatEvent> = parse_completion(payload)
            .into_iter()
            .map(Result::unwrap)
            .collect();

        assert!(matches!(&events[0], ChatEvent::TextDelta(text) if text == "hello"));
        assert!(matches!(
            &events[1],
            ChatEvent::ToolCallDelta { name, arguments, .. }
                if name.as_deref() == Some("read_file") && arguments == "{\"path\":\"a.rs\"}"
        ));
        assert!(matches!(
            events.last(),
            Some(ChatEvent::Done {
                finish_reason: FinishReason::ToolCalls
            })
        ));
    }
}
