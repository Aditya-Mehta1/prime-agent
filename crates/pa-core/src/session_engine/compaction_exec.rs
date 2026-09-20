//! The compaction executor: assemble and run the summarization request.
//! Port of compact() in core/compaction/compaction.ts (summarizer call via
//! pa-ai's completion facade).

use super::compaction::{build_summarization_prompt, CutPointResult};
use super::compaction_utils::{
    compute_file_lists, extract_file_ops_from_message, format_file_operations, FileOperations,
};
use super::messages::convert_to_llm;
use pa_types::ai::{AssistantMessage, TextContent, UserContent, UserContentBlock, UserMessage};
use pa_types::session::{AgentMessage, CompactionEntry, FileEntry};

/// Details stored on the compaction entry for file tracking.
#[derive(Debug, Default, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionDetails {
    pub read_files: Vec<String>,
    pub modified_files: Vec<String>,
}

/// The result of running a compaction.
#[derive(Debug, Clone, PartialEq)]
pub struct CompactionResult {
    pub summary: String,
    pub first_kept_entry_id: String,
    pub tokens_before: u64,
    pub usage: Option<pa_types::ai::Usage>,
}

/// How the summarizer is invoked (test seam over pa-ai completion).
pub type SummarizerFn = Box<
    dyn FnOnce(
            pa_types::ai::Model,
            Vec<AgentMessage>,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = anyhow::Result<AssistantMessage>> + Send>,
        > + Send,
>;

/// Assemble the summarization messages for the conversation slice.
pub fn build_summarization_request(
    messages: &[AgentMessage],
    custom_instructions: Option<&str>,
    previous_summary: Option<&str>,
    #[allow(unused_variables)] reserve_tokens: u64,
) -> Vec<AgentMessage> {
    let llm_messages = convert_to_llm(messages);
    let conversation_text = super::compaction_utils::serialize_conversation(&llm_messages);
    let mut prompt_text = format!("<conversation>\n{conversation_text}\n</conversation>\n\n");
    if let Some(previous_summary) = previous_summary {
        prompt_text.push_str(&format!(
            "<previous-summary>\n{previous_summary}\n</previous-summary>\n\n"
        ));
    }
    prompt_text.push_str(&build_summarization_prompt(
        custom_instructions,
        previous_summary,
    ));
    vec![AgentMessage::User(UserMessage {
        content: UserContent::Blocks(vec![UserContentBlock::Text(TextContent {
            text: prompt_text,
            text_signature: None,
            rest: Default::default(),
        })]),
        timestamp: 0,
        rest: Default::default(),
    })]
}

/// File operations preserved across prior compactions plus current messages.
fn extract_file_operations(
    messages: &[AgentMessage],
    entries: &[FileEntry],
    prev_compaction_index: Option<usize>,
) -> FileOperations {
    let mut ops = FileOperations::default();
    if let Some(index) = prev_compaction_index {
        if let Some(FileEntry::Compaction { payload, .. }) = entries.get(index) {
            if payload.from_hook != Some(true) {
                if let Some(details) = payload.details.clone() {
                    if let Ok(details) = serde_json::from_value::<CompactionDetails>(details) {
                        ops.read.extend(details.read_files);
                        ops.edited.extend(details.modified_files);
                    }
                }
            }
        }
    }
    for message in messages {
        extract_file_ops_from_message(message, &mut ops);
    }
    ops
}

/// Inputs to a compaction run.
pub struct CompactRequest<'a> {
    /// Conversation messages (whole context, in order).
    pub messages: &'a [AgentMessage],
    /// The chosen cut point.
    pub cut: &'a CutPointResult,
    /// Id of the first kept entry.
    pub first_kept_entry_id: &'a str,
    /// Context tokens before compaction.
    pub tokens_before: u64,
    /// `/compact <instructions>` guidance.
    pub custom_instructions: Option<&'a str>,
    /// Previous summary for update-mode summarization.
    pub previous_summary: Option<&'a str>,
    /// Budget for the summary output.
    pub reserve_tokens: u64,
    /// Model for the summarizer call.
    pub model: pa_types::ai::Model,
}

/// Run compaction over a conversation slice: summarize the dropped prefix,
/// keeping from `first_kept_entry_id`. `summarize` performs the model call.
pub async fn compact_with(
    request: CompactRequest<'_>,
    summarize: SummarizerFn,
) -> anyhow::Result<CompactionResult> {
    let CompactRequest {
        messages,
        cut,
        first_kept_entry_id,
        tokens_before,
        custom_instructions,
        previous_summary,
        reserve_tokens,
        model,
    } = request;
    // Messages to summarize: everything before the cut.
    let summarized = &messages[..cut.first_kept_entry_index.min(messages.len())];
    let request_messages = build_summarization_request(
        summarized,
        custom_instructions,
        previous_summary,
        reserve_tokens,
    );
    let assistant = summarize(model, request_messages).await?;
    let usage = (assistant.usage.total_tokens > 0).then_some(assistant.usage);
    let summary = assistant
        .content
        .iter()
        .filter_map(|block| match block {
            pa_types::ai::AssistantContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    Ok(CompactionResult {
        summary,
        first_kept_entry_id: first_kept_entry_id.to_string(),
        tokens_before,
        usage,
    })
}

/// The compaction entry to persist for a result.
///
/// `fromHook` carries the TS `fromExtension` origin: whether a compaction
/// extension produced the summary (`agent-session.ts` passes its
/// `fromExtension` flag into `appendCompaction`). The Rust engine has no
/// extension seam yet, so every built-in compaction records `fromHook:
/// false`, the exact durable value TS writes for its built-in path — never
/// a missing key.
pub fn compaction_entry_for(
    result: &CompactionResult,
    details: &CompactionDetails,
    custom_instructions: Option<&str>,
) -> CompactionEntry {
    CompactionEntry {
        summary: result.summary.clone(),
        first_kept_entry_id: result.first_kept_entry_id.clone(),
        tokens_before: result.tokens_before,
        details: Some(serde_json::to_value(details).unwrap_or_default()),
        from_hook: Some(false),
        custom_instructions: custom_instructions.map(str::to_string),
        usage: result.usage,
        harness_digest: None,
    }
}

/// Full-file-list details for a compact run (prev compaction ops + messages).
pub fn details_for(
    messages: &[AgentMessage],
    entries: &[FileEntry],
    prev_compaction_index: Option<usize>,
) -> CompactionDetails {
    let ops = extract_file_operations(messages, entries, prev_compaction_index);
    let (read_files, modified_files) = compute_file_lists(&ops);
    CompactionDetails {
        read_files,
        modified_files,
    }
}

/// The XML file-ops block appended to a summary presentation.
pub fn file_ops_block(read_files: &[String], modified_files: &[String]) -> String {
    format_file_operations(read_files, modified_files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pa_types::session::EntryBase;

    fn user(text: &str) -> AgentMessage {
        AgentMessage::User(UserMessage {
            content: UserContent::Text(text.to_string()),
            timestamp: 0,
            rest: Default::default(),
        })
    }

    fn summary_assistant(text: &str) -> AssistantMessage {
        AssistantMessage {
            content: vec![pa_types::ai::AssistantContentBlock::Text(TextContent {
                text: text.to_string(),
                text_signature: None,
                rest: Default::default(),
            })],
            api: "openai-completions".to_string(),
            provider: "test".to_string(),
            model: "m".to_string(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            usage: Default::default(),
            stop_reason: pa_types::ai::StopReason::Stop,
            stop_reason_raw: None,
            error_message: None,
            timestamp: 0,
            rest: Default::default(),
        }
    }

    /// The entry records the TS wire record: `fromHook: false` (the
    /// built-in origin — TS passes `fromExtension`), the file-operation
    /// details, the summarizer usage, and the custom instructions.
    #[test]
    fn compaction_entry_records_the_ts_wire_fields() {
        let usage = pa_types::ai::Usage {
            input: 20,
            output: 10,
            cache_read: 80,
            cache_write: 0,
            total_tokens: 110,
            cost: Default::default(),
        };
        let result = CompactionResult {
            summary: "the overflow summary".to_string(),
            first_kept_entry_id: "e4".to_string(),
            tokens_before: 214,
            usage: Some(usage),
        };
        let details = CompactionDetails {
            read_files: vec!["a.rs".to_string()],
            modified_files: vec![],
        };
        assert_eq!(
            compaction_entry_for(&result, &details, Some("focus")),
            CompactionEntry {
                summary: "the overflow summary".to_string(),
                first_kept_entry_id: "e4".to_string(),
                tokens_before: 214,
                details: Some(serde_json::json!({
                    "readFiles": ["a.rs"],
                    "modifiedFiles": [],
                })),
                from_hook: Some(false),
                custom_instructions: Some("focus".to_string()),
                usage: Some(usage),
                harness_digest: None,
            }
        );
    }

    #[tokio::test]
    async fn compact_summarizes_prefix_and_returns_result() {
        let messages = vec![user("one"), user("two"), user("three")];
        let entries: Vec<FileEntry> = messages
            .iter()
            .enumerate()
            .map(|(index, _)| FileEntry::Message {
                message: user(&format!("m{index}")),
                base: EntryBase {
                    id: Some(format!("e{index}")),
                    parent_id: None,
                    timestamp: Some("2024-01-01T00:00:00.000Z".to_string()),
                    rest: Default::default(),
                },
            })
            .collect();
        let cut = CutPointResult {
            first_kept_entry_index: 1,
            turn_start_index: None,
            is_split_turn: false,
        };
        let model: pa_types::ai::Model = serde_json::from_value(serde_json::json!({
            "id": "m", "name": "m", "api": "openai-completions", "provider": "test",
            "baseUrl": "http://localhost", "reasoning": false, "input": ["text"],
            "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
            "contextWindow": 1000, "maxTokens": 100
        }))
        .unwrap();
        let summarize: SummarizerFn = Box::new(|_model, request| {
            Box::pin(async move {
                // The request only contains the dropped prefix.
                match &request[0] {
                    AgentMessage::User(user) => {
                        let text = user.content.text();
                        assert!(text.contains("<conversation>"));
                        assert!(text.contains("one"));
                        assert!(!text.contains("two"));
                    }
                    _ => panic!("expected user request"),
                }
                Ok(summary_assistant("## Goal\nship it"))
            })
        });
        let result = compact_with(
            CompactRequest {
                messages: &messages,
                cut: &cut,
                first_kept_entry_id: "e1",
                tokens_before: 1_000,
                custom_instructions: Some("focus"),
                previous_summary: None,
                reserve_tokens: 1_000,
                model,
            },
            summarize,
        )
        .await
        .unwrap();
        assert!(result.summary.starts_with("## Goal"));
        assert_eq!(result.first_kept_entry_id, "e1");
        assert_eq!(result.tokens_before, 1_000);
        // The persisted entry carries the summary + details.
        let details = details_for(&messages, &entries, None);
        let entry = compaction_entry_for(&result, &details, Some("focus"));
        assert_eq!(entry.summary, "## Goal\nship it");
        assert_eq!(entry.custom_instructions.as_deref(), Some("focus"));
    }

    #[test]
    fn summarization_request_shape() {
        let messages = vec![user("hello"), user("world")];
        let request = build_summarization_request(&messages, Some("be brief"), None, 1_000);
        match &request[0] {
            AgentMessage::User(user) => {
                let text = user.content.text();
                assert!(text.starts_with(
                    "<conversation>\n[User]: hello\n\n[User]: world\n</conversation>"
                ));
                assert!(text.contains("<user-instructions>\nThe user provided these instructions"));
                assert!(text.ends_with("redefining them."));
            }
            _ => panic!("expected user message"),
        }
    }
}
