//! Compaction: replacing old turns with one generated summary.
//!
//! Trimming drops history and keeps nothing; compaction keeps the decisions.
//! This module decides *what* would be replaced and how the summary is shaped,
//! and nothing else: the request that produces the prose belongs to the caller,
//! behind [`Summarizer`], so `imp-core` stays free of I/O and the rule set can
//! be tested against plain messages.
//!
//! The invariants are the same ones trimming already protects, and they are the
//! reason compaction is not allowed to cut wherever it likes:
//!
//! - index 0 is the system prompt — configuration, not history, so it stays;
//! - the cut lands on a `user` message, so a turn is never half-replaced;
//! - an assistant `tool_calls` message is never separated from its results,
//!   because a provider rejects that transcript.

use async_trait::async_trait;

use crate::error::Result;
use crate::message::{Message, Role};
use crate::tokens::estimate_messages;

/// Prefix every generated summary carries.
///
/// It is what makes a summary recognisable in a transcript, a database dump or
/// a resumed session, and it is the marker §11's R3 asks for: a summary is
/// always visible as a summary.
pub const SUMMARY_MARKER: &str = "[summary]";

/// The role a summary travels as.
///
/// A `system` message is the honest shape — the summary is context, not
/// something the user said — and OpenAI-compatible APIs accept one anywhere in
/// the array. Some local chat templates are strict about non-leading system
/// messages, so the role is a provider quirk (`summary_as_user`) rather than a
/// hard-coded choice.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SummaryRole {
    /// `system`, the default.
    #[default]
    System,
    /// `user`, for backends whose template rejects a second system message.
    User,
}

impl SummaryRole {
    /// Parse the configured spelling, rejecting anything unknown.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "system" => Some(Self::System),
            "user" => Some(Self::User),
            _ => None,
        }
    }
}

/// The messages a summary would replace.
///
/// Indices are into the history it was planned from: `[from, to)` is removed
/// and one summary message is inserted in its place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    /// First message replaced. Always at least 1, so the prompt survives.
    pub from: usize,
    /// One past the last message replaced.
    pub to: usize,
}

impl Plan {
    /// How many messages are leaving the transcript.
    pub fn len(&self) -> usize {
        self.to.saturating_sub(self.from)
    }

    /// Whether the plan would replace nothing.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Decide what to summarize, given a history that does not fit `budget`.
///
/// `budget` covers the messages alone; the caller has already reserved room for
/// the tool schemas. `keep_turns` is how many of the most recent user turns are
/// kept verbatim, and is clamped to at least one so a turn always has the
/// prompt it belongs to.
///
/// Returns `None` when nothing can usefully be replaced: the history already
/// fits, or the only thing left is the newest turn, which is the conversation
/// the model is in the middle of holding and must not be summarized away.
pub fn plan(history: &[Message], budget: usize, keep_turns: usize) -> Option<Plan> {
    if history.len() <= 2 || estimate_messages(history) <= budget {
        return None;
    }
    cut(history, keep_turns)
}

/// Plan a compaction regardless of whether the history currently fits.
///
/// This is `/compact`: the user asked for it, so the budget is not consulted.
/// The same boundaries apply — the prompt survives and no turn is split.
pub fn plan_forced(history: &[Message], keep_turns: usize) -> Option<Plan> {
    cut(history, keep_turns)
}

/// The boundary decision the two entry points share.
fn cut(history: &[Message], keep_turns: usize) -> Option<Plan> {
    // Walk back to the start of the `keep_turns`-th most recent turn. Counting
    // from a `user` message is what guarantees the cut never separates an
    // assistant `tool_calls` from the results that answer it: everything from
    // that user message onwards belongs to a turn that began at a boundary.
    let keep = keep_turns.max(1);
    let mut seen = 0;
    let mut cut = None;
    for (index, message) in history.iter().enumerate().rev() {
        if index == 0 {
            break;
        }
        if message.role == Role::User {
            seen += 1;
            if seen == keep {
                cut = Some(index);
                break;
            }
        }
    }

    let to = cut?;
    if to <= 1 {
        // There is no complete earlier turn between the prompt and the kept
        // window, so there is nothing a summary could stand in for.
        return None;
    }
    Some(Plan { from: 1, to })
}

/// The message that stands in for the replaced range.
pub fn summary_message(role: SummaryRole, summary: &str) -> Message {
    let body = format!("{SUMMARY_MARKER} {summary}");
    match role {
        SummaryRole::System => Message::system(body),
        SummaryRole::User => Message::user(body),
    }
}

/// Whether a message is a generated summary.
pub fn is_summary(message: &Message) -> bool {
    matches!(message.role, Role::System | Role::User)
        && message
            .content
            .as_deref()
            .map(|content| content.trim_start().starts_with(SUMMARY_MARKER))
            .unwrap_or(false)
}

/// Render messages as a plain transcript for the summarizer.
///
/// Tool calls and their results are included deliberately: a summary that drops
/// which command was run and what it printed is worse than no summary, because
/// the model will re-run it.
pub fn transcript(messages: &[Message]) -> String {
    let mut out = String::new();
    for message in messages {
        match message.role {
            Role::System => {
                if let Some(content) = &message.content {
                    out.push_str("Context: ");
                    out.push_str(content.trim());
                    out.push_str("\n\n");
                }
            }
            Role::User => {
                if let Some(content) = &message.content {
                    out.push_str("User: ");
                    out.push_str(content.trim());
                    out.push_str("\n\n");
                }
            }
            Role::Assistant => {
                if let Some(content) = &message.content
                    && !content.trim().is_empty()
                {
                    out.push_str("Assistant: ");
                    out.push_str(content.trim());
                    out.push_str("\n\n");
                }
                for call in message.tool_calls.iter().flatten() {
                    out.push_str(&format!(
                        "Assistant called {}({})\n\n",
                        call.function.name,
                        call.function.arguments.trim()
                    ));
                }
            }
            Role::Tool => {
                if let Some(content) = &message.content {
                    out.push_str("Tool result: ");
                    out.push_str(content.trim());
                    out.push_str("\n\n");
                }
            }
        }
    }
    out.trim_end().to_string()
}

/// Produces the summary that replaces the messages leaving the transcript.
///
/// The trait exists so the loop can ask for a summary without knowing how one
/// is made: `imp-cli` implements it against the session's provider, and the
/// loop's tests implement it with a fake.
#[async_trait]
pub trait Summarizer: Send + Sync {
    /// Summarize `messages` into prose a model can continue the work from.
    ///
    /// `cancel` is the turn's token: compaction runs inside a turn, so it must
    /// stop when the user does.
    ///
    /// An `Err` means compaction did not happen; the caller falls back to
    /// dropping history rather than failing the turn.
    async fn summarize(
        &self,
        messages: &[Message],
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<String>;
}

/// Bound a rendered transcript so the summarization request cannot itself
/// overflow.
///
/// The head carries what was asked for and the tail carries what was decided,
/// so the middle is what goes when a transcript is too long: it is the part a
/// summary can most safely lose, and the two ends are what the next turn needs.
pub fn cap_transcript(text: &str, cap: usize) -> String {
    let chars = text.chars().count();
    if chars <= cap {
        return text.to_string();
    }
    let head = cap * 3 / 5;
    let tail = cap - head;
    let start: String = text.chars().take(head).collect();
    let end: String = text.chars().skip(chars.saturating_sub(tail)).collect();
    format!("{start}\n… [middle omitted] …\n{end}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{FunctionCall, ToolCall};

    fn user(text: &str) -> Message {
        Message::user(text)
    }

    fn assistant(text: &str) -> Message {
        Message::assistant(text)
    }

    /// A complete turn: user, tool call, result, assistant answer.
    fn tool_turn(text: &str) -> Vec<Message> {
        vec![
            Message::user(text),
            Message::assistant_with_tool_calls(
                None,
                vec![ToolCall {
                    id: format!("call_{text}"),
                    kind: "function".to_string(),
                    function: FunctionCall {
                        name: "read_file".to_string(),
                        arguments: "{\"path\":\"a.rs\"}".to_string(),
                    },
                }],
            ),
            Message::tool_result(format!("call_{text}"), "some file contents"),
            Message::assistant("done"),
        ]
    }

    #[test]
    fn a_history_that_fits_is_left_alone() {
        let history = vec![Message::system("prompt"), user("hi"), assistant("hello")];
        assert_eq!(plan(&history, 10_000, 2), None);
    }

    #[test]
    fn the_system_prompt_always_survives() {
        let mut history = vec![Message::system("prompt")];
        for index in 0..6 {
            history.extend(tool_turn(&format!("turn {index}")));
        }
        let plan = plan(&history, 1, 2).expect("this history does not fit");
        assert_eq!(plan.from, 1);
    }

    #[test]
    fn the_cut_lands_on_a_user_message() {
        let mut history = vec![Message::system("prompt")];
        for index in 0..6 {
            history.extend(tool_turn(&format!("turn {index}")));
        }
        let plan = plan(&history, 1, 2).expect("this history does not fit");
        assert_eq!(history[plan.to].role, Role::User);
    }

    #[test]
    fn a_summary_never_separates_a_call_from_its_result() {
        let mut history = vec![Message::system("prompt")];
        for index in 0..4 {
            history.extend(tool_turn(&format!("turn {index}")));
        }
        let plan = plan(&history, 1, 1).expect("this history does not fit");

        // Every tool result still in the kept range has its call before it.
        for (index, message) in history.iter().enumerate().skip(plan.to) {
            if message.role == Role::Tool {
                let id = message.tool_call_id.as_deref().unwrap();
                let answered = history[plan.to..index].iter().any(|earlier| {
                    earlier
                        .tool_calls
                        .iter()
                        .flatten()
                        .any(|call| call.id == id)
                });
                assert!(answered, "tool result {id} lost its call at {index}");
            }
        }
    }

    #[test]
    fn the_newest_turn_is_never_summarized_away() {
        let history = vec![
            Message::system("prompt"),
            user("only turn"),
            assistant("answer"),
        ];
        assert_eq!(plan(&history, 1, 2), None);
    }

    #[test]
    fn keep_turns_counts_whole_turns() {
        let mut history = vec![Message::system("prompt")];
        for index in 0..5 {
            history.extend(tool_turn(&format!("turn {index}")));
        }
        let plan = plan(&history, 1, 3).expect("this history does not fit");
        // Three kept turns means the cut is at the third-from-last user message.
        let users_after = history[plan.to..]
            .iter()
            .filter(|message| message.role == Role::User)
            .count();
        assert_eq!(users_after, 3);
    }

    #[test]
    fn zero_kept_turns_is_treated_as_one() {
        let mut history = vec![Message::system("prompt"), user("first"), assistant("a")];
        history.extend(tool_turn("second"));
        let plan = plan(&history, 1, 0).expect("this history does not fit");
        assert_eq!(history[plan.to].role, Role::User);
        assert_eq!(history[plan.to].content.as_deref(), Some("second"));
    }

    #[test]
    fn a_summary_is_recognisable_in_either_role() {
        for role in [SummaryRole::System, SummaryRole::User] {
            let message = summary_message(role, "we read a.rs and it compiled");
            assert!(is_summary(&message));
            assert!(
                message
                    .content
                    .as_deref()
                    .unwrap()
                    .starts_with(SUMMARY_MARKER)
            );
            match role {
                SummaryRole::System => assert_eq!(message.role, Role::System),
                SummaryRole::User => assert_eq!(message.role, Role::User),
            }
        }
    }

    #[test]
    fn an_ordinary_message_is_not_a_summary() {
        assert!(!is_summary(&user("hello")));
        assert!(!is_summary(&Message::tool_result("c1", "out")));
    }

    #[test]
    fn the_transcript_keeps_calls_and_results() {
        let messages = tool_turn("read a.rs");
        let text = transcript(&messages);
        assert!(text.contains("User: read a.rs"), "{text}");
        assert!(text.contains("Assistant called read_file"), "{text}");
        assert!(text.contains("Tool result: some file contents"), "{text}");
    }

    #[test]
    fn the_role_spelling_is_strict() {
        assert_eq!(SummaryRole::parse("system"), Some(SummaryRole::System));
        assert_eq!(SummaryRole::parse("user"), Some(SummaryRole::User));
        assert_eq!(SummaryRole::parse("assistant"), None);
    }
}
