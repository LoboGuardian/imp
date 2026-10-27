//! Discovering the context window a backend actually serves.
//!
//! The OpenAI-compatible API does not publish it, and the window is exactly the
//! number that decides whether a turn fits: a local model loaded at 4k will
//! reject the second request of a turn that read a large file, and the failure
//! arrives as an opaque provider error. Each known backend publishes the value
//! somewhere else, so this module asks the endpoint `imp` already talks to
//! first, then the origin-level endpoints that only a local server has.
//!
//! Every probe is best effort. A backend that does not answer, or answers
//! something unexpected, yields `None` and the caller falls back to
//! configuration — discovery never fails a session.

use std::time::Duration;

use serde_json::Value;

use crate::OpenAiProvider;

/// Longest a single probe may take.
///
/// A local server answers in milliseconds; anything slower is a server that is
/// already in trouble, and the caller has configuration to fall back on.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Which backend published a window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextSource {
    /// `data[].max_model_len` on the OpenAI-compatible `/models`.
    Vllm,
    /// LM Studio's `/api/v0/models`.
    LmStudio,
    /// `llama-server`'s `/props`.
    LlamaCpp,
    /// Ollama's `/api/ps`.
    Ollama,
}

impl ContextSource {
    /// Name shown to a user, stable enough to be written into a report.
    pub fn label(self) -> &'static str {
        match self {
            Self::Vllm => "vllm",
            Self::LmStudio => "lm studio",
            Self::LlamaCpp => "llama.cpp",
            Self::Ollama => "ollama",
        }
    }
}

/// What a backend reports about the configured model.
///
/// Only the window is universally available; `loaded` and `tool_use` come from
/// LM Studio, which is also the only one of the four that will happily serve a
/// model it cannot load and fail later.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelInfo {
    /// Model the report is about.
    pub model: String,
    /// Backend that answered.
    pub source: ContextSource,
    /// Window the model is *actually* running with, when the backend knows.
    pub context_window: Option<usize>,
    /// Largest window the model could be loaded with, when it differs.
    pub max_context: Option<usize>,
    /// Whether the model is loaded, when the backend says.
    pub loaded: Option<bool>,
    /// Whether the backend advertises tool calling, when it says.
    pub tool_use: Option<bool>,
}

impl ModelInfo {
    fn new(model: &str, source: ContextSource) -> Self {
        Self {
            model: model.to_string(),
            source,
            context_window: None,
            max_context: None,
            loaded: None,
            tool_use: None,
        }
    }

    /// A one-line description, for `/context` and `imp doctor`.
    pub fn describe(&self) -> String {
        let mut parts = vec![self.source.label().to_string()];
        if let Some(loaded) = self.loaded {
            parts.push(if loaded { "loaded" } else { "not loaded" }.to_string());
        }
        if self.tool_use == Some(false) {
            parts.push("no tool use".to_string());
        }
        parts.join(" · ")
    }
}

impl OpenAiProvider {
    /// Ask the backend what context window the model has.
    ///
    /// Probes, in order: the endpoint's own `/models` (vLLM, and any backend
    /// that adopts `max_model_len`), then LM Studio, `llama-server` and Ollama
    /// at the origin. The origin-level endpoints are only tried for a loopback
    /// address: a local server is the only place they exist, and a gateway
    /// should not receive them.
    ///
    /// Returns `None` when nothing answers with a number the caller can use.
    pub async fn probe_context(&self, model: &str) -> Option<ModelInfo> {
        let local = reqwest::Url::parse(self.base_url())
            .map(|url| is_loopback(&url))
            .unwrap_or(false);

        if let Some(info) = self.probe_openai_models(model).await {
            return Some(info);
        }
        if !local {
            return None;
        }
        if let Some(info) = self.probe_lm_studio(model).await {
            return Some(info);
        }
        if let Some(info) = self.probe_llama_cpp(model).await {
            return Some(info);
        }
        self.probe_ollama(model).await
    }

    /// `GET {base}/models`, for `max_model_len` (vLLM and friends).
    async fn probe_openai_models(&self, model: &str) -> Option<ModelInfo> {
        let url = format!("{}/models", self.base_url());
        let body = self.probe_json(&url, PROBE_TIMEOUT).await?;
        let entry = body
            .get("data")?
            .as_array()?
            .iter()
            .find(|entry| entry.get("id").and_then(Value::as_str) == Some(model))?;
        let window = entry.get("max_model_len").and_then(Value::as_u64)? as usize;
        let mut info = ModelInfo::new(model, ContextSource::Vllm);
        info.context_window = Some(window);
        Some(info)
    }

    /// `GET {origin}/api/v0/models`, LM Studio's own REST surface.
    ///
    /// Unlike the others this reports the *loaded* window, which is the number
    /// that matters: LM Studio will happily accept a request for a model it has
    /// not loaded and fail with a load error later.
    async fn probe_lm_studio(&self, model: &str) -> Option<ModelInfo> {
        let origin = origin_of(self.base_url())?;
        let body = self
            .probe_json(&format!("{origin}/api/v0/models"), PROBE_TIMEOUT)
            .await?;
        let entry = body
            .get("data")?
            .as_array()?
            .iter()
            .find(|entry| entry.get("id").and_then(Value::as_str) == Some(model))?;

        let mut info = ModelInfo::new(model, ContextSource::LmStudio);
        info.loaded = entry
            .get("state")
            .and_then(Value::as_str)
            .map(|state| state == "loaded");
        info.context_window = entry
            .get("loaded_context_length")
            .and_then(Value::as_u64)
            .map(|value| value as usize);
        info.max_context = entry
            .get("max_context_length")
            .and_then(Value::as_u64)
            .map(|value| value as usize);
        info.tool_use = entry
            .get("capabilities")
            .and_then(Value::as_array)
            .map(|caps| caps.iter().any(|cap| cap.as_str() == Some("tool_use")));
        Some(info)
    }

    /// `GET {origin}/props`, `llama-server`'s runtime report.
    async fn probe_llama_cpp(&self, model: &str) -> Option<ModelInfo> {
        let origin = origin_of(self.base_url())?;
        let body = self
            .probe_json(&format!("{origin}/props"), PROBE_TIMEOUT)
            .await?;
        let window = body
            .get("default_generation_settings")
            .and_then(|settings| settings.get("n_ctx"))
            .or_else(|| body.get("n_ctx"))
            .and_then(Value::as_u64)? as usize;

        let mut info = ModelInfo::new(model, ContextSource::LlamaCpp);
        info.context_window = Some(window);
        Some(info)
    }

    /// `GET {origin}/api/ps`, Ollama's list of loaded models.
    async fn probe_ollama(&self, model: &str) -> Option<ModelInfo> {
        let origin = origin_of(self.base_url())?;
        let body = self
            .probe_json(&format!("{origin}/api/ps"), PROBE_TIMEOUT)
            .await?;
        let entries = body.get("models")?.as_array()?;
        // Ollama names a model `family:tag`, so a prefix match covers the usual
        // "model specified without its tag" case. A single loaded model is
        // taken as the answer rather than failing on a naming difference.
        let entry = entries
            .iter()
            .find(|entry| {
                entry
                    .get("name")
                    .and_then(Value::as_str)
                    .map(|name| name.starts_with(model) || model.starts_with(name))
                    .unwrap_or(false)
            })
            .or_else(|| (entries.len() == 1).then(|| &entries[0]))?;
        let window = entry.get("context_length").and_then(Value::as_u64)? as usize;

        let mut info = ModelInfo::new(model, ContextSource::Ollama);
        info.context_window = Some(window);
        info.loaded = Some(true);
        Some(info)
    }
}

/// The scheme-and-authority part of a base URL, without a trailing slash.
fn origin_of(base_url: &str) -> Option<String> {
    let url = reqwest::Url::parse(base_url).ok()?;
    let host = url.host_str()?;
    let port = url
        .port()
        .map(|port| format!(":{port}"))
        .unwrap_or_default();
    Some(format!("{}://{host}{port}", url.scheme()))
}

/// Whether a URL points at this machine.
///
/// Discovery of the origin-level endpoints is limited to it: they are local
/// server conveniences, and a hosted gateway should not be asked for `/props`.
fn is_loopback(url: &reqwest::Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    // `host_str` renders an IPv6 address with its brackets.
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.parse::<std::net::IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_origin_drops_the_api_path() {
        assert_eq!(
            origin_of("http://127.0.0.1:1234/v1").as_deref(),
            Some("http://127.0.0.1:1234")
        );
        assert_eq!(
            origin_of("https://api.example.com/v1/").as_deref(),
            Some("https://api.example.com")
        );
    }

    #[test]
    fn only_local_addresses_count_as_loopback() {
        for url in [
            "http://127.0.0.1:1234/v1",
            "http://localhost:8000/v1",
            "http://[::1]:8080/v1",
        ] {
            let parsed = reqwest::Url::parse(url).unwrap();
            assert!(is_loopback(&parsed), "{url} should be loopback");
        }
        let remote = reqwest::Url::parse("https://api.openai.com/v1").unwrap();
        assert!(!is_loopback(&remote));
    }
}
