//! Cheap token estimation for context budgeting.
//!
//! `imp-core` deliberately has no tokenizer dependency: tokenization differs
//! per backend, and a real encoder would drag in a large table the loop does not
//! need. The estimator is a conservative character heuristic — good enough to
//! keep a request under a configured budget and to decide when to drop old
//! turns, which is all the agent loop uses it for.

use crate::message::Message;
use crate::provider::ToolSchema;

/// Characters per token assumed for ASCII-heavy prose and source.
///
/// English and code average a little over four characters per token. Rounding
/// the estimate *up* errs toward sending less, which is the safe direction when
/// the context window is small.
const CHARS_PER_TOKEN: usize = 4;

/// Framing tokens an OpenAI-shaped backend adds per message.
const MESSAGE_OVERHEAD: usize = 4;

/// Estimate the tokens a single string costs.
pub fn estimate(text: &str) -> usize {
    text.chars().count().div_ceil(CHARS_PER_TOKEN)
}

/// Estimate the tokens one message costs, framing included.
pub fn estimate_message(message: &Message) -> usize {
    let mut tokens = MESSAGE_OVERHEAD;
    if let Some(content) = &message.content {
        tokens += estimate(content);
    }
    if let Some(calls) = &message.tool_calls {
        for call in calls {
            tokens += estimate(&call.function.name) + estimate(&call.function.arguments) + 4;
        }
    }
    tokens
}

/// Estimate the tokens a whole transcript costs.
pub fn estimate_messages(messages: &[Message]) -> usize {
    messages.iter().map(estimate_message).sum()
}

/// Estimate the tokens the advertised tool schemas cost.
///
/// The schemas travel beside the messages, so a context budget has to reserve
/// room for them before deciding how much history fits.
pub fn estimate_tools(tools: &[ToolSchema]) -> usize {
    tools
        .iter()
        .map(|tool| {
            estimate(&tool.name)
                + estimate(&tool.description)
                + estimate(&tool.parameters.to_string())
                + 8
        })
        .sum()
}

/// Estimate what one whole request costs: transcript plus the schemas beside it.
///
/// This is the number that decides whether a request fits, and the number
/// `/context` reports, so both read it from here rather than adding the parts
/// up in two places.
pub fn estimate_request(messages: &[Message], tools: &[ToolSchema]) -> usize {
    estimate_messages(messages) + estimate_tools(tools)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{FunctionCall, ToolCall};

    #[test]
    fn a_non_empty_string_costs_at_least_one_token() {
        assert_eq!(estimate(""), 0);
        assert!(estimate("a") >= 1);
    }

    #[test]
    fn a_longer_string_costs_more() {
        assert!(estimate(&"word ".repeat(100)) > estimate("word"));
    }

    #[test]
    fn tool_call_arguments_are_counted() {
        let plain = Message::assistant("done");
        let with_call = Message::assistant_with_tool_calls(
            None,
            vec![ToolCall {
                id: "call_1".to_string(),
                kind: "function".to_string(),
                function: FunctionCall {
                    name: "read_file".to_string(),
                    arguments: serde_json::json!({ "path": "a/very/long/file/name.rs" })
                        .to_string(),
                },
            }],
        );
        assert!(estimate_message(&with_call) > estimate_message(&plain));
    }

    #[test]
    fn tool_schemas_have_a_non_zero_cost() {
        let tools = vec![ToolSchema {
            name: "read_file".to_string(),
            description: "Read a file.".to_string(),
            parameters: serde_json::json!({ "type": "object", "properties": { "path": { "type": "string" } } }),
        }];
        assert!(estimate_tools(&tools) > 0);
    }
}
