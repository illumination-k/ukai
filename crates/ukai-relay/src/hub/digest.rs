//! A session's recorded history read for a secretary: the messages and tool
//! calls, chunks joined, rather than the `session/update`s themselves.

use serde_json::Value;
use ukai_acp::secretary::{ENTRY_CHARS, Entry, EntryKind, SUMMARY_CHARS, clip};

/// The entries of `history`, oldest first, each clipped.
pub fn entries(history: &[Value]) -> Vec<Entry> {
    let mut entries: Vec<Entry> = Vec::new();
    for message in history {
        let Some(update) = message.pointer("/params/update") else {
            continue;
        };
        let kind = match update.get("sessionUpdate").and_then(Value::as_str) {
            Some("user_message_chunk") => EntryKind::User,
            Some("agent_message_chunk") => EntryKind::Agent,
            Some("tool_call") => {
                let title = update
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("a tool call");
                entries.push(Entry {
                    kind: EntryKind::Tool,
                    text: title.to_owned(),
                });
                continue;
            }
            _ => continue,
        };
        let text = update.get("content").map(content_text).unwrap_or_default();
        match entries.last_mut() {
            Some(last) if last.kind == kind => last.text.push_str(&text),
            _ => entries.push(Entry { kind, text }),
        }
    }
    entries.retain(|entry| !entry.text.trim().is_empty());
    for entry in &mut entries {
        entry.text = clip(&entry.text, ENTRY_CHARS);
    }
    entries
}

/// The last `limit` entries of `history`.
pub fn tail(history: &[Value], limit: usize) -> Vec<Entry> {
    let mut all = entries(history);
    let start = all.len().saturating_sub(limit);
    all.split_off(start)
}

/// The start of the last prompt and of the last reply in `history`.
pub fn last_prompt_and_reply(history: &[Value]) -> (Option<String>, Option<String>) {
    let all = entries(history);
    let last = |kind| {
        all.iter()
            .rev()
            .find(|entry| entry.kind == kind)
            .map(|entry| clip(&entry.text, SUMMARY_CHARS))
    };
    (last(EntryKind::User), last(EntryKind::Agent))
}

/// The session's title from its latest `session_info_update` that has one.
pub fn title(history: &[Value]) -> Option<String> {
    history.iter().rev().find_map(|message| {
        let update = message.pointer("/params/update")?;
        (update.get("sessionUpdate")?.as_str()? == "session_info_update")
            .then(|| update.get("title")?.as_str().map(str::to_owned))
            .flatten()
    })
}

fn content_text(content: &Value) -> String {
    match content.get("type").and_then(Value::as_str) {
        Some("text") => content
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        Some("resource_link") => format!(
            "[{}]",
            content
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("a file")
        ),
        Some("image") => "[an image]".to_owned(),
        Some("resource") => "[an attached file]".to_owned(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn update(update: Value) -> Value {
        json!({ "jsonrpc": "2.0", "method": "session/update", "params": { "sessionId": "s", "update": update } })
    }

    fn chunk(kind: &str, text: &str) -> Value {
        update(json!({ "sessionUpdate": kind, "content": { "type": "text", "text": text } }))
    }

    fn history() -> Vec<Value> {
        vec![
            chunk("user_message_chunk", "fix "),
            chunk("user_message_chunk", "the build"),
            chunk("agent_message_chunk", "Looking"),
            chunk("agent_message_chunk", " at it."),
            update(
                json!({ "sessionUpdate": "tool_call", "toolCallId": "t", "title": "Run cargo build" }),
            ),
            update(
                json!({ "sessionUpdate": "tool_call_update", "toolCallId": "t", "status": "completed" }),
            ),
            chunk("agent_message_thought_chunk", "hmm"),
            chunk("agent_message_chunk", "Fixed."),
            update(json!({ "sessionUpdate": "session_info_update", "title": "Build fix" })),
        ]
    }

    #[test]
    fn joins_chunks_and_keeps_tool_calls() {
        let kinds_and_texts: Vec<_> = entries(&history())
            .into_iter()
            .map(|entry| (entry.kind, entry.text))
            .collect();
        assert_eq!(
            kinds_and_texts,
            [
                (EntryKind::User, "fix the build".to_owned()),
                (EntryKind::Agent, "Looking at it.".to_owned()),
                (EntryKind::Tool, "Run cargo build".to_owned()),
                (EntryKind::Agent, "Fixed.".to_owned()),
            ]
        );
    }

    #[test]
    fn the_tail_is_the_newest_entries() {
        let texts: Vec<_> = tail(&history(), 2).into_iter().map(|e| e.text).collect();
        assert_eq!(texts, ["Run cargo build", "Fixed."]);
        assert_eq!(tail(&history(), 100).len(), 4);
    }

    #[test]
    fn summarizes_the_last_prompt_reply_and_title() {
        assert_eq!(
            last_prompt_and_reply(&history()),
            (Some("fix the build".into()), Some("Fixed.".into()))
        );
        assert_eq!(title(&history()), Some("Build fix".into()));
        assert_eq!(last_prompt_and_reply(&[]), (None, None));
        assert_eq!(title(&[]), None);
    }

    #[test]
    fn non_text_content_is_named() {
        let history = [update(json!({
            "sessionUpdate": "user_message_chunk",
            "content": { "type": "resource_link", "name": "log.txt", "uri": "file:///log.txt" }
        }))];
        assert_eq!(entries(&history)[0].text, "[log.txt]");
    }
}
