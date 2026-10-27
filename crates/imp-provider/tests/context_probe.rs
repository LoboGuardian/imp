//! The context probe, against the wire shapes the known backends actually send.
//!
//! Each backend publishes the window in a different place, and a local server
//! is exactly where a wrong guess is expensive: the number decides whether the
//! second request of a turn is rejected. The fixtures below are trimmed copies
//! of real responses.

use std::collections::HashMap;
use std::sync::Arc;

use imp_provider::{ContextSource, OpenAiProvider};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Serve `routes` by request path, answering 404 for anything else.
///
/// Returns the base URL (`/v1`-rooted, as a provider would be configured).
async fn spawn(routes: Vec<(&'static str, &'static str)>) -> String {
    let routes: Arc<HashMap<&'static str, &'static str>> = Arc::new(routes.into_iter().collect());
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let routes = routes.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let read = socket.read(&mut buf).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..read]).to_string();
                let path = head
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or("")
                    .to_string();
                let (status, body) = match routes.get(path.as_str()) {
                    Some(body) => (200, *body),
                    None => (404, "{\"error\":\"not found\"}"),
                };
                let reason = if status == 200 { "OK" } else { "Not Found" };
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    format!("http://{address}/v1")
}

#[tokio::test]
async fn lm_studio_reports_the_loaded_window_and_its_capabilities() {
    let base = spawn(vec![
        (
            "/v1/models",
            // LM Studio's OpenAI-compatible list carries no window at all.
            r#"{"data":[{"id":"liquid/lfm3-vl-2b","object":"model"}]}"#,
        ),
        (
            "/api/v0/models",
            r#"{"data":[
                {"id":"other","state":"not-loaded","max_context_length":8192},
                {"id":"liquid/lfm3-vl-2b","state":"loaded",
                 "max_context_length":262144,"loaded_context_length":128000,
                 "capabilities":["tool_use"]}
            ]}"#,
        ),
    ])
    .await;

    let info = OpenAiProvider::new(&base, "")
        .probe_context("liquid/lfm3-vl-2b")
        .await
        .expect("lm studio should answer");

    assert_eq!(info.source, ContextSource::LmStudio);
    assert_eq!(info.context_window, Some(128_000));
    assert_eq!(info.max_context, Some(262_144));
    assert_eq!(info.loaded, Some(true));
    assert_eq!(info.tool_use, Some(true));
    assert_eq!(info.describe(), "lm studio · loaded");
}

#[tokio::test]
async fn lm_studio_reports_a_model_that_is_not_loaded() {
    let base = spawn(vec![
        ("/v1/models", r#"{"data":[]}"#),
        (
            "/api/v0/models",
            r#"{"data":[{"id":"prism-ml/bonsai-27b","state":"not-loaded",
                 "max_context_length":262144,"capabilities":["tool_use"]}]}"#,
        ),
    ])
    .await;

    let info = OpenAiProvider::new(&base, "")
        .probe_context("prism-ml/bonsai-27b")
        .await
        .expect("an unloaded model is still reported");

    assert_eq!(info.loaded, Some(false));
    assert_eq!(info.context_window, None, "nothing was loaded to measure");
}

#[tokio::test]
async fn lm_studio_flags_a_model_without_tool_calling() {
    let base = spawn(vec![
        ("/v1/models", r#"{"data":[]}"#),
        (
            "/api/v0/models",
            r#"{"data":[{"id":"m","state":"loaded","loaded_context_length":4096,
                 "max_context_length":4096,"capabilities":[]}]}"#,
        ),
    ])
    .await;

    let info = OpenAiProvider::new(&base, "")
        .probe_context("m")
        .await
        .expect("lm studio should answer");

    assert_eq!(info.tool_use, Some(false));
    assert_eq!(info.describe(), "lm studio · loaded · no tool use");
}

#[tokio::test]
async fn vllm_publishes_the_window_on_the_openai_model_list() {
    let base = spawn(vec![(
        "/v1/models",
        r#"{"data":[{"id":"m","max_model_len":32768}]}"#,
    )])
    .await;

    let info = OpenAiProvider::new(&base, "")
        .probe_context("m")
        .await
        .expect("vllm should answer");

    assert_eq!(info.source, ContextSource::Vllm);
    assert_eq!(info.context_window, Some(32_768));
    assert_eq!(info.loaded, None, "vllm does not report load state");
}

#[tokio::test]
async fn llama_cpp_reports_n_ctx_on_props() {
    let base = spawn(vec![
        ("/v1/models", r#"{"data":[{"id":"m"}]}"#),
        (
            "/props",
            r#"{"default_generation_settings":{"n_ctx":4096},"total_slots":1}"#,
        ),
    ])
    .await;

    let info = OpenAiProvider::new(&base, "")
        .probe_context("m")
        .await
        .expect("llama.cpp should answer");

    assert_eq!(info.source, ContextSource::LlamaCpp);
    assert_eq!(info.context_window, Some(4096));
}

#[tokio::test]
async fn ollama_reports_the_loaded_context() {
    let base = spawn(vec![
        ("/v1/models", r#"{"data":[{"id":"m"}]}"#),
        (
            "/api/ps",
            r#"{"models":[{"name":"llama3:latest","context_length":8192}]}"#,
        ),
    ])
    .await;

    let info = OpenAiProvider::new(&base, "")
        .probe_context("m")
        .await
        .expect("ollama should answer");

    assert_eq!(info.source, ContextSource::Ollama);
    assert_eq!(info.context_window, Some(8192));
    assert_eq!(info.loaded, Some(true));
}

#[tokio::test]
async fn a_backend_that_publishes_nothing_yields_nothing() {
    // Every path answers 404, as an endpoint that only implements
    // `/chat/completions` would.
    let base = spawn(vec![]).await;

    assert!(
        OpenAiProvider::new(&base, "")
            .probe_context("m")
            .await
            .is_none(),
        "discovery must degrade to configuration, not invent a number"
    );
}
