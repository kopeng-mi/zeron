// Zeron's read-only Claude Code mod: at the end of each main turn it logs one
// `zeron:` JSON line (a `system`/`ui_log` frame on Zeron's stdout stream) with
// the exact context use, session cost, the turn's prompt-cache totals
// (subagents included) and an estimated breakdown of what fills the context.
// It never changes a prompt, a request or a tool call.

// Characters per token for the breakdown estimate; `other` (tool schemas,
// tool output, formatting) takes whatever the measured total leaves.
const CHARS_PER_TOKEN = 4

let cache = { read: 0, write: 0, input: 0 }
let system = 0
let memory = 0
let skills = 0
let reminders = 0

const len = (text) => (typeof text === 'string' ? text.length : 0)
const tokens = (chars) => Math.round(chars / CHARS_PER_TOKEN)

export function register(on) {
  on('turn.step', async function* ($, e, next) {
    const result = yield* next(e)
    const u = result && result.usage
    if (u) {
      cache.read += u.cache_read_input_tokens || 0
      cache.write += u.cache_creation_input_tokens || 0
      cache.input += u.input_tokens || 0
    }
    return result
  })

  on('prompt.compose', async ($, e, next) => {
    const result = await next(e)
    system = ((result && result.sections) || []).reduce((n, s) => n + len(s.text), 0)
    return result
  })

  on('prompt.context', async ($, e, next) => {
    const result = await next(e)
    memory = ((result && result.blocks) || []).reduce((n, b) => n + len(b.text), 0)
    return result
  })

  on('prompt.attachment', async ($, e, next) => {
    const result = await next(e)
    const text = result && 'text' in result ? result.text : e.text
    if (e.type === 'skill_listing' || e.type === 'agent_listing_delta') skills += len(text)
    else reminders += len(text)
    return result
  })

  on('turn.complete', async ($, e, next) => {
    const result = await next(e)
    // A subagent's turn already added its requests to `cache`.
    if (e.agentId) return result
    try {
      const usage = await $.session.usage()
      let tools = 0
      let mcp = 0
      for (const tool of await $.tool.list()) {
        const size = len(tool.name) + len(tool.description)
        if (tool.mcp) mcp += size
        else tools += size
      }
      let chat = 0
      for (const m of await $.session.messages()) {
        chat += len(m.text) + (m.toolUses && m.toolUses.length ? JSON.stringify(m.toolUses).length : 0)
      }
      const fixed = {
        system: tokens(system),
        memory: tokens(memory),
        tools: tokens(tools),
        mcp: tokens(mcp),
        skills: tokens(skills),
        reminders: tokens(reminders),
        messages: tokens(chat),
      }
      const total = usage.context && usage.context.tokens
      const used = Object.values(fixed).reduce((n, v) => n + v, 0)
      const line = {
        v: 1,
        context: usage.context,
        cost: usage.cost,
        cache,
        breakdown: { ...fixed, other: total ? Math.max(0, total - used) : 0 },
      }
      $.ui.log('zeron:' + JSON.stringify(line))
    } catch (err) {
      $.ui.log('zeron-error:' + String(err))
    }
    cache = { read: 0, write: 0, input: 0 }
    return result
  })
}
