//! Branch summarization for tree navigation. Port of
//! core/compaction/branch-summarization.ts.

use std::collections::HashSet;

use pa_types::session::{AgentMessage, CompactionSummaryMessage, FileEntry};

use super::compaction::estimate_tokens;
use super::compaction_utils::{
    compute_file_lists, extract_file_ops_from_message, format_file_operations,
    serialize_conversation, FileOperations,
};
use super::messages::{convert_to_llm, BRANCH_SUMMARY_PREFIX, BRANCH_SUMMARY_SUFFIX};

/// Details stored on a branch summary entry.
#[derive(Debug, Default, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchSummaryDetails {
    pub read_files: Vec<String>,
    pub modified_files: Vec<String>,
}

/// The result of generating a branch summary.
#[derive(Debug, Default, Clone)]
pub struct BranchSummaryResult {
    pub summary: Option<String>,
    pub read_files: Vec<String>,
    pub modified_files: Vec<String>,
    pub aborted: bool,
    pub error: Option<String>,
    pub usage: Option<pa_types::ai::Usage>,
}

/// Prepared summarization inputs.
#[derive(Debug, Default)]
pub struct BranchPreparation {
    pub messages: Vec<AgentMessage>,
    pub file_ops: FileOperations,
    pub total_tokens: u64,
}

/// Entry-path info between two tree positions.
#[derive(Debug, Default)]
pub struct CollectEntriesResult {
    pub entries: Vec<FileEntry>,
    pub common_ancestor_id: Option<String>,
}

fn parent_path(entries: &[FileEntry], leaf_id: &str) -> Vec<String> {
    let mut path = Vec::new();
    let mut current = Some(leaf_id.to_string());
    while let Some(id) = current {
        let Some(entry) = entries.iter().find(|entry| entry.id() == Some(id.as_str())) else {
            break;
        };
        path.push(id.clone());
        current = entry.parent_id().map(str::to_string);
    }
    path
}

/// Entries to summarize when navigating old-leaf -> target: the old path back
/// to (excluding) the common ancestor with the target path.
pub fn collect_entries_for_branch_summary(
    entries: &[FileEntry],
    old_leaf_id: Option<&str>,
    target_id: &str,
) -> CollectEntriesResult {
    let Some(old_leaf_id) = old_leaf_id else {
        return CollectEntriesResult::default();
    };
    let old_path: HashSet<String> = parent_path(entries, old_leaf_id).into_iter().collect();
    let target_path = parent_path(entries, target_id);
    let common_ancestor_id = target_path
        .iter()
        .rev()
        .find(|id| old_path.contains(*id))
        .cloned();
    let mut collected: Vec<FileEntry> = Vec::new();
    let mut current = Some(old_leaf_id.to_string());
    while let Some(id) = current {
        if Some(&id) == common_ancestor_id.as_ref() {
            break;
        }
        let Some(entry) = entries
            .iter()
            .find(|entry| entry.id() == Some(id.as_str()))
            .cloned()
        else {
            break;
        };
        current = entry.parent_id().map(str::to_string);
        collected.push(entry);
    }
    collected.reverse();
    CollectEntriesResult {
        entries: collected,
        common_ancestor_id,
    }
}

/// The message an entry contributes to summarizer input (compaction entries
/// become their summary message; bookkeeping entries contribute nothing).
fn get_message_from_entry(entry: &FileEntry) -> Option<AgentMessage> {
    match entry {
        FileEntry::Message { message, .. } => match message {
            // Tool results stay attached to their assistant tool call.
            AgentMessage::ToolResult(_) => None,
            _ => Some(message.clone()),
        },
        FileEntry::CustomMessage { payload, .. } => {
            if payload.custom_type == "harness_digest" {
                return None;
            }
            Some(AgentMessage::Custom(pa_types::session::CustomMessage {
                custom_type: payload.custom_type.clone(),
                content: payload.content.clone(),
                display: payload.display,
                details: payload.details.clone(),
                timestamp: super::super::session::timestamp_to_millis(entry.timestamp()),
                rest: Default::default(),
            }))
        }
        FileEntry::BranchSummary { payload, .. } => Some(AgentMessage::BranchSummary(
            pa_types::session::BranchSummaryMessage {
                summary: payload.summary.clone(),
                from_id: payload.from_id.clone(),
                timestamp: super::super::session::timestamp_to_millis(entry.timestamp()),
            },
        )),
        FileEntry::Compaction { payload, .. } => {
            Some(AgentMessage::CompactionSummary(CompactionSummaryMessage {
                summary: payload.summary.clone(),
                tokens_before: payload.tokens_before,
                retained_message_count: None,
                custom_instructions: payload.custom_instructions.clone(),
                harness_digest: payload.harness_digest.clone(),
                timestamp: super::super::session::timestamp_to_millis(entry.timestamp()),
            }))
        }
        _ => None,
    }
}

/// Prepare entries under a token budget: newest-to-oldest until the budget
/// is hit. File ops are collected from ALL entries (cumulative tracking).
pub fn prepare_branch_entries(entries: &[FileEntry], token_budget: u64) -> BranchPreparation {
    let mut file_ops = FileOperations::default();
    // Cumulative tracking from prior branch summaries (never extension ones).
    for entry in entries {
        if let FileEntry::BranchSummary { payload, .. } = entry {
            if payload.from_hook != Some(true) {
                if let Some(details) = payload.details.clone() {
                    if let Ok(details) = serde_json::from_value::<BranchSummaryDetails>(details) {
                        file_ops.read.extend(details.read_files);
                        file_ops.edited.extend(details.modified_files);
                    }
                }
            }
        }
    }
    let mut messages: Vec<AgentMessage> = Vec::new();
    let mut total_tokens = 0u64;
    for entry in entries.iter().rev() {
        let Some(message) = get_message_from_entry(entry) else {
            continue;
        };
        extract_file_ops_from_message(&message, &mut file_ops);
        let tokens = estimate_tokens(&message);
        if token_budget > 0 && total_tokens + tokens > token_budget {
            // Compaction/branch summaries squeeze in under 90% budget.
            let boundary = matches!(
                entry,
                FileEntry::Compaction { .. } | FileEntry::BranchSummary { .. }
            );
            if boundary && total_tokens < token_budget * 9 / 10 {
                messages.insert(0, message);
                total_tokens += tokens;
            }
            break;
        }
        messages.insert(0, message);
        total_tokens += tokens;
    }
    BranchPreparation {
        messages,
        file_ops,
        total_tokens,
    }
}

const BRANCH_SUMMARY_PREAMBLE: &str = "The user explored a different conversation branch before returning here.\nSummary of that exploration:\n\n";

const BRANCH_SUMMARY_PROMPT: &str = "Create a structured summary of this conversation branch for context when returning later.\n\nUse this EXACT format:\n\n## Goal\n[What was the user trying to accomplish in this branch?]\n\n## Constraints & Preferences\n- [Any constraints, preferences, or requirements mentioned]\n- [Or \"(none)\" if none were mentioned]\n\n## Progress\n### Done\n- [x] [Completed tasks/changes]\n\n### In Progress\n- [ ] [Work that was started but not finished]\n\n### Blocked\n- [Issues preventing progress, if any]\n\n## Key Decisions\n- **[Decision]**: [Brief rationale]\n\n## Next Steps\n1. [What should happen next to continue this work]\n\nKeep each section concise. Preserve exact file paths, function names, and error messages.";

/// Build the branch-summary request messages for the model.
pub fn build_branch_summary_request(
    entries: &[FileEntry],
    token_budget: u64,
    custom_instructions: Option<&str>,
    replace_instructions: bool,
) -> (Vec<AgentMessage>, BranchPreparation) {
    let preparation = prepare_branch_entries(entries, token_budget);
    let messages = if preparation.messages.is_empty() {
        Vec::new()
    } else {
        let llm_messages = convert_to_llm(&preparation.messages);
        let conversation_text = serialize_conversation(&llm_messages);
        let instructions = match (replace_instructions, custom_instructions) {
            (true, Some(custom)) => custom.to_string(),
            (false, Some(custom)) => {
                format!("{BRANCH_SUMMARY_PROMPT}\n\nAdditional focus: {custom}")
            }
            _ => BRANCH_SUMMARY_PROMPT.to_string(),
        };
        let prompt_text =
            format!("<conversation>\n{conversation_text}\n</conversation>\n\n{instructions}");
        vec![AgentMessage::User(pa_types::ai::UserMessage {
            content: pa_types::ai::UserContent::Text(prompt_text),
            timestamp: 0,
            rest: Default::default(),
        })]
    };
    (messages, preparation)
}

/// Assemble the final summary text from a model response.
pub fn finalize_branch_summary(
    response_text: &str,
    preparation: &BranchPreparation,
) -> BranchSummaryResult {
    let (read_files, modified_files) = compute_file_lists(&preparation.file_ops);
    let mut summary = format!("{BRANCH_SUMMARY_PREAMBLE}{response_text}");
    summary.push_str(&format_file_operations(&read_files, &modified_files));
    BranchSummaryResult {
        summary: Some(if summary.is_empty() {
            "No summary generated".to_string()
        } else {
            summary
        }),
        read_files,
        modified_files,
        aborted: false,
        error: None,
        usage: None,
    }
}

/// The presentation message users see for a branch summary.
pub fn branch_summary_presentation(summary: &str) -> String {
    format!("{BRANCH_SUMMARY_PREFIX}{summary}{BRANCH_SUMMARY_SUFFIX}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use pa_types::session::EntryBase;

    fn entry(id: &str, parent: Option<&str>, message: AgentMessage) -> FileEntry {
        FileEntry::Message {
            message,
            base: EntryBase {
                id: Some(id.to_string()),
                parent_id: parent.map(str::to_string),
                timestamp: Some("2024-01-01T00:00:00.000Z".to_string()),
                rest: Default::default(),
            },
        }
    }

    fn user(text: &str) -> AgentMessage {
        AgentMessage::User(pa_types::ai::UserMessage {
            content: pa_types::ai::UserContent::Text(text.to_string()),
            timestamp: 0,
            rest: Default::default(),
        })
    }

    #[test]
    fn collects_old_branch_to_common_ancestor() {
        // root -> u1 -> a1 -> u2 (old leaf)
        //        \\-> u3 (target, sibling of a1's subtree? no: sibling of u1's children)
        let entries = vec![
            entry("u1", None, user("start")),
            entry("a1", Some("u1"), user("assistant turn")),
            entry("u2", Some("a1"), user("old leaf")),
            entry("u3", Some("u1"), user("target")),
        ];
        let result = collect_entries_for_branch_summary(&entries, Some("u2"), "u3");
        assert_eq!(result.common_ancestor_id.as_deref(), Some("u1"));
        // u2 and a1 are collected (target ancestor excluded).
        assert_eq!(result.entries.len(), 2);
        assert_eq!(result.entries[0].id(), Some("a1"));
        assert_eq!(result.entries[1].id(), Some("u2"));
    }

    #[test]
    fn preparation_respects_budget_and_keeps_boundaries() {
        let mut entries = Vec::new();
        for i in 0..10 {
            entries.push(entry(
                &format!("e{i}"),
                None,
                user(&format!("message {i} with padding")),
            ));
        }
        // Each message estimates to ~6 tokens; a 6-token budget keeps only
        // the newest one before the walk breaks.
        let preparation = prepare_branch_entries(&entries, 6);
        assert_eq!(preparation.messages.len(), 1);
        // A budget below any single message keeps nothing.
        let tight = prepare_branch_entries(&entries, 1);
        assert_eq!(tight.messages.len(), 0);
        let unlimited = prepare_branch_entries(&entries, 0);
        assert_eq!(unlimited.messages.len(), 10);
    }

    #[test]
    fn request_and_finalize() {
        let entries = vec![entry("e0", None, user("explore the widget"))];
        let (messages, preparation) =
            build_branch_summary_request(&entries, 0, Some("focus on x"), false);
        assert_eq!(messages.len(), 1);
        match &messages[0] {
            AgentMessage::User(user) => {
                let text = user.content.text();
                assert!(text.starts_with("<conversation>"));
                assert!(text.contains("Additional focus: focus on x"));
            }
            _ => panic!("expected user"),
        }
        let result = finalize_branch_summary("## Goal\nexplore", &preparation);
        let summary = result.summary.unwrap();
        assert!(summary.contains("The user explored a different conversation branch"));
        assert!(summary.contains("## Goal"));
        // Presentation wraps in the branch envelope.
        assert!(branch_summary_presentation("s").contains("[branch-summary]"));
    }

    #[test]
    fn compaction_entries_become_summary_messages() {
        let compaction = FileEntry::Compaction {
            payload: pa_types::session::CompactionEntry {
                summary: "prior state".to_string(),
                first_kept_entry_id: "x".to_string(),
                tokens_before: 10,
                details: None,
                from_hook: None,
                custom_instructions: None,
                usage: None,
                harness_digest: None,
            },
            base: EntryBase {
                id: Some("c0".to_string()),
                parent_id: None,
                timestamp: Some("2024-01-01T00:00:00.000Z".to_string()),
                rest: Default::default(),
            },
        };
        let message = get_message_from_entry(&compaction).unwrap();
        match message {
            AgentMessage::CompactionSummary(summary) => {
                assert_eq!(summary.summary, "prior state");
                assert_eq!(summary.tokens_before, 10);
            }
            _ => panic!("expected compaction summary"),
        }
    }
}
