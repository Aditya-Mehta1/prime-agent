//! LLM-facing message conversion. Port of convertToLlm and the message
//! presentation constants in core/messages.ts.

use pa_types::ai::{
    AssistantMessage, TextContent, ToolResultMessage, UserContent, UserContentBlock, UserMessage,
};
use pa_types::session::AgentMessage;

pub const COMPACTION_SUMMARY_PREFIX: &str = "[compaction-summary]\n\nThe conversation history before this point was compacted into the following summary:\n\n<summary>\n";
pub const COMPACTION_SUMMARY_SUFFIX: &str = "\n</summary>";
pub const BRANCH_SUMMARY_PREFIX: &str = "[branch-summary]\n\nThe following is a summary of a branch that this conversation came back from:\n\n<summary>\n";
pub const BRANCH_SUMMARY_SUFFIX: &str = "\n</summary>";
pub const HARNESS_DIGEST_PREFIX: &str = "[harness-digest]\n\nThe persistent memories produced across this session so far:\n\n<harness_state>\n";
pub const HARNESS_DIGEST_SUFFIX: &str = "\n</harness_state>";

pub use pa_types::slash_commands::{
    SESSION_SLASH_COMMAND_CUSTOM_TYPE, SESSION_SLASH_COMMAND_RESULT_CUSTOM_TYPE,
};
pub const COMPACTION_OUTCOME_CUSTOM_TYPE: &str = "compaction_outcome";
pub const REFINEMENT_OUTCOME_CUSTOM_TYPE: &str = "refinement_outcome";
pub const REFINEMENT_NOTICE_CUSTOM_TYPE: &str = "refinement_notice";
pub const HEARTBEAT_PROMPT_CUSTOM_TYPE: &str = "heartbeat_prompt";

/// Bash output rendered as a fenced block with a fence longer than any
/// backtick run inside the output.
fn bash_output_to_text(
    output: &str,
    exit_code: Option<i64>,
    cancelled: bool,
    truncated: bool,
    full_output_path: Option<&str>,
) -> String {
    let mut text = String::new();
    if !output.is_empty() {
        let longest = output
            .match_indices('`')
            .fold(0usize, |longest, (index, _)| {
                let mut run = 0usize;
                let mut chars = output[index..].chars();
                while chars.next() == Some('`') {
                    run += 1;
                }
                run.max(longest)
            });
        let fence = "`".repeat(longest.max(3).max(longest + 1));
        text.push_str(&fence);
        text.push('\n');
        text.push_str(output);
        text.push('\n');
        text.push_str(&fence);
    } else {
        text.push_str("(no output)");
    }
    if cancelled {
        text.push_str("\n\n(command cancelled)");
    } else if let Some(code) = exit_code {
        if code != 0 {
            text.push_str(&format!("\n\nCommand exited with code {code}"));
        }
    }
    if truncated {
        match full_output_path {
            Some(path) => text.push_str(&format!("\n\n[Output truncated. Full output: {path}]")),
            None => text.push_str("\n\n[Output truncated.]"),
        }
    }
    text
}

/// Bash execution as user-facing text for LLM context.
pub fn bash_execution_to_text(message: &pa_types::session::BashExecutionMessage) -> String {
    format!(
        "Ran `{}`\n{}",
        message.command,
        bash_output_to_text(
            &message.output,
            message.exit_code,
            message.cancelled,
            message.truncated,
            message.full_output_path.as_deref(),
        )
    )
}

fn text_block(text: String) -> UserContentBlock {
    UserContentBlock::Text(TextContent {
        text,
        text_signature: None,
        rest: Default::default(),
    })
}

/// The LLM message view of session messages: session-only roles become user
/// text turns; bookkeeping custom types drop out entirely.
pub fn convert_to_llm(messages: &[AgentMessage]) -> Vec<AgentMessage> {
    let mut out = Vec::new();
    for message in messages {
        let converted = match message {
            AgentMessage::BashExecution(bash) => {
                if bash.exclude_from_context.unwrap_or(false) {
                    continue;
                }
                AgentMessage::User(UserMessage {
                    content: UserContent::Blocks(vec![text_block(bash_execution_to_text(bash))]),
                    timestamp: bash.timestamp,
                    rest: Default::default(),
                })
            }
            AgentMessage::Custom(custom) => {
                if matches!(
                    custom.custom_type.as_str(),
                    SESSION_SLASH_COMMAND_CUSTOM_TYPE
                        | SESSION_SLASH_COMMAND_RESULT_CUSTOM_TYPE
                        | COMPACTION_OUTCOME_CUSTOM_TYPE
                        | REFINEMENT_OUTCOME_CUSTOM_TYPE
                ) {
                    continue;
                }
                AgentMessage::User(UserMessage {
                    content: custom.content.clone(),
                    timestamp: custom.timestamp,
                    rest: Default::default(),
                })
            }
            AgentMessage::BranchSummary(summary) => AgentMessage::User(UserMessage {
                content: UserContent::Blocks(vec![text_block(format!(
                    "{BRANCH_SUMMARY_PREFIX}{}{BRANCH_SUMMARY_SUFFIX}",
                    summary.summary
                ))]),
                timestamp: summary.timestamp,
                rest: Default::default(),
            }),
            AgentMessage::CompactionSummary(summary) => {
                let digest_block = summary
                    .harness_digest
                    .as_deref()
                    .map(|digest| {
                        format!("{HARNESS_DIGEST_PREFIX}{digest}{HARNESS_DIGEST_SUFFIX}\n\n")
                    })
                    .unwrap_or_default();
                AgentMessage::User(UserMessage {
                    content: UserContent::Blocks(vec![text_block(format!(
                        "{digest_block}{COMPACTION_SUMMARY_PREFIX}{}{COMPACTION_SUMMARY_SUFFIX}",
                        summary.summary
                    ))]),
                    timestamp: summary.timestamp,
                    rest: Default::default(),
                })
            }
            AgentMessage::User(_) | AgentMessage::Assistant(_) | AgentMessage::ToolResult(_) => {
                message.clone()
            }
        };
        out.push(converted);
    }
    out
}

/// Convenience alias types used by the summarizer call.
pub type LlmAssistantMessage = AssistantMessage;
pub type LlmToolResultMessage = ToolResultMessage;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bash_output_fences_and_status() {
        let rendered = bash_output_to_text(
            "has `ticks` inside",
            Some(1),
            false,
            true,
            Some("/tmp/full.txt"),
        );
        assert!(rendered.contains("```\nhas `ticks` inside\n```")); // default fence
        assert!(rendered.contains("Command exited with code 1"));
        assert!(rendered.contains("[Output truncated. Full output: /tmp/full.txt]"));
        assert!(bash_output_to_text("", None, false, false, None).contains("(no output)"));
        let cancelled = bash_output_to_text("x", None, true, false, None);
        assert!(cancelled.contains("(command cancelled)"));
        // A 3-backtick run in the output forces a 4-backtick fence.
        let fenced = bash_output_to_text("a ``` b", None, false, false, None);
        assert!(fenced.contains("````\na ``` b\n````"));
    }

    #[test]
    fn conversion_wraps_summaries_and_drops_bookkeeping() {
        let messages = vec![
            AgentMessage::User(UserMessage {
                content: UserContent::Text("keep me".to_string()),
                timestamp: 0,
                rest: Default::default(),
            }),
            AgentMessage::Custom(pa_types::session::CustomMessage {
                custom_type: SESSION_SLASH_COMMAND_CUSTOM_TYPE.to_string(),
                content: UserContent::Text("/model".to_string()),
                display: true,
                details: None,
                timestamp: 0,
                rest: Default::default(),
            }),
            AgentMessage::Custom(pa_types::session::CustomMessage {
                custom_type: "extension_note".to_string(),
                content: UserContent::Text("a note".to_string()),
                display: true,
                details: None,
                timestamp: 0,
                rest: Default::default(),
            }),
            AgentMessage::BranchSummary(pa_types::session::BranchSummaryMessage {
                summary: "the branch".to_string(),
                from_id: "x".to_string(),
                timestamp: 0,
            }),
            AgentMessage::CompactionSummary(pa_types::session::CompactionSummaryMessage {
                summary: "the story".to_string(),
                tokens_before: 1,
                retained_message_count: None,
                custom_instructions: None,
                harness_digest: Some("digest".to_string()),
                timestamp: 0,
            }),
        ];
        let converted = convert_to_llm(&messages);
        // Bookkeeping custom dropped; other custom becomes user.
        assert_eq!(converted.len(), 4);
        assert!(matches!(converted[0], AgentMessage::User(_)));
        let note = match &converted[1] {
            AgentMessage::User(user) => user.content.text(),
            _ => panic!("expected user"),
        };
        assert_eq!(note, "a note");
        let branch = match &converted[2] {
            AgentMessage::User(user) => user.content.text(),
            _ => panic!("expected user"),
        };
        assert!(branch.contains("[branch-summary]"));
        assert!(branch.contains("the branch"));
        let compaction = match &converted[3] {
            AgentMessage::User(user) => user.content.text(),
            _ => panic!("expected user"),
        };
        assert!(compaction.contains("[harness-digest]"));
        assert!(compaction.contains("<harness_state>\ndigest\n</harness_state>"));
        assert!(compaction.contains("[compaction-summary]"));
        assert!(compaction.contains("the story"));
    }

    #[test]
    fn bash_execution_becomes_user_text() {
        let bash = AgentMessage::BashExecution(pa_types::session::BashExecutionMessage {
            command: "cargo test".to_string(),
            output: "ok".to_string(),
            exit_code: Some(0),
            cancelled: false,
            truncated: false,
            full_output_path: None,
            timestamp: 0,
            exclude_from_context: None,
        });
        let converted = convert_to_llm(std::slice::from_ref(&bash));
        match &converted[0] {
            AgentMessage::User(user) => {
                assert!(user.content.text().starts_with("Ran `cargo test`"));
                assert!(user.content.text().contains("```"));
            }
            _ => panic!("expected user"),
        }
        // Excluded executions drop out entirely.
        let excluded = AgentMessage::BashExecution(pa_types::session::BashExecutionMessage {
            exclude_from_context: Some(true),
            command: "secret".to_string(),
            output: String::new(),
            exit_code: None,
            cancelled: false,
            truncated: false,
            full_output_path: None,
            timestamp: 0,
        });
        assert!(convert_to_llm(std::slice::from_ref(&excluded)).is_empty());
    }
}
