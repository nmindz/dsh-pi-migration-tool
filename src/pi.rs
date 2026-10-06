//! Oh My Pi (OMP) session transcripts: discovery, JSONL decoding, and encoding.
//!
//! A Pi session file begins with a fixed-width 256-byte title slot, followed by
//! the session header and then append-only entries that form a tree through
//! `id`/`parentId`.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use walkdir::WalkDir;

use crate::convert::ORIGIN_TYPE;
use crate::dedup;
use serde_json::{json, Map, Value};

/// Session format version this tool emits.
pub const CURRENT_VERSION: u64 = 3;

/// Exact byte width of the leading title slot, including its newline.
pub const TITLE_SLOT_BYTES: usize = 256;

/// A complete Pi session file held in memory.
#[derive(Debug, Clone)]
pub struct SessionFile {
    /// Current session title; empty when none was set.
    pub title: String,
    /// Who set the title: `user` for a manual rename, `auto` for a generated
    /// one, `None` when the slot carries no source.
    pub title_source: Option<String>,
    /// Title-slot update time in ISO-8601 with milliseconds.
    pub title_updated_at: String,
    /// Session header row.
    pub header: Map<String, Value>,
    /// Entry rows in file order.
    pub entries: Vec<Value>,
}

impl SessionFile {
    /// Session identifier from the header.
    pub fn id(&self) -> &str {
        self.header.get("id").and_then(Value::as_str).unwrap_or("")
    }

    /// Working directory from the header.
    pub fn cwd(&self) -> &str {
        self.header.get("cwd").and_then(Value::as_str).unwrap_or("")
    }

    /// Header creation timestamp in ISO-8601.
    pub fn timestamp(&self) -> &str {
        self.header
            .get("timestamp")
            .and_then(Value::as_str)
            .unwrap_or("")
    }

    /// Number of conversation messages. A session with none is the empty shell
    /// a cancelled or never-used launch leaves behind.
    pub fn message_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| e.get("type").and_then(Value::as_str) == Some("message"))
            .count()
    }

    /// True when a model answered at least once. Assistant messages Claude
    /// Code wrote itself, such as API errors, do not count.
    pub fn has_reply(&self) -> bool {
        self.entries.iter().any(|e| {
            let message = e.get("message");
            e.get("type").and_then(Value::as_str) == Some("message")
                && message.and_then(|m| m.get("role")).and_then(Value::as_str) == Some("assistant")
                && message.and_then(|m| m.get("model")).and_then(Value::as_str)
                    != Some(crate::claude::SYNTHETIC_MODEL)
        })
    }

    /// True when this tool wrote the file, from a DSH session or a Claude Code import.
    pub fn is_migrated(&self) -> bool {
        self.entries.iter().any(|e| {
            match e.get("customType").and_then(Value::as_str) {
                Some(ORIGIN_TYPE) => true,
                Some(IMPORT_TYPE) => import_record(e).is_some_and(|record| record.ours),
                _ => false,
            }
        })
    }

    /// Counts entries by type, with message entries split by role.
    pub fn histogram(&self) -> BTreeMap<String, usize> {
        let mut out = BTreeMap::new();
        for entry in &self.entries {
            let kind = entry.get("type").and_then(Value::as_str).unwrap_or("?");
            let key = match kind {
                "message" => {
                    let role = entry
                        .get("message")
                        .and_then(|m| m.get("role"))
                        .and_then(Value::as_str)
                        .unwrap_or("?");
                    format!("message:{role}")
                }
                "custom" | "custom_message" => {
                    let custom = entry
                        .get("customType")
                        .and_then(Value::as_str)
                        .unwrap_or("?");
                    format!("{kind}:{custom}")
                }
                other => other.to_string(),
            };
            *out.entry(key).or_insert(0) += 1;
        }
        out
    }
}

/// A Pi session discovered on disk.
#[derive(Debug, Clone)]
pub struct SessionRef {
    /// Session identifier from the header.
    pub id: String,
    /// Path of the session file.
    pub path: PathBuf,
    /// Project bucket directory name.
    pub bucket: String,
    /// Working directory from the header.
    pub cwd: String,
    /// Header creation timestamp in ISO-8601.
    pub timestamp: String,
    /// Current title from the title slot.
    pub title: String,
    /// File size in bytes.
    pub size: u64,
    /// Number of entry rows, excluding the title slot and header.
    pub entries: usize,
    /// Number of conversation messages, counted during discovery.
    pub messages: usize,
    /// True when this tool wrote the file, from a DSH session or a Claude Code import.
    pub migrated: bool,
    /// Distinct fingerprints of the human prompts, for duplicate detection.
    pub prompts: Vec<u64>,
    /// Claude Code sessions this file records being imported from.
    pub claude_sources: Vec<String>,
}

/// `customType` OMP gives the record it leaves on a session imported from
/// another harness; this tool writes the same record on its Claude imports.
pub const IMPORT_TYPE: &str = "foreign_session_import";

/// The origin a `foreign_session_import` record names.
pub struct ImportRecord {
    /// Harness the session came from, for example `claude`.
    pub source: String,
    /// Session id in that harness.
    pub source_id: String,
    /// True when this tool, not OMP, performed the import.
    pub ours: bool,
}

/// Reads a `foreign_session_import` record, or `None` for any other entry.
pub fn import_record(entry: &Value) -> Option<ImportRecord> {
    if entry.get("customType").and_then(Value::as_str) != Some(IMPORT_TYPE) {
        return None;
    }
    let data = entry.get("data")?;
    Some(ImportRecord {
        source: data.get("source")?.as_str()?.to_string(),
        source_id: data.get("sourceId")?.as_str()?.to_string(),
        ours: data.get("tool").and_then(Value::as_str) == Some(env!("CARGO_PKG_NAME")),
    })
}

/// Builds the fixed-width title slot line, truncating an over-long title.
///
/// The slot is rewritten in place by Pi when a session is renamed, so it must
/// always occupy exactly [`TITLE_SLOT_BYTES`] including its newline.
pub fn title_slot(title: &str, source: Option<&str>, updated_at: &str) -> String {
    let stamp = serde_json::to_string(updated_at).unwrap_or_else(|_| "\"\"".into());
    let source = source
        .map(|s| format!(",\"source\":{}", serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())))
        .unwrap_or_default();
    let mut title = title.to_string();
    loop {
        let escaped = serde_json::to_string(&title).unwrap_or_else(|_| "\"\"".into());
        let fixed = format!(
            "{{\"type\":\"title\",\"v\":1,\"title\":{escaped}{source},\"updatedAt\":{stamp},\"pad\":\"\"}}"
        );
        // One byte of the budget belongs to the terminating newline.
        let used = fixed.len() + 1;
        if used <= TITLE_SLOT_BYTES {
            let pad = " ".repeat(TITLE_SLOT_BYTES - used);
            return format!(
                "{{\"type\":\"title\",\"v\":1,\"title\":{escaped}{source},\"updatedAt\":{stamp},\"pad\":\"{pad}\"}}"
            );
        }
        if title.is_empty() {
            // An updatedAt long enough to overflow alone cannot be shortened here.
            return fixed;
        }
        let overflow = used - TITLE_SLOT_BYTES;
        let mut cut = title.len().saturating_sub(overflow);
        while cut > 0 && !title.is_char_boundary(cut) {
            cut -= 1;
        }
        title.truncate(cut);
    }
}

/// True when the first record of `path` is a Pi title slot or session header,
/// the two shapes a Pi session file can open with.
pub fn is_session_file(path: &Path) -> Result<bool> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    for line in BufReader::new(file).lines() {
        let line = line.with_context(|| format!("read {}", path.display()))?;
        if line.trim().is_empty() {
            continue;
        }
        let kind = serde_json::from_str::<Value>(&line)
            .ok()
            .and_then(|v| v.get("type").and_then(Value::as_str).map(str::to_string));
        return Ok(matches!(kind.as_deref(), Some("title" | "session")));
    }
    Ok(false)
}

/// Reads a Pi session file into memory.
pub fn read_file(path: &Path) -> Result<SessionFile> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let reader = BufReader::with_capacity(1 << 20, file);

    let mut title = String::new();
    let mut title_source = None;
    let mut title_updated_at = String::new();
    let mut header: Option<Map<String, Value>> = None;
    let mut entries = Vec::new();

    for line in reader.lines() {
        let line = line.with_context(|| format!("read {}", path.display()))?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value): Result<Value, _> = serde_json::from_str(trimmed) else {
            continue;
        };
        let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
        if header.is_none() {
            if kind == "title" {
                title = value
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                title_source = value
                    .get("source")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                title_updated_at = value
                    .get("updatedAt")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                continue;
            }
            if kind == "session" {
                header = value.as_object().cloned();
                continue;
            }
            return Err(anyhow!("{} has no session header", path.display()));
        }
        entries.push(value);
    }

    Ok(SessionFile {
        title,
        title_source,
        title_updated_at,
        header: header.ok_or_else(|| anyhow!("{} has no session header", path.display()))?,
        entries,
    })
}

/// Writes a Pi session file, creating parent directories as needed.
pub fn write_file(path: &Path, file: &SessionFile) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create {}", parent.display()))?;
    }
    let out = File::create(path).with_context(|| format!("create {}", path.display()))?;
    let mut writer = BufWriter::with_capacity(1 << 20, out);
    writeln!(
        writer,
        "{}",
        title_slot(&file.title, file.title_source.as_deref(), &file.title_updated_at)
    )?;
    writeln!(writer, "{}", serde_json::to_string(&Value::Object(file.header.clone()))?)?;
    for entry in &file.entries {
        writeln!(writer, "{}", serde_json::to_string(entry)?)?;
    }
    writer.flush()?;
    Ok(())
}

/// Builds a Pi session header row.
pub fn make_header(id: &str, timestamp: &str, cwd: &str, extra: &[(&str, Value)]) -> Map<String, Value> {
    let mut header = Map::new();
    header.insert("type".into(), json!("session"));
    header.insert("version".into(), json!(CURRENT_VERSION));
    header.insert("id".into(), json!(id));
    header.insert("timestamp".into(), json!(timestamp));
    header.insert("cwd".into(), json!(cwd));
    for (key, value) in extra {
        header.insert((*key).to_string(), value.clone());
    }
    header
}

/// Lists every Pi session under `root`, newest header timestamp first.
///
/// Subagent sessions live one level deeper, in a directory named after their
/// parent session file, so the walk descends past the project bucket.
pub fn discover(root: &Path) -> Vec<SessionRef> {
    let mut out = Vec::new();
    for entry in WalkDir::new(root)
        .min_depth(2)
        .max_depth(4)
        .into_iter()
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let bucket = path
            .strip_prefix(root)
            .ok()
            .and_then(|rel| rel.components().next())
            .map(|first| first.as_os_str().to_string_lossy().to_string())
            .unwrap_or_default();
        if let Some(found) = inspect(path, &bucket) {
            out.push(found);
        }
    }
    out.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    out
}

/// Reads one Pi session file's listing metadata without holding its entries.
fn inspect(path: &Path, bucket: &str) -> Option<SessionRef> {
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let file = File::open(path).ok()?;
    let reader = BufReader::with_capacity(1 << 16, file);

    let mut title = String::new();
    let mut header: Option<Value> = None;
    let mut entries = 0usize;
    let mut messages = 0usize;
    let mut migrated = false;
    let mut prompts: Vec<u64> = Vec::new();
    let mut claude_sources: Vec<String> = Vec::new();
    for line in reader.lines() {
        let Ok(line) = line else { break };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if header.is_none() {
            let value: Value = serde_json::from_str(trimmed).ok()?;
            match value.get("type").and_then(Value::as_str) {
                Some("title") => {
                    title = value
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                }
                Some("session") => header = Some(value),
                _ => return None,
            }
            continue;
        }
        entries += 1;
        if trimmed.starts_with("{\"type\":\"message\"") {
            messages += 1;
            // Only user prompts feed duplicate detection; parsing stays off the
            // assistant and tool rows that make up the bulk of a session.
            if trimmed.contains("\"role\":\"user\"") {
                if let Some(prompt) = serde_json::from_str::<Value>(trimmed)
                    .ok()
                    .and_then(|value| user_prompt(&value))
                {
                    prompts.push(prompt);
                }
            }
        } else if !migrated && trimmed.contains(ORIGIN_TYPE) {
            migrated = true;
        } else if trimmed.contains(IMPORT_TYPE) {
            if let Some(record) = serde_json::from_str::<Value>(trimmed)
                .ok()
                .and_then(|value| import_record(&value))
            {
                migrated |= record.ours;
                if record.source == "claude" {
                    claude_sources.push(record.source_id);
                }
            }
        }
    }
    prompts.sort_unstable();
    prompts.dedup();

    let header = header?;
    Some(SessionRef {
        id: header
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        path: path.to_path_buf(),
        bucket: bucket.to_string(),
        cwd: header
            .get("cwd")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        timestamp: header
            .get("timestamp")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        title,
        size,
        entries,
        messages,
        migrated,
        prompts,
        claude_sources,
    })
}

/// Fingerprint of a `message` entry's text when it is a user prompt.
fn user_prompt(entry: &Value) -> Option<u64> {
    let message = entry.get("message")?;
    if message.get("role").and_then(Value::as_str) != Some("user") {
        return None;
    }
    dedup::fingerprint(&dedup::content_text(message.get("content")?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_slot_is_exactly_the_fixed_width() {
        let slot = title_slot("hello", None, "2026-10-06T17:06:45.489Z");
        assert_eq!(slot.len() + 1, TITLE_SLOT_BYTES);
        let parsed: Value = serde_json::from_str(&slot).unwrap();
        assert_eq!(parsed["title"], json!("hello"));
    }

    #[test]
    fn title_slot_carries_its_source_inside_the_budget() {
        let slot = title_slot("hello", Some("user"), "2026-10-06T17:06:45.489Z");
        assert_eq!(slot.len() + 1, TITLE_SLOT_BYTES);
        assert!(slot.starts_with(r#"{"type":"title","v":1,"title":"hello","source":"user","updatedAt""#));
    }

    #[test]
    fn title_slot_truncates_an_over_long_title() {
        let long = "x".repeat(1000);
        let slot = title_slot(&long, Some("auto"), "2026-10-06T17:06:45.489Z");
        assert_eq!(slot.len() + 1, TITLE_SLOT_BYTES);
        let parsed: Value = serde_json::from_str(&slot).unwrap();
        assert!(parsed["title"].as_str().unwrap().len() < 1000);
    }

    #[test]
    fn title_slot_keeps_escaped_characters_inside_the_budget() {
        let slot = title_slot("a\"b\\c\nd", None, "2026-10-06T17:06:45.489Z");
        assert_eq!(slot.len() + 1, TITLE_SLOT_BYTES);
        let parsed: Value = serde_json::from_str(&slot).unwrap();
        assert_eq!(parsed["title"], json!("a\"b\\c\nd"));
    }
}
