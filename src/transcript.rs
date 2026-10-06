//! Readable projection of either format: the browser's preview pane and the
//! Markdown export share this one rendering.

use serde_json::Value;

use crate::convert::iso;
use crate::dsh;
use crate::pi;

/// Who produced one transcript block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Speaker {
    /// A human prompt.
    User,
    /// Model prose.
    Assistant,
    /// Model reasoning.
    Thinking,
    /// An outgoing tool call.
    ToolCall,
    /// A tool's returned output.
    ToolResult,
    /// System or developer instruction.
    System,
    /// Session metadata: titles, model changes, compactions.
    Meta,
}

impl Speaker {
    /// Short label shown in front of a block.
    pub fn label(self) -> &'static str {
        match self {
            Speaker::User => "user",
            Speaker::Assistant => "assistant",
            Speaker::Thinking => "thinking",
            Speaker::ToolCall => "tool",
            Speaker::ToolResult => "result",
            Speaker::System => "system",
            Speaker::Meta => "meta",
        }
    }
}

/// One readable transcript block.
#[derive(Debug, Clone)]
pub struct Block {
    /// Producer of this block.
    pub speaker: Speaker,
    /// One-line heading, for example a tool name.
    pub heading: String,
    /// Block body, already flattened to text.
    pub body: String,
    /// ISO-8601 time of the source row.
    pub time: String,
}

/// Flattens a content-block array to text, naming blocks that carry no text.
fn flatten(content: &Value) -> String {
    if let Some(text) = content.as_str() {
        return text.to_string();
    }
    let Some(items) = content.as_array() else {
        return String::new();
    };
    let mut out = Vec::new();
    for item in items {
        match item.get("type").and_then(Value::as_str) {
            Some("text") | Some("reasoning") => {
                out.push(item.get("text").and_then(Value::as_str).unwrap_or("").to_string());
            }
            Some("thinking") => {
                out.push(item.get("thinking").and_then(Value::as_str).unwrap_or("").to_string());
            }
            Some(other) => out.push(format!("[{other}]")),
            None => {}
        }
    }
    out.join("\n")
}

/// Renders a tool call as `name(arguments)` with the arguments pretty-printed.
fn render_call(name: &str, arguments: &Value) -> (String, String) {
    let body = match arguments {
        Value::String(raw) => serde_json::from_str::<Value>(raw)
            .map(|parsed| serde_json::to_string_pretty(&parsed).unwrap_or_else(|_| raw.clone()))
            .unwrap_or_else(|_| raw.clone()),
        other => serde_json::to_string_pretty(other).unwrap_or_default(),
    };
    (name.to_string(), body)
}

/// Builds a readable transcript from a Pi session file.
pub fn from_pi(file: &pi::SessionFile) -> Vec<Block> {
    let mut out = Vec::new();
    for entry in &file.entries {
        let time = entry
            .get("timestamp")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let kind = entry.get("type").and_then(Value::as_str).unwrap_or("");
        match kind {
            "message" => {
                let message = entry.get("message").unwrap_or(&Value::Null);
                match message.get("role").and_then(Value::as_str).unwrap_or("") {
                    "user" => out.push(Block {
                        speaker: Speaker::User,
                        heading: String::new(),
                        body: flatten(message.get("content").unwrap_or(&Value::Null)),
                        time,
                    }),
                    "developer" => out.push(Block {
                        speaker: Speaker::System,
                        heading: "developer".into(),
                        body: flatten(message.get("content").unwrap_or(&Value::Null)),
                        time,
                    }),
                    "assistant" => {
                        let model = message.get("model").and_then(Value::as_str).unwrap_or("");
                        let empty: Vec<Value> = Vec::new();
                        for block in message
                            .get("content")
                            .and_then(Value::as_array)
                            .unwrap_or(&empty)
                        {
                            match block.get("type").and_then(Value::as_str) {
                                Some("thinking") => out.push(Block {
                                    speaker: Speaker::Thinking,
                                    heading: String::new(),
                                    body: block
                                        .get("thinking")
                                        .and_then(Value::as_str)
                                        .unwrap_or("")
                                        .to_string(),
                                    time: time.clone(),
                                }),
                                Some("text") => out.push(Block {
                                    speaker: Speaker::Assistant,
                                    heading: model.to_string(),
                                    body: block
                                        .get("text")
                                        .and_then(Value::as_str)
                                        .unwrap_or("")
                                        .to_string(),
                                    time: time.clone(),
                                }),
                                Some("toolCall") => {
                                    let (heading, body) = render_call(
                                        block.get("name").and_then(Value::as_str).unwrap_or(""),
                                        block.get("arguments").unwrap_or(&Value::Null),
                                    );
                                    out.push(Block {
                                        speaker: Speaker::ToolCall,
                                        heading,
                                        body,
                                        time: time.clone(),
                                    });
                                }
                                _ => {}
                            }
                        }
                    }
                    "toolResult" => out.push(Block {
                        speaker: Speaker::ToolResult,
                        heading: message
                            .get("toolName")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        body: flatten(message.get("content").unwrap_or(&Value::Null)),
                        time,
                    }),
                    _ => {}
                }
            }
            "custom_message" => out.push(Block {
                speaker: Speaker::System,
                heading: entry
                    .get("customType")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                body: flatten(entry.get("content").unwrap_or(&Value::Null)),
                time,
            }),
            "session_init" => out.push(Block {
                speaker: Speaker::System,
                heading: "system prompt".into(),
                body: entry
                    .get("systemPrompt")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                time,
            }),
            "title_change" => out.push(Block {
                speaker: Speaker::Meta,
                heading: "title".into(),
                body: entry.get("title").and_then(Value::as_str).unwrap_or("").to_string(),
                time,
            }),
            "model_change" => out.push(Block {
                speaker: Speaker::Meta,
                heading: "model".into(),
                body: entry.get("model").and_then(Value::as_str).unwrap_or("").to_string(),
                time,
            }),
            "compaction" => out.push(Block {
                speaker: Speaker::Meta,
                heading: "compaction".into(),
                body: entry.get("summary").and_then(Value::as_str).unwrap_or("").to_string(),
                time,
            }),
            _ => {}
        }
    }
    out
}

/// Builds a readable transcript from a DSH log.
pub fn from_dsh(log: &dsh::Log) -> Vec<Block> {
    let mut out = Vec::new();
    for event in &log.events {
        let time = iso(event.time);
        match event.kind.as_str() {
            "user/message" | "developer/message" => {
                let message = event.field("message").cloned().unwrap_or(Value::Null);
                let source = message
                    .get("source")
                    .and_then(|s| s.get("kind"))
                    .and_then(Value::as_str)
                    .unwrap_or("user");
                let speaker = if event.kind == "user/message" && source == "user" {
                    Speaker::User
                } else {
                    Speaker::System
                };
                out.push(Block {
                    speaker,
                    heading: if speaker == Speaker::User {
                        String::new()
                    } else {
                        source.to_string()
                    },
                    body: flatten(message.get("content").unwrap_or(&Value::Null)),
                    time,
                });
            }
            "system/message" => out.push(Block {
                speaker: Speaker::System,
                heading: "system prompt".into(),
                body: flatten(
                    event
                        .field("message")
                        .and_then(|m| m.get("content"))
                        .unwrap_or(&Value::Null),
                ),
                time,
            }),
            "assistant/message" => {
                let message = event.field("message").cloned().unwrap_or(Value::Null);
                let model = message
                    .get("source")
                    .and_then(|s| s.get("model"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let empty: Vec<Value> = Vec::new();
                for block in message
                    .get("content")
                    .and_then(Value::as_array)
                    .unwrap_or(&empty)
                {
                    match block.get("type").and_then(Value::as_str) {
                        Some("reasoning") => out.push(Block {
                            speaker: Speaker::Thinking,
                            heading: String::new(),
                            body: block.get("text").and_then(Value::as_str).unwrap_or("").to_string(),
                            time: time.clone(),
                        }),
                        Some("text") => out.push(Block {
                            speaker: Speaker::Assistant,
                            heading: model.clone(),
                            body: block.get("text").and_then(Value::as_str).unwrap_or("").to_string(),
                            time: time.clone(),
                        }),
                        Some("tool-call") => {
                            let (heading, body) = render_call(
                                block.get("name").and_then(Value::as_str).unwrap_or(""),
                                block.get("arguments").unwrap_or(&Value::Null),
                            );
                            out.push(Block {
                                speaker: Speaker::ToolCall,
                                heading,
                                body,
                                time: time.clone(),
                            });
                        }
                        _ => {}
                    }
                }
            }
            "tool/result" => {
                let message = event.field("message").cloned().unwrap_or(Value::Null);
                let content = if message.get("role").and_then(Value::as_str) == Some("tool") {
                    message.get("content").cloned().unwrap_or(Value::Null)
                } else {
                    message
                        .get("content")
                        .and_then(Value::as_array)
                        .and_then(|items| items.first())
                        .and_then(|block| block.get("content").cloned())
                        .unwrap_or(Value::Null)
                };
                out.push(Block {
                    speaker: Speaker::ToolResult,
                    heading: String::new(),
                    body: flatten(&content),
                    time,
                });
            }
            "session/title" => out.push(Block {
                speaker: Speaker::Meta,
                heading: "title".into(),
                body: event.field("title").and_then(Value::as_str).unwrap_or("").to_string(),
                time,
            }),
            "model/selection" => out.push(Block {
                speaker: Speaker::Meta,
                heading: "model".into(),
                body: format!(
                    "{}/{}",
                    event.field("provider").and_then(Value::as_str).unwrap_or(""),
                    event.field("model").and_then(Value::as_str).unwrap_or("")
                ),
                time,
            }),
            "compaction/summary" => out.push(Block {
                speaker: Speaker::Meta,
                heading: "compaction".into(),
                body: flatten(event.field("summary").unwrap_or(&Value::Null)),
                time,
            }),
            _ => {}
        }
    }
    out
}

/// Renders a transcript as Markdown.
pub fn to_markdown(title: &str, cwd: &str, blocks: &[Block]) -> String {
    let mut out = String::new();
    out.push_str(&format!("# {}\n\n", if title.is_empty() { "(untitled session)" } else { title }));
    out.push_str(&format!("`{cwd}`\n\n"));
    for block in blocks {
        match block.speaker {
            Speaker::User => out.push_str(&format!("## user · {}\n\n{}\n\n", block.time, block.body)),
            Speaker::Assistant => out.push_str(&format!(
                "### assistant{}\n\n{}\n\n",
                if block.heading.is_empty() {
                    String::new()
                } else {
                    format!(" · {}", block.heading)
                },
                block.body
            )),
            Speaker::Thinking => {
                out.push_str(&format!("<details><summary>thinking</summary>\n\n{}\n\n</details>\n\n", block.body))
            }
            Speaker::ToolCall => out.push_str(&format!(
                "**→ {}**\n\n```json\n{}\n```\n\n",
                block.heading, block.body
            )),
            Speaker::ToolResult => out.push_str(&format!(
                "<details><summary>result{}</summary>\n\n```\n{}\n```\n\n</details>\n\n",
                if block.heading.is_empty() {
                    String::new()
                } else {
                    format!(" · {}", block.heading)
                },
                block.body
            )),
            Speaker::System => out.push_str(&format!(
                "<details><summary>{}</summary>\n\n{}\n\n</details>\n\n",
                if block.heading.is_empty() { "system" } else { &block.heading },
                block.body
            )),
            Speaker::Meta => out.push_str(&format!("> _{}_: {}\n\n", block.heading, block.body)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pi_assistant_blocks_split_by_speaker() {
        let file = pi::SessionFile {
            title: "t".into(),
            title_updated_at: String::new(),
            header: pi::make_header("x", "2026-01-01T00:00:00.000Z", "/w", &[]),
            entries: vec![json!({
                "type": "message",
                "timestamp": "2026-01-01T00:00:01.000Z",
                "message": {
                    "role": "assistant",
                    "model": "m",
                    "content": [
                        {"type": "thinking", "thinking": "hmm"},
                        {"type": "text", "text": "done"},
                        {"type": "toolCall", "name": "bash", "arguments": {"command": "ls"}}
                    ]
                }
            })],
        };
        let blocks = from_pi(&file);
        let speakers: Vec<Speaker> = blocks.iter().map(|b| b.speaker).collect();
        assert_eq!(speakers, [Speaker::Thinking, Speaker::Assistant, Speaker::ToolCall]);
        assert!(blocks[2].body.contains("\"command\""));
    }

    #[test]
    fn markdown_keeps_user_turns_as_headings() {
        let blocks = vec![Block {
            speaker: Speaker::User,
            heading: String::new(),
            body: "hi".into(),
            time: "2026-01-01T00:00:00.000Z".into(),
        }];
        let md = to_markdown("Title", "/w", &blocks);
        assert!(md.starts_with("# Title"));
        assert!(md.contains("## user · 2026-01-01T00:00:00.000Z"));
    }
}
