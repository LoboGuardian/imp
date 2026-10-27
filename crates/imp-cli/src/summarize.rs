//! Compaction's I/O half: asking the provider for a summary.
//!
//! The decision about *what* to summarize lives in `imp_core::compact`, which
//! has no network. This is the piece that turns that decision into one
//! tool-less request, so the loop can compact without knowing how a provider is
//! reached or configured.

use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use imp_core::compact::{Summarizer, cap_transcript, transcript};
use imp_core::error::{Error, Result};
use imp_core::message::Message;
use imp_core::provider::{ChatEvent, ChatRequest, Provider};
use tokio_util::sync::CancellationToken;

/// Room the summarizer is given to answer.
///
/// A summary is a paragraph, not an essay: the budget is small on purpose, both
/// because the turn it is protecting is tight and because a long summary
/// defeats the point.
const SUMMARY_MAX_TOKENS: u32 = 600;

/// How much of the replaced history the summarizer is shown.
///
/// ~24k characters is roughly 6k tokens. A conversation that reached compaction
/// is by definition close to the limit, so the request that summarizes it has to
/// be bounded independently or it becomes the overflow it is preventing.
const SUMMARY_INPUT_CHARS: usize = 24_000;

/// What the summarizer is asked for.
///
/// Written as an instruction to preserve the things a model needs to continue:
/// a summary that loses the file paths and the failed command is worse than no
/// summary, because the model will simply repeat the work.
const INSTRUCTIONS: &str = "\
You are compacting a conversation so another model can continue it without \
re-reading the earlier part. Write a summary of the transcript you are given.

Keep, concisely and concretely:
- what the user asked for, including any correction to an earlier request;
- decisions made and the reasons, if they were stated;
- file paths, function names and commands that were used, and what they returned;
- errors encountered and whether they were resolved;
- what is still unfinished.

Drop: greetings, restatements of the plan, and any tool output that was simply \
a directory listing or a truncated dump. Do not invent anything that is not in \
the transcript. Reply with the summary only, no preamble.";

/// A [`Summarizer`] backed by the session's provider.
pub struct ProviderSummarizer {
    provider: Arc<dyn Provider>,
    /// Model that writes the summary; the session's model unless configured.
    model: String,
}

impl ProviderSummarizer {
    /// Summarize with `model` through `provider`.
    pub fn new(provider: Arc<dyn Provider>, model: impl Into<String>) -> Self {
        Self {
            provider,
            model: model.into(),
        }
    }
}

#[async_trait]
impl Summarizer for ProviderSummarizer {
    async fn summarize(&self, messages: &[Message], cancel: CancellationToken) -> Result<String> {
        let body = cap_transcript(&transcript(messages), SUMMARY_INPUT_CHARS);
        if body.trim().is_empty() {
            return Err(Error::Provider(
                "there was nothing in the replaced history to summarize".to_string(),
            ));
        }

        let request = ChatRequest {
            model: self.model.clone(),
            messages: vec![Message::system(INSTRUCTIONS), Message::user(body)],
            tools: Vec::new(),
            temperature: Some(0.0),
            max_tokens: Some(SUMMARY_MAX_TOKENS),
            parallel_tool_calls: None,
            include_usage: false,
        };

        let mut stream = self.provider.stream(request, cancel);
        let mut summary = String::new();
        while let Some(event) = stream.next().await {
            // Usage, tool deltas and the done marker are irrelevant to a
            // summarization: the only thing wanted is the prose.
            if let ChatEvent::TextDelta(delta) = event? {
                summary.push_str(&delta);
            }
        }

        let summary = summary.trim().to_string();
        if summary.is_empty() {
            return Err(Error::Provider("the model returned no summary".to_string()));
        }
        Ok(summary)
    }
}
