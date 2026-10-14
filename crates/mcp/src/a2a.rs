//! Agent-to-agent roles: which tools a chat gets, which chats it may touch,
//! the on-demand usage guide, and transcripts written as files.
//!
//! The server speaks for one chat (`ZERON_CHAT_ID`). A chat with a parent is
//! a **subagent**: it can message its parent and read its family, nothing
//! else. A top-level chat is an **orchestrator**: it spawns, messages, stops
//! and watches chats, but never another orchestrator's children. Without a
//! chat (a terminal) the server is **free** and keeps the full surface.

use std::path::{Path, PathBuf};

use zeron_proto::Chat;

use crate::transcript::RenderedMessage;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Role {
    Free,
    Orchestrator { me: String },
    Subagent { me: String, parent: String },
}

/// Tools an orchestrator (and a terminal) lists.
pub const ORCHESTRATOR_TOOLS: &[&str] = &[
    "whoami",
    "zeron_help",
    "list_devices",
    "list_projects",
    "list_harnesses",
    "list_models",
    "list_chats",
    "get_chat",
    "chat_status",
    "create_chat",
    "create_chats",
    "send_message",
    "send_messages",
    "stop_chat",
    "read_chat",
    "read_subagent",
    "search_chats",
    "wait_for_turn",
    "archive_chat",
];

/// Tools a subagent lists.
pub const SUBAGENT_TOOLS: &[&str] = &["whoami", "zeron_help", "send_message", "read_chat", "get_chat"];

/// Old names still answered (never listed) so existing prompts keep working.
pub fn canonical(name: &str) -> &str {
    match name {
        "interrupt_chat" => "stop_chat",
        other => other,
    }
}

impl Role {
    pub fn tools(&self) -> &'static [&'static str] {
        match self {
            Role::Subagent { .. } => SUBAGENT_TOOLS,
            _ => ORCHESTRATOR_TOOLS,
        }
    }

    /// Whether `name` (already canonical) may be called.
    pub fn allows(&self, name: &str) -> bool {
        self.tools().contains(&name)
            || (name == "respond_to_input" && !matches!(self, Role::Subagent { .. }))
    }

    /// Steering, stopping, answering or archiving `chat`.
    pub fn may_control(&self, chat: &Chat) -> Result<(), String> {
        match self {
            Role::Free => Ok(()),
            Role::Subagent { .. } => Err("subagents can only message their parent".into()),
            Role::Orchestrator { me } => {
                if chat.id == *me {
                    return Err("that is your own chat".into());
                }
                match chat.parent_chat_id.as_deref() {
                    Some(parent) if parent != me => Err(format!(
                        "chat {} belongs to another orchestrator ({}); you can only control your own children and top-level chats",
                        crate::zeron::short(&chat.id),
                        crate::zeron::short(parent)
                    )),
                    _ => Ok(()),
                }
            }
        }
    }

    /// Reading `chat`'s transcript or metadata.
    pub fn may_read(&self, chat: &Chat) -> Result<(), String> {
        match self {
            Role::Subagent { me, parent } => {
                let family = chat.id == *me
                    || chat.id == *parent
                    || chat.parent_chat_id.as_deref() == Some(parent.as_str());
                if family {
                    Ok(())
                } else {
                    Err("subagents can read their parent, their siblings and themselves".into())
                }
            }
            _ => Ok(()),
        }
    }
}

/// The detailed guide, by topic. Tool descriptions stay one line; this is
/// where the how and why live, fetched only when an agent asks.
pub fn help(topic: Option<&str>) -> String {
    let topic = topic.map(str::trim).unwrap_or("").to_ascii_lowercase();
    let sections: &[(&str, &str)] = &[
        ("delegate", HELP_DELEGATE),
        ("messages", HELP_MESSAGES),
        ("status", HELP_STATUS),
        ("files", HELP_FILES),
    ];
    match sections.iter().find(|(name, _)| *name == topic) {
        Some((_, text)) => (*text).to_owned(),
        None => {
            let mut out = String::from(HELP_OVERVIEW);
            for (_, text) in sections {
                out.push_str("\n\n");
                out.push_str(text);
            }
            out
        }
    }
}

const HELP_OVERVIEW: &str = "\
# Zeron agent tools
Chats run coding agents. A top-level chat is an orchestrator; the chats it \
creates are its subagents. Topics: delegate, messages, status, files \
(zeron_help {topic}).";

const HELP_DELEGATE: &str = "\
## delegate
- create_chat {prompt} starts a subagent and returns at once. create_chats \
starts several together.
- The subagent does not see your conversation: write a self-contained \
prompt (goal, context, constraints, what to report).
- Its final answer comes back to you automatically as a Zeron notice when its \
turn ends. Keep working or end your turn; don't poll.
- Pick a host with project/device; harness and model from list_harnesses / \
list_models on that host.";

const HELP_MESSAGES: &str = "\
## messages
Orchestrator → subagent: send_message {chat, text}.
- Working subagent: steered at its next break (tools keep running). \
interrupt: true stops its turn and sends right away.
- Idle subagent: starts a turn. Its answer comes back as a notice.
- Waiting on a question: the text answers it (an option label picks that \
option; anything else is a free-text answer). answers: [...] for multi-question forms.
- A subagent blocked in send_message {wait: true} gets your text as its tool \
result, not a new turn.
Subagent → orchestrator: send_message {text} (no chat: it always goes to \
your parent).
- Lands in the parent's agent inbox: steered into its running turn or \
batched into its next one. Never a queue row in the composer.
- wait: false (default): keep working; the reply arrives as a message. \
wait: true: block until the parent replies (timeout_secs, default 600), \
for questions you truly can't continue without.
- Your final answer is sent automatically when your turn ends: don't \
send_message it.";

const HELP_STATUS: &str = "\
## status
chat_status {chat?} → each of your children: idle, working, awaitingInput \
(with the question), errored (with the error), or offline (host gone or \
silent). Notices arrive on their own when a child completes, errors or asks; \
use chat_status to check without reading transcripts. stop_chat {chat} \
stops a turn. wait_for_turn blocks on one chat; rarely needed.";

const HELP_FILES: &str = "\
## files
read_chat and read_subagent write the transcript to a Markdown file and \
return its path; read it with your file tools (rg, head, tail, sed -n) \
instead of loading it whole. Format:
  # <title> · <id8> · <harness> · <status>
  ## <n> <role> <time>
  <text>
  > tool <one line per tool call>
  > error <message>
  > subagent <id> <status> <label>
Messages are numbered from 1 (oldest). Reasoning is left out unless \
reasoning: true. Each call rewrites the file, so it is current; files are \
removed when the server exits.";

/// Per-process folder for transcript files, removed on exit.
pub fn files_dir() -> PathBuf {
    std::env::temp_dir()
        .join("zeron")
        .join("chats")
        .join(std::process::id().to_string())
}

/// Remove this process's transcript files.
pub fn clean_files() {
    let _ = std::fs::remove_dir_all(files_dir());
}

/// Write `messages` as the compact Markdown described in `HELP_FILES`.
pub fn write_transcript(
    dir: &Path,
    file: &str,
    header: &str,
    messages: &[RenderedMessage],
) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("{file}.md"));
    // Temp then rename: a reader never sees a half-written file.
    let tmp = dir.join(format!("{file}.md.tmp"));
    std::fs::write(&tmp, render_markdown(header, messages))?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

pub fn render_markdown(header: &str, messages: &[RenderedMessage]) -> String {
    let mut out = format!("# {header}\n");
    for (index, message) in messages.iter().enumerate() {
        let role = match message.role {
            zeron_doc::MessageRole::User => "user",
            zeron_doc::MessageRole::Assistant => "assistant",
            zeron_doc::MessageRole::System => "system",
        };
        // `2026-10-14T19:02:11Z` → `10-14 19:02`: enough to order, fewer tokens.
        let time = message
            .created_at_iso
            .get(5..16)
            .map(|t| t.replace('T', " "))
            .unwrap_or_default();
        out.push_str(&format!("\n## {} {role} {time}\n", index + 1));
        if let Some(reasoning) = &message.reasoning {
            out.push_str(&format!("> reasoning {}\n", reasoning.replace('\n', " ")));
        }
        for tool in &message.tools {
            out.push_str(&format!("> tool {tool}\n"));
        }
        for agent in &message.subagents {
            out.push_str(&format!(
                "> subagent {} {} {}\n",
                agent.id,
                agent.status.unwrap_or("unknown"),
                agent.name
            ));
        }
        for error in &message.errors {
            out.push_str(&format!("> error {error}\n"));
        }
        if message.pending_input.is_some() {
            out.push_str("> waiting on an answer (see get_chat pendingInput)\n");
        }
        let text = message.text.trim();
        if !text.is_empty() {
            out.push_str(text);
            out.push('\n');
        }
    }
    out
}

/// A file name that can't escape the folder.
pub fn file_stem(raw: &str) -> String {
    raw.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chat(id: &str, parent: Option<&str>) -> Chat {
        let mut chat: Chat = serde_json::from_value(serde_json::json!({
            "id": id, "deviceId": "d", "archived": false, "createdAt": "2026-01-01T00:00:00Z"
        }))
        .unwrap();
        chat.parent_chat_id = parent.map(str::to_owned);
        chat
    }

    #[test]
    fn subagents_get_a_small_tool_set() {
        let sub = Role::Subagent { me: "c".into(), parent: "p".into() };
        assert!(sub.allows("send_message"));
        assert!(!sub.allows("create_chat"));
        assert!(!sub.allows("respond_to_input"));
        let orch = Role::Orchestrator { me: "p".into() };
        assert!(orch.allows("create_chat"));
        assert!(orch.allows("respond_to_input"));
        assert_eq!(canonical("interrupt_chat"), "stop_chat");
    }

    #[test]
    fn orchestrators_control_their_children_and_top_level_chats_only() {
        let orch = Role::Orchestrator { me: "p".into() };
        assert!(orch.may_control(&chat("c", Some("p"))).is_ok());
        assert!(orch.may_control(&chat("t", None)).is_ok());
        assert!(orch.may_control(&chat("x", Some("other"))).is_err());
        assert!(orch.may_control(&chat("p", None)).is_err());
    }

    #[test]
    fn subagents_read_only_their_family() {
        let sub = Role::Subagent { me: "c".into(), parent: "p".into() };
        assert!(sub.may_read(&chat("p", None)).is_ok());
        assert!(sub.may_read(&chat("sibling", Some("p"))).is_ok());
        assert!(sub.may_read(&chat("c", Some("p"))).is_ok());
        assert!(sub.may_read(&chat("x", None)).is_err());
    }

    #[test]
    fn help_serves_one_topic_or_everything() {
        assert!(help(Some("files")).starts_with("## files"));
        let all = help(None);
        assert!(all.contains("## delegate") && all.contains("## status"));
    }

    #[test]
    fn file_stems_cannot_escape_the_folder() {
        assert_eq!(file_stem("../a b"), "___a_b");
    }
}
