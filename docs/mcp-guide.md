# Zeron agent-to-agent guide

How chats delegate work to each other through the Zeron MCP server: what
each side can do, how messages travel, and how to write prompts that work.
Engine internals live in [mcp.md](mcp.md).

## Roles

Every chat's agent gets the Zeron tools automatically. Which tools it sees
depends on where the chat sits:

| Role | Who | Can |
| --- | --- | --- |
| **Orchestrator** | A top-level chat | Spawn subagents, message, steer, interrupt and stop them, answer their questions, check their status, read any chat |
| **Subagent** | A chat with a parent | Message its parent, read its parent, siblings and itself |
| **Free** | `zeron mcp` run from a terminal | Everything an orchestrator can, with no attribution |

The role is read once from `ZERON_CHAT_ID` when the server starts. An
orchestrator can control its own children and other top-level chats, never
another orchestrator's children. A subagent cannot spawn chats.

## Tools

### Orchestrator

| Tool | Use |
| --- | --- |
| `create_chat` / `create_chats` | Start one or several subagents with a self-contained prompt. Returns at once. |
| `send_message` / `send_messages` | Message a chat (see [Messages](#messages)). |
| `chat_status` | Every child's status in one call, or one chat's. |
| `stop_chat` | Stop a chat's running turn. |
| `read_chat` | Write a chat's transcript to a Markdown file and return the path. |
| `read_subagent` | List a chat's harness subagents, or write one's transcript to a file. |
| `search_chats` | Search every transcript by words (typo-tolerant) or regex. |
| `get_chat`, `list_chats` | Metadata, status, last message, pending question. |
| `list_devices`, `list_projects`, `list_harnesses`, `list_models` | Pick where a subagent runs and with what. |
| `wait_for_turn` | Block on one chat's turn. Rarely needed. |
| `archive_chat` | Tidy up finished children. |
| `whoami`, `zeron_help` | Your role and pending results; the detailed guide. |

### Subagent

| Tool | Use |
| --- | --- |
| `send_message` | Message your parent. No `chat` argument: it always goes to the parent. |
| `read_chat` | Your parent's, a sibling's or your own transcript, as a file. |
| `get_chat` | Metadata for the same chats. |
| `whoami`, `zeron_help` | Your parent; the detailed guide. |

`interrupt_chat` and `respond_to_input` still work for old prompts but are
no longer listed: use `stop_chat` and `send_message`.

## Messages

### Orchestrator → subagent

`send_message { chat, text }` picks the delivery from the subagent's state:

| Subagent is | What happens |
| --- | --- |
| Working | Steered into its turn at the next break; its tools keep running. |
| Working, and you pass `interrupt: true` | Its turn is stopped, then the message is sent. |
| Idle | A new turn starts with your message. |
| Waiting on a question | Your text answers it. Text matching an option label picks that option; anything else is a free-text answer. Use `answers: [{question_id, labels}]` for multi-question forms. |
| Blocked in its own `send_message { wait: true }` | Your text becomes the result of that call. No new turn. |

The subagent's final response comes back to you as an automatic
`[Zeron notice …]` when its turn ends. Don't poll or ask it how it's going.

### Subagent → orchestrator

`send_message { text }` puts the message in the parent's **agent inbox**:

- Parent working: steered into its turn at the next break.
- Parent idle: starts a turn (several messages arriving together are batched).
- It never becomes a row in the parent's composer queue.

Choose whether to wait:

- `wait: false` (default): keep working. A reply arrives later as a message
  in your chat.
- `wait: true`: block until the parent answers; the answer is the tool
  result. `timeout_secs` defaults to 600. On timeout you get
  `timedOut: true` and should continue with your best judgement.

Your final answer goes to the parent automatically when your turn ends. Use
`send_message` for questions and findings the parent needs now, not for the
final report or progress pings.

## Status

`chat_status` returns each child as one of:

| Status | Meaning |
| --- | --- |
| `idle` | Finished or not started. |
| `working` | Running a turn. |
| `awaitingInput` | Blocked on a question, included as `question`. Answer with `send_message`. |
| `errored` | The turn failed, with `error`. |
| `offline` | Says working but its host has been silent past the staleness window (45 s): the host is gone or crashed. |

Notices arrive on their own when a child completes, errors or asks, so
`chat_status` is for checking in, not for polling.

## Transcript files

`read_chat` and `read_subagent` return a path instead of the transcript:

```json
{ "path": "…/Temp/zeron/chats/<pid>/<n>/<chat-id>.md", "messages": 42, "status": "idle" }
```

Read it with file tools (`rg`, `head`, `tail`, `sed -n`). The format keeps
tokens low:

```markdown
# Fix login bug · 9f3a1c2e · claude-code · idle

## 1 user 10-14 14:02
Check the auth flow and fix the token refresh.

## 2 assistant 10-14 14:03
> tool Read src/auth.rs
> tool Bash cargo test (exit 0)
Found it: the refresh token was read before the lock.
```

- Messages are numbered from 1, oldest first.
- `> tool`, `> subagent`, `> error` lines carry the non-prose parts.
- Reasoning is left out unless `reasoning: true`.
- Each call rewrites the file; files are deleted when the MCP server exits.

## Writing a good delegation

The subagent starts with none of your context. Give it:

1. **Goal**: what done looks like.
2. **Context**: files, branches, decisions already made.
3. **Constraints**: what not to touch, which tools or models to use.
4. **Report**: what to put in its final message.

```json
{
  "prompt": "Goal: make `cargo test -p zeron-mcp` pass on Windows.\nContext: failures are in crates/mcp/src/tools.rs tests; transcript files moved to per-instance folders in a2a.rs.\nConstraints: don't change public tool names.\nReport: the root cause in one sentence and the files you changed.",
  "title": "Fix MCP tests"
}
```

For parallel work use `create_chats` with one request per independent task,
then end your turn: each result arrives as its own notice.

## Examples

Spawn two subagents and collect results as they finish:

```text
create_chats { requests: [
  { title: "Review API", prompt: "…" },
  { title: "Review tests", prompt: "…" }
] }
→ end the turn; two notices arrive later
```

A subagent asks and waits:

```text
send_message { text: "Keep the v1 endpoint or remove it?", wait: true }
→ { "reply": "Keep it, mark it deprecated." }
```

Redirect a working subagent without stopping it, then check on everyone:

```text
send_message { chat: "Review API", text: "Skip the admin routes." }
chat_status {}
```
