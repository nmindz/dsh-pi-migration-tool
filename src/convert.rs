//! Bidirectional conversion between DSH session logs and Pi session transcripts.
//!
//! # Mapping
//!
//! | DSH event | Pi entry |
//! |---|---|
//! | `session` header | `session` header + title slot |
//! | `session/title` | `title_change` and the title slot |
//! | `model/selection` | `model_change` |
//! | `system/message` (first) | `session_init` |
//! | `developer/message` | `message` role `developer` |
//! | `user/message` source `user` | `message` role `user` |
//! | `user/message` other source | `custom_message` |
//! | `assistant/message` | `message` role `assistant` |
//! | `tool/call` | `custom` `tool_execution_start` |
//! | `tool/result` | `message` role `toolResult` |
//! | `compaction/summary` | `compaction` |
//! | everything else | `custom` `ai.deepseek.dsh.<type>` carrying the row verbatim |
//!
//! # Round-trip fidelity
//!
//! Every mapped entry carries a `dsh` field holding the source `seq`, event type,
//! surface intent, and the source payload with exactly those fields removed that
//! the Pi entry already materializes (message content, tool-result metadata,
//! compaction summary, title). Nothing is stored twice, and converting back
//! rebuilds the original rows rather than an approximation.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use chrono::{DateTime, SecondsFormat, TimeZone, Utc};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::dsh;
use crate::pi;

/// Prefix of every Pi `customType` this tool owns.
pub const NAMESPACE: &str = "ai.deepseek.dsh.";

/// `customType` of the entry that preserves the original DSH session header.
pub const ORIGIN_TYPE: &str = "ai.deepseek.dsh.origin";

/// Epoch milliseconds rendered as the ISO-8601 form Pi persists.
pub fn iso(ms: i64) -> String {
    Utc.timestamp_millis_opt(ms)
        .single()
        .unwrap_or_else(Utc::now)
        .to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// ISO-8601 timestamp parsed back to epoch milliseconds.
pub fn from_iso(text: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|t| t.timestamp_millis())
}

/// Assigns the 8-hex entry ids Pi uses, derived from the source so repeated
/// conversions of one session produce byte-identical output.
struct IdMinter {
    session: String,
    used: HashSet<String>,
}

impl IdMinter {
    fn new(session: &str) -> Self {
        Self {
            session: session.to_string(),
            used: HashSet::new(),
        }
    }

    fn mint(&mut self, discriminator: &str) -> String {
        for nonce in 0u32.. {
            let mut hasher = Sha256::new();
            hasher.update(self.session.as_bytes());
            hasher.update(b"\0");
            hasher.update(discriminator.as_bytes());
            hasher.update(b"\0");
            hasher.update(nonce.to_le_bytes());
            let id = hasher
                .finalize()
                .iter()
                .take(4)
                .map(|b| format!("{b:02x}"))
                .collect::<String>();
            if self.used.insert(id.clone()) {
                return id;
            }
        }
        unreachable!("32-bit nonce space exhausted for one entry id")
    }
}

/// Accumulates Pi entries, chaining each one to the previous through `parentId`.
struct PiBuilder {
    minter: IdMinter,
    entries: Vec<Value>,
    parent: Value,
}

impl PiBuilder {
    fn new(session: &str) -> Self {
        Self {
            minter: IdMinter::new(session),
            entries: Vec::new(),
            parent: Value::Null,
        }
    }

    /// Appends one entry, filling in `id`, `parentId`, and `timestamp`.
    fn push(&mut self, discriminator: &str, time_ms: i64, mut fields: Map<String, Value>) {
        let id = self.minter.mint(discriminator);
        fields.insert("id".into(), json!(id));
        fields.insert("parentId".into(), self.parent.clone());
        fields.insert("timestamp".into(), json!(iso(time_ms)));
        self.parent = json!(id);
        self.entries.push(Value::Object(fields));
    }

    /// Appends a Pi entry this tool previously parked in a DSH log, keeping its
    /// own identity and only re-chaining it onto the current parent.
    fn push_verbatim(&mut self, entry: &Value) {
        let Some(fields) = entry.as_object() else {
            return;
        };
        let mut fields = fields.clone();
        let id = match fields.get("id").and_then(Value::as_str) {
            Some(id) => id.to_string(),
            None => self.minter.mint("verbatim"),
        };
        self.minter.used.insert(id.clone());
        fields.insert("id".into(), json!(id.clone()));
        fields.insert("parentId".into(), self.parent.clone());
        self.parent = json!(id);
        self.entries.push(Value::Object(fields));
    }
}

/// Returns `data` with the dotted paths removed that the Pi entry materializes.
///
/// Each path is at most two segments, which covers every field this tool lifts
/// out of a DSH payload (`content`, `message.content`, `meta`, `summary`, `title`).
fn strip(data: &Value, paths: &[&str]) -> Value {
    let Some(object) = data.as_object() else {
        return data.clone();
    };
    let mut out = object.clone();
    for path in paths {
        match path.split_once('.') {
            None => {
                out.remove(*path);
            }
            Some((head, tail)) => {
                if let Some(Value::Object(inner)) = out.get(head) {
                    let mut inner = inner.clone();
                    inner.remove(tail);
                    out.insert(head.to_string(), Value::Object(inner));
                }
            }
        }
    }
    Value::Object(out)
}

/// Builds the `dsh` provenance field attached to a mapped entry.
fn provenance(event: &dsh::Event, stripped: Value) -> Value {
    let mut out = Map::new();
    out.insert("seq".into(), json!(event.seq));
    out.insert("type".into(), json!(event.kind));
    if let Some(op) = event.envelope("surfaceOp") {
        out.insert("surfaceOp".into(), op.clone());
    }
    if let Some(seqs) = event.envelope("sourceEventSeqs") {
        out.insert("sourceEventSeqs".into(), seqs.clone());
    }
    if let Some(ignorable) = event.envelope("ignorable") {
        out.insert("ignorable".into(), ignorable.clone());
    }
    out.insert("data".into(), stripped);
    Value::Object(out)
}

/// Restores the stripped DSH payload recorded on a Pi entry.
fn restored_payload(entry: &Value) -> Option<Map<String, Value>> {
    entry
        .get("dsh")?
        .get("data")?
        .as_object()
        .cloned()
}

/// Writes `value` at a dotted path inside a DSH payload, creating the parent.
fn set_path(data: &mut Map<String, Value>, path: &str, value: Value) {
    match path.split_once('.') {
        None => {
            data.insert(path.to_string(), value);
        }
        Some((head, tail)) => {
            let mut inner = match data.get(head) {
                Some(Value::Object(existing)) => existing.clone(),
                _ => Map::new(),
            };
            inner.insert(tail.to_string(), value);
            data.insert(head.to_string(), Value::Object(inner));
        }
    }
}

/// Concatenates the text of a DSH content-block array.
fn block_text(blocks: &Value) -> String {
    let Some(items) = blocks.as_array() else {
        return blocks.as_str().unwrap_or_default().to_string();
    };
    items
        .iter()
        .filter_map(|block| match block.get("type").and_then(Value::as_str) {
            Some("text") | Some("reasoning") => block.get("text").and_then(Value::as_str),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

/// True when every block is one Pi can render as ordinary message content.
fn blocks_are_plain(blocks: &Value) -> bool {
    blocks.as_array().is_some_and(|items| {
        !items.is_empty()
            && items.iter().all(|block| {
                matches!(
                    block.get("type").and_then(Value::as_str),
                    Some("text") | Some("image")
                )
            })
    })
}

/// Rewrites DSH assistant content blocks into Pi content blocks.
///
/// `replay_blocks` is the parallel array DSH keeps under
/// `message.source.replayState.blocks`; it carries the provider reasoning
/// signature that Pi stores on the thinking block itself.
fn assistant_blocks_to_pi(blocks: &Value, replay_blocks: Option<&Vec<Value>>) -> Vec<Value> {
    let Some(items) = blocks.as_array() else {
        return Vec::new();
    };
    items
        .iter()
        .enumerate()
        .map(|(index, block)| match block.get("type").and_then(Value::as_str) {
            Some("reasoning") => {
                let mut out = Map::new();
                out.insert("type".into(), json!("thinking"));
                out.insert(
                    "thinking".into(),
                    json!(block.get("text").and_then(Value::as_str).unwrap_or("")),
                );
                let signature = block
                    .get("thinkingSignature")
                    .or_else(|| block.get("signature"))
                    .or_else(|| {
                        replay_blocks
                            .and_then(|replay| replay.get(index))
                            .and_then(|replay| replay.get("thinkingSignature"))
                    });
                if let Some(signature) = signature {
                    out.insert("thinkingSignature".into(), signature.clone());
                }
                Value::Object(out)
            }
            Some("tool-call") => {
                let mut out = Map::new();
                out.insert("type".into(), json!("toolCall"));
                out.insert("id".into(), block.get("id").cloned().unwrap_or(Value::Null));
                out.insert("name".into(), block.get("name").cloned().unwrap_or(Value::Null));
                out.insert("arguments".into(), parse_arguments(block.get("arguments")));
                Value::Object(out)
            }
            _ => block.clone(),
        })
        .collect()
}

/// Rewrites Pi assistant content blocks back into DSH content blocks.
fn assistant_blocks_to_dsh(blocks: &Value) -> Vec<Value> {
    let Some(items) = blocks.as_array() else {
        return Vec::new();
    };
    items
        .iter()
        .map(|block| match block.get("type").and_then(Value::as_str) {
            Some("thinking") => json!({
                "type": "reasoning",
                "text": block.get("thinking").and_then(Value::as_str).unwrap_or(""),
            }),
            Some("toolCall") => json!({
                "type": "tool-call",
                "id": block.get("id").cloned().unwrap_or(Value::Null),
                "name": block.get("name").cloned().unwrap_or(Value::Null),
                "arguments": stringify_arguments(block.get("arguments")),
            }),
            _ => block.clone(),
        })
        .collect()
}

/// DSH stores tool arguments as a JSON string; Pi stores the decoded object.
fn parse_arguments(raw: Option<&Value>) -> Value {
    match raw {
        Some(Value::String(text)) => serde_json::from_str(text).unwrap_or(json!({})),
        Some(other) => other.clone(),
        None => json!({}),
    }
}

/// Re-encodes Pi tool arguments as the JSON string DSH persists.
fn stringify_arguments(raw: Option<&Value>) -> Value {
    match raw {
        Some(Value::String(text)) => json!(text),
        Some(other) => json!(serde_json::to_string(other).unwrap_or_else(|_| "{}".into())),
        None => json!("{}"),
    }
}

/// Tool-call id, tool content, and error flag of a DSH `tool/result` payload,
/// across the nested pre-v4 shape and the flat v4 shape.
fn tool_result_parts(data: &Value) -> (String, Value, bool) {
    let message = data.get("message").unwrap_or(&Value::Null);
    if message.get("role").and_then(Value::as_str) == Some("tool") {
        return (
            message
                .get("toolCallId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            message.get("content").cloned().unwrap_or(json!([])),
            message.get("isError").and_then(Value::as_bool).unwrap_or(false),
        );
    }
    let nested = message
        .get("content")
        .and_then(Value::as_array)
        .and_then(|items| {
            items
                .iter()
                .find(|b| b.get("type").and_then(Value::as_str) == Some("tool-result"))
        });
    match nested {
        Some(block) => (
            block
                .get("toolCallId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            block.get("content").cloned().unwrap_or(json!([])),
            block.get("isError").and_then(Value::as_bool).unwrap_or(false),
        ),
        None => (
            message
                .get("source")
                .and_then(|s| s.get("callId"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            message.get("content").cloned().unwrap_or(json!([])),
            false,
        ),
    }
}

/// Removes the tool output from a `tool/result` payload.
///
/// The pre-v4 shape nests the output inside a `tool-result` block; the wrapper
/// stays so the reverse direction can refill it, and the v4 shape drops
/// `message.content` outright. `meta` goes with it because the Pi entry carries
/// it as the tool result's `details`.
fn strip_tool_result(data: &Value) -> Value {
    let Some(object) = data.as_object() else {
        return data.clone();
    };
    let mut out = object.clone();
    out.remove("meta");
    let Some(mut message) = out.get("message").and_then(Value::as_object).cloned() else {
        return Value::Object(out);
    };
    let nested = message
        .get("content")
        .and_then(Value::as_array)
        .filter(|items| {
            items
                .iter()
                .any(|b| b.get("type").and_then(Value::as_str) == Some("tool-result"))
        })
        .cloned();
    match nested {
        Some(items) => {
            let rebuilt: Vec<Value> = items
                .into_iter()
                .map(|block| match block.as_object() {
                    Some(fields)
                        if block.get("type").and_then(Value::as_str) == Some("tool-result") =>
                    {
                        let mut inner = fields.clone();
                        inner.remove("content");
                        Value::Object(inner)
                    }
                    _ => block,
                })
                .collect();
            message.insert("content".into(), Value::Array(rebuilt));
        }
        None => {
            message.remove("content");
        }
    }
    out.insert("message".into(), Value::Object(message));
    Value::Object(out)
}

/// Puts the tool output back where [`strip_tool_result`] took it from.
fn restore_tool_result_content(payload: &mut Map<String, Value>, content: Value) {
    let Some(mut message) = payload.get("message").and_then(Value::as_object).cloned() else {
        set_path(payload, "message.content", content);
        return;
    };
    let nested = message
        .get("content")
        .and_then(Value::as_array)
        .filter(|items| {
            items
                .iter()
                .any(|b| b.get("type").and_then(Value::as_str) == Some("tool-result"))
        })
        .cloned();
    match nested {
        Some(items) => {
            let rebuilt: Vec<Value> = items
                .into_iter()
                .map(|block| match block.as_object() {
                    Some(fields)
                        if block.get("type").and_then(Value::as_str) == Some("tool-result") =>
                    {
                        let mut inner = fields.clone();
                        inner.insert("content".into(), content.clone());
                        Value::Object(inner)
                    }
                    _ => block,
                })
                .collect();
            message.insert("content".into(), Value::Array(rebuilt));
        }
        None => {
            message.insert("content".into(), content);
        }
    }
    payload.insert("message".into(), Value::Object(message));
}

/// Summary of one conversion, surfaced by both the CLI and the browser.
#[derive(Debug, Clone, Default)]
pub struct Report {
    /// Entries or events written.
    pub rows: usize,
    /// Rows carried verbatim because no native counterpart exists.
    pub passthrough: usize,
    /// Source rows this tool mapped onto a native counterpart.
    pub mapped: usize,
}

/// Converts a DSH log into a Pi session file.
pub fn dsh_to_pi(log: &dsh::Log, source: &Path) -> (pi::SessionFile, Report) {
    let mut builder = PiBuilder::new(&log.header.id);
    let mut report = Report::default();

    // Pre-pass: pre-v4 logs keep usage on `assistant/attempt`, not on the message.
    let mut usage_by_step: HashMap<(i64, i64), Value> = HashMap::new();
    let mut tool_names: HashMap<String, String> = HashMap::new();
    let mut declared_tools: Vec<Value> = Vec::new();
    for event in &log.events {
        match event.kind.as_str() {
            "assistant/attempt" => {
                let turn = event.field("turn").and_then(Value::as_i64).unwrap_or(0);
                let step = event.field("step").and_then(Value::as_i64).unwrap_or(0);
                let empty: Vec<Value> = Vec::new();
                for item in event
                    .field("stream")
                    .and_then(Value::as_array)
                    .unwrap_or(&empty)
                {
                    let chunk = item.get("chunk").unwrap_or(&Value::Null);
                    if chunk.get("type").and_then(Value::as_str) == Some("usage") {
                        if let Some(usage) = chunk.get("usage") {
                            usage_by_step.insert((turn, step), usage.clone());
                        }
                    }
                }
            }
            "tool/call" => {
                if let (Some(id), Some(name)) = (
                    event.field("callId").and_then(Value::as_str),
                    event.field("name").and_then(Value::as_str),
                ) {
                    tool_names.insert(id.to_string(), name.to_string());
                }
            }
            "request/header" if declared_tools.is_empty() => {
                if let Some(tools) = event
                    .field("header")
                    .and_then(|h| h.get("tools"))
                    .and_then(Value::as_array)
                {
                    declared_tools = tools
                        .iter()
                        .filter_map(|t| t.get("name").cloned())
                        .collect();
                }
            }
            _ => {}
        }
    }

    let projected_from_pi = log.header.raw.get("piHeader").and_then(Value::as_object);
    let projected = projected_from_pi.is_some();
    let header = match projected_from_pi {
        Some(saved) => saved.clone(),
        None => {
            let mut header_extra: Vec<(&str, Value)> = Vec::new();
            if let Some(parent) = log.header.raw.get("parentSession") {
                header_extra.push(("parentSession", parent.clone()));
            }
            pi::make_header(
                &log.header.id,
                &iso(log.header.created_at),
                &log.header.cwd,
                &header_extra,
            )
        }
    };

    // Origin record: everything the reverse direction needs to rebuild the header.
    let mut origin = Map::new();
    origin.insert("type".into(), json!("custom"));
    origin.insert("customType".into(), json!(ORIGIN_TYPE));
    origin.insert(
        "data".into(),
        json!({
            "header": Value::Object(log.header.raw.clone()),
            "generation": log.header.version,
            "sourcePath": source.to_string_lossy(),
            "tool": env!("CARGO_PKG_NAME"),
            "toolVersion": env!("CARGO_PKG_VERSION"),
        }),
    );
    if !projected {
        builder.push("origin", log.header.created_at, origin);
    }

    let saved_title = log.header.raw.get("piTitle");
    let mut title = saved_title
        .and_then(|t| t.get("title"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let mut title_updated_at = saved_title
        .and_then(|t| t.get("updatedAt"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| iso(log.header.created_at));
    let mut system_emitted = false;
    let mut pending_pruned_tokens: i64 = 0;
    let mut compaction_positions: Vec<usize> = Vec::new();

    for event in &log.events {
        // Scaffolding this tool synthesized for a Pi session never belonged to it.
        if projected
            && matches!(
                event.kind.as_str(),
                "turn/start" | "turn/end" | "step/start" | "step/end"
            )
        {
            continue;
        }
        // An entry parked as an ignorable `pi/*` event restores as itself.
        if let Some(entry) = event
            .kind
            .strip_prefix("pi/")
            .and(event.field("entry"))
            .filter(|entry| entry.is_object())
        {
            builder.push_verbatim(entry);
            report.passthrough += 1;
            continue;
        }

        let disc = format!("{}#{}", event.kind, event.seq);
        let mut native = true;
        match event.kind.as_str() {
            "session/title" => {
                let next = event
                    .field("title")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let source_kind = event
                    .field("source")
                    .and_then(|s| s.get("kind"))
                    .and_then(Value::as_str)
                    .unwrap_or("auto");
                let mut fields = Map::new();
                fields.insert("type".into(), json!("title_change"));
                fields.insert("title".into(), json!(next));
                fields.insert(
                    "source".into(),
                    json!(if source_kind == "user" { "user" } else { "auto" }),
                );
                if !title.is_empty() {
                    fields.insert("previousTitle".into(), json!(title));
                }
                fields.insert("trigger".into(), json!(source_kind));
                fields.insert("dsh".into(), provenance(event, strip(&event.data, &["title"])));
                builder.push(&disc, event.time, fields);
                title = next;
                title_updated_at = iso(event.time);
            }
            "model/selection" => {
                let provider = event
                    .field("provider")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let model = event
                    .field("model")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let mut fields = Map::new();
                fields.insert("type".into(), json!("model_change"));
                fields.insert(
                    "model".into(),
                    json!(if provider.is_empty() {
                        model.to_string()
                    } else {
                        format!("{provider}/{model}")
                    }),
                );
                fields.insert("role".into(), json!("default"));
                fields.insert("dsh".into(), provenance(event, strip(&event.data, &[])));
                builder.push(&disc, event.time, fields);
            }
            "system/message" if !system_emitted => {
                system_emitted = true;
                let content = event.field("message").and_then(|m| m.get("content"));
                let prompt = content.map(block_text).unwrap_or_default();
                // `systemPrompt` is text, so it can only stand in for the source
                // content when that content is exactly one text block.
                let collapsible = content.and_then(Value::as_array).is_some_and(|items| {
                    items.len() == 1
                        && items[0].get("type").and_then(Value::as_str) == Some("text")
                });
                let mut fields = Map::new();
                fields.insert("type".into(), json!("session_init"));
                fields.insert("systemPrompt".into(), json!(prompt));
                if !declared_tools.is_empty() {
                    fields.insert("tools".into(), json!(declared_tools));
                }
                fields.insert(
                    "dsh".into(),
                    provenance(
                        event,
                        if collapsible {
                            strip(&event.data, &["message.content"])
                        } else {
                            event.data.clone()
                        },
                    ),
                );
                builder.push(&disc, event.time, fields);
            }
            "developer/message" | "user/message" => {
                // `user/message` stores the message flat; `developer/message` wraps it.
                let wrapped = event.field("message").is_some();
                let message = event
                    .field("message")
                    .cloned()
                    .unwrap_or_else(|| event.data.clone());
                let content = message.get("content").cloned().unwrap_or(json!([]));
                let source_kind = message
                    .get("source")
                    .and_then(|s| s.get("kind"))
                    .and_then(Value::as_str)
                    .unwrap_or("user")
                    .to_string();
                let is_developer = event.kind == "developer/message";
                let plain = blocks_are_plain(&content);
                let stripped = strip(
                    &event.data,
                    if wrapped { &["message.content"] } else { &["content"] },
                );

                let mut fields = Map::new();
                if plain && (is_developer || source_kind == "user") {
                    let mut msg = Map::new();
                    msg.insert(
                        "role".into(),
                        json!(if is_developer { "developer" } else { "user" }),
                    );
                    msg.insert("content".into(), content);
                    msg.insert("attribution".into(), json!("user"));
                    msg.insert("timestamp".into(), json!(event.time));
                    fields.insert("type".into(), json!("message"));
                    fields.insert("message".into(), Value::Object(msg));
                } else {
                    let saved_pi = message.get("source").and_then(|s| s.get("pi"));
                    fields.insert("type".into(), json!("custom_message"));
                    fields.insert(
                        "customType".into(),
                        saved_pi
                            .and_then(|pi| pi.get("customType"))
                            .cloned()
                            .unwrap_or_else(|| {
                                json!(format!("{NAMESPACE}{}.{source_kind}", event.kind))
                            }),
                    );
                    fields.insert("content".into(), content);
                    fields.insert(
                        "display".into(),
                        saved_pi
                            .and_then(|pi| pi.get("display"))
                            .cloned()
                            .unwrap_or(json!(true)),
                    );
                    fields.insert(
                        "attribution".into(),
                        saved_pi
                            .and_then(|pi| pi.get("attribution"))
                            .cloned()
                            .unwrap_or(json!("agent")),
                    );
                }
                fields.insert("dsh".into(), provenance(event, stripped));
                builder.push(&disc, event.time, fields);
            }
            "assistant/message" => {
                let message = event.field("message").cloned().unwrap_or(Value::Null);
                let source = message.get("source").cloned().unwrap_or(Value::Null);
                let response = source
                    .get("replayState")
                    .and_then(|r| r.get("response"))
                    .cloned()
                    .unwrap_or(Value::Null);
                let replay_blocks = source
                    .get("replayState")
                    .and_then(|r| r.get("blocks"))
                    .and_then(Value::as_array)
                    .cloned();
                let turn = event.field("turn").and_then(Value::as_i64).unwrap_or(0);
                let step = event.field("step").and_then(Value::as_i64).unwrap_or(0);

                let mut msg = Map::new();
                msg.insert("role".into(), json!("assistant"));
                if let Some(api) = response.get("api") {
                    msg.insert("api".into(), api.clone());
                }
                if let Some(provider) = source.get("provider") {
                    msg.insert("provider".into(), provider.clone());
                }
                if let Some(model) = source.get("model") {
                    msg.insert("model".into(), model.clone());
                }
                msg.insert(
                    "content".into(),
                    json!(assistant_blocks_to_pi(
                        message.get("content").unwrap_or(&Value::Null),
                        replay_blocks.as_ref(),
                    )),
                );
                let usage = event
                    .field("usage")
                    .cloned()
                    .or_else(|| usage_by_step.get(&(turn, step)).cloned());
                if let Some(usage) = usage {
                    msg.insert("usage".into(), pi_usage(&usage));
                }
                if let Some(stop) = response.get("stopReason") {
                    msg.insert("stopReason".into(), stop.clone());
                }
                if let Some(response_id) = response.get("responseId") {
                    msg.insert("responseId".into(), response_id.clone());
                }
                msg.insert("timestamp".into(), json!(event.time));

                let mut fields = Map::new();
                fields.insert("type".into(), json!("message"));
                fields.insert("message".into(), Value::Object(msg));
                fields.insert(
                    "dsh".into(),
                    provenance(event, strip(&event.data, &["message.content"])),
                );
                builder.push(&disc, event.time, fields);
            }
            "tool/call" => {
                let call_id = event
                    .field("callId")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let name = event.field("name").and_then(Value::as_str).unwrap_or("");
                let args = parse_arguments(event.field("arguments"));
                let mut summary = Map::new();
                for key in ["command", "path", "file_path", "pattern", "url"] {
                    if let Some(value) = args.get(key) {
                        summary.insert(key.to_string(), value.clone());
                    }
                }
                let mut data = Map::new();
                data.insert("toolCallId".into(), json!(call_id));
                data.insert("toolName".into(), json!(name));
                data.insert("startedAt".into(), json!(iso(event.time)));
                if !summary.is_empty() {
                    data.insert("args".into(), Value::Object(summary));
                }
                for key in ["i", "intent", "description"] {
                    if let Some(Value::String(intent)) = args.get(key) {
                        data.insert("intent".into(), json!(intent));
                        break;
                    }
                }

                let mut fields = Map::new();
                fields.insert("type".into(), json!("custom"));
                fields.insert("customType".into(), json!("tool_execution_start"));
                fields.insert("data".into(), Value::Object(data));
                fields.insert("dsh".into(), provenance(event, strip(&event.data, &[])));
                builder.push(&disc, event.time, fields);
            }
            "tool/result" => {
                let (call_id, content, is_error) = tool_result_parts(&event.data);
                let mut msg = Map::new();
                msg.insert("role".into(), json!("toolResult"));
                msg.insert("toolCallId".into(), json!(call_id.clone()));
                msg.insert(
                    "toolName".into(),
                    json!(tool_names.get(&call_id).cloned().unwrap_or_default()),
                );
                msg.insert("content".into(), content);
                if let Some(meta) = event.field("meta") {
                    msg.insert("details".into(), meta.clone());
                }
                msg.insert("isError".into(), json!(is_error));
                msg.insert("timestamp".into(), json!(event.time));

                let mut fields = Map::new();
                fields.insert("type".into(), json!("message"));
                fields.insert("message".into(), Value::Object(msg));
                fields.insert(
                    "dsh".into(),
                    provenance(event, strip_tool_result(&event.data)),
                );
                builder.push(&disc, event.time, fields);
            }
            "compaction/prune" => {
                pending_pruned_tokens += event
                    .field("shadowedTokenCount")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                native = false;
                passthrough(&mut builder, &disc, event);
            }
            "compaction/summary" => {
                let summary = event.field("summary").map(block_text).unwrap_or_default();
                let mut fields = Map::new();
                fields.insert("type".into(), json!("compaction"));
                fields.insert("summary".into(), json!(summary));
                fields.insert("firstKeptEntryId".into(), Value::Null);
                fields.insert("tokensBefore".into(), json!(pending_pruned_tokens));
                fields.insert("fromExtension".into(), json!(false));
                fields.insert(
                    "dsh".into(),
                    provenance(event, strip(&event.data, &["summary"])),
                );
                builder.push(&disc, event.time, fields);
                compaction_positions.push(builder.entries.len() - 1);
                pending_pruned_tokens = 0;
            }
            _ => {
                native = false;
                passthrough(&mut builder, &disc, event);
            }
        }
        if native {
            report.mapped += 1;
        } else {
            report.passthrough += 1;
        }
    }

    // A compaction keeps everything from the next entry onward.
    for position in compaction_positions {
        let next_id = builder
            .entries
            .get(position + 1)
            .and_then(|e| e.get("id"))
            .cloned();
        if let (Some(next_id), Some(Value::Object(entry))) =
            (next_id, builder.entries.get_mut(position))
        {
            entry.insert("firstKeptEntryId".into(), next_id);
        }
    }

    // A Pi session that carried an empty title keeps it; only a DSH log without
    // any recorded title falls back to its first prompt.
    if title.is_empty() && saved_title.is_none() {
        title = first_prompt_title(log);
    }
    report.rows = builder.entries.len();

    (
        pi::SessionFile {
            title,
            title_updated_at,
            header,
            entries: builder.entries,
        },
        report,
    )
}

/// Emits one DSH event Pi has no native home for, preserving the row verbatim.
fn passthrough(builder: &mut PiBuilder, disc: &str, event: &dsh::Event) {
    let mut fields = Map::new();
    fields.insert("type".into(), json!("custom"));
    fields.insert(
        "customType".into(),
        json!(format!("{NAMESPACE}{}", event.kind)),
    );
    fields.insert("data".into(), json!({ "dsh": event.raw.clone() }));
    builder.push(disc, event.time, fields);
}

/// Rewrites a DSH usage record into Pi's usage record.
fn pi_usage(usage: &Value) -> Value {
    let get = |key: &str| usage.get(key).and_then(Value::as_i64).unwrap_or(0);
    json!({
        "input": get("inputTokens"),
        "output": get("outputTokens"),
        "cacheRead": get("cacheReadTokens"),
        "cacheWrite": get("cacheWriteTokens"),
        "totalTokens": get("totalTokens"),
        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0 },
    })
}

/// Fallback title taken from the first user prompt, as Pi's own fallback does.
fn first_prompt_title(log: &dsh::Log) -> String {
    for event in &log.events {
        if event.kind != "user/message" {
            continue;
        }
        let message = event
            .field("message")
            .cloned()
            .unwrap_or_else(|| event.data.clone());
        if message
            .get("source")
            .and_then(|s| s.get("kind"))
            .and_then(Value::as_str)
            != Some("user")
        {
            continue;
        }
        let text = block_text(message.get("content").unwrap_or(&Value::Null));
        let trimmed = text.trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut title: String = trimmed.chars().take(60).collect();
        if trimmed.chars().count() > 60 {
            title.push('…');
        }
        return title.replace('\n', " ");
    }
    String::new()
}

/// Walks a Pi entry tree from its leaf back to the root, returning root-first order.
pub fn leaf_chain(entries: &[Value]) -> Vec<Value> {
    let by_id: HashMap<&str, &Value> = entries
        .iter()
        .filter_map(|e| Some((e.get("id")?.as_str()?, e)))
        .collect();
    let Some(leaf) = entries.last() else {
        return Vec::new();
    };
    let mut chain = Vec::new();
    let mut seen = HashSet::new();
    let mut cursor = Some(leaf);
    while let Some(entry) = cursor {
        let Some(id) = entry.get("id").and_then(Value::as_str) else {
            break;
        };
        if !seen.insert(id.to_string()) {
            break;
        }
        chain.push(entry.clone());
        cursor = entry
            .get("parentId")
            .and_then(Value::as_str)
            .and_then(|parent| by_id.get(parent).copied());
    }
    chain.reverse();
    chain
}

/// Converts a Pi session file into a DSH log.
///
/// Sessions that came from DSH restore their original events; native Pi sessions
/// are translated and given the turn and step scaffolding DSH replay expects.
pub fn pi_to_dsh(file: &pi::SessionFile, follow_leaf: bool) -> (dsh::Log, Report) {
    let entries = if follow_leaf {
        leaf_chain(&file.entries)
    } else {
        file.entries.clone()
    };
    let mut report = Report::default();

    let origin = entries
        .iter()
        .find(|entry| entry.get("customType").and_then(Value::as_str) == Some(ORIGIN_TYPE));
    let round_trip = origin.is_some();

    // Assistant tool-call blocks only need a synthesized `tool/call` when the Pi
    // file does not already carry a `tool_execution_start` entry for them.
    let declared_calls: HashSet<String> = entries
        .iter()
        .filter(|entry| {
            entry.get("customType").and_then(Value::as_str) == Some("tool_execution_start")
        })
        .filter_map(|entry| {
            entry
                .get("data")?
                .get("toolCallId")?
                .as_str()
                .map(str::to_string)
        })
        .collect();

    let created_at = from_iso(file.timestamp()).unwrap_or_else(|| Utc::now().timestamp_millis());
    let header = match origin.and_then(|o| o.get("data")).and_then(|d| d.get("header")) {
        Some(Value::Object(saved)) => dsh::Header {
            version: saved
                .get("version")
                .and_then(Value::as_u64)
                .unwrap_or(dsh::CURRENT_GENERATION),
            id: saved
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or(file.id())
                .to_string(),
            created_at: saved
                .get("createdAt")
                .and_then(Value::as_i64)
                .unwrap_or(created_at),
            cwd: saved
                .get("cwd")
                .and_then(Value::as_str)
                .unwrap_or(file.cwd())
                .to_string(),
            raw: saved.clone(),
        },
        _ => {
            let mut raw = Map::new();
            raw.insert("type".into(), json!("session"));
            raw.insert("version".into(), json!(dsh::CURRENT_GENERATION));
            raw.insert("id".into(), json!(file.id()));
            raw.insert("createdAt".into(), json!(created_at));
            raw.insert("cwd".into(), json!(file.cwd()));
            raw.insert("isSeeded".into(), json!(false));
            raw.insert("delegationDepth".into(), json!(0));
            raw.insert("agentPreset".into(), json!("standard"));
            if let Some(parent) = file.header.get("parentSession") {
                raw.insert("parentSession".into(), parent.clone());
            }
            // DSH headers are open objects, so a Pi session keeps its own header
            // and title here instead of losing the fields DSH has no slot for.
            raw.insert("piHeader".into(), Value::Object(file.header.clone()));
            raw.insert(
                "piTitle".into(),
                json!({ "title": file.title, "updatedAt": file.title_updated_at }),
            );
            dsh::Header {
                version: dsh::CURRENT_GENERATION,
                id: file.id().to_string(),
                created_at,
                cwd: file.cwd().to_string(),
                raw,
            }
        }
    };

    let mut events: Vec<dsh::Event> = Vec::new();
    let mut seq: i64 = 0;
    let mut seq_map: HashMap<i64, i64> = HashMap::new();
    let mut scaffold = Scaffold::default();

    for entry in &entries {
        let kind = entry.get("type").and_then(Value::as_str).unwrap_or("");
        let custom_type = entry.get("customType").and_then(Value::as_str).unwrap_or("");
        let time = entry
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(from_iso)
            .unwrap_or(created_at);

        if custom_type == ORIGIN_TYPE {
            continue;
        }

        // A row this tool parked verbatim restores exactly, only renumbered.
        if kind == "custom" && custom_type.starts_with(NAMESPACE) {
            if let Some(Value::Object(saved)) = entry.get("data").and_then(|d| d.get("dsh")) {
                let mut raw = saved.clone();
                let old_seq = raw.get("seq").and_then(Value::as_i64);
                raw.insert("seq".into(), json!(seq));
                remap_source_seqs(&mut raw, &seq_map);
                if let Some(event) = dsh::Event::from_value(Value::Object(raw)) {
                    if let Some(old) = old_seq {
                        seq_map.insert(old, seq);
                    }
                    events.push(event);
                    seq += 1;
                    report.passthrough += 1;
                }
                continue;
            }
        }

        let prov = entry.get("dsh");
        let old_seq = prov.and_then(|p| p.get("seq")).and_then(Value::as_i64);
        let restored = restored_payload(entry);
        let mut envelope: Vec<(&str, Value)> = Vec::new();
        if let Some(op) = prov.and_then(|p| p.get("surfaceOp")) {
            envelope.push(("surfaceOp", op.clone()));
        }
        if let Some(seqs) = prov
            .and_then(|p| p.get("sourceEventSeqs"))
            .and_then(Value::as_array)
        {
            let remapped: Vec<Value> = seqs
                .iter()
                .filter_map(Value::as_i64)
                .map(|old| json!(seq_map.get(&old).copied().unwrap_or(old)))
                .collect();
            envelope.push(("sourceEventSeqs", json!(remapped)));
        }
        if let Some(ignorable) = prov.and_then(|p| p.get("ignorable")) {
            envelope.push(("ignorable", ignorable.clone()));
        }
        let dsh_kind = prov
            .and_then(|p| p.get("type"))
            .and_then(Value::as_str)
            .map(str::to_string);

        let mut emitted: Vec<dsh::Event> = Vec::new();
        match kind {
            "message" => {
                let message = entry.get("message").cloned().unwrap_or(Value::Null);
                let role = message.get("role").and_then(Value::as_str).unwrap_or("");
                let content = message.get("content").cloned().unwrap_or(json!([]));
                match role {
                    "user" | "developer" => {
                        if !round_trip {
                            scaffold.open_turn_for_user(&mut emitted, time);
                        }
                        let kind = dsh_kind.unwrap_or_else(|| {
                            if role == "developer" {
                                "developer/message".into()
                            } else {
                                "user/message".into()
                            }
                        });
                        let payload = match restored {
                            Some(mut saved) => {
                                // `user/message` keeps the message flat, others wrap it.
                                let path = if saved.contains_key("message") {
                                    "message.content"
                                } else {
                                    "content"
                                };
                                set_path(&mut saved, path, content);
                                Value::Object(saved)
                            }
                            None => json!({
                                "content": content,
                                "source": { "kind": "user" },
                                "role": role,
                                "id": entry.get("id").cloned().unwrap_or(Value::Null),
                            }),
                        };
                        emitted.push(dsh::make_event(&kind, 0, time, payload, &envelope));
                    }
                    "assistant" => {
                        if !round_trip {
                            scaffold.open_step_for_assistant(&mut emitted, time);
                        }
                        let blocks = assistant_blocks_to_dsh(&content);
                        let payload = match restored {
                            Some(mut saved) => {
                                set_path(&mut saved, "message.content", json!(blocks.clone()));
                                Value::Object(saved)
                            }
                            None => {
                                let mut source = Map::new();
                                source.insert("kind".into(), json!("model"));
                                if let Some(provider) = message.get("provider") {
                                    source.insert("provider".into(), provider.clone());
                                }
                                if let Some(model) = message.get("model") {
                                    source.insert("model".into(), model.clone());
                                }
                                let mut response = Map::new();
                                response.insert("kind".into(), json!("pi-ai"));
                                response.insert("version".into(), json!(2));
                                for key in ["api", "provider", "model", "responseId", "stopReason"] {
                                    if let Some(value) = message.get(key) {
                                        response.insert(key.into(), value.clone());
                                    }
                                }
                                source.insert(
                                    "replayState".into(),
                                    json!({ "response": Value::Object(response) }),
                                );
                                json!({
                                    "turn": scaffold.turn.max(1),
                                    "step": scaffold.step.max(1),
                                    "message": {
                                        "role": "assistant",
                                        "content": blocks.clone(),
                                        "source": Value::Object(source),
                                        "id": entry.get("id").cloned().unwrap_or(Value::Null),
                                    },
                                })
                            }
                        };
                        emitted.push(dsh::make_event(
                            &dsh_kind.unwrap_or_else(|| "assistant/message".into()),
                            0,
                            time,
                            payload,
                            &envelope,
                        ));
                        if !round_trip {
                            for block in blocks {
                                if block.get("type").and_then(Value::as_str) != Some("tool-call") {
                                    continue;
                                }
                                let already_declared = block
                                    .get("id")
                                    .and_then(Value::as_str)
                                    .is_some_and(|id| declared_calls.contains(id));
                                if already_declared {
                                    continue;
                                }
                                emitted.push(dsh::make_event(
                                    "tool/call",
                                    0,
                                    time,
                                    json!({
                                        "turn": scaffold.turn.max(1),
                                        "step": scaffold.step.max(1),
                                        "callId": block.get("id").cloned().unwrap_or(Value::Null),
                                        "name": block.get("name").cloned().unwrap_or(Value::Null),
                                        "arguments": block.get("arguments").cloned().unwrap_or(json!("{}")),
                                    }),
                                    &[],
                                ));
                            }
                        }
                    }
                    "toolResult" => {
                        let payload = match restored {
                            Some(mut saved) => {
                                restore_tool_result_content(&mut saved, content);
                                if let Some(details) = message.get("details") {
                                    saved.insert("meta".into(), details.clone());
                                }
                                Value::Object(saved)
                            }
                            None => json!({
                                "turn": scaffold.turn.max(1),
                                "step": scaffold.step.max(1),
                                "message": {
                                    "role": "tool",
                                    "source": {
                                        "kind": "tool",
                                        "callId": message.get("toolCallId").cloned().unwrap_or(Value::Null),
                                    },
                                    "toolCallId": message.get("toolCallId").cloned().unwrap_or(Value::Null),
                                    "content": content,
                                    "isError": message.get("isError").cloned().unwrap_or(json!(false)),
                                },
                            }),
                        };
                        emitted.push(dsh::make_event(
                            &dsh_kind.unwrap_or_else(|| "tool/result".into()),
                            0,
                            time,
                            payload,
                            &envelope,
                        ));
                    }
                    _ => emitted.push(pi_only_event(entry, time)),
                }
            }
            "custom" if custom_type == "tool_execution_start" => {
                let payload = match restored {
                    Some(saved) => Value::Object(saved),
                    None => {
                        let data = entry.get("data").cloned().unwrap_or(Value::Null);
                        json!({
                            "turn": scaffold.turn.max(1),
                            "step": scaffold.step.max(1),
                            "callId": data.get("toolCallId").cloned().unwrap_or(Value::Null),
                            "name": data.get("toolName").cloned().unwrap_or(Value::Null),
                            "arguments": json!("{}"),
                        })
                    }
                };
                emitted.push(dsh::make_event(
                    &dsh_kind.unwrap_or_else(|| "tool/call".into()),
                    0,
                    time,
                    payload,
                    &envelope,
                ));
            }
            "custom_message" => {
                let content = entry.get("content").cloned().unwrap_or(json!([]));
                let content = if content.is_string() {
                    json!([{ "type": "text", "text": content }])
                } else {
                    content
                };
                let payload = match restored {
                    Some(mut saved) => {
                        let path = if saved.contains_key("message") {
                            "message.content"
                        } else {
                            "content"
                        };
                        set_path(&mut saved, path, content);
                        Value::Object(saved)
                    }
                    None => {
                        // A native Pi record keeps its own type and flags under
                        // `source.pi`, which DSH treats as opaque source metadata.
                        let source_kind = custom_type
                            .rsplit('.')
                            .next()
                            .filter(|_| custom_type.starts_with(NAMESPACE))
                            .unwrap_or(custom_type)
                            .to_string();
                        json!({
                            "content": content,
                            "source": {
                                "kind": source_kind,
                                "pi": {
                                    "customType": custom_type,
                                    "display": entry.get("display").cloned().unwrap_or(json!(true)),
                                    "attribution": entry
                                        .get("attribution")
                                        .cloned()
                                        .unwrap_or(json!("agent")),
                                },
                            },
                            "role": "user",
                            "id": entry.get("id").cloned().unwrap_or(Value::Null),
                        })
                    }
                };
                emitted.push(dsh::make_event(
                    &dsh_kind.unwrap_or_else(|| "user/message".into()),
                    0,
                    time,
                    payload,
                    &envelope,
                ));
            }
            "session_init" => {
                let content = json!([{
                    "type": "text",
                    "text": entry.get("systemPrompt").and_then(Value::as_str).unwrap_or(""),
                }]);
                let payload = match restored {
                    Some(mut saved) => {
                        // Content the source kept verbatim is already in place.
                        let kept = saved
                            .get("message")
                            .and_then(|m| m.get("content"))
                            .is_some();
                        if !kept {
                            set_path(&mut saved, "message.content", content);
                        }
                        Value::Object(saved)
                    }
                    None => json!({
                        "turn": 1,
                        "step": 1,
                        "message": { "role": "system", "content": content },
                    }),
                };
                let mut extra = envelope.clone();
                if extra.is_empty() {
                    extra.push(("surfaceOp", json!("append")));
                }
                emitted.push(dsh::make_event(
                    &dsh_kind.unwrap_or_else(|| "system/message".into()),
                    0,
                    time,
                    payload,
                    &extra,
                ));
            }
            "model_change" => {
                let payload = match restored {
                    Some(saved) => Value::Object(saved),
                    None => {
                        let model = entry.get("model").and_then(Value::as_str).unwrap_or("");
                        let (provider, name) = model.split_once('/').unwrap_or(("", model));
                        json!({ "provider": provider, "model": name })
                    }
                };
                emitted.push(dsh::make_event(
                    &dsh_kind.unwrap_or_else(|| "model/selection".into()),
                    0,
                    time,
                    payload,
                    &envelope,
                ));
            }
            "title_change" => {
                let title = entry.get("title").cloned().unwrap_or(json!(""));
                let payload = match restored {
                    Some(mut saved) => {
                        saved.insert("title".into(), title);
                        Value::Object(saved)
                    }
                    None => json!({
                        "title": title,
                        "messageSeqs": [],
                        "source": { "kind": entry.get("source").and_then(Value::as_str).unwrap_or("auto") },
                    }),
                };
                emitted.push(dsh::make_event(
                    &dsh_kind.unwrap_or_else(|| "session/title".into()),
                    0,
                    time,
                    payload,
                    &envelope,
                ));
            }
            "compaction" => {
                let summary = json!([{
                    "type": "text",
                    "text": entry.get("summary").and_then(Value::as_str).unwrap_or(""),
                }]);
                let payload = match restored {
                    Some(mut saved) => {
                        saved.insert("summary".into(), summary);
                        Value::Object(saved)
                    }
                    None => json!({
                        "compactionId": format!("pi-{}", entry.get("id").and_then(Value::as_str).unwrap_or("0")),
                        "summary": summary,
                    }),
                };
                emitted.push(dsh::make_event(
                    &dsh_kind.unwrap_or_else(|| "compaction/summary".into()),
                    0,
                    time,
                    payload,
                    &envelope,
                ));
            }
            _ => emitted.push(pi_only_event(entry, time)),
        }

        for mut event in emitted {
            if let Value::Object(raw) = &mut event.raw {
                raw.insert("seq".into(), json!(seq));
            }
            event.seq = seq;
            if let Some(old) = old_seq {
                seq_map.insert(old, seq);
            }
            events.push(event);
            seq += 1;
            report.mapped += 1;
        }
    }

    if !round_trip {
        let mut tail = Vec::new();
        scaffold.close(&mut tail, events.last().map(|e| e.time).unwrap_or(created_at));
        for mut event in tail {
            if let Value::Object(raw) = &mut event.raw {
                raw.insert("seq".into(), json!(seq));
            }
            event.seq = seq;
            seq += 1;
            events.push(event);
        }
    }

    report.rows = events.len();
    (dsh::Log { header, events }, report)
}

/// Wraps a Pi-only entry as an ignorable DSH event so DSH loads it without refusal.
fn pi_only_event(entry: &Value, time: i64) -> dsh::Event {
    let kind = entry.get("type").and_then(Value::as_str).unwrap_or("entry");
    dsh::make_event(
        &format!("pi/{kind}"),
        0,
        time,
        json!({ "entry": entry.clone() }),
        &[("ignorable", json!(true))],
    )
}

/// Rewrites `sourceEventSeqs` through the old-to-new sequence map.
fn remap_source_seqs(raw: &mut Map<String, Value>, seq_map: &HashMap<i64, i64>) {
    let Some(seqs) = raw.get("sourceEventSeqs").and_then(Value::as_array).cloned() else {
        return;
    };
    let remapped: Vec<Value> = seqs
        .iter()
        .filter_map(Value::as_i64)
        .map(|old| json!(seq_map.get(&old).copied().unwrap_or(old)))
        .collect();
    raw.insert("sourceEventSeqs".into(), json!(remapped));
}

/// Turn and step bookkeeping synthesized for a native Pi session.
#[derive(Default)]
struct Scaffold {
    turn: i64,
    step: i64,
    in_turn: bool,
    in_step: bool,
    assistant_in_step: bool,
}

impl Scaffold {
    fn open_turn_for_user(&mut self, out: &mut Vec<dsh::Event>, time: i64) {
        self.close_step(out, time);
        self.close_turn(out, time);
        self.turn += 1;
        self.in_turn = true;
        out.push(dsh::make_event("turn/start", 0, time, json!({ "turn": self.turn }), &[]));
        self.step = 0;
        self.open_step(out, time);
    }

    fn open_step_for_assistant(&mut self, out: &mut Vec<dsh::Event>, time: i64) {
        if !self.in_turn {
            self.turn += 1;
            self.in_turn = true;
            out.push(dsh::make_event("turn/start", 0, time, json!({ "turn": self.turn }), &[]));
            self.step = 0;
        }
        if self.in_step && self.assistant_in_step {
            self.close_step(out, time);
        }
        if !self.in_step {
            self.open_step(out, time);
        }
        self.assistant_in_step = true;
    }

    fn open_step(&mut self, out: &mut Vec<dsh::Event>, time: i64) {
        self.step += 1;
        self.in_step = true;
        self.assistant_in_step = false;
        out.push(dsh::make_event(
            "step/start",
            0,
            time,
            json!({ "turn": self.turn, "step": self.step }),
            &[],
        ));
    }

    fn close_step(&mut self, out: &mut Vec<dsh::Event>, time: i64) {
        if !self.in_step {
            return;
        }
        self.in_step = false;
        self.assistant_in_step = false;
        out.push(dsh::make_event(
            "step/end",
            0,
            time,
            json!({ "turn": self.turn, "step": self.step }),
            &[],
        ));
    }

    fn close_turn(&mut self, out: &mut Vec<dsh::Event>, time: i64) {
        if !self.in_turn {
            return;
        }
        self.in_turn = false;
        out.push(dsh::make_event(
            "turn/end",
            0,
            time,
            json!({ "turn": self.turn, "reason": { "kind": "settled" } }),
            &[],
        ));
    }

    fn close(&mut self, out: &mut Vec<dsh::Event>, time: i64) {
        self.close_step(out, time);
        self.close_turn(out, time);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dsh_event(kind: &str, seq: i64, data: Value, extra: &[(&str, Value)]) -> dsh::Event {
        dsh::make_event(kind, seq, 1_700_000_000_000 + seq, data, extra)
    }

    fn sample_log() -> dsh::Log {
        let mut raw = Map::new();
        raw.insert("type".into(), json!("session"));
        raw.insert("version".into(), json!(4));
        raw.insert("id".into(), json!("sess-1"));
        raw.insert("createdAt".into(), json!(1_700_000_000_000i64));
        raw.insert("cwd".into(), json!("/work/x"));
        dsh::Log {
            header: dsh::Header {
                version: 4,
                id: "sess-1".into(),
                created_at: 1_700_000_000_000,
                cwd: "/work/x".into(),
                raw,
            },
            events: vec![
                dsh_event(
                    "user/message",
                    0,
                    json!({"content": [{"type": "text", "text": "hi"}], "source": {"kind": "user"}, "role": "user", "id": "u1"}),
                    &[("surfaceOp", json!("append"))],
                ),
                dsh_event(
                    "assistant/message",
                    1,
                    json!({"turn": 1, "step": 1, "message": {"role": "assistant", "content": [
                        {"type": "reasoning", "text": "think"},
                        {"type": "tool-call", "id": "c1", "name": "bash", "arguments": "{\"command\":\"ls\"}"}
                    ], "source": {"kind": "model", "provider": "anthropic", "model": "m", "replayState": {
                        "response": {"kind": "pi-ai", "api": "anthropic-messages", "stopReason": "toolUse"},
                        "blocks": [{"type": "reasoning", "thinkingSignature": "sig"}, {"type": "tool-call"}]
                    }}, "id": "a1"},
                    "usage": {"inputTokens": 4, "outputTokens": 9, "totalTokens": 13}}),
                    &[("surfaceOp", json!("append"))],
                ),
                dsh_event(
                    "tool/call",
                    2,
                    json!({"turn": 1, "step": 1, "callId": "c1", "name": "bash", "arguments": "{\"command\":\"ls\"}"}),
                    &[],
                ),
                dsh_event(
                    "tool/result",
                    3,
                    json!({"turn": 1, "step": 1, "message": {"role": "tool", "source": {"kind": "tool", "callId": "c1"},
                           "toolCallId": "c1", "content": [{"type": "text", "text": "out"}], "isError": false, "id": "t1"},
                           "meta": {"path": "/work/x", "lines": [{"number": 1, "text": "a"}]}}),
                    &[("surfaceOp", json!("append")), ("sourceEventSeqs", json!([2]))],
                ),
                dsh_event("hook/invoked", 4, json!({"point": "UserPromptSubmit"}), &[]),
            ],
        }
    }

    #[test]
    fn dsh_messages_become_pi_messages() {
        let (file, report) = dsh_to_pi(&sample_log(), Path::new("/tmp/session.v4.jsonl.zstd"));
        assert_eq!(file.header["id"], json!("sess-1"));
        assert_eq!(file.title, "hi");
        assert!(report.mapped >= 4);

        let roles: Vec<&str> = file
            .entries
            .iter()
            .filter(|e| e["type"] == json!("message"))
            .map(|e| e["message"]["role"].as_str().unwrap())
            .collect();
        assert_eq!(roles, ["user", "assistant", "toolResult"]);

        let assistant = file
            .entries
            .iter()
            .find(|e| e["message"]["role"] == json!("assistant"))
            .unwrap();
        let blocks = assistant["message"]["content"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], json!("thinking"));
        assert_eq!(blocks[0]["thinkingSignature"], json!("sig"));
        assert_eq!(blocks[1]["type"], json!("toolCall"));
        assert_eq!(blocks[1]["arguments"]["command"], json!("ls"));
        assert_eq!(assistant["message"]["usage"]["totalTokens"], json!(13));
        assert_eq!(assistant["message"]["stopReason"], json!("toolUse"));

        let result = file
            .entries
            .iter()
            .find(|e| e["message"]["role"] == json!("toolResult"))
            .unwrap();
        assert_eq!(result["message"]["toolName"], json!("bash"));
        assert_eq!(result["message"]["details"]["path"], json!("/work/x"));
    }

    #[test]
    fn pi_entries_chain_through_parent_ids() {
        let (file, _) = dsh_to_pi(&sample_log(), Path::new("/tmp/s.zstd"));
        assert_eq!(file.entries[0]["parentId"], Value::Null);
        for pair in file.entries.windows(2) {
            assert_eq!(pair[1]["parentId"], pair[0]["id"]);
        }
    }

    #[test]
    fn a_round_trip_restores_every_source_row_exactly() {
        let log = sample_log();
        let (file, _) = dsh_to_pi(&log, Path::new("/tmp/s.zstd"));
        let (back, _) = pi_to_dsh(&file, false);

        assert_eq!(back.header.raw, log.header.raw);
        assert_eq!(back.events.len(), log.events.len());
        for (before, after) in log.events.iter().zip(back.events.iter()) {
            assert_eq!(after.raw, before.raw, "row {} changed", before.seq);
        }
    }

    #[test]
    fn nothing_is_stored_twice_in_the_pi_entry() {
        let (file, _) = dsh_to_pi(&sample_log(), Path::new("/tmp/s.zstd"));
        let assistant = file
            .entries
            .iter()
            .find(|e| e["message"]["role"] == json!("assistant"))
            .unwrap();
        assert!(assistant["dsh"]["data"]["message"].get("content").is_none());
        let result = file
            .entries
            .iter()
            .find(|e| e["message"]["role"] == json!("toolResult"))
            .unwrap();
        assert!(result["dsh"]["data"]["message"].get("content").is_none());
        assert!(result["dsh"]["data"].get("meta").is_none());
    }

    #[test]
    fn conversion_is_deterministic() {
        let log = sample_log();
        let (a, _) = dsh_to_pi(&log, Path::new("/tmp/s.zstd"));
        let (b, _) = dsh_to_pi(&log, Path::new("/tmp/s.zstd"));
        assert_eq!(a.entries, b.entries);
    }

    #[test]
    fn pre_v4_tool_results_round_trip_through_their_nested_block() {
        let mut raw = Map::new();
        raw.insert("type".into(), json!("session"));
        raw.insert("version".into(), json!(3));
        raw.insert("id".into(), json!("s3"));
        raw.insert("createdAt".into(), json!(1_700_000_000_000i64));
        raw.insert("cwd".into(), json!("/w"));
        let log = dsh::Log {
            header: dsh::Header {
                version: 3,
                id: "s3".into(),
                created_at: 1_700_000_000_000,
                cwd: "/w".into(),
                raw,
            },
            events: vec![dsh_event(
                "tool/result",
                0,
                json!({"turn": 1, "step": 1, "message": {"source": {"kind": "tool", "callId": "c9"},
                       "content": [{"type": "tool-result", "toolCallId": "c9",
                                    "content": [{"type": "text", "text": "legacy"}]}]}}),
                &[("surfaceOp", json!("append"))],
            )],
        };
        let (file, _) = dsh_to_pi(&log, Path::new("/tmp/s.zstd"));
        let entry = &file.entries[1];
        assert_eq!(entry["message"]["content"][0]["text"], json!("legacy"));
        let (back, _) = pi_to_dsh(&file, false);
        assert_eq!(back.events[0].raw, log.events[0].raw);
    }

    #[test]
    fn native_pi_sessions_gain_turn_and_step_scaffolding() {
        let file = pi::SessionFile {
            title: "t".into(),
            title_updated_at: iso(1_700_000_000_000),
            header: pi::make_header("p1", &iso(1_700_000_000_000), "/work/x", &[]),
            entries: vec![
                json!({"type": "message", "id": "a1", "parentId": null, "timestamp": iso(1_700_000_000_001),
                       "message": {"role": "user", "content": [{"type": "text", "text": "hi"}]}}),
                json!({"type": "message", "id": "a2", "parentId": "a1", "timestamp": iso(1_700_000_000_002),
                       "message": {"role": "assistant", "content": [{"type": "text", "text": "yo"}]}}),
            ],
        };
        let (log, _) = pi_to_dsh(&file, false);
        let kinds: Vec<&str> = log.events.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(
            kinds,
            ["turn/start", "step/start", "user/message", "assistant/message", "step/end", "turn/end"]
        );
        for (index, event) in log.events.iter().enumerate() {
            assert_eq!(event.seq, index as i64, "sequences must stay dense");
            assert_eq!(event.raw["seq"], json!(index as i64));
        }
    }

    #[test]
    fn a_startup_only_log_carries_no_conversation() {
        let mut raw = Map::new();
        raw.insert("type".into(), json!("session"));
        raw.insert("version".into(), json!(4));
        raw.insert("id".into(), json!("empty-1"));
        raw.insert("createdAt".into(), json!(1_700_000_000_000i64));
        raw.insert("cwd".into(), json!("/w"));
        let log = dsh::Log {
            header: dsh::Header {
                version: 4,
                id: "empty-1".into(),
                created_at: 1_700_000_000_000,
                cwd: "/w".into(),
                raw,
            },
            events: vec![
                dsh_event("permission/preset", 0, json!({"preset": "danger-full-access"}), &[]),
                dsh_event("sandbox/mode", 1, json!({"mode": "danger-full-access"}), &[]),
                dsh_event("approval/policy", 2, json!({"policy": "never"}), &[]),
            ],
        };
        assert_eq!(log.message_count(), 0);
        let (file, _) = dsh_to_pi(&log, Path::new("/tmp/s.zstd"));
        assert_eq!(file.message_count(), 0, "startup records are not a conversation");
        assert!(file.title.is_empty());

        let (back, _) = pi_to_dsh(&file, false);
        assert_eq!(back.message_count(), 0);
    }

    #[test]
    fn leaf_chain_follows_parent_pointers() {
        let entries = vec![
            json!({"id": "a", "parentId": null}),
            json!({"id": "b", "parentId": "a"}),
            json!({"id": "orphan", "parentId": "a"}),
        ];
        let chain = leaf_chain(&entries);
        let ids: Vec<&str> = chain.iter().map(|e| e["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["a", "orphan"]);
    }
}
