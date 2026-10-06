//! DeepSeek Harness session logs: discovery, zstd-framed JSONL decoding, and encoding.
//!
//! A DSH session directory holds one file per released generation
//! (`session.v2.jsonl.zstd` … `session.v4.jsonl.zstd`); generations are never
//! rewritten, so the newest one present is the authoritative log.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Map, Value};

/// Newest generation this tool emits when writing a DSH log.
pub const CURRENT_GENERATION: u64 = 4;

/// Logical header of a DSH session log (its first physical row).
#[derive(Debug, Clone)]
pub struct Header {
    /// Released generation of the physical rows that follow.
    pub version: u64,
    /// Session identifier, also the name of the on-disk session directory.
    pub id: String,
    /// Session creation time in epoch milliseconds.
    pub created_at: i64,
    /// Working directory the session ran in.
    pub cwd: String,
    /// Every header field, including ones this tool does not interpret.
    pub raw: Map<String, Value>,
}

impl Header {
    /// Parses a decoded header row, failing when the required fields are absent.
    pub fn from_value(value: &Value) -> Result<Self> {
        let obj = value
            .as_object()
            .ok_or_else(|| anyhow!("session header is not a JSON object"))?;
        if obj.get("type").and_then(Value::as_str) != Some("session") {
            return Err(anyhow!("first row is not a session header"));
        }
        Ok(Self {
            version: obj.get("version").and_then(Value::as_u64).unwrap_or(0),
            id: obj
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("session header has no id"))?
                .to_string(),
            created_at: obj.get("createdAt").and_then(Value::as_i64).unwrap_or(0),
            cwd: obj
                .get("cwd")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            raw: obj.clone(),
        })
    }

    /// Serializes the header back to a physical row.
    pub fn to_value(&self) -> Value {
        Value::Object(self.raw.clone())
    }
}

/// One decoded DSH session event.
#[derive(Debug, Clone)]
pub struct Event {
    /// Event type, for example `assistant/message`.
    pub kind: String,
    /// Monotonic position of this event in the log.
    pub seq: i64,
    /// Event time in epoch milliseconds.
    pub time: i64,
    /// Type-specific payload.
    pub data: Value,
    /// Complete physical row, preserved so unknown fields survive a round trip.
    pub raw: Value,
}

impl Event {
    /// Parses one decoded physical row into an event.
    pub fn from_value(value: Value) -> Option<Self> {
        let obj = value.as_object()?;
        let kind = obj.get("type")?.as_str()?.to_string();
        Some(Self {
            kind,
            seq: obj.get("seq").and_then(Value::as_i64).unwrap_or(-1),
            time: obj.get("time").and_then(Value::as_i64).unwrap_or(0),
            data: obj.get("data").cloned().unwrap_or(Value::Null),
            raw: value,
        })
    }

    /// Reads a field from the event envelope (not from `data`).
    pub fn envelope(&self, key: &str) -> Option<&Value> {
        self.raw.as_object()?.get(key)
    }

    /// Reads a field from the event payload.
    pub fn field(&self, key: &str) -> Option<&Value> {
        self.data.as_object()?.get(key)
    }
}

/// A complete DSH session log held in memory.
#[derive(Debug, Clone)]
pub struct Log {
    /// Logical session header.
    pub header: Header,
    /// Events in log order.
    pub events: Vec<Event>,
}

impl Log {
    /// Counts events by type, for the browser's detail pane.
    pub fn histogram(&self) -> BTreeMap<String, usize> {
        let mut out = BTreeMap::new();
        for event in &self.events {
            *out.entry(event.kind.clone()).or_insert(0) += 1;
        }
        out
    }

    /// Latest recorded session title, when the log set one.
    pub fn title(&self) -> Option<String> {
        self.events
            .iter()
            .rev()
            .find(|e| e.kind == "session/title")
            .and_then(|e| e.field("title").and_then(Value::as_str))
            .map(str::to_string)
    }
}

/// A DSH session discovered on disk.
#[derive(Debug, Clone)]
pub struct SessionRef {
    /// Session identifier taken from the directory name.
    pub id: String,
    /// Authoritative log file (newest generation present).
    pub path: PathBuf,
    /// Generation of `path`.
    pub generation: u64,
    /// Other generations present in the same directory.
    pub other_generations: Vec<u64>,
    /// Project bucket directory name.
    pub bucket: String,
    /// Working directory read from the header.
    pub cwd: String,
    /// Session creation time in epoch milliseconds.
    pub created_at: i64,
    /// Recorded title, when the log set one.
    pub title: Option<String>,
    /// Compressed size of `path` in bytes.
    pub size: u64,
    /// Number of events, counted during discovery.
    pub events: usize,
}

/// Decodes a zstd-framed JSONL log into its header and events.
pub fn read_log(path: &Path) -> Result<Log> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let decoder = zstd::stream::read::Decoder::new(file)
        .with_context(|| format!("zstd decode {}", path.display()))?;
    let reader = BufReader::with_capacity(1 << 20, decoder);

    let mut header: Option<Header> = None;
    let mut events = Vec::new();
    for (index, line) in reader.lines().enumerate() {
        let line = line.with_context(|| format!("read row {} of {}", index, path.display()))?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value: Value = match serde_json::from_str(trimmed) {
            Ok(value) => value,
            // A truncated tail row is normal for a session killed mid-write.
            Err(_) => continue,
        };
        if header.is_none() {
            header = Some(Header::from_value(&value)?);
            continue;
        }
        if let Some(event) = Event::from_value(value) {
            events.push(event);
        }
    }

    Ok(Log {
        header: header.ok_or_else(|| anyhow!("{} is empty", path.display()))?,
        events,
    })
}

/// Reads only the header row, without decoding the rest of the log.
pub fn read_header(path: &Path) -> Result<Header> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let decoder = zstd::stream::read::Decoder::new(file)
        .with_context(|| format!("zstd decode {}", path.display()))?;
    let mut reader = BufReader::new(decoder);
    let mut first = String::new();
    reader.read_line(&mut first)?;
    let value: Value = serde_json::from_str(first.trim())
        .with_context(|| format!("parse header of {}", path.display()))?;
    Header::from_value(&value)
}

/// Writes a zstd-framed JSONL log, creating parent directories as needed.
pub fn write_log(path: &Path, log: &Log, compression_level: i32) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create {}", parent.display()))?;
    }
    let file = File::create(path).with_context(|| format!("create {}", path.display()))?;
    let mut encoder = zstd::stream::write::Encoder::new(file, compression_level)?;
    writeln!(encoder, "{}", serde_json::to_string(&log.header.to_value())?)?;
    for event in &log.events {
        writeln!(encoder, "{}", serde_json::to_string(&event.raw)?)?;
    }
    encoder.finish()?.flush()?;
    Ok(())
}

/// Builds a physical event row from its parts.
pub fn make_event(
    kind: &str,
    seq: i64,
    time: i64,
    data: Value,
    extra: &[(&str, Value)],
) -> Event {
    let mut obj = Map::new();
    obj.insert("type".into(), json!(kind));
    obj.insert("seq".into(), json!(seq));
    obj.insert("time".into(), json!(time));
    obj.insert("data".into(), data.clone());
    for (key, value) in extra {
        obj.insert((*key).to_string(), value.clone());
    }
    Event {
        kind: kind.to_string(),
        seq,
        time,
        data,
        raw: Value::Object(obj),
    }
}

/// Lists every DSH session under `root`, newest activity first.
pub fn discover(root: &Path) -> Vec<SessionRef> {
    let mut out = Vec::new();
    let Ok(buckets) = std::fs::read_dir(root) else {
        return out;
    };
    for bucket in buckets.flatten() {
        if !bucket.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let bucket_name = bucket.file_name().to_string_lossy().to_string();
        let Ok(sessions) = std::fs::read_dir(bucket.path()) else {
            continue;
        };
        for session in sessions.flatten() {
            if !session.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            if let Some(found) = inspect_session_dir(&session.path(), &bucket_name) {
                out.push(found);
            }
        }
    }
    out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    out
}

/// Reads one session directory, picking its newest generation.
fn inspect_session_dir(dir: &Path, bucket: &str) -> Option<SessionRef> {
    let mut generations: Vec<(u64, PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if let Some(generation) = generation_of(&name) {
            generations.push((generation, entry.path()));
        }
    }
    if generations.is_empty() {
        return None;
    }
    generations.sort_by_key(|(generation, _)| *generation);
    let (generation, path) = generations.last().cloned()?;
    let others = generations
        .iter()
        .map(|(g, _)| *g)
        .filter(|g| *g != generation)
        .collect();

    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let id = dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let id = id.strip_prefix("session-").unwrap_or(&id).to_string();

    // DSH keeps the title and the event count in rows rather than the header, so
    // listing still streams the log; it only parses the rows it needs.
    let scan = scan_light(&path).ok()?;
    Some(SessionRef {
        id: if scan.header.id.is_empty() { id } else { scan.header.id.clone() },
        path,
        generation,
        other_generations: others,
        bucket: bucket.to_string(),
        cwd: scan.header.cwd.clone(),
        created_at: scan.header.created_at,
        title: scan.title,
        size,
        events: scan.events,
    })
}

/// Listing metadata gathered without decoding every row to JSON.
struct LightScan {
    header: Header,
    title: Option<String>,
    events: usize,
}

/// Streams a log, parsing only the header and any `session/title` rows.
fn scan_light(path: &Path) -> Result<LightScan> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let decoder = zstd::stream::read::Decoder::new(file)
        .with_context(|| format!("zstd decode {}", path.display()))?;
    let reader = BufReader::with_capacity(1 << 20, decoder);

    let mut header: Option<Header> = None;
    let mut title = None;
    let mut events = 0usize;
    for line in reader.lines() {
        let Ok(line) = line else { break };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if header.is_none() {
            let value: Value = serde_json::from_str(trimmed)
                .with_context(|| format!("parse header of {}", path.display()))?;
            header = Some(Header::from_value(&value)?);
            continue;
        }
        events += 1;
        if trimmed.starts_with("{\"type\":\"session/title\"") {
            if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
                if let Some(found) = value
                    .get("data")
                    .and_then(|d| d.get("title"))
                    .and_then(Value::as_str)
                {
                    title = Some(found.to_string());
                }
            }
        }
    }

    Ok(LightScan {
        header: header.ok_or_else(|| anyhow!("{} is empty", path.display()))?,
        title,
        events,
    })
}

/// Generation encoded in a DSH log file name, if the name is one.
fn generation_of(name: &str) -> Option<u64> {
    if name == "session.jsonl.zstd" {
        return Some(0);
    }
    let rest = name.strip_prefix("session.v")?.strip_suffix(".jsonl.zstd")?;
    rest.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_parsing_accepts_released_names_only() {
        assert_eq!(generation_of("session.v4.jsonl.zstd"), Some(4));
        assert_eq!(generation_of("session.jsonl.zstd"), Some(0));
        assert_eq!(generation_of("session.lock"), None);
        assert_eq!(generation_of("session.vX.jsonl.zstd"), None);
    }

    #[test]
    fn header_requires_the_session_discriminant() {
        let bad = serde_json::json!({ "type": "user/message", "id": "x" });
        assert!(Header::from_value(&bad).is_err());
    }
}
