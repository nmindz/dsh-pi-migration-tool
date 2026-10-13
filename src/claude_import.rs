//! One-way import of Claude Code transcripts into Pi session files.
//!
//! # Mapping
//!
//! | Claude Code record | Pi entry |
//! |---|---|
//! | `user` prompt (text and images) | `message` role `user` |
//! | `user` `tool_result` block | `message` role `toolResult` |
//! | `user` with `isMeta` | `custom_message` `claude-code.meta`, hidden, still in context |
//! | `user` with `isCompactSummary` | `compaction`, sized by the `compact_boundary` before it |
//! | `assistant` records of one response | one `message` role `assistant`, preceded by `model_change` when the model changes |
//! | `queued_command` attachment | `message` role `user` |
//! | `custom-title`, `agent-name`, `ai-title`, `summary` | title slot and one `title_change` |
//! | subagent `agent-<id>.jsonl` | Pi subagent file beside the session |
//!
//! Everything else is Claude Code's runtime bookkeeping (hook output, context
//! attachments, file-history snapshots, mode and permission switches, cost
//! state) and is not carried.
//!
//! # Shape
//!
//! Claude Code writes every content block of a response as its own record and
//! starts running a tool before the response finishes, so the blocks of one
//! response arrive interleaved with tool results and injected meta prompts.
//! Pi needs one assistant message whose tool results follow it directly, so
//! continuation blocks merge into the open message and meta prompts and queued
//! prompts wait until the next turn starts.
//!
//! Claude's `parentUuid` tree also branches at every parallel tool call: each
//! result hangs off its own `tool_use` record. Following it literally would
//! drop sibling results from the active path. Entries therefore chain in file
//! order, and only a turn that starts from an earlier point of the
//! conversation, a rewind, opens a Pi branch there.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::claude::{self, Name, NameSource, Names, Origin};
use crate::convert::{iso, IdMinter};
use crate::paths;
use crate::pi;

/// `customType` of the hidden custom message holding a Claude `isMeta` prompt.
pub const META_TYPE: &str = "claude-code.meta";

/// Pi session id for a Claude Code session.
///
/// Derived rather than reused, so the imported copy never shares an id with
/// its source and an id prefix always names one session; deterministic, so a
/// repeated import lands on the same file.
pub fn pi_session_id(claude_key: &str) -> String {
    let digest = Sha256::digest(format!("claude-code:{claude_key}").as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    // RFC 9562 version 8 (custom), RFC 4122 variant.
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// Where an imported session lands and what it is called there.
#[derive(Debug, Clone)]
pub struct Target {
    /// Pi session id.
    pub id: String,
    /// Working directory.
    pub cwd: String,
    /// Session start in epoch milliseconds.
    pub started: i64,
}

impl Target {
    fn resolve(origin: &Origin, path: &Path, fallback_cwd: &str) -> Target {
        let started = origin.started.unwrap_or_else(|| {
            std::fs::metadata(path)
                .map(|m| claude::mtime_ms(&m))
                .unwrap_or(0)
        });
        Target {
            id: pi_session_id(&claude::session_id(path)),
            cwd: origin.cwd.clone().unwrap_or_else(|| fallback_cwd.to_string()),
            started,
        }
    }

    /// Reads only as much of the transcript as the target needs.
    pub fn of(path: &Path, fallback_cwd: &str) -> Result<Target> {
        Ok(Target::resolve(&claude::read_origin(path)?, path, fallback_cwd))
    }

    /// Pi session file this target names, under `root`.
    pub fn file(&self, root: &Path) -> PathBuf {
        root.join(paths::encode_pi_bucket(&self.cwd))
            .join(paths::pi_file_name(&iso(self.started), &self.id))
    }
}

/// Directory holding a Pi session's subagent files: the session file without `.jsonl`.
pub fn subagent_dir(main: &Path) -> PathBuf {
    main.with_extension("")
}

/// Counts surfaced by the CLI and the browser.
#[derive(Debug, Clone, Default)]
pub struct Report {
    /// Pi entries written, subagents included.
    pub entries: usize,
    /// Claude records carried into those entries.
    pub mapped: usize,
    /// Bookkeeping records left behind.
    pub skipped: usize,
    /// Rewinds that opened a Pi branch.
    pub branches: usize,
}

/// A converted session: the main file and its subagents.
pub struct Import {
    /// The session itself.
    pub main: pi::SessionFile,
    /// Subagent files, by file name inside [`subagent_dir`].
    pub subagents: Vec<(String, pi::SessionFile)>,
    /// What the conversion did.
    pub report: Report,
}

/// Converts one Claude Code session and every subagent transcript beside it.
///
/// `fallback_cwd` stands in for a transcript that never recorded a working
/// directory; discovery derives it from the project bucket.
pub fn import(path: &Path, fallback_cwd: &str) -> Result<Import> {
    let records = claude::read_records(path)?;
    if records.is_empty() && std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) > 0 {
        bail!("{} holds no readable record", path.display());
    }
    let claude_id = claude::session_id(path);
    let target = Target::resolve(&Origin::of(&records), path, fallback_cwd);
    let mut report = Report::default();

    let main = convert(
        &records,
        Source {
            pi_id: &target.id,
            claude_id: &claude_id,
            kind: "claude",
            path,
            cwd: &target.cwd,
            started: target.started,
            parent: None,
            name: None,
            subagent: false,
        },
        &mut report,
    );
    let mut subagents = Vec::new();
    for file in claude::subagent_files(path) {
        let records = match claude::read_records(&file) {
            Ok(records) if !records.is_empty() => records,
            _ => continue,
        };
        let stem = claude::session_id(&file);
        let agent_id = records
            .iter()
            .find_map(|r| r.get("agentId").and_then(Value::as_str))
            .map(str::to_string)
            .unwrap_or_else(|| stem.trim_start_matches("agent-").to_string());
        let origin = Origin::of(&records);
        let converted = convert(
            &records,
            Source {
                pi_id: &pi_session_id(&format!("{claude_id}/{agent_id}")),
                claude_id: &agent_id,
                kind: "claude-subagent",
                path: &file,
                cwd: origin.cwd.as_deref().unwrap_or(&target.cwd),
                started: origin.started.unwrap_or(target.started),
                parent: Some(&target.id),
                name: claude::subagent_description(&file).map(|text| Name {
                    text,
                    source: NameSource::Ai,
                }),
                subagent: true,
            },
            &mut report,
        );
        subagents.push((format!("{stem}.jsonl"), converted));
    }
    Ok(Import {
        main,
        subagents,
        report,
    })
}

/// Identity of the transcript being converted.
struct Source<'a> {
    pi_id: &'a str,
    claude_id: &'a str,
    kind: &'a str,
    path: &'a Path,
    cwd: &'a str,
    started: i64,
    parent: Option<&'a str>,
    /// Name that outranks the transcript's own name records.
    name: Option<Name>,
    subagent: bool,
}

/// The assistant message still accepting blocks from later records.
struct Open {
    /// Claude response id shared by every record of the response.
    response: String,
    /// Index of the Pi entry.
    index: usize,
    /// Last explicit stop reason any record of the response carried.
    stop: Option<String>,
    /// True when an API error produced the response.
    failed: bool,
}

/// An entry held back until the open response's tool results are in.
struct Deferred {
    disc: String,
    time: i64,
    fields: Map<String, Value>,
}

/// Accumulates Pi entries, tracking turns so rewinds can branch.
///
/// A turn is the run of entries a user prompt, assistant response, or
/// compaction starts. Every converted Claude record belongs to one turn, and a
/// later record that names it as parent continues from that turn's last entry.
struct Builder {
    minter: IdMinter,
    entries: Vec<Value>,
    tail: Value,
    turns: Vec<Value>,
    turn: usize,
    turn_of: HashMap<String, usize>,
    entry_of: HashMap<String, String>,
    open: Option<Open>,
    deferred: Vec<Deferred>,
    branches: usize,
}

impl Builder {
    fn new(session: &str) -> Self {
        Self {
            minter: IdMinter::new(session),
            entries: Vec::new(),
            tail: Value::Null,
            turns: vec![Value::Null],
            turn: 0,
            turn_of: HashMap::new(),
            entry_of: HashMap::new(),
            open: None,
            deferred: Vec::new(),
            branches: 0,
        }
    }

    /// Chains one entry onto the current tail and returns its id.
    fn append(&mut self, disc: &str, time: i64, mut fields: Map<String, Value>) -> String {
        let id = self.minter.mint(disc);
        fields.insert("id".into(), json!(id));
        fields.insert("parentId".into(), self.tail.clone());
        fields.insert("timestamp".into(), json!(iso(time)));
        self.entries.push(Value::Object(fields));
        self.tail = json!(id);
        self.turns[self.turn] = self.tail.clone();
        id
    }

    /// Records that a Claude record now lives in the current turn.
    fn claim(&mut self, uuid: Option<&str>, entry: Option<&str>) {
        let Some(uuid) = uuid else { return };
        self.turn_of.insert(uuid.to_string(), self.turn);
        if let Some(entry) = entry {
            self.entry_of.entry(uuid.to_string()).or_insert_with(|| entry.to_string());
        }
    }

    /// Emits every held-back entry, in arrival order, at the end of the current turn.
    fn flush(&mut self) {
        for held in std::mem::take(&mut self.deferred) {
            self.append(&held.disc, held.time, held.fields);
        }
    }

    /// Appends now, or holds back while a response may still gain tool calls.
    fn append_or_defer(&mut self, disc: String, time: i64, fields: Map<String, Value>) {
        if self.open.is_some() {
            self.deferred.push(Deferred { disc, time, fields });
        } else {
            self.append(&disc, time, fields);
        }
    }

    /// The turn of the nearest converted record on a parent chain, walking past
    /// records that were not converted.
    fn turn_on_chain<'a>(&self, mut cursor: Option<&'a str>, parents: &HashMap<&'a str, Option<&'a str>>) -> Option<usize> {
        let mut seen = HashSet::new();
        while let Some(uuid) = cursor {
            if let Some(turn) = self.turn_of.get(uuid) {
                return Some(*turn);
            }
            if !seen.insert(uuid) {
                break;
            }
            cursor = parents.get(uuid).copied().flatten();
        }
        None
    }

    /// The entry a record continues from: the last entry of its turn, or the
    /// tail when its chain reaches no converted record. Compaction boundaries
    /// in subagent transcripts point at records the compaction rewrote away,
    /// and a fresh root mid-file is never a rewind.
    fn resolve<'a>(&self, cursor: Option<&'a str>, parents: &HashMap<&'a str, Option<&'a str>>) -> Value {
        self.turn_on_chain(cursor, parents)
            .map(|turn| self.turns[turn].clone())
            .unwrap_or_else(|| self.tail.clone())
    }

    /// Moves back to the turn a tool result belongs to.
    ///
    /// A prompt typed while a tool runs starts a turn before the tool's result
    /// is written; Claude Code hangs both off the tool call and continues from
    /// the result, leaving the prompt on a dead branch. The result returns to
    /// its call's turn so the call stays answered on the path that continues.
    fn rejoin<'a>(&mut self, cursor: Option<&'a str>, parents: &HashMap<&'a str, Option<&'a str>>) {
        let Some(turn) = self.turn_on_chain(cursor, parents) else {
            return;
        };
        if turn != self.turn {
            self.branches += 1;
            self.turn = turn;
            self.tail = self.turns[turn].clone();
        }
    }

    /// Starts a turn. Held-back entries close the previous turn first; then the
    /// turn continues from the record `cursor` names, or from the tail when
    /// `cursor` is `None`.
    fn start_turn<'a>(&mut self, cursor: Option<Option<&'a str>>, parents: &HashMap<&'a str, Option<&'a str>>) {
        self.flush();
        let parent = match cursor {
            Some(cursor) => self.resolve(cursor, parents),
            None => self.tail.clone(),
        };
        if parent != self.tail {
            self.branches += 1;
        }
        self.tail = parent.clone();
        self.turns.push(parent);
        self.turn = self.turns.len() - 1;
        self.open = None;
    }
}

/// Converts one transcript's records into a Pi session file.
fn convert(records: &[Value], source: Source, report: &mut Report) -> pi::SessionFile {
    let parents: HashMap<&str, Option<&str>> = records
        .iter()
        .filter_map(|r| {
            let uuid = r.get("uuid")?.as_str()?;
            let parent = r
                .get("parentUuid")
                .and_then(Value::as_str)
                .or_else(|| r.get("logicalParentUuid").and_then(Value::as_str));
            Some((uuid, parent))
        })
        .collect();
    let uuids = claude::record_uuids(records);

    let mut builder = Builder::new(source.pi_id);
    let mut names = Names::default();
    let mut tool_names: HashMap<String, String> = HashMap::new();
    let mut last_model: Option<String> = None;
    let mut boundary: Option<Value> = None;
    let mut compactions: Vec<(usize, Option<String>)> = Vec::new();
    let mut time = source.started;
    let mut updated = source.started;

    for (line, record) in records.iter().enumerate() {
        let kind = record.get("type").and_then(Value::as_str).unwrap_or("");
        let uuid = record.get("uuid").and_then(Value::as_str);
        let parent = record.get("parentUuid").and_then(Value::as_str);
        if let Some(ms) = record.get("timestamp").and_then(claude::timestamp_ms) {
            time = ms;
            updated = updated.max(ms);
        }
        let disc = uuid.map(str::to_string).unwrap_or_else(|| format!("line-{line}"));

        if let Some(field) = Names::field(kind) {
            if let Some(text) = record.get(field).and_then(Value::as_str) {
                names.observe(kind, text);
            }
            report.mapped += 1;
            continue;
        }
        if matches!(kind, "user" | "assistant") && !source.subagent && claude::is_sidechain(record) {
            report.skipped += 1;
            continue;
        }

        let carried = match kind {
            "user" => {
                let message = record.get("message").unwrap_or(&Value::Null);
                let content = message.get("content").unwrap_or(&Value::Null);
                let flag = |name: &str| record.get(name).and_then(Value::as_bool) == Some(true);
                if flag("isCompactSummary") {
                    builder.start_turn(Some(parent), &parents);
                    let metadata = boundary.take().unwrap_or(Value::Null);
                    let number = |key: &str| metadata.get(key).and_then(Value::as_i64).unwrap_or(0);
                    let mut fields = Map::new();
                    fields.insert("type".into(), json!("compaction"));
                    fields.insert("summary".into(), json!(crate::dedup::content_text(content)));
                    fields.insert("firstKeptEntryId".into(), json!(""));
                    fields.insert("tokensBefore".into(), json!(number("preTokens")));
                    if metadata.get("postTokens").is_some() {
                        fields.insert("tokensAfter".into(), json!(number("postTokens")));
                    }
                    fields.insert("fromExtension".into(), json!(false));
                    let id = builder.append(&disc, time, fields);
                    builder.claim(uuid, Some(&id));
                    let kept = metadata
                        .get("preservedSegment")
                        .and_then(|s| s.get("headUuid"))
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    compactions.push((builder.entries.len() - 1, kept));
                    true
                } else if flag("isMeta") {
                    match user_content(content) {
                        Some(content) => {
                            let mut fields = Map::new();
                            fields.insert("type".into(), json!("custom_message"));
                            fields.insert("customType".into(), json!(META_TYPE));
                            fields.insert("content".into(), content);
                            fields.insert("display".into(), json!(false));
                            fields.insert("attribution".into(), json!("agent"));
                            builder.claim(uuid, None);
                            builder.append_or_defer(disc, time, fields);
                            true
                        }
                        None => false,
                    }
                } else {
                    let (results, runs) = split_user(content);
                    let prompted = !runs.is_empty();
                    if !results.is_empty() {
                        builder.rejoin(parent, &parents);
                    }
                    for (index, block) in results.iter().enumerate() {
                        let call = block.get("tool_use_id").and_then(Value::as_str).unwrap_or("");
                        let mut msg = Map::new();
                        msg.insert("role".into(), json!("toolResult"));
                        msg.insert("toolCallId".into(), json!(call));
                        msg.insert(
                            "toolName".into(),
                            json!(tool_names.get(call).map(String::as_str).unwrap_or("unknown")),
                        );
                        msg.insert("content".into(), tool_result_content(block.get("content")));
                        msg.insert(
                            "isError".into(),
                            json!(block.get("is_error").and_then(Value::as_bool) == Some(true)),
                        );
                        msg.insert("timestamp".into(), json!(time));
                        let id = builder.append(&format!("{disc}:result:{index}"), time, message_entry(msg));
                        builder.claim(uuid, Some(&id));
                    }
                    if !runs.is_empty() {
                        // Text after tool results answers the same turn; only a
                        // record of nothing but prompt can start from elsewhere.
                        builder.start_turn(results.is_empty().then_some(parent), &parents);
                        for (index, run) in runs.into_iter().enumerate() {
                            let id = builder.append(
                                &format!("{disc}:prompt:{index}"),
                                time,
                                message_entry(user_message(run, time)),
                            );
                            builder.claim(uuid, Some(&id));
                        }
                    }
                    !results.is_empty() || prompted
                }
            }
            "assistant" => {
                let message = record.get("message").unwrap_or(&Value::Null);
                let response = message.get("id").and_then(Value::as_str).unwrap_or("");
                let blocks = assistant_blocks(message.get("content"), &mut tool_names);
                let error = record.get("error").and_then(Value::as_str);
                let failed = record.get("isApiErrorMessage").and_then(Value::as_bool) == Some(true);
                let stop = message.get("stop_reason").and_then(Value::as_str);
                let continues = !response.is_empty()
                    && builder.open.as_ref().is_some_and(|open| open.response == response);
                if continues {
                    let open = builder.open.as_mut().expect("checked above");
                    if let Some(stop) = stop {
                        open.stop = Some(stop.to_string());
                    }
                    open.failed |= failed;
                    let index = open.index;
                    let (stop, failed) = (open.stop.clone(), open.failed);
                    if let Some(Value::Object(entry)) = builder.entries.get_mut(index) {
                        let id = entry.get("id").and_then(Value::as_str).map(str::to_string);
                        if let Some(Value::Object(msg)) = entry.get_mut("message") {
                            if let Some(Value::Array(content)) = msg.get_mut("content") {
                                content.extend(blocks);
                            }
                            if let Some(usage) = message.get("usage") {
                                msg.insert("usage".into(), pi_usage(usage));
                            }
                            if let Some(error) = error {
                                msg.insert("errorMessage".into(), json!(error));
                            }
                            let has_calls = has_tool_calls(msg);
                            msg.insert("stopReason".into(), json!(stop_reason(stop.as_deref(), failed, has_calls)));
                        }
                        builder.claim(uuid, id.as_deref());
                    }
                    true
                } else if blocks.is_empty() && error.is_none() {
                    false
                } else {
                    builder.start_turn(Some(parent), &parents);
                    let model = message
                        .get("model")
                        .and_then(Value::as_str)
                        .filter(|m| !m.is_empty())
                        .unwrap_or("unknown");
                    if model != claude::SYNTHETIC_MODEL && model != "unknown" && last_model.as_deref() != Some(model) {
                        let mut fields = Map::new();
                        fields.insert("type".into(), json!("model_change"));
                        fields.insert("model".into(), json!(format!("anthropic/{model}")));
                        builder.append(&format!("{disc}:model"), time, fields);
                        last_model = Some(model.to_string());
                    }
                    let mut msg = Map::new();
                    msg.insert("role".into(), json!("assistant"));
                    msg.insert("content".into(), Value::Array(blocks));
                    msg.insert("api".into(), json!("anthropic-messages"));
                    msg.insert("provider".into(), json!("anthropic"));
                    msg.insert("model".into(), json!(model));
                    msg.insert(
                        "usage".into(),
                        pi_usage(message.get("usage").unwrap_or(&Value::Null)),
                    );
                    let has_calls = has_tool_calls(&msg);
                    msg.insert("stopReason".into(), json!(stop_reason(stop, failed, has_calls)));
                    msg.insert("timestamp".into(), json!(time));
                    if !response.is_empty() {
                        msg.insert("responseId".into(), json!(response));
                    }
                    if let Some(error) = error {
                        msg.insert("errorMessage".into(), json!(error));
                    }
                    if let Some(status) = record.get("apiErrorStatus").and_then(Value::as_i64) {
                        msg.insert("errorStatus".into(), json!(status));
                    }
                    let id = builder.append(&disc, time, message_entry(msg));
                    builder.claim(uuid, Some(&id));
                    builder.open = Some(Open {
                        response: response.to_string(),
                        index: builder.entries.len() - 1,
                        stop: stop.map(str::to_string),
                        failed,
                    });
                    true
                }
            }
            "attachment" => match claude::queued_prompt(record, &uuids) {
                Some(prompt) => {
                    let at = record
                        .get("attachment")
                        .and_then(|a| a.get("timestamp"))
                        .and_then(claude::timestamp_ms)
                        .unwrap_or(time);
                    let content = user_content(prompt).unwrap_or_else(|| json!(""));
                    builder.claim(uuid, None);
                    builder.append_or_defer(disc, at, message_entry(user_message(content, at)));
                    true
                }
                None => false,
            },
            "system" if record.get("subtype").and_then(Value::as_str) == Some("compact_boundary") => {
                boundary = record.get("compactMetadata").cloned();
                true
            }
            _ => false,
        };
        if carried {
            report.mapped += 1;
        } else {
            report.skipped += 1;
        }
    }
    builder.flush();

    // A compaction keeps the segment Claude preserved, or nothing before it.
    for (position, kept) in compactions {
        let first_kept = kept
            .and_then(|uuid| builder.entry_of.get(&uuid).cloned())
            .or_else(|| {
                builder
                    .entries
                    .get(position + 1)
                    .and_then(|e| e.get("id"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_default();
        if let Some(Value::Object(entry)) = builder.entries.get_mut(position) {
            entry.insert("firstKeptEntryId".into(), json!(first_kept));
        }
    }

    let name = source.name.or_else(|| names.best());
    let (title, title_source) = match &name {
        Some(name) => (name.text.clone(), Some(name.source.title_source().to_string())),
        None => (fallback_title(&builder.entries), None),
    };
    if let Some(name) = &name {
        let mut fields = Map::new();
        fields.insert("type".into(), json!("title_change"));
        fields.insert("title".into(), json!(name.text));
        fields.insert("source".into(), json!(name.source.title_source()));
        builder.append("title", updated, fields);
    }
    let mut record = Map::new();
    record.insert("type".into(), json!("custom"));
    record.insert("customType".into(), json!(pi::IMPORT_TYPE));
    record.insert(
        "data".into(),
        json!({
            "source": source.kind,
            "sourceId": source.claude_id,
            "sourcePath": source.path.to_string_lossy(),
            "sourceCwd": source.cwd,
            "tool": env!("CARGO_PKG_NAME"),
            "toolVersion": env!("CARGO_PKG_VERSION"),
        }),
    );
    builder.append("import", updated, record);

    let mut extra: Vec<(&str, Value)> = Vec::new();
    if let Some(parent) = source.parent {
        extra.push(("parentSession", json!(parent)));
    }
    report.entries += builder.entries.len();
    report.branches += builder.branches;
    pi::SessionFile {
        title,
        title_source,
        title_updated_at: iso(updated),
        header: pi::make_header(source.pi_id, &iso(source.started), source.cwd, &extra),
        entries: builder.entries,
    }
}

/// Wraps a message body in a `message` entry.
fn message_entry(message: Map<String, Value>) -> Map<String, Value> {
    let mut fields = Map::new();
    fields.insert("type".into(), json!("message"));
    fields.insert("message".into(), Value::Object(message));
    fields
}

/// A user message body.
fn user_message(content: Value, time: i64) -> Map<String, Value> {
    let mut msg = Map::new();
    msg.insert("role".into(), json!("user"));
    msg.insert("content".into(), content);
    msg.insert("attribution".into(), json!("user"));
    msg.insert("timestamp".into(), json!(time));
    msg
}

/// One Claude content block as Pi user-side content: text and base64 images
/// keep their shape, anything else becomes a text marker naming its type.
fn pi_part(block: &Value) -> Option<Value> {
    let kind = block.get("type").and_then(Value::as_str)?;
    Some(match kind {
        "text" => json!({
            "type": "text",
            "text": block.get("text").and_then(Value::as_str).unwrap_or(""),
        }),
        "image" => {
            let source = block.get("source");
            let data = source.and_then(|s| s.get("data")).and_then(Value::as_str);
            let mime = source.and_then(|s| s.get("media_type")).and_then(Value::as_str);
            match (data, mime) {
                (Some(data), Some(mime)) => json!({"type": "image", "data": data, "mimeType": mime}),
                _ => json!({"type": "text", "text": "[image]"}),
            }
        }
        other => json!({"type": "text", "text": format!("[{other}]")}),
    })
}

/// User-side content: a string stays a string, an array keeps every block Pi
/// can hold. `None` when nothing remains.
fn user_content(content: &Value) -> Option<Value> {
    match content {
        Value::String(text) if !text.is_empty() => Some(json!(text)),
        Value::Array(items) => {
            let parts: Vec<Value> = items.iter().filter_map(pi_part).collect();
            (!parts.is_empty()).then_some(Value::Array(parts))
        }
        _ => None,
    }
}

/// Splits a user record's content into its tool results and the prompt runs
/// between them; tool results come first, as the provider requires.
fn split_user(content: &Value) -> (Vec<&Value>, Vec<Value>) {
    let items = match content {
        Value::Array(items) => items,
        other => return (Vec::new(), user_content(other).into_iter().collect()),
    };
    let mut results = Vec::new();
    let mut runs = Vec::new();
    let mut run: Vec<Value> = Vec::new();
    for block in items {
        if block.get("type").and_then(Value::as_str) == Some("tool_result") {
            if !run.is_empty() {
                runs.push(Value::Array(std::mem::take(&mut run)));
            }
            results.push(block);
        } else if let Some(part) = pi_part(block) {
            run.push(part);
        }
    }
    if !run.is_empty() {
        runs.push(Value::Array(run));
    }
    (results, runs)
}

/// Tool output as Pi content blocks.
fn tool_result_content(content: Option<&Value>) -> Value {
    match content {
        Some(Value::String(text)) if text.is_empty() => json!([]),
        Some(Value::String(text)) => json!([{"type": "text", "text": text}]),
        Some(Value::Array(items)) => Value::Array(items.iter().filter_map(pi_part).collect()),
        _ => json!([]),
    }
}

/// Claude assistant blocks as Pi assistant content, remembering tool names so
/// their results can be labelled.
fn assistant_blocks(content: Option<&Value>, tool_names: &mut HashMap<String, String>) -> Vec<Value> {
    let Some(Value::Array(items)) = content else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|block| match block.get("type").and_then(Value::as_str)? {
            "text" => Some(json!({
                "type": "text",
                "text": block.get("text").and_then(Value::as_str).unwrap_or(""),
            })),
            "thinking" => {
                let mut out = Map::new();
                out.insert("type".into(), json!("thinking"));
                out.insert(
                    "thinking".into(),
                    json!(block.get("thinking").and_then(Value::as_str).unwrap_or("")),
                );
                if let Some(signature) = block.get("signature").filter(|s| s.is_string()) {
                    out.insert("thinkingSignature".into(), signature.clone());
                }
                Some(Value::Object(out))
            }
            "redacted_thinking" => Some(json!({
                "type": "redactedThinking",
                "data": block.get("data").and_then(Value::as_str).unwrap_or(""),
            })),
            "tool_use" => {
                let id = block.get("id").cloned().unwrap_or(Value::Null);
                let name = block.get("name").cloned().unwrap_or(Value::Null);
                if let (Some(id), Some(name)) = (id.as_str(), name.as_str()) {
                    tool_names.insert(id.to_string(), name.to_string());
                }
                let arguments = block
                    .get("input")
                    .filter(|input| input.is_object())
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                Some(json!({"type": "toolCall", "id": id, "name": name, "arguments": arguments}))
            }
            _ => None,
        })
        .collect()
}

/// True when an assistant message body holds a tool call.
fn has_tool_calls(msg: &Map<String, Value>) -> bool {
    msg.get("content")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items
                .iter()
                .any(|b| b.get("type").and_then(Value::as_str) == Some("toolCall"))
        })
}

/// Pi stop reason for a Claude response.
fn stop_reason(stop: Option<&str>, failed: bool, has_calls: bool) -> &'static str {
    if failed {
        return "error";
    }
    match stop {
        Some("tool_use") => "toolUse",
        Some("max_tokens") => "length",
        Some("end_turn" | "stop_sequence" | "pause_turn") => "stop",
        Some(_) => "error",
        // Intermediate records of a streamed response carry no stop reason yet.
        None if has_calls => "toolUse",
        None => "stop",
    }
}

/// Claude usage as Pi usage; Claude records no cost.
fn pi_usage(usage: &Value) -> Value {
    let get = |key: &str| usage.get(key).and_then(Value::as_i64).unwrap_or(0);
    let input = get("input_tokens");
    let output = get("output_tokens");
    let cache_read = get("cache_read_input_tokens");
    let cache_write = get("cache_creation_input_tokens");
    json!({
        "input": input,
        "output": output,
        "cacheRead": cache_read,
        "cacheWrite": cache_write,
        "totalTokens": input + output + cache_read + cache_write,
        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0 },
    })
}

/// Title for an unnamed session: its first typed prompt, as Pi's own fallback.
fn fallback_title(entries: &[Value]) -> String {
    for entry in entries {
        let Some(message) = entry.get("message") else { continue };
        if message.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        let text = crate::dedup::content_text(message.get("content").unwrap_or(&Value::Null));
        let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if collapsed.is_empty() || collapsed.starts_with('<') {
            continue;
        }
        let mut title: String = collapsed.chars().take(60).collect();
        if collapsed.chars().count() > 60 {
            title.push('…');
        }
        return title;
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(uuid: &str, parent: Option<&str>, content: Value) -> Value {
        json!({"type": "user", "uuid": uuid, "parentUuid": parent, "cwd": "/w",
               "timestamp": "2026-09-01T10:00:00.000Z", "message": {"role": "user", "content": content}})
    }

    fn assistant(uuid: &str, parent: &str, response: &str, block: Value, stop: Option<&str>) -> Value {
        json!({"type": "assistant", "uuid": uuid, "parentUuid": parent,
               "timestamp": "2026-09-01T10:00:01.000Z",
               "message": {"id": response, "model": "claude-x", "content": [block], "stop_reason": stop}})
    }

    fn call(id: &str) -> Value {
        json!({"type": "tool_use", "id": id, "name": "Bash", "input": {"command": "ls"}})
    }

    fn result(uuid: &str, parent: &str, call: &str) -> Value {
        user(uuid, Some(parent), json!([{"type": "tool_result", "tool_use_id": call, "content": "out"}]))
    }

    fn run(records: &[Value]) -> pi::SessionFile {
        let source = Source {
            pi_id: "pi-id",
            claude_id: "claude-id",
            kind: "claude",
            path: Path::new("/claude/claude-id.jsonl"),
            cwd: "/w",
            started: 0,
            parent: None,
            name: None,
            subagent: false,
        };
        convert(records, source, &mut Report::default())
    }

    /// Message entries on the path from the last entry back to the root, root first.
    fn active(file: &pi::SessionFile) -> Vec<Value> {
        crate::convert::leaf_chain(&file.entries)
            .into_iter()
            .filter(|e| e.get("type").and_then(Value::as_str) == Some("message"))
            .collect()
    }

    fn roles(path: &[Value]) -> Vec<String> {
        path.iter()
            .map(|e| e["message"]["role"].as_str().unwrap_or("").to_string())
            .collect()
    }

    #[test]
    fn a_streamed_response_with_parallel_calls_is_one_message_followed_by_every_result() {
        // Claude writes one record per block; each result hangs off its own call record.
        let file = run(&[
            user("u1", None, json!("list both directories please")),
            assistant("a1", "u1", "r1", json!({"type": "thinking", "thinking": "hm", "signature": "sig"}), None),
            assistant("a2", "a1", "r1", call("t1"), None),
            assistant("a3", "a2", "r1", call("t2"), Some("tool_use")),
            result("u2", "a2", "t1"),
            result("u3", "a3", "t2"),
            assistant("a4", "u3", "r2", json!({"type": "text", "text": "done"}), Some("end_turn")),
        ]);
        let path = active(&file);
        assert_eq!(roles(&path), ["user", "assistant", "toolResult", "toolResult", "assistant"]);
        let first = &path[1]["message"];
        assert_eq!(first["content"].as_array().unwrap().len(), 3);
        assert_eq!(first["content"][0]["thinkingSignature"], json!("sig"));
        assert_eq!(first["stopReason"], json!("toolUse"));
        assert_eq!(path[2]["message"]["toolName"], json!("Bash"));
        assert_eq!(path[3]["message"]["toolCallId"], json!("t2"));
    }

    #[test]
    fn a_prompt_typed_while_a_tool_runs_stays_off_the_path_that_answers_the_call() {
        let file = run(&[
            user("u1", None, json!("run the build for me")),
            assistant("a1", "u1", "r1", call("t1"), Some("tool_use")),
            user("p1", Some("a1"), json!("also bump the version")),
            result("u2", "a1", "t1"),
            user("u3", Some("u2"), json!([{"type": "text", "text": "[Request interrupted by user for tool use]"}])),
            assistant("a2", "u3", "r2", json!({"type": "text", "text": "ok"}), Some("end_turn")),
        ]);
        let path = active(&file);
        assert_eq!(roles(&path), ["user", "assistant", "toolResult", "user", "assistant"]);
        let on_path: HashSet<&str> = path.iter().map(|e| e["id"].as_str().unwrap()).collect();
        let typed = file
            .entries
            .iter()
            .find(|e| e["message"]["content"] == json!("also bump the version"))
            .expect("the typed prompt is kept");
        assert!(!on_path.contains(typed["id"].as_str().unwrap()));
    }

    #[test]
    fn a_rewind_branches_from_the_turn_the_new_prompt_names() {
        let file = run(&[
            user("u1", None, json!("first question about css")),
            assistant("a1", "u1", "r1", json!({"type": "text", "text": "one"}), Some("end_turn")),
            user("u2", Some("a1"), json!("second question, abandoned")),
            assistant("a2", "u2", "r2", json!({"type": "text", "text": "two"}), Some("end_turn")),
            user("u3", Some("a1"), json!("second question, rewritten")),
            assistant("a3", "u3", "r3", json!({"type": "text", "text": "three"}), Some("end_turn")),
        ]);
        let path = active(&file);
        assert_eq!(path.len(), 4);
        assert_eq!(path[2]["message"]["content"], json!("second question, rewritten"));
    }

    #[test]
    fn meta_and_queued_prompts_wait_for_the_open_response_to_finish() {
        let queued = json!({"type": "attachment", "uuid": "q1", "parentUuid": "a1",
            "attachment": {"type": "queued_command", "prompt": "and then deploy it", "commandMode": "prompt"}});
        let replayed = json!({"type": "attachment", "uuid": "q2", "parentUuid": "a1",
            "attachment": {"type": "queued_command", "prompt": "already a record", "source_uuid": "u1"}});
        let mut meta = user("m1", Some("a1"), json!("Stop hook feedback: keep going"));
        meta["isMeta"] = json!(true);
        let file = run(&[
            user("u1", None, json!("build the project now")),
            assistant("a1", "u1", "r1", call("t1"), None),
            queued,
            replayed,
            meta,
            result("u2", "a1", "t1"),
            assistant("a2", "u2", "r2", json!({"type": "text", "text": "built"}), Some("end_turn")),
        ]);
        let path = crate::convert::leaf_chain(&file.entries);
        let kinds: Vec<String> = path
            .iter()
            .filter(|e| matches!(e["type"].as_str(), Some("message" | "custom_message")))
            .map(|e| match e["type"].as_str() {
                Some("custom_message") => format!("meta:{}", e["display"]),
                _ => e["message"]["role"].as_str().unwrap().to_string(),
            })
            .collect();
        assert_eq!(kinds, ["user", "assistant", "toolResult", "user", "meta:false", "assistant"]);
        assert!(!file.entries.iter().any(|e| e["message"]["content"] == json!("already a record")));
    }

    #[test]
    fn a_compaction_whose_boundary_points_nowhere_continues_the_conversation() {
        let boundary = json!({"type": "system", "subtype": "compact_boundary", "uuid": "b1",
            "parentUuid": null, "logicalParentUuid": "rewritten-away",
            "compactMetadata": {"preTokens": 900, "postTokens": 40, "preservedSegment": {"headUuid": "a1"}}});
        let mut summary = user("s1", Some("b1"), json!("Summary: built the thing"));
        summary["isCompactSummary"] = json!(true);
        let file = run(&[
            user("u1", None, json!("build the project now")),
            assistant("a1", "u1", "r1", json!({"type": "text", "text": "built"}), Some("end_turn")),
            boundary,
            summary,
            user("u2", Some("s1"), json!("now test the project")),
        ]);
        let path = crate::convert::leaf_chain(&file.entries);
        let compaction = path
            .iter()
            .find(|e| e["type"] == json!("compaction"))
            .expect("the compaction is on the active path");
        assert_eq!(compaction["tokensBefore"], json!(900));
        let kept = path.iter().find(|e| e["message"]["role"] == json!("assistant")).unwrap();
        assert_eq!(compaction["firstKeptEntryId"], kept["id"]);
        let last = path.iter().rev().find(|e| e["type"] == json!("message")).unwrap();
        assert_eq!(last["message"]["content"], json!("now test the project"));
    }

    #[test]
    fn a_user_rename_titles_the_import_and_the_import_names_its_source() {
        let file = run(&[
            json!({"type": "ai-title", "aiTitle": "Generated"}),
            user("u1", None, json!("build the project now")),
            json!({"type": "custom-title", "customTitle": "Mine"}),
            json!({"type": "ai-title", "aiTitle": "Generated later"}),
        ]);
        assert_eq!(file.title, "Mine");
        assert_eq!(file.title_source.as_deref(), Some("user"));
        let record = file.entries.iter().find_map(pi::import_record).expect("import record");
        assert_eq!(record.source, "claude");
        assert_eq!(record.source_id, "claude-id");
        assert!(record.ours);
    }

    #[test]
    fn an_imported_session_is_recognized_on_the_next_scan() {
        let root = std::env::temp_dir().join(format!("pi-transfuse-claude-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let file = run(&[user("u1", None, json!("build the project now"))]);
        pi::write_file(&root.join("-w").join("s.jsonl"), &file).unwrap();
        let index = crate::dedup::Index::new(&pi::discover(&root));
        let prompts: Vec<u64> = crate::dedup::fingerprint("build the project now").into_iter().collect();
        assert!(matches!(index.judge("claude-id", &prompts), crate::dedup::Verdict::Imported(_)));
        assert!(matches!(index.judge("other-id", &prompts), crate::dedup::Verdict::Covered(_)));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn commands_and_unanswered_prompts_are_not_a_conversation() {
        let command = json!("<command-name>/model</command-name>");
        let mut failure = assistant("a1", "u2", "r1", json!({"type": "text", "text": "API Error: 401"}), Some("stop_sequence"));
        failure["message"]["model"] = json!(claude::SYNTHETIC_MODEL);
        failure["isApiErrorMessage"] = json!(true);
        let silent = run(&[user("u1", None, command), user("u2", Some("u1"), json!("Hello! Who are you?")), failure]);
        assert!(silent.message_count() > 0);
        assert!(!silent.has_reply());

        let answered = run(&[
            user("u1", None, json!("Hello! Who are you?")),
            assistant("a1", "u1", "r1", json!({"type": "text", "text": "Claude."}), Some("end_turn")),
        ]);
        assert!(answered.has_reply());
    }

    #[test]
    fn session_ids_are_stable_and_never_the_source_id() {
        let id = pi_session_id("c77f2061-d798-467e-bd38-89dbbac784cc");
        assert_eq!(id, pi_session_id("c77f2061-d798-467e-bd38-89dbbac784cc"));
        assert_ne!(id, "c77f2061-d798-467e-bd38-89dbbac784cc");
        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "8");
    }
}
