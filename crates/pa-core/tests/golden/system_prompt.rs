//! Golden parity for the assembled system prompt (B-6): the Rust
//! `build_system_prompt` output must match the TS prompt for the same
//! fixture state. The golden corpus file is produced by
//! `tests/golden/system-prompt.mjs`, which assembles the real TypeScript
//! `buildSystemPrompt` (from the /tmp/pa-golden copy of prime-agent) over
//! the same bundled skills directory; both sides normalize that directory
//! to `<skills-dir>` so the corpus is checkout-independent.

use pa_core::prompts::system_prompt::{build_system_prompt, BuildSystemPromptOptions};
use pa_core::skills::load_skills_from_dir;
use std::path::Path;

const GOLDEN: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/golden/corpus/system-prompt.json"
);

#[derive(serde::Deserialize)]
struct GoldenCorpus {
    #[serde(rename = "systemPrompt")]
    system_prompt: String,
}

/// The workspace bundled skills directory (source-checkout layout):
/// pa-core lives at `<root>/crates/pa-core`.
fn bundled_skills_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .join("skills")
}

#[test]
fn system_prompt_matches_ts_golden_for_fixture_state() {
    let skills_dir = bundled_skills_dir();
    let mut loaded = load_skills_from_dir(&skills_dir, "package");
    // Both products enumerate skills in raw readdir order, which is
    // directory- and runtime-dependent (the shipped TS binary, the jiti
    // harness, and Rust read the same directory in different orders); the
    // battery normalizes order for the same reason. Pin content, not
    // enumeration order: sort to the golden's order.
    loaded
        .skills
        .sort_by(|left, right| left.name.cmp(&right.name));
    let golden: GoldenCorpus =
        serde_json::from_str(&std::fs::read_to_string(GOLDEN).expect("golden corpus"))
            .expect("golden corpus json");

    let prompt = build_system_prompt(&BuildSystemPromptOptions {
        cwd: "/w".to_string(),
        messages_path: Some("/w/sessions/fixture-session.jsonl".to_string()),
        skills: loaded.skills,
        selected_tools: Some(vec!["ipython"]),
        allow_recursion: Some(true),
        rlm_depth: Some(0),
        ..Default::default()
    })
    .replace(skills_dir.to_string_lossy().as_ref(), "<skills-dir>");

    assert_eq!(prompt, golden.system_prompt);
}
