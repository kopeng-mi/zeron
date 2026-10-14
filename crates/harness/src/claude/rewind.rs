//! Where to cut a Claude session for a rewind.
//!
//! The CLI resumes a session cut back at a message with
//! `--resume-session-at=<uuid>` (in place: same session id, and later plain
//! resumes follow the cut). The uuid must name a message record (`user` or
//! `assistant`) — other record kinds (`attachment`, `system`) are rejected
//! with "No message found". Zeron tags each prompt's user line with the
//! chat's message id ([`super::turn_uuid`]), so the turn to forget is found
//! by id; prompts sent before that tagging are matched by text.

use std::collections::HashMap;

use serde_json::Value;

/// One record's links, kept small: sessions run to hundreds of MB.
struct Record {
    parent: Option<String>,
    message: bool,
}

/// Where to resume so the turn `message_id` opened, and everything after
/// it, is forgotten. `Ok(Some(uuid))`: resume at that message. `Ok(None)`:
/// the turn opened the session, so start a fresh one. `Err`: the turn is
/// not in this session.
pub(crate) fn rewind_point<'a>(
    lines: impl Iterator<Item = &'a str>,
    message_id: &str,
    prompt: &str,
) -> Result<Option<String>, String> {
    let prompt = prompt.trim();
    let mut records: HashMap<String, Record> = HashMap::new();
    let mut by_id = None;
    let mut by_text = None;
    for line in lines {
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(uuid) = record.get("uuid").and_then(Value::as_str) else {
            continue;
        };
        let kind = record.get("type").and_then(Value::as_str).unwrap_or("");
        let parent = record
            .get("parentUuid")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let sidechain = record.get("isSidechain").and_then(Value::as_bool) == Some(true);
        if kind == "user" && !sidechain {
            if uuid == message_id {
                by_id = Some(uuid.to_owned());
            } else if !prompt.is_empty() && user_text(&record).is_some_and(|t| t.contains(prompt))
            {
                // Latest wins: an edit targets the newest copy of a
                // repeated prompt more often than an older one.
                by_text = Some(uuid.to_owned());
            }
        }
        records.insert(
            uuid.to_owned(),
            Record {
                parent,
                message: kind == "user" || kind == "assistant",
            },
        );
    }
    let target = by_id
        .or(by_text)
        .ok_or_else(|| "this message isn't in Claude's saved session".to_string())?;
    let mut at = records.get(&target).and_then(|r| r.parent.clone());
    // Bounded: a malformed file must not loop forever on a parent cycle.
    for _ in 0..records.len() {
        let Some(id) = at else {
            return Ok(None);
        };
        match records.get(&id) {
            Some(record) if record.message => return Ok(Some(id)),
            Some(record) => at = record.parent.clone(),
            None => return Ok(None),
        }
    }
    Err("Claude's saved session has a broken message chain".into())
}

/// A user record's typed text: a plain string, or its text blocks joined.
/// Tool results and other block kinds are not prompts.
fn user_text(record: &Value) -> Option<String> {
    if record.get("isMeta").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    match record.pointer("/message/content")? {
        Value::String(text) => Some(text.clone()),
        Value::Array(blocks) => {
            let text: Vec<&str> = blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect();
            (!text.is_empty()).then(|| text.join("\n"))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape the CLI wrote in a live 2.1.296 session (trimmed).
    const SESSION: &str = r#"{"type":"queue-operation"}
{"type":"user","uuid":"u1","parentUuid":null,"message":{"role":"user","content":"Remember ALPHA."}}
{"type":"attachment","uuid":"at1","parentUuid":"u1"}
{"type":"assistant","uuid":"a1","parentUuid":"at1","message":{"content":[{"type":"text","text":"OK"}]}}
{"type":"attachment","uuid":"at2","parentUuid":"a1"}
{"type":"last-prompt"}
{"type":"user","uuid":"u2","parentUuid":"at2","message":{"role":"user","content":"Also remember BRAVO."}}
{"type":"assistant","uuid":"a2","parentUuid":"u2","message":{"content":[{"type":"text","text":"OK"}]}}"#;

    #[test]
    fn cuts_at_the_last_message_before_the_turn() {
        assert_eq!(
            rewind_point(SESSION.lines(), "u2", "ignored"),
            Ok(Some("a1".into()))
        );
    }

    #[test]
    fn the_opening_turn_starts_fresh() {
        assert_eq!(rewind_point(SESSION.lines(), "u1", ""), Ok(None));
    }

    #[test]
    fn untagged_prompts_match_by_text() {
        assert_eq!(
            rewind_point(SESSION.lines(), "zeron-id", "  Also remember BRAVO.\n"),
            Ok(Some("a1".into()))
        );
    }

    #[test]
    fn a_missing_turn_is_an_error() {
        assert!(rewind_point(SESSION.lines(), "nope", "never sent").is_err());
    }

    #[test]
    fn tool_results_are_not_prompts() {
        let session = r#"{"type":"user","uuid":"u1","parentUuid":null,"message":{"content":"go"}}
{"type":"assistant","uuid":"a1","parentUuid":"u1","message":{"content":[]}}
{"type":"user","uuid":"t1","parentUuid":"a1","message":{"content":[{"type":"tool_result","content":"go"}]}}"#;
        assert_eq!(rewind_point(session.lines(), "x", "go"), Ok(None));
    }
}
