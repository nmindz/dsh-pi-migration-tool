//! Duplicate detection for Claude Code imports.
//!
//! A Claude Code session can already be in Pi without sharing any identifier
//! with it: an earlier import, OMP's own `--from-claude`, or a detour through
//! DSH all mint new session ids. Two signals survive those trips:
//!
//! - the `foreign_session_import` record an importer leaves, naming the Claude
//!   session it came from, which identifies an import exactly;
//! - the text of the human prompts, which every faithful conversion carries
//!   verbatim, which identifies a copy by content.
//!
//! A prompt only counts toward a content match when it is long enough to be
//! specific: short replies such as `yes` or `continue` recur across unrelated
//! sessions, and envelopes a harness writes on the user's behalf
//! (`<command-name>`, `<task-notification>`, `[Request interrupted by user]`)
//! are not typed text.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::PathBuf;

use serde_json::Value;

use crate::pi;

/// Fewest characters a normalized prompt needs to count as a fingerprint.
pub const MIN_PROMPT_CHARS: usize = 16;

/// Text of a message's content: the string itself, or every text block joined
/// by newlines. Images and other non-text blocks contribute nothing.
pub fn content_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Fingerprint of one human prompt, or `None` when the prompt is too short or
/// machine-written to identify a conversation.
///
/// Whitespace runs collapse to one space before hashing, so a copy that
/// re-wrapped or re-joined the prompt still matches.
pub fn fingerprint(text: &str) -> Option<u64> {
    let mut words = text.split_whitespace().peekable();
    if words
        .peek()
        .is_some_and(|first| first.starts_with('<') || first.starts_with('['))
    {
        return None;
    }
    let mut hasher = DefaultHasher::new();
    let mut chars = 0usize;
    for (index, word) in words.enumerate() {
        if index > 0 {
            ' '.hash(&mut hasher);
            chars += 1;
        }
        for c in word.chars() {
            c.hash(&mut hasher);
        }
        chars += word.chars().count();
    }
    (chars >= MIN_PROMPT_CHARS).then(|| hasher.finish())
}

/// A Pi session another session was matched against.
#[derive(Debug, Clone)]
pub struct Twin {
    /// Pi session file.
    pub path: PathBuf,
    /// Pi session id.
    pub id: String,
    /// Pi session title; empty when it has none.
    pub title: String,
}

impl Twin {
    /// Title, or the session id when the session is untitled.
    pub fn label(&self) -> &str {
        if self.title.is_empty() {
            &self.id
        } else {
            &self.title
        }
    }
}

/// How a Claude Code session relates to what Pi already holds.
#[derive(Debug, Clone)]
pub enum Verdict {
    /// Nothing in Pi resembles it.
    Fresh,
    /// A Pi session records being imported from exactly this Claude session.
    Imported(Twin),
    /// One Pi session already holds every fingerprinted prompt of it.
    Covered(Twin),
    /// A Pi session holds some, not all, of its fingerprinted prompts.
    Overlap {
        /// Pi session sharing the most prompts.
        twin: Twin,
        /// Prompts found in that session.
        shared: usize,
        /// Fingerprinted prompts of the Claude session.
        total: usize,
    },
}

impl Verdict {
    /// True when importing would add a second copy of a conversation Pi holds.
    pub fn is_duplicate(&self) -> bool {
        matches!(self, Verdict::Imported(_) | Verdict::Covered(_))
    }

    /// One-line explanation, or `None` for a fresh session.
    pub fn describe(&self) -> Option<String> {
        match self {
            Verdict::Fresh => None,
            Verdict::Imported(twin) => Some(format!("already imported as {}", twin.label())),
            Verdict::Covered(twin) => Some(format!("every prompt already in {}", twin.label())),
            Verdict::Overlap {
                twin,
                shared,
                total,
            } => Some(format!("{shared}/{total} prompts also in {}", twin.label())),
        }
    }

    /// Pi session the verdict points at.
    pub fn twin(&self) -> Option<&Twin> {
        match self {
            Verdict::Fresh => None,
            Verdict::Imported(twin) | Verdict::Covered(twin) | Verdict::Overlap { twin, .. } => {
                Some(twin)
            }
        }
    }
}

/// What Pi already holds, keyed for matching Claude Code sessions against it.
#[derive(Debug, Default)]
pub struct Index {
    twins: Vec<Twin>,
    by_source: HashMap<String, usize>,
    by_prompt: HashMap<u64, Vec<usize>>,
}

impl Index {
    /// Indexes discovered Pi sessions by import origin and prompt fingerprint.
    pub fn new(sessions: &[pi::SessionRef]) -> Self {
        let mut index = Index::default();
        for session in sessions {
            let slot = index.twins.len();
            index.twins.push(Twin {
                path: session.path.clone(),
                id: session.id.clone(),
                title: session.title.clone(),
            });
            for source in &session.claude_sources {
                index.by_source.entry(source.clone()).or_insert(slot);
            }
            for prompt in &session.prompts {
                index.by_prompt.entry(*prompt).or_default().push(slot);
            }
        }
        index
    }

    /// Judges one Claude Code session by its id and its distinct prompt fingerprints.
    pub fn judge(&self, claude_id: &str, prompts: &[u64]) -> Verdict {
        if let Some(slot) = self.by_source.get(claude_id) {
            return Verdict::Imported(self.twins[*slot].clone());
        }
        let mut shared: HashMap<usize, usize> = HashMap::new();
        for prompt in prompts {
            for slot in self.by_prompt.get(prompt).into_iter().flatten() {
                *shared.entry(*slot).or_insert(0) += 1;
            }
        }
        // Most shared prompts wins; on a tie the earlier, newer session does.
        let Some((slot, count)) = shared
            .into_iter()
            .max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(&a.0)))
        else {
            return Verdict::Fresh;
        };
        let twin = self.twins[slot].clone();
        if count == prompts.len() {
            Verdict::Covered(twin)
        } else {
            Verdict::Overlap {
                twin,
                shared: count,
                total: prompts.len(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pi_session(id: &str, prompts: &[&str], sources: &[&str]) -> pi::SessionRef {
        let mut fingerprints: Vec<u64> = prompts.iter().filter_map(|p| fingerprint(p)).collect();
        fingerprints.sort_unstable();
        fingerprints.dedup();
        pi::SessionRef {
            id: id.into(),
            path: PathBuf::from(format!("/pi/{id}.jsonl")),
            bucket: String::new(),
            cwd: String::new(),
            timestamp: String::new(),
            title: format!("title {id}"),
            size: 0,
            entries: 0,
            messages: 0,
            migrated: false,
            prompts: fingerprints,
            claude_sources: sources.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn prints(prompts: &[&str]) -> Vec<u64> {
        prompts.iter().filter_map(|p| fingerprint(p)).collect()
    }

    #[test]
    fn fingerprint_ignores_whitespace_layout() {
        assert_eq!(
            fingerprint("fix the  flaky\nlogin test please"),
            fingerprint("  fix the flaky login test please ")
        );
        assert_ne!(
            fingerprint("fix the flaky login test please"),
            fingerprint("fix the flaky signup test please")
        );
    }

    #[test]
    fn short_and_machine_written_prompts_have_no_fingerprint() {
        assert_eq!(fingerprint("continue"), None);
        assert_eq!(fingerprint("<command-name>/model</command-name> and more text"), None);
        assert_eq!(fingerprint("[Request interrupted by user for tool use]"), None);
        assert!(fingerprint("explain the retry policy here").is_some());
    }

    #[test]
    fn content_text_joins_text_blocks_and_skips_images() {
        let content = json!([
            {"type": "text", "text": "one"},
            {"type": "image", "data": "x", "mimeType": "image/png"},
            {"type": "text", "text": "two"}
        ]);
        assert_eq!(content_text(&content), "one\ntwo");
        assert_eq!(content_text(&json!("plain")), "plain");
    }

    #[test]
    fn a_recorded_import_wins_over_content() {
        let sessions = vec![
            pi_session("copy", &["explain the retry policy here"], &[]),
            pi_session("import", &[], &["claude-1"]),
        ];
        let index = Index::new(&sessions);
        let verdict = index.judge("claude-1", &prints(&["explain the retry policy here"]));
        assert!(matches!(&verdict, Verdict::Imported(t) if t.id == "import"));
        assert!(verdict.is_duplicate());
    }

    #[test]
    fn every_prompt_in_one_session_is_a_duplicate_and_some_is_an_overlap() {
        let sessions = vec![pi_session(
            "dsh-copy",
            &["explain the retry policy here", "now add jitter to the backoff"],
            &[],
        )];
        let index = Index::new(&sessions);

        let covered = index.judge("c1", &prints(&["explain the retry policy here"]));
        assert!(matches!(&covered, Verdict::Covered(t) if t.id == "dsh-copy"));
        assert!(covered.is_duplicate());

        let partial = index.judge(
            "c2",
            &prints(&["explain the retry policy here", "a prompt only Claude saw"]),
        );
        assert!(matches!(partial, Verdict::Overlap { shared: 1, total: 2, .. }));
        assert!(!partial.is_duplicate());

        assert!(matches!(index.judge("c3", &prints(&["unrelated question about css"])), Verdict::Fresh));
        assert!(matches!(index.judge("c4", &[]), Verdict::Fresh));
    }

    #[test]
    fn the_session_sharing_most_prompts_is_reported() {
        let sessions = vec![
            pi_session("one", &["explain the retry policy here"], &[]),
            pi_session(
                "both",
                &["explain the retry policy here", "now add jitter to the backoff"],
                &[],
            ),
        ];
        let index = Index::new(&sessions);
        let verdict = index.judge(
            "c",
            &prints(&[
                "explain the retry policy here",
                "now add jitter to the backoff",
                "and document it in the readme",
            ]),
        );
        assert!(matches!(&verdict, Verdict::Overlap { twin, shared: 2, .. } if twin.id == "both"));
    }
}
