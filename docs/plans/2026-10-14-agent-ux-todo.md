# Agent UX to-do (fork)

Open work from the agent-to-agent and status-line sessions, in order.

## Agent-to-agent

- [x] **Steered-in marker.** A message steered into a running turn through
  the agent inbox (`DeliverEvent` → steer) leaves no transcript entry. Record
  a small system entry so the transcript shows `↳ Message from X steered in`.
- [x] **Waiting state on rows.** A subagent blocked in
  `send_message { wait: true }` lives only in the engine's `AgentReplies`.
  Sync it (session field or doc flag) so its sidebar row and the parent's
  tree line show amber "waiting on you".
- [ ] **Live end-to-end check** with two harnesses (Claude orchestrator,
  Pi or Codex subagent): ask with `wait: true`, reply, steer, interrupt,
  `chat_status` showing offline when the host dies.

## Status line

Show these in each turn's footer, in the muted footer style (decided: under
the turn footer, e.g. `42 MB · cache 91% hit · warm 3m left`):

- [x] **Memory** of the spawned agent's process tree (resident set), sampled
  while the turn runs.
- [x] **Cache hit / miss** per turn from usage: `cache_read_input_tokens`
  vs `cache_creation_input_tokens` + `input_tokens` (Claude; other harnesses
  where they report it).
- [x] **Cache warm countdown** for Claude. Read the TTL from the usage
  (`cache_creation.ephemeral_1h_input_tokens` vs `ephemeral_5m_input_tokens`):
  subscriptions within included usage get 1 hour, otherwise 5 minutes
  ([docs](https://code.claude.com/docs/en/prompt-caching)). Count down from
  the last turn's end; no keep-warm pings.

## Earlier leftovers

- [ ] Rewind: live check in the app (Edit on an earlier message, ↑ recall,
  Esc list) and confirm Claude forgets the cut turns.
- [ ] Engine Windows test failures (ConPTY x5, `list_drives`): investigate.
- [ ] Open upstream PRs not merged: #779, #663, #717, #701, #705, #566, #688,
  #898, #880.
