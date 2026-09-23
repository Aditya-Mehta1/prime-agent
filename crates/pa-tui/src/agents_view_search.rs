//! The agents-view session search (TS `session-view-search.ts`, redesigned
//! per Kevin's 2026-09-23 directive): a picker filter over the session's
//! identity fields — the display NAME (primary), the durable session ID,
//! and the CWD — never the transcript content.
//!
//! TS parity: the TS product's `createUnifiedSearchableText` also joins
//! `firstMessage`, `allMessagesText` (the capped transcript corpus), the
//! roster recap `summary`, `sessionFile`/`path`, and `parentSessionPath`
//! into one match corpus, so a query like "fast" surfaces every session
//! whose transcript mentions it. Kevin's directive removes those fields:
//! "filtering by session name is the most important. First message is
//! annoying to filter by — it leads to many unrelated sessions showing up.
//! Session file path (we filter by session id anyway) and the roster
//! summary line aren't needed." This module is that deliberate divergence;
//! the query language itself (`re:` regexes, quoted phrases, all-must-match
//! tokens) stays TS-shaped.
//!
//! The matching algorithm follows the session/command-picker standard
//! (VS Code quick-open `fuzzyScorer.ts`/`filters.ts`, Zed's project
//! switcher, tmux choose-tree): fuzzy subsequence matching on the display
//! label, ranked exact > prefix > contiguous substring > fuzzy, recency as
//! the tiebreaker. VS Code `doScoreItemFuzzySingle`: "If we have a prefix
//! match on the label, we give a much higher baseScore to elevate these
//! matches over others."

use crate::fuzzy::fuzzy_match;

/// The strict fuzzy ceiling above which a token counts as unmatched (TS
/// `STRICT_FUZZY_MAX_TOKEN_SCORE`).
const STRICT_FUZZY_MAX_TOKEN_SCORE: f64 = 25.0;

/// One record's match targets, in rank order (lower tier ranks first).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionSearchText {
    /// The session display name (daemon `sessionName`, saved `name`).
    pub name: String,
    /// The durable session id (daemon `sessionId`, saved `id`).
    pub id: String,
    /// The session working directory.
    pub cwd: String,
}

impl SessionSearchText {
    /// The regex-mode corpus: the same restricted fields, joined.
    fn corpus(&self) -> String {
        [self.name.as_str(), self.id.as_str(), self.cwd.as_str()]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// One parsed search query (TS `ParsedSearchQuery`): `re:` enters regex
/// mode; otherwise whitespace tokens with `"quoted phrase"` support.
pub struct ParsedSearchQuery {
    regex: Option<fancy_regex::Regex>,
    tokens: Vec<SearchToken>,
    /// A query that cannot match anything (an invalid `re:` pattern).
    matches_never: bool,
}

enum SearchToken {
    Fuzzy(String),
    Phrase(String),
}

/// Parse a query; invalid `re:` patterns parse to no matches.
pub fn parse_search_query(query: &str) -> ParsedSearchQuery {
    let trimmed = query.trim();
    if let Some(pattern) = trimmed.strip_prefix("re:") {
        let pattern = pattern.trim();
        if pattern.is_empty() {
            return ParsedSearchQuery {
                regex: None,
                tokens: Vec::new(),
                matches_never: true,
            };
        }
        let matches_never = match fancy_regex::Regex::new(&format!("(?i){pattern}")) {
            Ok(regex) => {
                return ParsedSearchQuery {
                    regex: Some(regex),
                    tokens: Vec::new(),
                    matches_never: false,
                }
            }
            Err(_) => true,
        };
        return ParsedSearchQuery {
            regex: None,
            tokens: Vec::new(),
            matches_never,
        };
    }
    ParsedSearchQuery {
        regex: None,
        tokens: tokenize(trimmed),
        matches_never: false,
    }
}

fn tokenize(trimmed: &str) -> Vec<SearchToken> {
    let mut tokens = Vec::new();
    let mut buffer = String::new();
    let mut in_quote = false;
    for ch in trimmed.chars() {
        if ch == '"' {
            if in_quote {
                push_token(&mut tokens, &mut buffer, SearchToken::Phrase);
            } else {
                push_token(&mut tokens, &mut buffer, SearchToken::Fuzzy);
            }
            in_quote = !in_quote;
            continue;
        }
        if !in_quote && ch.is_whitespace() {
            push_token(&mut tokens, &mut buffer, SearchToken::Fuzzy);
            continue;
        }
        buffer.push(ch);
    }
    if in_quote {
        // Unbalanced quotes fall back to plain whitespace tokenization.
        return trimmed
            .split_whitespace()
            .map(|token| SearchToken::Fuzzy(token.to_string()))
            .collect();
    }
    // Whatever is left in the buffer belongs to the last quote state.
    let kind = if in_quote {
        SearchToken::Phrase
    } else {
        SearchToken::Fuzzy
    };
    push_token(&mut tokens, &mut buffer, kind);
    tokens
}

fn push_token(tokens: &mut Vec<SearchToken>, buffer: &mut String, kind: fn(String) -> SearchToken) {
    let value = buffer.trim().to_string();
    buffer.clear();
    if !value.is_empty() {
        tokens.push(kind(value));
    }
}

/// Score one record's targets against the query: `Some(score)` when the
/// query matches (lower is better — the `fuzzy_match` convention), `None`
/// otherwise. Every token must match at least one target; the score sums
/// per-token tier scores, so a record ranks by its weakest matching token.
pub fn score_search(targets: &SessionSearchText, query: &ParsedSearchQuery) -> Option<f64> {
    if query.matches_never {
        return None;
    }
    if let Some(regex) = &query.regex {
        let corpus = targets.corpus();
        if corpus.is_empty() {
            return None;
        }
        return regex
            .find(&corpus)
            .ok()
            .flatten()
            .map(|found| found.start() as f64 * 0.1);
    }
    if query.tokens.is_empty() {
        return Some(0.0);
    }
    let mut total = 0.0f64;
    for token in &query.tokens {
        total += match token {
            SearchToken::Phrase(value) => contiguous_token_score(value, targets)?,
            SearchToken::Fuzzy(value) => token_score(value, targets)?,
        };
    }
    Some(total)
}

/// The tier stride: within-tier quality differences are always smaller, so
/// the worst token's tier decides the rank before its quality does.
const TIER_STRIDE: f64 = 100_000.0;

/// One fuzzy token: the best tier it reaches across the targets (TS kept
/// the contiguous-first-then-fuzzy order; this generalizes it per field).
fn token_score(token: &str, targets: &SessionSearchText) -> Option<f64> {
    contiguous_token_score(token, targets).or_else(|| {
        let score = fuzzy_match(token, &targets.name)?;
        (score <= STRICT_FUZZY_MAX_TOKEN_SCORE).then_some(4.0 * TIER_STRIDE + score)
    })
}

/// One contiguous token (or the contiguous phase of a fuzzy token). The
/// tiers mirror VS Code quick-open scoring: the identity match is highest
/// (`PATH_IDENTITY_SCORE` — a pasted full id is unambiguous), then the
/// name ranks exact > prefix > substring, then the id prefix and
/// substring (paste-a-fragment targeting), then the CWD basename and
/// full path.
fn contiguous_token_score(token: &str, targets: &SessionSearchText) -> Option<f64> {
    let needle = normalize(token);
    if needle.is_empty() {
        return Some(0.0);
    }
    if needle == normalize(&targets.id) {
        return Some(0.0);
    }
    if let Some(score) = label_score(&needle, &targets.name) {
        return Some(TIER_STRIDE + score);
    }
    if let Some(found) = targets.id.find(&needle) {
        let tier = if found == 0 { 5.0 } else { 6.0 };
        return Some(tier * TIER_STRIDE + found as f64);
    }
    if let Some(found) = cwd_basename(&targets.cwd).find(&needle) {
        return Some(7.0 * TIER_STRIDE + found as f64);
    }
    targets
        .cwd
        .find(&needle)
        .map(|found| 8.0 * TIER_STRIDE + found as f64)
}

/// The name tiers: exact, prefix (shorter labels win, VS Code
/// `prefixLengthBoost`), then substring (earlier wins).
fn label_score(needle: &str, name: &str) -> Option<f64> {
    let name = normalize(name);
    if name.is_empty() {
        return None;
    }
    if name == needle {
        return Some(0.0);
    }
    if name.strip_prefix(needle).is_some() {
        return Some(TIER_STRIDE + (name.chars().count() - needle.chars().count()) as f64);
    }
    name.find(needle)
        .map(|found| 2.0 * TIER_STRIDE + found as f64)
}

/// Lowercase and collapse whitespace (TS `normalizeWhitespaceLower`).
fn normalize(text: &str) -> String {
    text.to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The CWD's last path segment (`Path::file_name`), the directory label.
fn cwd_basename(cwd: &str) -> String {
    std::path::Path::new(cwd)
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn targets() -> SessionSearchText {
        SessionSearchText {
            name: "Gateway Worker".to_string(),
            id: "01a0b6b3-2e8d-71ab-b1c2-2a7db1bf8077".to_string(),
            cwd: "/Users/kevin/pi/prime-agent".to_string(),
        }
    }

    fn score(query: &str) -> Option<f64> {
        score_search(&targets(), &parse_search_query(query))
    }

    #[test]
    fn name_exact_outranks_prefix_outranks_substring_outranks_fuzzy() {
        let exact = score("gateway worker").expect("exact name matches");
        let prefix = score("gateway").expect("name prefix matches");
        let substring = score("worker").expect("name substring matches");
        let fuzzy = score("gtwy").expect("name fuzzy matches");
        assert!(exact < prefix, "exact {exact} < prefix {prefix}");
        assert!(prefix < substring, "prefix {prefix} < substring {substring}");
        assert!(substring < fuzzy, "substring {substring} < fuzzy {fuzzy}");
    }

    #[test]
    fn shorter_prefix_wins() {
        let short = score_search(
            &SessionSearchText {
                name: "run books".to_string(),
                ..targets()
            },
            &parse_search_query("run"),
        );
        let long = score_search(
            &SessionSearchText {
                name: "runway cleanup crew".to_string(),
                ..targets()
            },
            &parse_search_query("run"),
        );
        let (short, long) = (short.expect("matches"), long.expect("matches"));
        assert!(short < long, "shorter label {short} < longer {long}");
    }

    #[test]
    fn the_session_id_targets_by_paste_prefix_and_fragment() {
        assert!(score("01a0b6b3").is_some(), "pasted uuid prefix matches");
        // The name is primary: a name fuzzy match outranks a bare id
        // fragment; only the identity paste outranks the name.
        let fragment = score("b1c2").expect("middle fragment matches");
        let name_fuzzy = score("gtwy").expect("name fuzzy matches");
        assert!(
            name_fuzzy < fragment,
            "name fuzzy {name_fuzzy} outranks id fragment {fragment}"
        );
        let identity = score("01a0b6b3-2e8d-71ab-b1c2-2a7db1bf8077").expect("full id matches");
        let name_exact = score("gateway worker").expect("exact name matches");
        assert!(
            identity < name_exact,
            "the pasted identity {identity} outranks the name exact {name_exact}"
        );
    }

    #[test]
    fn the_cwd_targets_basename_first() {
        assert!(score("prime-agent").is_some(), "cwd basename matches");
        assert!(score("kevin/pi").is_some(), "cwd path fragments match");
    }

    #[test]
    fn transcript_and_roster_fields_never_match() {
        // The corpus is name + id + cwd only: first messages, transcript
        // text, the recap summary, and session paths have no tier to hit.
        for query in ["backoff", "deploy", "recap", "sessions", "jsonl"] {
            assert!(
                score(query).is_none(),
                "{query:?} matches nothing in the restricted corpus"
            );
        }
    }

    #[test]
    fn every_token_must_match_and_phrases_stay_contiguous() {
        let both = score("gateway 01a0");
        assert!(both.is_some(), "tokens may match different targets");
        assert!(score("gateway zebra").is_none(), "all tokens must match");
        let phrase = parse_search_query(r#""gateway worker" 01a0"#);
        assert!(score_search(&targets(), &phrase).is_some(), "phrases match");
        let split_phrase = parse_search_query(r#""worker gateway""#);
        assert!(
            score_search(&targets(), &split_phrase).is_none(),
            "phrases are contiguous substrings, not fuzzy"
        );
    }

    #[test]
    fn regex_mode_searches_the_same_restricted_corpus() {
        let hit = parse_search_query("re:gateway.*worker");
        assert!(score_search(&targets(), &hit).is_some());
        let transcript_only = parse_search_query("re:backoff");
        assert!(score_search(&targets(), &transcript_only).is_none());
        let invalid = parse_search_query("re:[");
        assert!(score_search(&targets(), &invalid).is_none());
    }

    #[test]
    fn an_empty_query_matches_everything_at_zero() {
        assert_eq!(score(""), Some(0.0));
    }

    #[test]
    fn fuzzy_keeps_the_strict_ceiling() {
        // A sprawling subsequence over a long name scores past the strict
        // ceiling and rejects (TS `STRICT_FUZZY_MAX_TOKEN_SCORE`).
        let sprawled = SessionSearchText {
            name: "primary agent session worker running in the forest temple of doom"
                .to_string(),
            ..targets()
        };
        assert!(
            score_search(&sprawled, &parse_search_query("podm")).is_none(),
            "weak sprawling fuzzy matches reject"
        );
        // A compact subsequence still reaches the fuzzy tier.
        assert!(
            score_search(&sprawled, &parse_search_query("prm")).is_some(),
            "compact fuzzy matches stay"
        );
    }
}
