//! The tool catalog and its dispatch.
//!
//! Every tool is a thin composition of engine reads/writes from
//! [`Zeron`]; the only logic that lives here is argument resolution (chat
//! by prefix, project by path), sender attribution, and the "how do I
//! deliver a message to a chat in this state" choice the composer makes
//! for humans.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use zeron_doc::{MessageRole, SessionCommandPayload};
use zeron_proto::{
    Chat, ChatConfig, HarnessId, ReasoningLevel, RunRequest, SandboxLevel, Session, SessionStatus,
    Space, UserInputAnswer,
};

use crate::a2a::{self, Role};
use crate::search::{Matcher, rank};
use crate::transcript::{RenderOptions, RenderedMessage, SubagentRef, render_entries};
use crate::zeron::{HarnessInfo, MessageTurn, TurnOutcome, Zeron, session_for, short};
use zeron_rpc::methods;

/// Default and ceiling for the blocking waits.
const MAX_BATCH: usize = 32;
/// Transcripts fetched at once by `search_chats`; each is a short-lived watch.
const SEARCH_CONCURRENCY: usize = 8;
const SEARCH_DEFAULT_SCAN: usize = 200;
const SEARCH_MAX_SCAN: usize = 1000;
const DEFAULT_WAIT: Duration = Duration::from_secs(600);
const MAX_WAIT: Duration = Duration::from_secs(3600);
/// A session row older than this is not trusted to still be working
/// (the UI's staleness window): a crashed host must not read as busy forever.
const SESSION_STALE: chrono::Duration = chrono::Duration::seconds(45);

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: Value,
}

pub struct Tools {
    zeron: Arc<Zeron>,
    // Remember successful sends on this MCP connection so wait_for_turn after
    // wait:false also waits for a newly created chat with no session row yet.
    pending_turns: tokio::sync::Mutex<HashMap<String, Arc<PendingTurn>>>,
    /// Orchestrator, subagent or free: read once from the origin chat.
    role: tokio::sync::OnceCell<Role>,
    /// Where read_chat / read_subagent write transcript files.
    files: std::path::PathBuf,
}

fn chat_key_schema(extra: Value) -> Value {
    let mut properties = json!({
        "chat": {
            "type": "string",
            "description": "Chat id, unique id prefix, or exact title."
        }
    });
    if let (Some(base), Some(more)) = (properties.as_object_mut(), extra.as_object()) {
        for (k, v) in more {
            base.insert(k.clone(), v.clone());
        }
    }
    json!({ "type": "object", "properties": properties, "required": ["chat"] })
}

fn device_schema() -> Value {
    json!({ "type": "string", "description": "Host device id or exact name. Omit for the local engine." })
}

fn catalog() -> Vec<ToolDef> {
    let read_args = |extra: Value| {
        let mut props = json!({
            "reasoning": { "type": "boolean", "default": false, "description": "Include model thinking." },
            "tools": { "type": "boolean", "default": true, "description": "Include one line per tool call." }
        });
        if let (Some(props), Some(extra)) = (props.as_object_mut(), extra.as_object()) {
            props.extend(extra.clone());
        }
        props
    };
    let mut tools = vec![
        ToolDef {
            name: "whoami",
            description: "Your chat, role (orchestrator or subagent), parent, and the chats whose results you're waiting on.",
            input_schema: json!({ "type": "object", "properties": {} }),
        },
        ToolDef {
            name: "zeron_help",
            description: "Detailed usage guide. Topics: delegate, messages, status, files. Omit topic for all.",
            input_schema: json!({ "type": "object", "properties": {
                "topic": { "type": "string", "enum": ["delegate", "messages", "status", "files"] }
            }}),
        },
        ToolDef {
            name: "list_devices",
            description: "Devices in the workspace; chats and projects live on one.",
            input_schema: json!({ "type": "object", "properties": {} }),
        },
        ToolDef {
            name: "list_projects",
            description: "Projects (folders on a device).",
            input_schema: json!({ "type": "object", "properties": { "device": device_schema() } }),
        },
        ToolDef {
            name: "list_harnesses",
            description: "Agent harnesses available on a device.",
            input_schema: json!({ "type": "object", "properties": { "device": device_schema() } }),
        },
        ToolDef {
            name: "list_models",
            description: "Models a harness offers on a device; pass an id to create_chat.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "harness": { "type": "string", "description": "e.g. claude-code, codex." },
                    "device": device_schema()
                },
                "required": ["harness"]
            }),
        },
        ToolDef {
            name: "list_chats",
            description: "Chats with status and last activity, newest first. parent: your id lists your children.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "project": { "type": "string" },
                    "device": { "type": "string" },
                    "include_archived": { "type": "boolean", "default": false },
                    "parent": { "type": "string" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 500, "default": 50 }
                }
            }),
        },
        ToolDef {
            name: "get_chat",
            description: "One chat's metadata, status, last message, and any question it's blocked on.",
            input_schema: chat_key_schema(json!({})),
        },
        ToolDef {
            name: "chat_status",
            description: "Status of your children (or one chat): idle, working, awaitingInput, errored, or offline, with the question or error.",
            input_schema: json!({ "type": "object", "properties": {
                "chat": { "type": "string", "description": "One chat; omit for all your children." }
            }}),
        },
        ToolDef {
            name: "create_chat",
            description: "Start a subagent (kind side, default) or a standalone chat (kind chat). Returns at once; its final answer arrives as a notice. The prompt must be self-contained.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "prompt": { "type": "string", "description": "Goal, context, constraints, what to report." },
                    "title": { "type": "string" },
                    "kind": { "type": "string", "enum": ["side", "chat"] },
                    "project": { "type": "string", "description": "Project id, path or name." },
                    "device": { "type": "string" },
                    "harness": { "type": "string" },
                    "model": { "type": "string" },
                    "reasoning": { "type": "string", "description": "e.g. low, medium, high." },
                    "sandbox": { "type": "string", "enum": ["read-only", "workspace-write", "danger-full-access"], "default": "workspace-write" },
                    "parent": { "type": "string" },
                    "branch": { "type": "string" },
                    "cwd": { "type": "string" },
                    "wait": { "type": "boolean", "default": false, "description": "Block until the first turn ends." },
                    "notify": { "type": "boolean", "default": true },
                    "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 3600, "default": 600 }
                }
            }),
        },
        ToolDef {
            name: "send_message",
            description: "Message a chat: steers it if working (interrupt: true stops it first), starts a turn if idle, answers it if it's waiting on a question. The reply arrives as a notice.",
            input_schema: chat_key_schema(json!({
                "text": { "type": "string", "description": "Message, or the answer to its pending question (an option label picks it)." },
                "interrupt": { "type": "boolean", "default": false, "description": "Stop its running turn, then send." },
                "answers": { "type": "array", "description": "Multi-question forms: [{question_id, labels}].", "items": {
                    "type": "object",
                    "properties": { "question_id": { "type": "string" }, "labels": { "type": "array", "items": { "type": "string" } } },
                    "required": ["question_id", "labels"]
                }},
                "wait": { "type": "boolean", "default": false, "description": "Block until its turn ends and return the reply." },
                "notify": { "type": "boolean", "default": true },
                "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 3600, "default": 600 }
            })),
        },
        ToolDef {
            name: "stop_chat",
            description: "Stop a chat's running turn.",
            input_schema: chat_key_schema(json!({})),
        },
        ToolDef {
            name: "read_chat",
            description: "Write a chat's transcript to a Markdown file and return its path; read it with your file tools (zeron_help files).",
            input_schema: chat_key_schema(read_args(json!({}))),
        },
        ToolDef {
            name: "read_subagent",
            description: "List a chat's harness subagents, or write one's transcript to a file and return its path.",
            input_schema: chat_key_schema(read_args(json!({
                "subagent": { "type": "string", "description": "Id or unique fragment from the list; omit to list." }
            }))),
        },
        ToolDef {
            name: "search_chats",
            description: "Search all transcripts by words (typo-tolerant) or regex; returns chats with snippets.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "mode": { "type": "string", "enum": ["words", "regex"], "default": "words" },
                    "project": { "type": "string" },
                    "device": { "type": "string" },
                    "parent": { "type": "string" },
                    "include_archived": { "type": "boolean", "default": false },
                    "include_self": { "type": "boolean", "default": false },
                    "role": { "type": "string", "enum": ["user", "assistant"] },
                    "include_tools": { "type": "boolean", "default": true },
                    "include_reasoning": { "type": "boolean", "default": false },
                    "case_sensitive": { "type": "boolean", "default": false },
                    "min_match": { "type": "number", "minimum": 0, "maximum": 1, "default": 0.5 },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 100, "default": 10 },
                    "snippets": { "type": "integer", "minimum": 1, "maximum": 20, "default": 3 },
                    "max_chats": { "type": "integer", "minimum": 1, "maximum": SEARCH_MAX_SCAN, "default": SEARCH_DEFAULT_SCAN }
                },
                "required": ["query"]
            }),
        },
        ToolDef {
            name: "wait_for_turn",
            description: "Block until a chat's turn ends. Rarely needed: results arrive as notices.",
            input_schema: chat_key_schema(json!({
                "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 3600, "default": 600 }
            })),
        },
        ToolDef {
            name: "archive_chat",
            description: "Archive a chat (archived: false restores it).",
            input_schema: chat_key_schema(json!({
                "archived": { "type": "boolean", "default": true }
            })),
        },
    ];
    for (name, single, description) in [
        (
            "create_chats",
            "create_chat",
            "Start several chats at once (requests: create_chat arguments). Each answer arrives as its own notice.",
        ),
        (
            "send_messages",
            "send_message",
            "Message several chats at once (requests: send_message arguments).",
        ),
    ] {
        let item_schema = tools
            .iter()
            .find(|tool| tool.name == single)
            .unwrap()
            .input_schema
            .clone();
        tools.push(ToolDef {
            name, description,
            input_schema: json!({
                "type": "object",
                "properties": {"requests": {"type": "array", "minItems": 1, "maxItems": MAX_BATCH, "items": item_schema}},
                "required": ["requests"],
            }),
        });
    }
    tools.push(ToolDef {
        name: SHOW_VISUAL,
        description: SHOW_VISUAL_DESCRIPTION,
        input_schema: json!({
            "type": "object",
            "properties": {
                "script": {
                    "type": "string",
                    "description": "The Luau visual script: setup at the top level, ending with `return function() return ui.column { ... } end`."
                },
                "title": { "type": "string", "description": "Short name for the visual." },
                "assets": {
                    "type": "object",
                    "additionalProperties": { "type": "string" },
                    "description": "Files the script uses, by the name it uses them: { \"prism.glb\": \"models/prism.glb\", \"sky.hdr\": \"/tmp/sky.hdr\" }. Paths are on this machine, relative to your working directory. glTF models (.glb, or .gltf with everything embedded), PNG, JPEG, WebP, and Radiance .hdr images; at most 16 files, 8 MB each, 10 MB together."
                }
            },
            "required": ["script", "title"]
        }),
    });
    tools
}

/// The tools `role` lists, in catalog order, with the subagent's own
/// shapes for the two it shares.
fn catalog_for(role: &Role) -> Vec<ToolDef> {
    let mut tools: Vec<ToolDef> = catalog()
        .into_iter()
        .filter(|tool| role.tools().contains(&tool.name) && role.allows(tool.name))
        .collect();
    if matches!(role, Role::Subagent { .. }) {
        for tool in &mut tools {
            match tool.name {
                "send_message" => {
                    tool.description = "Message your parent (orchestrator): a question, a finding, a status. Lands in its agent inbox, never its composer queue. Your final answer is sent automatically; don't send it here.";
                    tool.input_schema = json!({
                        "type": "object",
                        "properties": {
                            "text": { "type": "string" },
                            "wait": { "type": "boolean", "default": false, "description": "Block until your parent replies; the reply is the result." },
                            "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 3600, "default": 600 }
                        },
                        "required": ["text"]
                    });
                }
                "read_chat" => {
                    tool.description = "Write your parent's, a sibling's or your own transcript to a Markdown file and return its path (zeron_help files).";
                }
                _ => {}
            }
        }
    }
    tools
}

/// Shows a page in the CALLING chat, so it is offered only to an agent that
/// Zeron launched (`ZERON_CHAT_ID` set).
const SHOW_VISUAL: &str = "show_visual";

const SHOW_VISUAL_DESCRIPTION: &str = "\
Show an interactive visual (chart, table, dashboard, diagram, simulation, 3D view) inline in this \
chat, above your final text reply; call it before writing that reply. Use it when a visual says more \
than prose. The reader already sees it, so the reply should not announce, locate, or restate it: add \
only what the visual doesn't say.\n\
\n\
A visual is a Luau script (Lua 5.1 plus `+=`, `continue`, and `if` expressions). Zeron runs it in a \
sandbox (no I/O or network; math.random is seeded, so data is the same on every render) and renders \
what it returns with Zeron's own native interface. Build data and state at the top level, then return \
a view function. Zeron calls the view again after every handler, so handlers just assign to state:\n\
\n\
local s = ui.state { metric = \"p95\" }\n\
local data = { { region = \"us-east\", p95 = 182, p99 = 240 }, { region = \"eu-west\", p95 = 140, p99 = 210 } }\n\
return function()\n\
  return ui.column { gap = 16,\n\
    ui.text { \"Latency by region\", size = \"lg\", weight = 600 },\n\
    ui.tabs { options = { \"p95\", \"p99\" }, value = s.metric, on_change = function(v) s.metric = v end },\n\
    ui.chart { type = \"bar\", height = 220, unit = \"ms\",\n\
      x = ui.map(data, function(d) return d.region end),\n\
      series = { { name = s.metric, values = ui.map(data, function(d) return d[s.metric] end) } } },\n\
  }\n\
end\n\
\n\
Zeron runs the script before showing it (setup, render, every draw, and each tab, toggle, slider, and \
button once) and returns the first error with its line; fix it and call again.\n\
\n\
Nodes are props tables: children are the array part, strings are plain text, nil and false children \
are skipped, and lists of nodes flatten (so ui.map results can be children). Unknown props are errors.\n\
- ui.column { gap = 8, align = \"start|center|end\", ... }, ui.row { gap = 12, align, justify = \
\"start|center|end|between\", wrap = true, ... }, ui.grid { columns = 3, gap = 16, ... }. Any node \
takes width = px or grow = true inside a row (sliders, charts, tables, canvases, and scenes fill a \
row's spare width by default), and key = \"name\" to keep its hover or camera when the tree changes \
shape. A row or column with on_click = function() end is clickable; hover = true gives \
it the hover wash.\n\
- ui.text { \"plain \", ui.span { \"bold\", weight = 600, color = \"accent\", mono = true }, size = \
\"xs|sm|body|lg|xl|2xl\", weight = 400-700, color, mono, italic, align, truncate }, or ui.text \"string\".\n\
- ui.stat { value = \"182 ms\", label = \"p95 latency\", delta = \"-12%\", tone = \"success\" }: a headline \
figure over a muted label; tone colors the delta.\n\
- ui.table { columns = { \"Region\", { label = \"p95\", align = \"end\" } }, rows = { { \"us-east\", 182 }, \
... } }: laid out like a markdown table (bold header, hairline row rules), numbers right-aligned; cells \
are strings, numbers, or nodes.\n\
- ui.chart { type = \"bar|line|area|scatter\", height = 220, x = { labels }, series = { { name, values = \
{ ... }, color } }, stacked, horizontal (bar), min, max, unit = \"ms\", prefix = \"$\", decimals, legend }. \
Scatter series take points = { { x, y }, ... } (x_unit, x_decimals). Axes, gridlines, the legend, and a \
hover readout are built in.\n\
- ui.rule {} is a hairline divider; ui.space { size = 16 }; ui.link { label, url = \"https://...\" } \
opens like a chat link.\n\
- Controls: ui.tabs { options = { ... }, value, on_change = function(option) end }, ui.toggle { label, \
value = bool, on_change = function(on) end }, ui.slider { label, value, min, max, step, unit, on_change \
= function(n) end }, ui.button { label, on_click = function() end, primary = bool }.\n\
- ui.canvas { height = 240, draw = function(g, w, h, t) ... end, animate = bool, on_click = \
function(x, y) end }: 2D drawing in pixels, origin top-left, w = the reply column's width. \
g:line(x1, y1, x2, y2, { color, width, dash }), g:polyline(pts, { color, width }), g:polygon(pts, \
{ fill, stroke, width }), g:rect(x, y, w, h, { fill, stroke, radius }), g:circle(x, y, r, { fill, \
stroke }), g:arc(x, y, r, a0, a1, { color } or { fill, inner = radius } for pie and donut sectors; \
radians clockwise from 3 o'clock), g:text(x, y, str, { size, color, weight, align = \
\"start|center|end\", baseline = \"top|middle|bottom\", mono }). pts are { x1, y1, x2, y2, ... } or \
{ { x, y }, ... }. g.pointer is { x, y, down } while the pointer is over the canvas (draw hover \
effects with it); t is seconds, and animate = true redraws every frame while visible.\n\
- ui.scene { height = 320, draw = function(s, t) ... end, orbit = true, spin = 0.3 (turntable \
radians per second), camera = { yaw = 0.6, pitch = 0.45, zoom = 1 }, animate = bool }: real 3D, y up, \
rendered on the GPU with depth, soft shadows from a key light, and bloom. Zeron frames the camera on \
your geometry and the reader drags to orbit. Solids (box, sphere, cylinder, surface, mesh) take \
material props: roughness (0 mirror to 1 matte), metal (0-1), glow (emits its color and blooms; \
use for lamps, beams, highlights), glass = true (refracts what's behind it; ior = 1.5, dispersion \
= 0-1 splits it into a rainbow, as a prism does). Build real forms from several primitives (legs, \
tops, rims), not single boxes, and give materials so surfaces read as wood, metal, or glass. s:box { at = { x, y, z }, size = { w, h, d } or n, color }, s:sphere { at, radius, color }, \
s:cylinder { at (base center), radius, height, color }, s:surface { x = { min, max }, z = { min, max }, \
f = function(x, z) return y end or values = { { ... } }, detail = 32, colors = { \"chart1\", \"chart3\" } \
(a ramp by height), wire = bool }, s:mesh { vertices = { { x, y, z }, ... }, faces = { { 1, 2, 3 }, ... }, \
color }, s:line(a, b, { color, width }), s:path(points, { color, width }), s:point(at, { radius, \
color, hint = \"shown on hover\" }), s:label(at, text, { color, size }), s:grid { size = 10, step = 1 }, \
s:axes { size = 1, labels = { \"x\", \"y\", \"z\" } }. Physical colors may be \"#rrggbb\" in scenes \
(a material's own color: gold, glass, wood), and only there.\n\
- Assets: pass real files in show_visual's assets (name -> path) and scripts use them by name. \
s:model { asset = \"prism.glb\", at, rotate = { x, y, z } degrees, scale, fit = 2 (center it and size \
its longest side), color and material props override the model's own } draws a glTF model with its \
materials and textures. Solids take texture = \"wood.jpg\" (multiplied by color; mesh takes uvs = { { u, v \
}, ... } per vertex). s:environment { asset = \"studio.hdr\", intensity = 1, background = false } lights \
the scene from an equirectangular image (an HDR is best) that metal and glass reflect and glass bends; \
background = true also draws it behind the scene. s:sprite { at, size = 48 (pixels), color, glow = 1, \
asset = \"flare.png\" } adds a camera-facing light (a flare or glint; a soft round glow without an \
asset) that blooms. ui.shader takes textures = { \"rock.jpg\", ... } as iTexture0-iTexture3 \
(repeating, mipmapped). Find or make assets with your own tools (download, convert KTX2 or EXR to PNG \
or HDR) before calling.\n\
- ui.shader { height = 360, code = [[ GLSL ]], buffers = { [[ GLSL ]], ... }, inputs = { speed = \
s.speed }, blend = \"alpha|light|opaque\", bloom = 0-3, orbit = bool, spin, camera = { yaw, pitch, \
zoom }, animate = true }: GPU shaders for what geometry can't express: raymarched SDFs and \
fractals, light and optics (beams, refraction, dispersion), volumes, fluids, particles, generative \
art. Write each pass as on Shadertoy: void mainImage(out vec4 fragColor, in vec2 fragCoord), with \
iResolution (vec3: width and height in pixels, then the display's pixel density), iTime, \
iTimeDelta, iFrame, iMouse (pixels from the bottom-left; xy where the pointer is, zw where it went \
down while it's held, negative otherwise), iCamera (vec4 yaw, pitch, zoom, 0; orbit = true lets the reader drag \
it), every inputs name as a float (bind them to state so sliders and toggles drive the shader), \
iNoise (a tiling 256x256 random RGBA texture), and iChannel0-iChannel3 with iChannelResolution: \
buffers run first, in order, each into a float texture that every pass reads as iChannel<i> (an \
earlier buffer's current frame; a buffer's own previous frame), for multi-pass work like blur, \
trails, accumulation, and simulation. The shader sits on the chat, never in a box: don't paint a \
background, vignette, or floor color. By default (blend = \"light\") the output is emitted light: \
brightness is opacity and black is clear, right for beams, glows, sparks, and light through glass. \
For solid shaded objects (fractals, SDF bodies) set blend = \"alpha\" and write alpha 0 where the ray \
misses. bloom = 0.6-1.5 adds a soft glow around bright light. \
The width varies with the reader (a phone is about 350px wide, often narrower than the shader is \
tall): scale the scene by min(iResolution.x, iResolution.y), not one axis, and keep the subject \
centered with room on both sides, so it fits and stays centered at any aspect. \
GLSL errors come back with the pass and line number.\n\
- Helpers: ui.scale(d0, d1, r0, r1) returns a mapping function; ui.ticks(lo, hi, n); ui.fmt(n, { unit, \
prefix, decimals }) groups digits; ui.map(list, function(v, i) end); ui.range(a, b, step); ui.clamp; \
ui.lerp; ui.series(i) is \"chart<i>\"; ui.color(token, alpha); ui.mix(a, b, t). print(...) output comes \
back to you.\n\
\n\
Colors are theme tokens, never hex or color names (except scene materials): text; muted (labels, axes, captions); faint \
(tertiary notes); accent (the one thing to notice); on-accent (text on an accent mark); success, \
warning, danger (status only); border (hairlines, gridlines); chart1, chart2, chart3, chart4, \
chart5, chart6 (series, in order; \
chart1 is the accent). They follow the reader's theme, light or dark, on frosted glass or an opaque \
surface, so never assume a background color.\n\
\n\
Style, enforced: layout nodes have no fill, border, shadow, padding, or font props. A visual is part \
of the reply, not a widget, so lay content directly on the chat with type, spacing, and alignment: no \
cards, tiles, boxed panels, banners, or canvas backgrounds. Stats are a row of plain ui.stat figures; \
a chart is a plain heading, then the chart; separate sections with gap or one ui.rule. Text renders \
in Zeron's interface font at the reader's text size: body for most text, sm and xs for labels and \
notes, lg and xl for headings, 2xl for a headline figure. The width is the reply column (about 720px, \
as narrow as 360px), so lay out fluidly with grow, wrap, and grid, and give charts, canvases, and \
scenes heights of 160-420px.";

/// Read `show_visual`'s asset files (paths relative to the agent's working
/// directory), base64-encoded by name for the engine.
async fn read_visual_assets(
    assets: &std::collections::BTreeMap<String, String>,
) -> anyhow::Result<serde_json::Map<String, Value>> {
    use base64::Engine as _;
    use zeron_proto::{
        VISUAL_MAX_ASSET_BYTES as MAX_ASSET_BYTES, VISUAL_MAX_ASSETS as MAX_ASSETS,
        VISUAL_MAX_TOTAL_ASSET_BYTES as MAX_TOTAL_ASSET_BYTES,
        valid_visual_asset_name as valid_name,
    };
    anyhow::ensure!(
        assets.len() <= MAX_ASSETS,
        "a visual takes at most {MAX_ASSETS} assets"
    );
    let mut out = serde_json::Map::new();
    let mut total = 0;
    let mut seen = std::collections::HashSet::new();
    for (name, path) in assets {
        anyhow::ensure!(
            valid_name(name),
            "asset name '{name}' must be a plain file name (letters, digits, '.', '-', '_')"
        );
        // Stored as files, and file systems often ignore case.
        anyhow::ensure!(
            seen.insert(name.to_ascii_lowercase()),
            "asset names must differ by more than case ('{name}')"
        );
        let metadata = tokio::fs::metadata(path)
            .await
            .with_context(|| format!("asset '{name}': can't read {path}"))?;
        anyhow::ensure!(metadata.is_file(), "asset '{name}': {path} is not a file");
        anyhow::ensure!(
            metadata.len() <= MAX_ASSET_BYTES as u64,
            "asset '{name}' is over {} MB",
            MAX_ASSET_BYTES / (1024 * 1024)
        );
        // Bounded however the file changes (or lies about its size).
        let mut bytes = Vec::new();
        {
            use tokio::io::AsyncReadExt as _;
            tokio::fs::File::open(path)
                .await
                .with_context(|| format!("asset '{name}': can't read {path}"))?
                .take(MAX_ASSET_BYTES as u64 + 1)
                .read_to_end(&mut bytes)
                .await
                .with_context(|| format!("asset '{name}': can't read {path}"))?;
        }
        anyhow::ensure!(
            bytes.len() <= MAX_ASSET_BYTES,
            "asset '{name}' is over {} MB",
            MAX_ASSET_BYTES / (1024 * 1024)
        );
        total += bytes.len();
        anyhow::ensure!(
            total <= MAX_TOTAL_ASSET_BYTES,
            "a visual's assets total at most {} MB",
            MAX_TOTAL_ASSET_BYTES / (1024 * 1024)
        );
        out.insert(
            name.clone(),
            Value::String(base64::engine::general_purpose::STANDARD.encode(bytes)),
        );
    }
    Ok(out)
}

// ---- argument shapes ---------------------------------------------------------

#[derive(Deserialize)]
struct ShowVisualArgs {
    script: String,
    title: String,
    #[serde(default)]
    assets: std::collections::BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct BatchArgs {
    requests: Vec<Value>,
}

#[derive(Deserialize)]
struct ChatArgs {
    chat: String,
}

#[derive(Deserialize)]
struct ListModelsArgs {
    harness: String,
    device: Option<String>,
}

#[derive(Deserialize, Default)]
struct ListChatsArgs {
    project: Option<String>,
    device: Option<String>,
    #[serde(default)]
    include_archived: bool,
    parent: Option<String>,
    limit: Option<usize>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum ChatKind {
    Chat,
    Side,
}

#[derive(Deserialize, Default)]
struct DeviceArgs {
    device: Option<String>,
}

/// Messages already in the transcript before a send. Replies are matched by
/// id, not by time: `createdAt` is stamped on the host's clock, which can run
/// behind the caller's when the chat lives on another device.
#[derive(Clone, Default)]
struct TurnStart {
    message_ids: Vec<String>,
}

#[derive(Clone)]
struct PendingTurn {
    baseline: Option<Session>,
    start: TurnStart,
    /// The sent user message; its turn is the one waited on.
    message_id: Option<String>,
}

#[derive(Deserialize, Default)]
struct CreateChatArgs {
    kind: Option<ChatKind>,
    project: Option<String>,
    device: Option<String>,
    parent: Option<String>,
    harness: Option<String>,
    model: Option<String>,
    reasoning: Option<String>,
    sandbox: Option<String>,
    title: Option<String>,
    branch: Option<String>,
    cwd: Option<String>,
    prompt: Option<String>,
    #[serde(default)]
    wait: bool,
    notify: Option<bool>,
    timeout_secs: Option<u64>,
}

#[derive(Deserialize)]
struct ReadSubagentArgs {
    chat: String,
    subagent: Option<String>,
    #[serde(default, alias = "reasoning")]
    include_reasoning: bool,
    #[serde(alias = "tools")]
    include_tools: Option<bool>,
}

#[derive(Deserialize)]
struct ReadChatArgs {
    chat: String,
    #[serde(default, alias = "reasoning")]
    include_reasoning: bool,
    #[serde(alias = "tools")]
    include_tools: Option<bool>,
}

#[derive(Deserialize)]
struct SearchArgs {
    query: String,
    mode: Option<String>,
    project: Option<String>,
    device: Option<String>,
    parent: Option<String>,
    #[serde(default)]
    include_archived: bool,
    #[serde(default)]
    include_self: bool,
    role: Option<String>,
    include_tools: Option<bool>,
    #[serde(default)]
    include_reasoning: bool,
    #[serde(default)]
    case_sensitive: bool,
    min_match: Option<f32>,
    limit: Option<usize>,
    snippets: Option<usize>,
    max_chats: Option<usize>,
}

/// What the tool keeps of a chat between scanning and reporting.
struct ScannedChat {
    chat: Chat,
    /// (id, role, ISO time) of every message, indexed like the transcript.
    messages: Vec<(String, &'static str, String)>,
}

#[derive(Deserialize)]
struct SendArgs {
    /// Required for orchestrators; a subagent always messages its parent.
    chat: Option<String>,
    #[serde(default)]
    text: String,
    /// Hidden: the old auto/run/steer/queue switch.
    mode: Option<String>,
    #[serde(default)]
    interrupt: bool,
    answers: Option<Vec<AnswerArg>>,
    #[serde(default)]
    wait: bool,
    notify: Option<bool>,
    timeout_secs: Option<u64>,
}

#[derive(Deserialize)]
struct WaitArgs {
    chat: String,
    timeout_secs: Option<u64>,
}

#[derive(Deserialize)]
struct ArchiveArgs {
    chat: String,
    archived: Option<bool>,
}

#[derive(Deserialize)]
struct RespondArgs {
    chat: String,
    request_id: Option<String>,
    #[serde(default)]
    answers: Vec<AnswerArg>,
}

#[derive(Deserialize, Clone)]
struct AnswerArg {
    question_id: String,
    labels: Vec<String>,
}

fn parse<T: serde::de::DeserializeOwned>(args: Value) -> Result<T, String> {
    serde_json::from_value(args).map_err(|e| format!("invalid arguments: {e}"))
}

fn wait_duration(secs: Option<u64>) -> Duration {
    secs.map(Duration::from_secs)
        .unwrap_or(DEFAULT_WAIT)
        .min(MAX_WAIT)
}

#[cfg(test)]
fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

fn parse_enum<T: serde::de::DeserializeOwned>(what: &str, raw: &str) -> Result<T, String> {
    serde_json::from_value(Value::String(raw.trim().to_owned()))
        .map_err(|_| format!("unknown {what}: {raw:?}"))
}

// ---- summaries ---------------------------------------------------------------

/// Live posture of one chat as the tools report it.
fn status_of(session: Option<&Session>) -> (String, Option<i64>) {
    let Some(session) = session else {
        return ("idle".into(), None);
    };
    let age = chrono::Utc::now() - session.updated_at;
    let stale = age > SESSION_STALE;
    let label = match session.status {
        SessionStatus::Working if stale => "working (stale — host may be offline)",
        SessionStatus::Working => "working",
        SessionStatus::AwaitingInput => "awaitingInput",
        SessionStatus::Errored => "errored",
        SessionStatus::Idle => "idle",
    };
    (label.into(), Some(age.num_seconds().max(0)))
}

fn summarize_chat(chat: &Chat, spaces: &[Space], sessions: &[Session]) -> Value {
    let space = chat
        .space_id
        .as_deref()
        .and_then(|id| spaces.iter().find(|s| s.id == id));
    let session = session_for(sessions, chat);
    let (status, status_age) = status_of(session.as_ref());
    json!({
        "id": chat.id,
        "title": chat.title,
        "project": space.map(|s| json!({
            "id": s.id,
            "name": s.display_name(),
            "path": s.path,
        })),
        "deviceId": chat.device_id,
        "cwd": chat.cwd.clone().or_else(|| space.map(|s| s.path.clone())),
        "branch": chat.branch,
        "harness": chat.config.as_ref().map(|c| c.harness),
        "model": chat.config.as_ref().and_then(|c| c.model.clone()),
        "reasoning": chat.config.as_ref().and_then(|c| c.reasoning),
        "archived": chat.archived,
        "parentChatId": chat.parent_chat_id,
        "status": status,
        "statusAgeSecs": status_age,
        "lastMessageAt": chat.last_message_at,
        "lastMessagePreview": chat.last_message_preview,
        "createdAt": chat.created_at,
    })
}

fn role_name(role: MessageRole) -> &'static str {
    match role {
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::System => "system",
    }
}

fn last_pending_input(messages: &[RenderedMessage]) -> Option<Value> {
    messages.iter().rev().find_map(|m| m.pending_input.clone())
}

// ---- dispatch ----------------------------------------------------------------

impl Tools {
    pub fn new(zeron: Arc<Zeron>) -> Self {
        Self {
            zeron,
            pending_turns: Default::default(),
            role: Default::default(),
            files: {
                static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                a2a::files_dir().join(
                    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                        .to_string(),
                )
            },
        }
    }

    /// The origin chat's role. A chat that can't be resolved yet reads as an
    /// orchestrator for this connection: it keeps working, and the engine
    /// still refuses a side chat creating chats.
    pub async fn role(&self) -> &Role {
        self.role
            .get_or_init(|| async {
                let Some(me) = self.zeron.origin().chat_id.clone() else {
                    return Role::Free;
                };
                match self.zeron.resolve_chat(&me).await {
                    Ok(chat) => match chat.parent_chat_id {
                        Some(parent) => Role::Subagent { me, parent },
                        None => Role::Orchestrator { me },
                    },
                    Err(_) => Role::Orchestrator { me },
                }
            })
            .await
    }

    pub async fn list(&self) -> Vec<ToolDef> {
        catalog_for(self.role().await)
    }

    pub async fn has(&self, name: &str) -> bool {
        self.role().await.allows(a2a::canonical(name))
    }

    /// Resolve a chat key and check this chat may touch it.
    async fn guarded(&self, key: &str, control: bool) -> anyhow::Result<Chat> {
        let chat = self.zeron.resolve_chat(key).await?;
        let role = self.role().await;
        let check = if control {
            role.may_control(&chat)
        } else {
            role.may_read(&chat)
        };
        check.map_err(anyhow::Error::msg)?;
        Ok(chat)
    }

    /// `Ok` is the tool's structured result; `Err` is a message the model
    /// should read (surfaced as `isError`, never as a protocol error).
    pub async fn call(&self, name: &str, args: Value) -> Result<Value, String> {
        let name = a2a::canonical(name);
        if !self.role().await.allows(name) {
            return Err(format!("{name} is not available to this chat (see whoami)"));
        }
        let result = match name {
            "whoami" => self.whoami().await,
            "zeron_help" => Ok(json!(a2a::help(args.get("topic").and_then(Value::as_str)))),
            "chat_status" => self.chat_status(args.get("chat").and_then(Value::as_str)).await,
            "list_devices" => self.list_devices().await,
            "list_projects" => self.list_projects(parse(args)?).await,
            "list_harnesses" => self.list_harnesses(parse(args)?).await,
            "list_models" => self.list_models(parse(args)?).await,
            "list_chats" => self.list_chats(parse(args)?).await,
            "get_chat" => self.get_chat(parse(args)?).await,
            "create_chat" => self.create_chat(parse(args)?).await,
            "create_chats" => self.batch(parse(args)?, true).await,
            "send_messages" => self.batch(parse(args)?, false).await,
            "read_chat" => self.read_chat(parse(args)?).await,
            "read_subagent" => self.read_subagent(parse(args)?).await,
            "search_chats" => self.search_chats(parse(args)?).await,
            "send_message" => self.send_message(parse(args)?).await,
            "wait_for_turn" => self.wait_for_turn(parse(args)?).await,
            "stop_chat" => self.interrupt_chat(parse(args)?).await,
            "respond_to_input" => self.respond_to_input(parse(args)?).await,
            "archive_chat" => self.archive_chat(parse(args)?).await,
            SHOW_VISUAL => self.show_visual(parse(args)?).await,
            other => return Err(format!("unknown tool: {other}")),
        };
        result.map_err(|e| format!("{e:#}"))
    }

    /// Poll every request together, including its optional wait. A waiting
    /// first chat must not prevent subsequent chats from receiving their work.
    async fn batch(&self, args: BatchArgs, create: bool) -> anyhow::Result<Value> {
        anyhow::ensure!(
            (1..=MAX_BATCH).contains(&args.requests.len()),
            "requests must contain between 1 and {MAX_BATCH} items"
        );
        let results = futures::future::join_all(args.requests.into_iter().enumerate().map(
            |(index, args)| async move {
                let result = if create {
                    match serde_json::from_value(args) {
                        Ok(args) => self.create_chat(args).await,
                        Err(error) => Err(error.into()),
                    }
                } else {
                    match serde_json::from_value(args) {
                        Ok(args) => self.send_message(args).await,
                        Err(error) => Err(error.into()),
                    }
                };
                match result {
                    Ok(result) => json!({"index": index, "isError": false, "result": result}),
                    Err(error) => {
                        json!({"index": index, "isError": true, "error": error.to_string()})
                    }
                }
            },
        ))
        .await;
        Ok(json!({"results": results}))
    }

    async fn whoami(&self) -> anyhow::Result<Value> {
        let origin = self.zeron.origin().clone();
        let local_device = self.zeron.local_device_id().await?;
        let engine = self.zeron.engine_info().await.unwrap_or(Value::Null);
        let chat = match origin.chat_id.as_deref() {
            Some(id) => match self.zeron.resolve_chat(id).await {
                Ok(chat) => {
                    let (spaces, sessions) =
                        tokio::try_join!(self.zeron.spaces(), self.zeron.sessions())?;
                    summarize_chat(&chat, &spaces, &sessions)
                }
                Err(_) => json!({ "id": id }),
            },
            None => Value::Null,
        };
        let pending_notices = match origin.chat_id.as_deref() {
            Some(id) => self
                .zeron
                .call(methods::LIST_TURN_NOTICES, json!({ "watcherChatId": id }))
                .await
                .ok()
                .and_then(|reply| reply.get("watches").cloned())
                .unwrap_or(Value::Null),
            None => Value::Null,
        };
        let role = match self.role().await {
            Role::Free => json!({ "role": "free" }),
            Role::Orchestrator { .. } => json!({ "role": "orchestrator" }),
            Role::Subagent { parent, .. } => json!({
                "role": "subagent",
                "parent": parent,
                "note": "Your final answer goes to your parent automatically when your turn ends. send_message reaches only your parent.",
            }),
        };
        Ok(json!({
            "role": role,
            "chat": chat,
            "pendingNotices": pending_notices,
            "originDeviceId": origin.device_id,
            "localDeviceId": local_device,
            "workspaceScope": engine.get("workspaceScope").cloned().unwrap_or(Value::Null),
            "note": if origin.chat_id.is_some() {
                "Messages you send are attributed to this chat; it cannot message itself. pendingNotices lists the chats whose turn results will arrive here automatically."
            } else {
                "Not running inside a chat: messages are sent without attribution."
            },
        }))
    }

    async fn list_devices(&self) -> anyhow::Result<Value> {
        let (devices, local) =
            tokio::try_join!(self.zeron.devices(), self.zeron.local_device_id())?;
        Ok(json!({
            "devices": devices.iter().map(|d| json!({
                "id": d.id,
                "name": d.name,
                "platform": d.platform,
                "local": d.id == local,
                "lastSeenAt": d.last_seen_at,
                "version": d.version,
            })).collect::<Vec<_>>()
        }))
    }

    async fn list_projects(&self, args: DeviceArgs) -> anyhow::Result<Value> {
        let (mut spaces, devices) = tokio::try_join!(self.zeron.spaces(), self.zeron.devices())?;
        if let Some(device) = args.device.as_deref() {
            let device = self.zeron.resolve_device_id(Some(device)).await?;
            spaces.retain(|s| s.device_id == device);
        }
        let device_name = |id: &str| devices.iter().find(|d| d.id == id).map(|d| d.name.clone());
        Ok(json!({
            "projects": spaces.iter().map(|s| json!({
                "id": s.id,
                "name": s.display_name(),
                "path": s.path,
                "deviceId": s.device_id,
                "deviceName": device_name(&s.device_id),
                "git": s.git_detected,
            })).collect::<Vec<_>>()
        }))
    }

    async fn list_harnesses(&self, args: DeviceArgs) -> anyhow::Result<Value> {
        let device = match args.device.as_deref() {
            Some(key) => Some(self.zeron.resolve_device_id(Some(key)).await?),
            None => None,
        };
        let harnesses = self.zeron.harnesses_on(device.as_deref()).await?;
        Ok(json!({
            "harnesses": harnesses.iter().map(|h| json!({
                "id": h.id,
                "name": h.name,
                "available": h.available(),
                "installed": h.installed,
                "steersMidTurn": h.steers_mid_turn(),
                "reasoningLevels": h.reasoning_levels,
            })).collect::<Vec<_>>()
        }))
    }

    async fn list_models(&self, args: ListModelsArgs) -> anyhow::Result<Value> {
        let harness: HarnessId =
            parse_enum("harness", &args.harness).map_err(anyhow::Error::msg)?;
        let device = match args.device.as_deref() {
            Some(key) => Some(self.zeron.resolve_device_id(Some(key)).await?),
            None => None,
        };
        let models = self.zeron.models_on(harness, device.as_deref()).await?;
        Ok(json!({
            "harness": harness,
            "models": models.iter().map(|m| json!({
                "id": m.id,
                "label": m.label,
                "description": m.description,
                "reasoningLevels": m.reasoning_levels,
            })).collect::<Vec<_>>()
        }))
    }

    /// Chats passing the project/device/archived/parent filters, with the
    /// spaces and sessions their summaries need.
    async fn filtered_chats(
        &self,
        project: Option<&str>,
        device: Option<&str>,
        include_archived: bool,
        parent: Option<&str>,
    ) -> anyhow::Result<(Vec<Chat>, Vec<Space>, Vec<Session>)> {
        let (mut chats, spaces, sessions) = tokio::try_join!(
            self.zeron.chats(),
            self.zeron.spaces(),
            self.zeron.sessions()
        )?;
        if let Some(project) = project {
            let (space, _) = self.zeron.resolve_target(Some(project), device).await?;
            let space = space.expect("project provided");
            chats.retain(|c| c.space_id.as_deref() == Some(space.id.as_str()));
        }
        if device.is_some() {
            let device = self.zeron.resolve_device_id(device).await?;
            chats.retain(|c| c.device_id == device);
        }
        if !include_archived {
            chats.retain(|c| !c.archived);
        }
        if let Some(parent) = parent {
            let parent = self.zeron.resolve_chat(parent).await?;
            chats.retain(|c| c.parent_chat_id.as_deref() == Some(parent.id.as_str()));
        }
        Ok((chats, spaces, sessions))
    }

    async fn list_chats(&self, args: ListChatsArgs) -> anyhow::Result<Value> {
        let (mut chats, spaces, sessions) = self
            .filtered_chats(
                args.project.as_deref(),
                args.device.as_deref(),
                args.include_archived,
                args.parent.as_deref(),
            )
            .await?;
        chats.sort_by(|a, b| {
            let a_at = a.last_message_at.unwrap_or(a.created_at);
            let b_at = b.last_message_at.unwrap_or(b.created_at);
            b_at.cmp(&a_at)
        });
        let total = chats.len();
        let limit = args.limit.unwrap_or(50).clamp(1, 500);
        Ok(json!({
            "total": total,
            "chats": chats.iter().take(limit)
                .map(|c| summarize_chat(c, &spaces, &sessions))
                .collect::<Vec<_>>()
        }))
    }

    async fn get_chat(&self, args: ChatArgs) -> anyhow::Result<Value> {
        let chat = self.guarded(&args.chat, false).await?;
        let (spaces, sessions, entries) = tokio::try_join!(
            self.zeron.spaces(),
            self.zeron.sessions(),
            self.zeron.transcript_on(&chat.id, Some(&chat.device_id))
        )?;
        let rendered = render_entries(&entries, RenderOptions::default());
        let mut summary = summarize_chat(&chat, &spaces, &sessions);
        summary["messageCount"] = json!(rendered.len());
        summary["pendingInput"] = last_pending_input(&rendered).unwrap_or(Value::Null);
        summary["lastMessage"] = rendered.last().map(|m| json!(m)).unwrap_or(Value::Null);
        Ok(summary)
    }

    async fn create_chat(&self, args: CreateChatArgs) -> anyhow::Result<Value> {
        if let Some(origin) = self.zeron.origin().chat_id.as_deref() {
            let chat = self.zeron.resolve_chat(origin).await?;
            anyhow::ensure!(
                chat.parent_chat_id.is_none(),
                "Side chats cannot create chats. Ask your parent chat to create another side chat."
            );
        }
        // Parent: the explicit `parent` argument, else the chat this server
        // speaks for. Resolved so a prefix/title works and a typo fails loud.
        let explicit_parent = args
            .parent
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty());
        anyhow::ensure!(
            args.kind != Some(ChatKind::Chat) || explicit_parent.is_none(),
            "kind chat cannot have a parent"
        );
        let parent_chat_id = if args.kind == Some(ChatKind::Chat) {
            None
        } else {
            match explicit_parent {
                Some(key) => Some(self.zeron.resolve_chat(key).await?.id),
                None => self.zeron.origin().chat_id.clone(),
            }
            // A hidden voice orchestrator creates top-level sessions.
            .filter(|parent| !zeron_proto::voice::is_orchestrator_chat(parent))
        };
        anyhow::ensure!(
            args.kind != Some(ChatKind::Side) || parent_chat_id.is_some(),
            "kind side requires a parent or origin chat"
        );
        let kind = if parent_chat_id.is_some() {
            ChatKind::Side
        } else {
            ChatKind::Chat
        };

        if let Some(parent) = parent_chat_id.as_deref() {
            let chat = self.zeron.resolve_chat(parent).await?;
            anyhow::ensure!(
                chat.parent_chat_id.is_none(),
                "Cannot create a child of a side chat. Choose a top-level parent chat."
            );
        }
        let (space, device_id) = self
            .zeron
            .resolve_target(args.project.as_deref(), args.device.as_deref())
            .await?;
        let harnesses = self.zeron.harnesses_on(Some(&device_id)).await?;
        let harness = match args.harness.as_deref() {
            Some(raw) => {
                let id: HarnessId = parse_enum("harness", raw).map_err(anyhow::Error::msg)?;
                anyhow::ensure!(
                    harnesses.iter().any(|h| h.id == id && h.available()),
                    "harness {raw} is not available on device {device_id} (see list_harnesses with device)"
                );
                id
            }
            None => default_harness(&harnesses).with_context(|| format!("device {device_id}"))?,
        };
        if let Some(model) = args.model.as_deref() {
            let models = self.zeron.models_on(harness, Some(&device_id)).await?;
            anyhow::ensure!(
                models.iter().any(|m| m.id == model),
                "model {model:?} is not offered by {harness:?} on device {device_id}; available: {}",
                models
                    .iter()
                    .map(|m| m.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        let reasoning: Option<ReasoningLevel> = match args.reasoning.as_deref() {
            Some(raw) => Some(parse_enum("reasoning level", raw).map_err(anyhow::Error::msg)?),
            None => None,
        };
        let sandbox: SandboxLevel = match args.sandbox.as_deref() {
            Some(raw) => parse_enum("sandbox", raw).map_err(anyhow::Error::msg)?,
            None => SandboxLevel::WorkspaceWrite,
        };
        let config = ChatConfig {
            harness,
            model: args.model.clone(),
            reasoning,
            model_options: Default::default(),
            sandbox,
        };

        let chat_id = uuid::Uuid::new_v4().to_string();
        let mut mutate = json!({
            "op": "createChat",
            "chatId": chat_id,
            "deviceId": device_id,
            "config": config,
        });
        if let Some(space) = &space {
            mutate["spaceId"] = json!(space.id);
        }
        if let Some(parent) = &parent_chat_id {
            mutate["parentChatId"] = json!(parent);
        }
        if let Some(branch) = args
            .branch
            .as_deref()
            .map(str::trim)
            .filter(|b| !b.is_empty())
        {
            mutate["branch"] = json!(branch);
        }
        let mut cwd = args
            .cwd
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .map(str::to_owned);
        // A projectless chat runs in its own scratch folder on a host that
        // creates them; naming it here keeps createChat and the first run
        // in agreement.
        if space.is_none()
            && cwd.is_none()
            && self.zeron.devices().await?.iter().any(|d| {
                d.id == device_id
                    && d.capabilities
                        .iter()
                        .any(|c| c == zeron_proto::capabilities::PROJECTLESS_SCRATCH_V1)
            })
        {
            cwd = Some(zeron_proto::projectless::scratch_cwd(
                &chat_id,
                chrono::Utc::now(),
            ));
        }
        if let Some(cwd) = &cwd {
            mutate["cwd"] = json!(cwd);
        }
        self.zeron.mutate(mutate).await?;
        if let Some(title) = args
            .title
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            self.zeron
                .mutate(json!({ "op": "renameChat", "chatId": chat_id, "title": title }))
                .await?;
        }

        let mut result = json!({
            "chatId": chat_id,
            "kind": kind,
            "deviceId": device_id,
            "project": space.as_ref().map(|s| json!({ "id": s.id, "name": s.display_name(), "path": s.path })),
            "harness": harness,
            "model": args.model,
            "reasoning": reasoning,
            "title": args.title,
            "parentChatId": parent_chat_id,
        });
        if let Some(prompt) = args.prompt.filter(|p| !p.trim().is_empty()) {
            // The row may not have folded into WatchChats yet; build the
            // chat locally from what we just wrote rather than re-reading.
            let chat = Chat {
                id: chat_id.clone(),
                device_id: device_id.clone(),
                title: args.title.clone(),
                archived: false,
                cwd: cwd.clone(),
                branch: args.branch.clone(),
                checkout_id: None,
                source_context: None,
                config: Some(config),
                last_message_preview: None,
                last_message_at: None,
                created_at: chrono::Utc::now(),
                harness_session_id: None,
                harness_session_cwd: None,
                parent_chat_id: parent_chat_id.clone(),
                space_id: space.as_ref().map(|s| s.id.clone()),
                last_seen_at: None,
                room_gen: None,
            };
            let notify = args.notify.unwrap_or(true);
            let body = self.attribute(&chat, prompt.trim(), notify).await;
            let (sent, message_id) = self
                .deliver(&chat, space.as_ref(), &harnesses, None, body, "run")
                .await?;
            result["sent"] = sent;
            let pending = Arc::new(PendingTurn {
                baseline: None,
                // A new chat has no earlier messages.
                start: TurnStart::default(),
                message_id: Some(message_id.clone()),
            });
            self.pending_turns
                .lock()
                .await
                .insert(chat.id.clone(), pending.clone());
            self.finish_send(
                &mut result,
                &chat,
                pending,
                &message_id,
                args.wait,
                notify,
                args.timeout_secs,
            )
            .await?;
        }
        Ok(result)
    }

    async fn read_chat(&self, args: ReadChatArgs) -> anyhow::Result<Value> {
        let chat = self.guarded(&args.chat, false).await?;
        let (entries, sessions) = tokio::try_join!(
            self.zeron.transcript_on(&chat.id, Some(&chat.device_id)),
            self.zeron.sessions()
        )?;
        let rendered = render_entries(
            &entries,
            RenderOptions {
                include_reasoning: args.include_reasoning,
                include_tools: args.include_tools.unwrap_or(true),
            },
        );
        let (status, _) = status_of(session_for(&sessions, &chat).as_ref());
        let harness = chat
            .config
            .as_ref()
            .map(|c| c.harness.as_str())
            .unwrap_or("unknown");
        let header = format!(
            "{} · {} · {harness} · {status}",
            chat.title.as_deref().unwrap_or("Untitled"),
            short(&chat.id)
        );
        let path = a2a::write_transcript(&self.files, &a2a::file_stem(&chat.id), &header, &rendered)
            .context("writing the transcript file")?;
        Ok(json!({
            "path": path,
            "format": "markdown (zeron_help files)",
            "messages": rendered.len(),
            "status": status,
            "pendingInput": last_pending_input(&rendered),
        }))
    }

    async fn read_subagent(&self, args: ReadSubagentArgs) -> anyhow::Result<Value> {
        let chat = self.guarded(&args.chat, false).await?;
        let entries = self.zeron.transcript(&chat.id).await?;
        let spawned: Vec<SubagentRef> = render_entries(&entries, RenderOptions::default())
            .into_iter()
            .flat_map(|m| m.subagents)
            .collect();
        let key = args.subagent.as_deref().map(str::trim).unwrap_or_default();
        if key.is_empty() {
            return Ok(json!({
                "chatId": chat.id,
                "title": chat.title,
                "subagents": spawned,
            }));
        }
        let found = pick_subagent(&spawned, key)?;
        let running = found.status == Some("running");
        let entries = self.zeron.subagent_transcript(&found.id, running).await?;
        let rendered = render_entries(
            &entries,
            RenderOptions {
                include_reasoning: args.include_reasoning,
                include_tools: args.include_tools.unwrap_or(true),
            },
        );
        let header = format!(
            "{} · subagent of {} · {}",
            found.name,
            short(&chat.id),
            found.status.unwrap_or("unknown")
        );
        let path = a2a::write_transcript(&self.files, &a2a::file_stem(&found.id), &header, &rendered)
            .context("writing the transcript file")?;
        Ok(json!({
            "path": path,
            "format": "markdown (zeron_help files)",
            "messages": rendered.len(),
            "subagent": found,
        }))
    }

    /// Children's (or one chat's) status, the way an orchestrator checks on
    /// its subagents without reading transcripts. `offline`: the row says
    /// working but its host has gone quiet past the staleness window.
    async fn chat_status(&self, key: Option<&str>) -> anyhow::Result<Value> {
        let chats = match key {
            Some(key) => vec![self.guarded(key, false).await?],
            None => {
                let me = match self.role().await {
                    Role::Orchestrator { me } => me.clone(),
                    Role::Subagent { parent, .. } => parent.clone(),
                    Role::Free => anyhow::bail!("not in a chat: pass chat"),
                };
                let mut chats = self.zeron.chats().await?;
                chats.retain(|c| !c.archived && c.parent_chat_id.as_deref() == Some(me.as_str()));
                chats
            }
        };
        let sessions = self.zeron.sessions().await?;
        let now = chrono::Utc::now();
        let mut rows = Vec::with_capacity(chats.len());
        for chat in &chats {
            let session = session_for(&sessions, chat);
            let stale = session.as_ref().is_some_and(|s| {
                s.status == SessionStatus::Working && now - s.updated_at > SESSION_STALE
            });
            let (status, _) = status_of(session.as_ref());
            let status = if stale { "offline".to_owned() } else { status };
            let mut row = json!({
                "chat": chat.id,
                "title": chat.title,
                "status": status,
                "lastActivity": chat.last_message_at.unwrap_or(chat.created_at),
            });
            if status == "awaitingInput" || status == "errored" {
                if let Ok(entries) = self.zeron.transcript_on(&chat.id, Some(&chat.device_id)).await {
                    let rendered = render_entries(&entries, RenderOptions::default());
                    if let Some(input) = last_pending_input(&rendered) {
                        row["question"] = input;
                    }
                    if let Some(error) = rendered.iter().rev().find_map(|m| m.errors.last()) {
                        row["error"] = json!(error);
                    }
                }
            }
            rows.push(row);
        }
        Ok(json!({ "chats": rows }))
    }

    /// A subagent's message to its orchestrator: into the parent's agent
    /// inbox (steered into its running turn or batched into its next one),
    /// never its composer queue. With `wait`, block until the parent's
    /// send_message lands here as the reply.
    async fn message_parent(
        &self,
        me: &str,
        parent: &str,
        text: &str,
        wait: bool,
        timeout_secs: Option<u64>,
    ) -> anyhow::Result<Value> {
        let title = match self.zeron.resolve_chat(me).await {
            Ok(chat) => chat.title.unwrap_or_default(),
            Err(_) => String::new(),
        };
        let label = if title.trim().is_empty() {
            short(me).to_owned()
        } else {
            format!("{} ({})", title.trim(), short(me))
        };
        let ask = if wait {
            "It is blocked waiting on your answer: send_message it."
        } else {
            "Reply with send_message only if it needs an answer."
        };
        let body = format!(
            "{}{label}, id {me}. {} {ask}]\n\n{text}",
            zeron_proto::agent_messages::MESSAGE_PREFIX,
            zeron_proto::agent_messages::FROM_SUBAGENT
        );
        let parent_chat = self.zeron.resolve_chat(parent).await?;
        let delivery = self
            .zeron
            .call(
                methods::DELIVER_EVENT,
                json!({ "chatId": parent, "text": body, "targetDeviceId": parent_chat.device_id }),
            )
            .await?;
        let mut result = json!({
            "to": parent,
            "delivery": delivery.get("delivery").cloned().unwrap_or(Value::Null),
        });
        if !wait {
            return Ok(result);
        }
        let deadline = Instant::now() + wait_duration(timeout_secs);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            let poll = left.min(Duration::from_secs(20));
            let reply = self
                .zeron
                .call(
                    methods::AWAIT_AGENT_REPLY,
                    json!({ "chatId": me, "waitMs": poll.as_millis() as u64 }),
                )
                .await?;
            if let Some(text) = reply.get("text").and_then(Value::as_str) {
                result["reply"] = json!(text);
                return Ok(result);
            }
        }
        // Gave up: a reply that slipped in at the last moment still counts,
        // and later ones arrive as ordinary messages.
        let late = self
            .zeron
            .call(methods::CANCEL_AGENT_REPLY, json!({ "chatId": me }))
            .await
            .ok()
            .and_then(|r| r.get("text").and_then(Value::as_str).map(str::to_owned));
        match late {
            Some(text) => result["reply"] = json!(text),
            None => {
                result["timedOut"] = json!(true);
                result["note"] = json!("No reply yet. It will arrive as a message in this chat; continue with your best judgement.");
            }
        }
        Ok(result)
    }

    async fn search_chats(&self, args: SearchArgs) -> anyhow::Result<Value> {
        let query = args.query.trim();
        if query.is_empty() {
            anyhow::bail!("query is empty");
        }
        let mode = args.mode.as_deref().unwrap_or("words");
        let matcher = match mode {
            "words" => Matcher::words(query),
            "regex" => Matcher::regex(query, args.case_sensitive),
            other => anyhow::bail!("unknown mode {other:?} (use words or regex)"),
        }
        .map_err(anyhow::Error::msg)?;
        let role = match args.role.as_deref() {
            None => None,
            Some(r @ ("user" | "assistant")) => Some(r),
            Some(other) => anyhow::bail!("unknown role {other:?} (use user or assistant)"),
        };

        let (mut chats, spaces, sessions) = self
            .filtered_chats(
                args.project.as_deref(),
                args.device.as_deref(),
                args.include_archived,
                args.parent.as_deref(),
            )
            .await?;
        if !args.include_self
            && let Some(origin) = self.zeron.origin().chat_id.as_deref()
        {
            chats.retain(|c| c.id != origin);
        }
        chats.sort_by_key(|c| std::cmp::Reverse(c.last_message_at.unwrap_or(c.created_at)));
        let candidates = chats.len();
        chats.truncate(
            args.max_chats
                .unwrap_or(SEARCH_DEFAULT_SCAN)
                .clamp(1, SEARCH_MAX_SCAN),
        );
        let attempted = chats.len();

        let options = RenderOptions {
            include_reasoning: args.include_reasoning,
            include_tools: args.include_tools.unwrap_or(true),
        };
        let (matcher_ref, role) = (&matcher, role);
        let fetched: Vec<Option<(ScannedChat, crate::search::ChatScan)>> = futures::stream::iter(
            chats,
        )
        .map(|chat| async move {
            let entries = match self.zeron.transcript(&chat.id).await {
                Ok(entries) => entries,
                Err(err) => {
                    tracing::debug!(chat = %chat.id, "search: transcript unavailable: {err}");
                    return None;
                }
            };
            let rendered = render_entries(&entries, options);
            let scan = matcher_ref.scan(
                chat.title.as_deref(),
                rendered
                    .iter()
                    .enumerate()
                    .filter(|(_, m)| role.is_none_or(|r| role_name(m.role) == r)),
            );
            let messages = rendered
                .into_iter()
                .map(|m| (m.id, role_name(m.role), m.created_at_iso))
                .collect();
            Some((ScannedChat { chat, messages }, scan))
        })
        .buffer_unordered(SEARCH_CONCURRENCY)
        .collect()
        .await;
        let (kept, scans): (Vec<_>, Vec<_>) = fetched.into_iter().flatten().unzip();

        let limit = args.limit.unwrap_or(10).clamp(1, 100);
        let snippets = args.snippets.unwrap_or(3).clamp(1, 20);
        let min_match = args.min_match.unwrap_or(0.5).clamp(0.0, 1.0);
        let (matched_chats, ranked) = rank(&matcher, &scans, min_match, limit, snippets);
        let results: Vec<Value> = ranked
            .iter()
            .map(|r| {
                let scanned = &kept[r.scan];
                let total = scanned.messages.len();
                json!({
                    "chat": summarize_chat(&scanned.chat, &spaces, &sessions),
                    "score": (f64::from(r.score) * 100.0).round() / 100.0,
                    "titleMatch": r.title_match,
                    "matchedTerms": r.matched,
                    "messageCount": total,
                    "matches": r.hits.iter().map(|h| {
                        let (id, role, at) = &scanned.messages[h.index];
                        json!({
                            "messageId": id,
                            "role": role,
                            "at": at,
                            "offsetFromNewest": total - 1 - h.index,
                            "matchedTerms": h.matched,
                            "snippet": h.snippet,
                        })
                    }).collect::<Vec<_>>(),
                })
            })
            .collect();
        Ok(json!({
            "query": query,
            "mode": mode,
            "candidateChats": candidates,
            "scannedChats": kept.len(),
            "unreadableChats": attempted - kept.len(),
            "truncated": attempted < candidates,
            "matchedChats": matched_chats,
            "results": results,
        }))
    }

    async fn send_message(&self, args: SendArgs) -> anyhow::Result<Value> {
        let text = args.text.trim();
        if let Role::Subagent { me, parent } = self.role().await.clone() {
            anyhow::ensure!(!text.is_empty(), "text is empty");
            return self
                .message_parent(&me, &parent, text, args.wait, args.timeout_secs)
                .await;
        }
        let key = args
            .chat
            .as_deref()
            .context("chat is required: which chat to message")?;
        let chat = self.guarded(key, true).await?;
        // A child blocked in send_message {wait: true} takes this as its
        // tool result instead of a new turn.
        if !text.is_empty() && args.answers.is_none() {
            let reply = self
                .zeron
                .call(
                    methods::DELIVER_AGENT_REPLY,
                    json!({ "chatId": chat.id, "text": text, "targetDeviceId": chat.device_id }),
                )
                .await;
            if reply.is_ok_and(|r| r["delivered"] == true) {
                return Ok(json!({
                    "chatId": chat.id,
                    "title": chat.title,
                    "sent": { "delivery": "reply", "note": "It was waiting on you: your text is its tool result." },
                }));
            }
        }
        let sessions = self.zeron.sessions().await?;
        let waiting = session_for(&sessions, &chat)
            .is_some_and(|s| s.status == SessionStatus::AwaitingInput);
        if waiting || args.answers.is_some() {
            return self.answer(&chat, text, args.answers).await;
        }
        anyhow::ensure!(!text.is_empty(), "text is empty");
        if args.interrupt {
            self.zeron
                .queue_command(&chat.id, &SessionCommandPayload::Interrupt {})
                .await?;
            self.settle(&chat, Duration::from_secs(8)).await;
        }
        if self.zeron.origin().chat_id.as_deref() == Some(chat.id.as_str()) {
            anyhow::bail!(
                "refusing to send a message to your own chat ({})",
                short(&chat.id)
            );
        }
        let (spaces, sessions, harnesses) = tokio::try_join!(
            self.zeron.spaces(),
            self.zeron.sessions(),
            self.zeron.harnesses_on(Some(&chat.device_id))
        )?;
        let space = chat
            .space_id
            .as_deref()
            .and_then(|id| spaces.iter().find(|s| s.id == id));
        let baseline = session_for(&sessions, &chat);
        let mode = args.mode.as_deref().unwrap_or("auto");
        let message_ids = self
            .zeron
            .transcript_on(&chat.id, Some(&chat.device_id))
            .await?
            .into_iter()
            .map(|m| m.id)
            .collect();
        let notify = args.notify.unwrap_or(true);
        let body = self.attribute(&chat, text, notify).await;
        let mut result = json!({
            "chatId": chat.id,
            "title": chat.title,
        });
        let (sent, message_id) = self
            .deliver(&chat, space, &harnesses, baseline.as_ref(), body, mode)
            .await?;
        result["sent"] = sent;
        let pending = Arc::new(PendingTurn {
            baseline,
            start: TurnStart { message_ids },
            message_id: Some(message_id.clone()),
        });
        self.pending_turns
            .lock()
            .await
            .insert(chat.id.clone(), pending.clone());
        self.finish_send(
            &mut result,
            &chat,
            pending,
            &message_id,
            args.wait,
            notify,
            args.timeout_secs,
        )
        .await?;
        Ok(result)
    }

    /// After a send: wait for the reply when asked, and otherwise (or when
    /// the wait runs out) have the engine notify this chat when the turn
    /// ends, so the caller never has to poll.
    #[allow(clippy::too_many_arguments)]
    async fn finish_send(
        &self,
        result: &mut Value,
        chat: &Chat,
        pending: Arc<PendingTurn>,
        message_id: &str,
        wait: bool,
        notify: bool,
        timeout_secs: Option<u64>,
    ) -> anyhow::Result<()> {
        let mut settled = false;
        if wait {
            let turn = self
                .await_pending_turn(chat, pending, wait_duration(timeout_secs))
                .await?;
            settled = turn["outcome"] == "completed" || turn["outcome"] == "errored";
            let input = turn["pendingInput"]["requestId"]
                .as_str()
                .map(str::to_owned);
            result["turn"] = turn;
            if notify && !settled {
                result["notice"] = self.watch_turn(chat, message_id, input.as_deref()).await;
            }
        } else if notify {
            result["notice"] = self.watch_turn(chat, message_id, None).await;
        }
        if !notify && !settled && self.zeron.origin().chat_id.is_some() {
            result["notice"] = json!("No automatic notice (notify=false).");
        }
        Ok(())
    }

    /// Ask the engine to tell this chat when `chat` finishes the turn that
    /// handles `message_id`. The result is the note returned to the agent.
    async fn watch_turn(
        &self,
        chat: &Chat,
        message_id: &str,
        reported_input: Option<&str>,
    ) -> Value {
        let Some(origin) = self.zeron.origin().chat_id.as_deref() else {
            return Value::Null;
        };
        let mut params = json!({
            "watcherChatId": origin,
            "chatId": chat.id,
            "messageId": message_id,
        });
        if let Some(request) = reported_input {
            params["reportedInput"] = json!(request);
        }
        match self.zeron.call(methods::NOTIFY_TURN_END, params).await {
            Ok(_) => json!(
                "When that turn ends, its final response arrives in this chat as an automatic Zeron notice. Don't poll, wait, or message it to check on progress: continue other work, or end your turn if you have nothing else to do."
            ),
            Err(err) => json!(format!(
                "Automatic notices are unavailable on this engine ({err:#}); use wait_for_turn to collect the reply."
            )),
        }
    }

    /// Drop this chat's pending notice about `chat` (its result was read
    /// through a wait, or this chat stopped it).
    async fn cancel_notice(&self, chat: &Chat) {
        let Some(origin) = self.zeron.origin().chat_id.as_deref() else {
            return;
        };
        let _ = self
            .zeron
            .call(
                methods::CANCEL_TURN_NOTICE,
                json!({ "watcherChatId": origin, "chatId": chat.id }),
            )
            .await;
    }

    async fn wait_for_turn(&self, args: WaitArgs) -> anyhow::Result<Value> {
        let chat = self.guarded(&args.chat, true).await?;
        let pending = self.pending_turns.lock().await.get(&chat.id).cloned();
        let turn = match pending {
            Some(pending) => {
                self.await_pending_turn(&chat, pending, wait_duration(args.timeout_secs))
                    .await?
            }
            None => {
                self.await_turn(
                    &chat,
                    None,
                    false,
                    wait_duration(args.timeout_secs),
                    TurnStart::default(),
                )
                .await?
            }
        };
        // The reply is in hand: the automatic notice would repeat it.
        if turn["outcome"] == "completed" || turn["outcome"] == "errored" {
            self.cancel_notice(&chat).await;
        }
        Ok(json!({ "chatId": chat.id, "title": chat.title, "turn": turn }))
    }

    async fn archive_chat(&self, args: ArchiveArgs) -> anyhow::Result<Value> {
        let chat = self.guarded(&args.chat, true).await?;
        let archived = args.archived.unwrap_or(true);
        self.zeron
            .mutate(json!({ "op": "setChatArchived", "chatId": chat.id, "archived": archived }))
            .await?;
        Ok(json!({ "chatId": chat.id, "title": chat.title, "archived": archived }))
    }

    async fn show_visual(&self, args: ShowVisualArgs) -> anyhow::Result<Value> {
        let chat_id = self
            .zeron
            .origin()
            .chat_id
            .clone()
            .context("show_visual only works from an agent Zeron launched in a chat")?;
        let assets = read_visual_assets(&args.assets).await?;
        let shown = self
            .zeron
            .call(
                zeron_rpc::methods::PUBLISH_VISUAL,
                json!({
                    "chatId": chat_id,
                    "title": args.title,
                    "script": args.script,
                    "assets": assets,
                }),
            )
            .await?;
        Ok(json!({
            "visual": shown,
            "message": "Shown to the reader above your reply. Don't mention or describe the visual; reply with only what it doesn't already say.",
        }))
    }

    async fn interrupt_chat(&self, args: ChatArgs) -> anyhow::Result<Value> {
        let chat = self.guarded(&args.chat, true).await?;
        let command_id = self
            .zeron
            .queue_command(&chat.id, &SessionCommandPayload::Interrupt {})
            .await?;
        // Stopping it is this chat's own doing; no notice about it.
        self.cancel_notice(&chat).await;
        Ok(json!({ "chatId": chat.id, "commandId": command_id }))
    }

    async fn respond_to_input(&self, args: RespondArgs) -> anyhow::Result<Value> {
        let chat = self.guarded(&args.chat, true).await?;
        self.respond(&chat, args.request_id, args.answers).await
    }

    /// Answer `chat`'s pending question from free text: a text equal to an
    /// option label picks it, anything else is a free-text answer to every
    /// open question. `answers` overrides for multi-question forms.
    async fn answer(
        &self,
        chat: &Chat,
        text: &str,
        answers: Option<Vec<AnswerArg>>,
    ) -> anyhow::Result<Value> {
        if let Some(answers) = answers {
            return self.respond(chat, None, answers).await;
        }
        anyhow::ensure!(!text.is_empty(), "text is empty: it would answer the pending question");
        let entries = self
            .zeron
            .transcript_on(&chat.id, Some(&chat.device_id))
            .await?;
        let rendered = render_entries(&entries, RenderOptions::default());
        let pending = last_pending_input(&rendered)
            .with_context(|| format!("chat {} has no pending question", short(&chat.id)))?;
        let answers = pending["questions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|q| {
                let id = q.get("id").and_then(Value::as_str)?;
                let label = q["options"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|o| o.get("label").and_then(Value::as_str))
                    .find(|l| l.eq_ignore_ascii_case(text))
                    .unwrap_or(text);
                Some(AnswerArg {
                    question_id: id.to_owned(),
                    labels: vec![label.to_owned()],
                })
            })
            .collect();
        let request = pending["requestId"].as_str().map(str::to_owned);
        let mut result = self.respond(chat, request, answers).await?;
        result["delivery"] = json!("answer");
        Ok(result)
    }

    /// Wait briefly for a stopped chat's turn to wind down.
    async fn settle(&self, chat: &Chat, max: Duration) {
        let deadline = Instant::now() + max;
        while Instant::now() < deadline {
            let working = self.zeron.sessions().await.ok().is_some_and(|sessions| {
                session_for(&sessions, chat).is_some_and(|s| s.status == SessionStatus::Working)
            });
            if !working {
                return;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    async fn respond(
        &self,
        chat: &Chat,
        request_id: Option<String>,
        answers: Vec<AnswerArg>,
    ) -> anyhow::Result<Value> {
        let args = RespondArgs {
            chat: chat.id.clone(),
            request_id,
            answers,
        };
        let request_id = match args.request_id {
            Some(id) => id,
            None => {
                let entries = self
                    .zeron
                    .transcript_on(&chat.id, Some(&chat.device_id))
                    .await?;
                let rendered = render_entries(&entries, RenderOptions::default());
                last_pending_input(&rendered)
                    .and_then(|p| {
                        p.get("requestId")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    })
                    .ok_or_else(|| {
                        anyhow::anyhow!("chat {} has no pending question", short(&chat.id))
                    })?
            }
        };
        if args.answers.is_empty() {
            anyhow::bail!("answers is empty");
        }
        let answers = args
            .answers
            .into_iter()
            .map(|a| UserInputAnswer {
                question_id: a.question_id,
                labels: a.labels,
            })
            .collect();
        let command_id = self
            .zeron
            .queue_command(
                &chat.id,
                &SessionCommandPayload::RespondInput {
                    request_id: request_id.clone(),
                    answers,
                },
            )
            .await?;
        Ok(json!({ "chatId": chat.id, "requestId": request_id, "commandId": command_id }))
    }

    // ---- shared pieces -------------------------------------------------------

    /// Prefix the sender's identity when this server speaks for a chat, so
    /// the receiving agent (and the human reading that transcript) can tell
    /// an agent-to-agent message from a typed one, and so the agent knows its
    /// answer is routed back on its own: the old "reply with send_message"
    /// instruction is what made chats answer, re-answer, and steer each other.
    /// The sender's full id is included: display prefixes need not be unique.
    async fn attribute(&self, target: &Chat, text: &str, notify: bool) -> String {
        let Some(origin_id) = self.zeron.origin().chat_id.as_deref() else {
            return text.to_owned();
        };
        if origin_id == target.id {
            return text.to_owned();
        }
        let title = match self.zeron.resolve_chat(origin_id).await {
            Ok(chat) => chat.title,
            Err(_) => None,
        };
        let label = match title {
            Some(title) if !title.trim().is_empty() => {
                format!("{} ({})", title.trim(), short(origin_id))
            }
            _ => short(origin_id).to_owned(),
        };
        let routing = if notify {
            "Your final response goes back to it automatically when this turn ends, so don't reply with send_message."
        } else {
            "It doesn't need a reply."
        };
        format!(
            "{}{label}, id {origin_id}. {routing}]\n\n{text}",
            zeron_proto::agent_messages::MESSAGE_PREFIX
        )
    }

    /// Pick and perform the delivery the composer would.
    async fn deliver(
        &self,
        chat: &Chat,
        space: Option<&Space>,
        harnesses: &[HarnessInfo],
        session: Option<&Session>,
        text: String,
        mode: &str,
    ) -> anyhow::Result<(Value, String)> {
        let harness = chat
            .config
            .as_ref()
            .map(|c| c.harness)
            .map_or_else(|| default_harness(harnesses), Ok)?;
        let (status, _) = status_of(session);
        // A quiet tool call can outlive the UI's stale-status window. Route
        // through steering and let the host decide whether a live run exists.
        let live = session.map(|s| s.status).unwrap_or(SessionStatus::Idle);
        let chosen = match mode {
            "auto" => match live {
                SessionStatus::Idle | SessionStatus::Errored => "run",
                SessionStatus::Working => "steer",
                SessionStatus::AwaitingInput => anyhow::bail!(
                    "chat {} is waiting for an answer; use respond_to_input (or mode 'queue' to hold this message for after the turn)",
                    short(&chat.id)
                ),
            },
            "run" | "steer" | "queue" => mode,
            other => anyhow::bail!("unknown mode {other:?}; expected auto, run, steer, or queue"),
        };
        // The user message's id, known up front so a wait (or a notice) can
        // anchor on the exact turn that handles it.
        let message_id = uuid::Uuid::new_v4().to_string();
        let (id, message_id) = match chosen {
            "run" => {
                let config = chat.config.clone();
                let cwd = chat
                    .cwd
                    .clone()
                    .or_else(|| space.map(|s| s.path.clone()))
                    .unwrap_or_else(|| "~".into());
                let request = RunRequest {
                    mcp: None,
                    prompt: text,
                    harness: Some(harness),
                    model: config.as_ref().and_then(|c| c.model.clone()),
                    reasoning: config.as_ref().and_then(|c| c.reasoning),
                    model_options: config
                        .as_ref()
                        .map(|c| c.model_options.clone())
                        .unwrap_or_default(),
                    cwd,
                    sandbox: config
                        .as_ref()
                        .map(|c| c.sandbox)
                        .unwrap_or(SandboxLevel::WorkspaceWrite),
                    auto_approve: false,
                    resume: None,
                    rewind_to: None,
                    prompt_id: None,
                    resume_at: None,
                    attachments: Vec::new(),
                    worktree: None,
                };
                let id = self
                    .zeron
                    .queue_command(
                        &chat.id,
                        &SessionCommandPayload::Run {
                            request,
                            message_id: message_id.clone(),
                        },
                    )
                    .await?;
                (id, message_id)
            }
            "steer" => {
                let id = self
                    .zeron
                    .queue_command(
                        &chat.id,
                        &SessionCommandPayload::Steer {
                            prompt: text,
                            message_id: Some(message_id.clone()),
                        },
                    )
                    .await?;
                (id, message_id)
            }
            // A queue row keeps its id when it becomes the user message.
            _ => {
                let id = self.zeron.queue_message(&chat.id, &text).await?;
                (id.clone(), id)
            }
        };
        Ok((
            json!({
                "delivery": chosen,
                "id": id,
                "chatStatusAtSend": status,
            }),
            message_id,
        ))
    }

    async fn await_pending_turn(
        &self,
        chat: &Chat,
        pending: Arc<PendingTurn>,
        timeout: Duration,
    ) -> anyhow::Result<Value> {
        let turn = match pending.message_id.as_deref() {
            Some(message_id) => {
                let turn = self
                    .zeron
                    .await_message_turn(chat, message_id, timeout, None)
                    .await?;
                message_turn_json(&turn)
            }
            None => {
                self.await_turn(
                    chat,
                    pending.baseline.as_ref(),
                    true,
                    timeout,
                    pending.start.clone(),
                )
                .await?
            }
        };
        if turn["timedOut"] == false {
            let mut turns = self.pending_turns.lock().await;
            // A concurrent send may already have installed a newer baseline.
            if turns
                .get(&chat.id)
                .is_some_and(|p| Arc::ptr_eq(p, &pending))
            {
                turns.remove(&chat.id);
            }
        }
        Ok(turn)
    }

    /// Wait, then report the outcome with the assistant messages that
    /// are not in `start`'s baseline.
    async fn await_turn(
        &self,
        chat: &Chat,
        baseline: Option<&Session>,
        expect_turn: bool,
        timeout: Duration,
        start: TurnStart,
    ) -> anyhow::Result<Value> {
        let deadline = Instant::now() + timeout;
        let (mut outcome, session) = self
            .zeron
            .wait_for_turn(chat, baseline, expect_turn, timeout)
            .await?;
        // Registry session updates can arrive before the separate transcript doc.
        // Keep the same deadline while waiting for a response to this send.
        let mut rendered = Vec::new();
        let mut replies = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                if outcome == TurnOutcome::Completed && expect_turn {
                    outcome = TurnOutcome::TimedOut;
                }
                break;
            }
            let entries = match tokio::time::timeout(
                remaining,
                self.zeron.transcript_on(&chat.id, Some(&chat.device_id)),
            )
            .await
            {
                Ok(entries) => entries?,
                Err(_) => {
                    outcome = TurnOutcome::TimedOut;
                    break;
                }
            };
            rendered = render_entries(&entries, RenderOptions::default());
            replies = rendered
                .iter()
                .filter(|m| m.role == zeron_doc::MessageRole::Assistant)
                .filter(|m| !start.message_ids.contains(&m.id))
                .cloned()
                .collect();
            if !expect_turn
                || outcome != TurnOutcome::Completed
                || replies
                    .iter()
                    .any(|m| m.status != Some(zeron_doc::MessageStatus::Streaming))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100).min(remaining)).await;
        }
        let (status, _) = status_of(session.as_ref());
        Ok(json!({
            "outcome": outcome,
            "status": status,
            "timedOut": outcome == TurnOutcome::TimedOut,
            "pendingInput": last_pending_input(&rendered),
            "replies": replies,
        }))
    }
}

/// A message-anchored wait in the same shape as [`Tools::await_turn`]'s.
fn message_turn_json(turn: &MessageTurn) -> Value {
    let (status, _) = status_of(turn.session.as_ref());
    json!({
        "outcome": turn.outcome,
        "status": status,
        "timedOut": turn.outcome == TurnOutcome::TimedOut,
        "delivered": turn.delivered,
        "pendingInput": turn.pending_input(),
        "replies": turn.replies(),
    })
}

/// claude-code when it is offered here, else the first available harness.
/// Exact id first, then a unique fragment of the id or of the spawn label.
fn pick_subagent(spawned: &[SubagentRef], key: &str) -> anyhow::Result<SubagentRef> {
    if let Some(exact) = spawned.iter().find(|s| s.id == key) {
        return Ok(exact.clone());
    }
    let needle = key.to_lowercase();
    let hits: Vec<&SubagentRef> = spawned
        .iter()
        .filter(|s| {
            s.id.to_lowercase().contains(&needle) || s.name.to_lowercase().contains(&needle)
        })
        .collect();
    match hits.as_slice() {
        [one] => Ok((*one).clone()),
        [] => anyhow::bail!(
            "no subagent matches {key:?}; omit `subagent` to list this chat's {}",
            spawned.len()
        ),
        many => anyhow::bail!(
            "{key:?} matches {} subagents ({}); use a longer fragment or the full id",
            many.len(),
            many.iter()
                .map(|s| s.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn default_harness(harnesses: &[HarnessInfo]) -> anyhow::Result<HarnessId> {
    harnesses
        .iter()
        .find(|h| h.id == HarnessId::ClaudeCode && h.available())
        .or_else(|| {
            harnesses
                .iter()
                .find(|h| h.available() && h.id != HarnessId::Mock)
        })
        .map(|h| h.id)
        .ok_or_else(|| {
            anyhow::anyhow!("no harness is available on this device (see list_harnesses)")
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zeron::Origin;
    use async_trait::async_trait;
    use futures::StreamExt;
    use std::sync::Mutex;
    use zeron_rpc::{RpcError, RpcReply, RpcService, memory_client, methods};

    /// Two devices with distinct catalogs and repeated project paths, plus
    /// two chats with a two-message transcript. Writes are recorded for assertions.
    #[derive(Default)]
    struct World {
        writes: Mutex<Vec<(String, Value)>>,
        dispatch_barrier: Option<tokio::sync::Barrier>,
        beta_parent: Option<String>,
        beta_remote: bool,
        extra_chats: Vec<Value>,
        catalog_error: Option<&'static str>,
        reads: Mutex<Vec<(String, Value)>>,
        /// Alpha spawned two subagents: `--sub--live` (running, doc only)
        /// and `--sub--frozen` (done, with a blob).
        alpha_subagents: bool,
        /// Replaces the assistant reply in Beta's transcript.
        beta_reply: Option<String>,
        /// A subagent is blocked in send_message {wait: true}.
        child_waiting: bool,
    }

    fn stream(item: Value) -> RpcReply {
        RpcReply::Stream(futures::stream::iter(vec![item]).boxed())
    }

    #[async_trait]
    impl RpcService for World {
        async fn handle(&self, method: &str, params: Value) -> Result<RpcReply, RpcError> {
            if matches!(method, methods::LIST_HARNESSES | methods::LIST_MODELS) {
                self.reads
                    .lock()
                    .unwrap()
                    .push((method.into(), params.clone()));
                if self.catalog_error == Some(method) {
                    return Err(RpcError::Failed("catalog unavailable".into()));
                }
            }
            let remote = params["targetDeviceId"] == "dev-remote";
            Ok(match method {
                methods::LOCAL_DEVICE => RpcReply::Value(json!({ "deviceId": "dev-local" })),
                methods::ENGINE_INFO => RpcReply::Value(json!({
                    "deviceId": "dev-local", "workspaceScope": "local"
                })),
                methods::WATCH_DEVICES => stream(json!([{
                    "id": "dev-local", "name": "Laptop", "platform": "linux",
                    "lastSeenAt": null
                }, {"id": "dev-remote", "name": "Worker", "platform": "linux", "lastSeenAt": null}])),
                methods::WATCH_SPACES => stream(json!([{
                    "id": "space-1", "deviceId": "dev-local", "path": "/repo/comet",
                    "gitDetected": true, "createdAt": "2026-09-01T00:00:00Z"
                }, {
                    "id": "space-remote", "deviceId": "dev-remote", "path": "/repo/comet",
                    "gitDetected": true, "createdAt": "2026-09-01T00:00:00Z"
                }, {
                    "id": "space-unique", "deviceId": "dev-remote", "path": "/repo/unique",
                    "gitDetected": true, "createdAt": "2026-09-01T00:00:00Z"
                }])),
                methods::WATCH_CHATS => {
                    let mut chats = json!([
                    {
                        "id": "chat-alpha-1", "deviceId": "dev-local", "title": "Alpha",
                        "archived": false, "spaceId": "space-1",
                        "config": { "harness": "claude-code", "model": "opus", "reasoning": null, "sandbox": "workspace-write" },
                        "createdAt": "2026-09-01T00:00:00Z"
                    },
                    {
                        "id": "chat-beta-2", "deviceId": if self.beta_remote { "dev-remote" } else { "dev-local" }, "title": "Beta",
                        "parentChatId": self.beta_parent,
                        "archived": false, "spaceId": "space-1",
                        "createdAt": "2026-09-02T00:00:00Z"
                    }
                    ]);
                    chats
                        .as_array_mut()
                        .unwrap()
                        .extend(self.extra_chats.clone());
                    stream(chats)
                }
                methods::WATCH_SESSIONS => stream(json!([])),
                methods::LIST_HARNESSES if remote => RpcReply::Value(json!([
                    {"id":"codex", "name":"Remote Codex", "installed":true},
                    {"id":"cursor", "name":"Disabled Cursor", "installed":true, "enabled":false}
                ])),
                methods::LIST_HARNESSES => RpcReply::Value(json!([
                    { "id": "claude-code", "name": "Claude Code", "supportsSteering": true,
                      "steeringMode": "step-boundary", "reasoningLevels": [], "installed": true, "enabled": true },
                    { "id": "codex", "name": "Codex", "supportsSteering": true,
                      "steeringMode": "turn-boundary", "reasoningLevels": [], "installed": false, "enabled": true }
                ])),
                methods::LIST_MODELS if remote => RpcReply::Value(json!([
                    {"id":"remote-model", "label":"Remote model"}
                ])),
                methods::LIST_MODELS => RpcReply::Value(json!([
                    { "id": "opus", "label": "Opus" }, { "id": "sonnet", "label": "Sonnet" }
                ])),
                methods::WATCH_DOC_MESSAGES
                    if params["chatId"].as_str() == Some("chat-alpha-1--sub--live") =>
                {
                    stream(json!({ "reset": [
                        { "id": "s1", "role": "assistant", "createdAt": 3, "deviceId": "dev-local",
                          "status": "streaming",
                          "parts": [{ "kind": "text", "id": "t", "text": "still reading" }] }
                    ]}))
                }
                methods::FETCH_TOOL_BLOB => {
                    if params["blobRef"] != "chat-alpha-1/chat-alpha-1--sub--frozen" {
                        return Err(RpcError::Failed("no such blob".into()));
                    }
                    let entries = json!([
                        { "id": "s1", "role": "assistant", "createdAt": 3, "deviceId": "dev-local",
                          "status": "complete",
                          "parts": [{ "kind": "text", "id": "t", "text": "found 3 bugs" }] }
                    ]);
                    RpcReply::Value(json!({ "text": entries.to_string() }))
                }
                methods::WATCH_DOC_MESSAGES
                    if self.alpha_subagents
                        && params["chatId"].as_str() == Some("chat-alpha-1") =>
                {
                    let chip = |suffix: &str, label: &str, status: &str| {
                        json!({
                            "kind": "tool", "id": suffix, "resolved": true,
                            "call": { "kind": "unknown", "name": label },
                            "subagentRef": format!("chat-alpha-1--sub--{suffix}"),
                            "subagentStatus": status
                        })
                    };
                    stream(json!({ "reset": [
                        { "id": "a1", "role": "assistant", "createdAt": 2, "deviceId": "dev-local",
                          "status": "complete",
                          "parts": [
                            chip("live", "Agent: scan the parser", "running"),
                            chip("frozen", "Agent: audit the lexer", "done")
                          ] }
                    ]}))
                }
                methods::WATCH_DOC_MESSAGES => {
                    let reply = match (&self.beta_reply, params["chatId"].as_str()) {
                        (Some(text), Some("chat-beta-2")) => text.as_str(),
                        _ => "hello back",
                    };
                    stream(json!({ "reset": [
                        { "id": "u1", "role": "user", "createdAt": 1, "deviceId": "dev-local",
                          "parts": [{ "kind": "text", "id": "t", "text": "hi" }] },
                        { "id": "a1", "role": "assistant", "createdAt": 2, "deviceId": "dev-local",
                          "status": "complete",
                          "parts": [{ "kind": "text", "id": "t", "text": reply }] }
                    ]}))
                }
                methods::NOTIFY_TURN_END | methods::CANCEL_TURN_NOTICE => {
                    self.writes
                        .lock()
                        .unwrap()
                        .push((method.to_owned(), params));
                    RpcReply::Value(json!({ "ok": true, "cancelled": true }))
                }
                methods::LIST_TURN_NOTICES => RpcReply::Value(json!({ "watches": [] })),
                methods::DELIVER_EVENT | methods::DELIVER_AGENT_REPLY | methods::CANCEL_AGENT_REPLY => {
                    self.writes
                        .lock()
                        .unwrap()
                        .push((method.to_owned(), params));
                    RpcReply::Value(json!({
                        "delivery": "held",
                        "delivered": self.child_waiting,
                        "text": null,
                    }))
                }
                methods::AWAIT_AGENT_REPLY => RpcReply::Value(json!({ "text": "use the v2 API" })),
                methods::PUBLISH_VISUAL => {
                    self.writes
                        .lock()
                        .unwrap()
                        .push((method.to_owned(), params.clone()));
                    RpcReply::Value(json!({ "id": "v:1", "title": params["title"] }))
                }
                methods::MUTATE | methods::QUEUE_COMMAND | methods::QUEUE_MESSAGE => {
                    self.writes
                        .lock()
                        .unwrap()
                        .push((method.to_owned(), params));
                    if method == methods::QUEUE_COMMAND
                        && let Some(barrier) = &self.dispatch_barrier
                    {
                        // No dispatch can finish until both chats have work.
                        // A sequential batch deadlocks here, before any waits.
                        barrier.wait().await;
                    }
                    RpcReply::Value(json!({ "commandId": "cmd-1", "id": "q-1" }))
                }
                other => return Err(RpcError::UnknownMethod(other.into())),
            })
        }
    }

    /// The transcript file a read tool wrote.
    fn file(result: &Value) -> String {
        std::fs::read_to_string(result["path"].as_str().expect("a path")).unwrap()
    }

    fn tools(world: Arc<World>, origin: Origin) -> Tools {
        let client = memory_client(world);
        Tools::new(Arc::new(Zeron::with_client(client, origin)))
    }

    #[tokio::test]
    async fn catalog_is_well_formed() {
        let defs = catalog();
        let mut names: Vec<&str> = defs.iter().map(|d| d.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), defs.len(), "tool names must be unique");
        for def in &defs {
            assert_eq!(def.input_schema["type"], "object", "{}", def.name);
            assert!(!def.description.is_empty());
        }
    }

    #[test]
    fn the_show_visual_example_passes_its_own_check() {
        let start = SHOW_VISUAL_DESCRIPTION.find("local s = ui.state").unwrap();
        let end = start
            + SHOW_VISUAL_DESCRIPTION[start..]
                .find("\n\nZeron runs the script")
                .unwrap();
        let report = zeron_visual::check(&SHOW_VISUAL_DESCRIPTION[start..end], 1).unwrap();
        assert_eq!(report.interactions, 2, "both tabs");
    }

    #[tokio::test]
    async fn show_visual_is_offered_only_inside_a_chat() {
        let world = Arc::new(World::default());
        let outside = tools(world.clone(), Origin::default());
        assert!(!outside.has("show_visual").await);
        assert!(
            outside
                .call(
                    "show_visual",
                    json!({ "script": "return ui.text 'x'", "title": "x" })
                )
                .await
                .is_err()
        );

        let inside = tools(
            world.clone(),
            Origin {
                chat_id: Some("chat-alpha-1".into()),
                device_id: Some("dev-local".into()),
            },
        );
        assert!(inside.has("show_visual").await);
        // The guidance names every color the runtime accepts.
        for name in zeron_visual::TOKEN_NAMES {
            assert!(SHOW_VISUAL_DESCRIPTION.contains(name), "{name}");
        }
        let shown = inside
            .call(
                "show_visual",
                json!({ "script": "return ui.text 'chart'", "title": "Chart" }),
            )
            .await
            .unwrap();
        assert_eq!(shown["visual"]["title"], "Chart");
        let writes = world.writes.lock().unwrap();
        let (method, params) = writes.last().unwrap();
        assert_eq!(method, methods::PUBLISH_VISUAL);
        assert_eq!(params["chatId"], "chat-alpha-1");
        assert_eq!(params["script"], "return ui.text 'chart'");
    }

    #[tokio::test]
    async fn read_subagent_lists_and_reads_running_and_frozen_transcripts() {
        let world = Arc::new(World {
            alpha_subagents: true,
            ..World::default()
        });
        let tools = tools(world, Origin::default());

        let read = tools
            .call("read_chat", json!({ "chat": "alpha" }))
            .await
            .unwrap();
        assert!(file(&read).contains("> subagent "));
        assert!(file(&read).contains(" running Agent: "));

        let listed = tools
            .call("read_subagent", json!({ "chat": "alpha" }))
            .await
            .unwrap();
        assert_eq!(listed["subagents"].as_array().unwrap().len(), 2);
        assert_eq!(listed["subagents"][1]["name"], "Agent: audit the lexer");

        let live = tools
            .call(
                "read_subagent",
                json!({ "chat": "alpha", "subagent": "live" }),
            )
            .await
            .unwrap();
        assert!(file(&live).contains("still reading"));

        // A label fragment resolves; the finished one comes from its blob.
        let frozen = tools
            .call(
                "read_subagent",
                json!({ "chat": "alpha", "subagent": "lexer" }),
            )
            .await
            .unwrap();
        assert_eq!(frozen["subagent"]["status"], "done");
        assert!(file(&frozen).contains("found 3 bugs"));

        let err = tools
            .call(
                "read_subagent",
                json!({ "chat": "alpha", "subagent": "Agent" }),
            )
            .await
            .unwrap_err();
        assert!(err.contains("matches 2 subagents"), "{err}");
    }

    #[tokio::test]
    async fn list_and_read_chats() {
        let world = Arc::new(World::default());
        let tools = tools(world, Origin::default());
        let listed = tools.call("list_chats", json!({})).await.unwrap();
        assert_eq!(listed["total"], 2);
        // Newest activity first: Beta was created later.
        assert_eq!(listed["chats"][0]["title"], "Beta");
        assert_eq!(listed["chats"][1]["project"]["name"], "comet");
        assert_eq!(listed["chats"][1]["status"], "idle");

        let read = tools
            .call("read_chat", json!({ "chat": "alpha" }))
            .await
            .unwrap();
        assert_eq!(read["messages"], 2);
        let text = file(&read);
        assert!(text.starts_with("# "));
        assert!(text.contains("\n## 1 user ") && text.contains("\n## 2 assistant "));
        assert!(text.contains("hello back"));
    }

    #[tokio::test]
    async fn search_chats_finds_content_loosely_and_by_regex() {
        let world = Arc::new(World {
            beta_reply: Some("The idle reaper kills persistent sessions after 20 minutes".into()),
            ..World::default()
        });
        let tools = tools(world, Origin::default());

        // Typo, fragment, and a word that is not in the title: still found.
        let found = tools
            .call("search_chats", json!({ "query": "persistant reaper" }))
            .await
            .unwrap();
        assert_eq!(found["scannedChats"], 2);
        assert_eq!(found["matchedChats"], 1);
        let hit = &found["results"][0];
        assert_eq!(hit["chat"]["title"], "Beta");
        let m = &hit["matches"][0];
        assert_eq!(m["role"], "assistant");
        assert_eq!(m["offsetFromNewest"], 0);
        assert!(m["snippet"].as_str().unwrap().contains("idle reaper"));

        // The hit's chat reads straight into a file.
        let read = tools
            .call("read_chat", json!({ "chat": "Beta" }))
            .await
            .unwrap();
        assert!(read["messages"].as_u64().unwrap() > 0);

        let regex = tools
            .call(
                "search_chats",
                json!({ "query": r"\d+ minutes", "mode": "regex" }),
            )
            .await
            .unwrap();
        assert_eq!(regex["results"][0]["chat"]["title"], "Beta");
        let strict = tools
            .call(
                "search_chats",
                json!({ "query": "REAPER", "mode": "regex", "case_sensitive": true }),
            )
            .await
            .unwrap();
        assert_eq!(strict["matchedChats"], 0);

        let users = tools
            .call("search_chats", json!({ "query": "reaper", "role": "user" }))
            .await
            .unwrap();
        assert_eq!(users["matchedChats"], 0);

        assert!(
            tools
                .call("search_chats", json!({ "query": "  " }))
                .await
                .is_err()
        );
        assert!(
            tools
                .call("search_chats", json!({ "query": "(", "mode": "regex" }))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn search_skips_the_calling_chat_and_reports_truncation() {
        let world = Arc::new(World::default());
        let origin = Origin {
            chat_id: Some("chat-alpha-1".into()),
            ..Origin::default()
        };
        let tools = tools(world, origin);
        let found = tools
            .call("search_chats", json!({ "query": "hello", "max_chats": 1 }))
            .await
            .unwrap();
        // Alpha is the caller; Beta is the only candidate and fits the cap.
        assert_eq!(found["candidateChats"], 1);
        assert_eq!(found["truncated"], false);
        assert_eq!(found["results"][0]["chat"]["title"], "Beta");

        let all = tools
            .call(
                "search_chats",
                json!({ "query": "hello", "include_self": true, "max_chats": 1 }),
            )
            .await
            .unwrap();
        assert_eq!(all["candidateChats"], 2);
        assert_eq!(all["truncated"], true);
    }

    #[tokio::test]
    async fn send_attributes_and_refuses_self() {
        let world = Arc::new(World::default());
        let tools = tools(
            world.clone(),
            Origin {
                chat_id: Some("chat-beta-2".into()),
                device_id: Some("dev-local".into()),
            },
        );
        let err = tools
            .call("send_message", json!({ "chat": "beta", "text": "loop" }))
            .await
            .unwrap_err();
        assert!(err.contains("own chat"), "{err}");

        let sent = tools
            .call(
                "send_message",
                json!({ "chat": "alpha", "text": "please review" }),
            )
            .await
            .unwrap();
        assert_eq!(sent["sent"]["delivery"], "run");
        let writes = world.writes.lock().unwrap();
        let (method, params) = writes
            .iter()
            .rev()
            .find(|(m, _)| m == methods::QUEUE_COMMAND)
            .expect("a queued command");
        assert_eq!(method, methods::QUEUE_COMMAND);
        assert_eq!(params["chatId"], "chat-alpha-1");
        assert_eq!(params["command"]["kind"], "run");
        let prompt = params["command"]["request"]["prompt"].as_str().unwrap();
        assert!(
            prompt.starts_with("[Message from Zeron chat Beta (chat-bet)"),
            "{prompt}"
        );
        assert!(prompt.ends_with("please review"));
        assert!(
            prompt.contains(", id chat-beta-2. Your final response goes back to it automatically"),
            "{prompt}"
        );
        // The receiver is told not to answer through the tool.
        assert!(!prompt.contains("Reply to it with"), "{prompt}");
        assert_eq!(params["command"]["request"]["cwd"], "/repo/comet");
        assert_eq!(params["command"]["request"]["harness"], "claude-code");
        assert_eq!(params["command"]["request"]["model"], "opus");
    }

    #[tokio::test]
    async fn replies_use_the_full_id_with_multiple_voice_orchestrators() {
        let first = "voice-orchestrator-11111111-1111-4111-8111-111111111111";
        let second = "voice-orchestrator-22222222-2222-4222-8222-222222222222";
        let world = Arc::new(World {
            extra_chats: [first, second]
                .into_iter()
                .map(|id| {
                    json!({
                        "id": id, "deviceId": "dev-local", "title": "Voice session",
                        "archived": false, "createdAt": "2026-10-01T00:00:00Z"
                    })
                })
                .collect(),
            ..Default::default()
        });
        let orchestrator = tools(
            world.clone(),
            Origin {
                chat_id: Some(second.into()),
                device_id: None,
            },
        );
        let error = orchestrator
            .zeron
            .resolve_chat("voice-or")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("2 chats start with"), "{error}");
        orchestrator
            .call(
                "send_message",
                json!({
                    "chat": "alpha", "text": "please review"
                }),
            )
            .await
            .unwrap();
        let reply_id = {
            let writes = world.writes.lock().unwrap();
            let prompt = writes
                .iter()
                .rev()
                .find(|(m, _)| m == methods::QUEUE_COMMAND)
                .unwrap()
                .1["command"]["request"]["prompt"]
                .as_str()
                .unwrap();
            prompt
                .split_once(", id ")
                .unwrap()
                .1
                .split_once(". Your final response")
                .unwrap()
                .0
                .to_owned()
        };
        assert_eq!(reply_id, second);
        let worker = tools(
            world.clone(),
            Origin {
                chat_id: Some("chat-alpha-1".into()),
                device_id: None,
            },
        );
        worker
            .call(
                "send_message",
                json!({
                    "chat": reply_id, "text": "review complete"
                }),
            )
            .await
            .unwrap();
        let writes = world.writes.lock().unwrap();
        assert_eq!(writes.last().unwrap().1["chatId"], second);
    }

    #[tokio::test]
    async fn auto_steers_busy_chats_even_at_turn_boundaries_or_after_long_quiet_tools() {
        let world = Arc::new(World::default());
        let tools = tools(world.clone(), Origin::default());
        let chats = tools.zeron.chats().await.unwrap();
        let session = Session {
            chat_id: chats[0].id.clone(),
            device_id: "dev-local".into(),
            status: SessionStatus::Working,
            started_at: None,
            updated_at: chrono::Utc::now() - chrono::Duration::minutes(10),
            last_completed_turn: None,
            running_subagents: 0,
            background_tasks: 0,
        };
        // No mid-turn capability is required for a live mailbox delivery.
        let (sent, message_id) = tools
            .deliver(
                &chats[0],
                None,
                &[],
                Some(&session),
                "follow up".into(),
                "auto",
            )
            .await
            .unwrap();
        assert_eq!(sent["delivery"], "steer");
        let writes = world.writes.lock().unwrap();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].0, methods::QUEUE_COMMAND);
        assert_eq!(writes[0].1["command"]["kind"], "steer");
        // The steer carries the id a wait or notice anchors on.
        assert_eq!(writes[0].1["command"]["messageId"], message_id);
    }

    #[tokio::test]
    async fn create_chat_writes_the_row_and_validates_model() {
        let world = Arc::new(World::default());
        let tools = tools(world.clone(), Origin::default());
        let err = tools
            .call(
                "create_chat",
                json!({ "project": "space-1", "model": "nope" }),
            )
            .await
            .unwrap_err();
        assert!(err.contains("not offered"), "{err}");

        let created = tools
            .call(
                "create_chat",
                json!({ "project": "space-1", "model": "sonnet", "title": "Review", "prompt": "go" }),
            )
            .await
            .unwrap();
        let chat_id = created["chatId"].as_str().unwrap().to_owned();
        let writes = world.writes.lock().unwrap();
        assert_eq!(writes.len(), 3, "createChat, renameChat, run");
        assert_eq!(writes[0].1["op"], "createChat");
        assert_eq!(writes[0].1["chatId"], chat_id);
        assert_eq!(writes[0].1["spaceId"], "space-1");
        assert!(
            writes[0].1.get("parentChatId").is_none(),
            "no origin, no parent"
        );
        assert_eq!(writes[0].1["config"]["harness"], "claude-code");
        assert_eq!(writes[0].1["config"]["model"], "sonnet");
        assert_eq!(writes[1].1["op"], "renameChat");
        assert_eq!(writes[2].0, methods::QUEUE_COMMAND);
        assert_eq!(writes[2].1["command"]["request"]["prompt"], "go");
    }

    #[tokio::test]
    async fn create_chat_records_the_origin_as_parent() {
        let world = Arc::new(World::default());
        let tools = tools(
            world.clone(),
            Origin {
                chat_id: Some("chat-beta-2".into()),
                device_id: None,
            },
        );
        let created = tools
            .call("create_chat", json!({ "project": "space-1" }))
            .await
            .unwrap();
        assert_eq!(created["parentChatId"], "chat-beta-2");
        assert_eq!(
            world.writes.lock().unwrap()[0].1["parentChatId"],
            "chat-beta-2"
        );

        // An explicit parent (by title) overrides the origin.
        let created = tools
            .call(
                "create_chat",
                json!({ "project": "space-1", "parent": "Alpha" }),
            )
            .await
            .unwrap();
        assert_eq!(created["parentChatId"], "chat-alpha-1");
        let err = tools
            .call(
                "create_chat",
                json!({ "project": "space-1", "parent": "nope" }),
            )
            .await
            .unwrap_err();
        assert!(err.contains("no chat matches"), "{err}");
    }

    #[tokio::test]
    async fn wait_on_an_idle_chat_returns_immediately() {
        let world = Arc::new(World::default());
        let tools = tools(world, Origin::default());
        let waited = tools
            .call(
                "wait_for_turn",
                json!({ "chat": "alpha", "timeout_secs": 5 }),
            )
            .await
            .unwrap();
        assert_eq!(waited["turn"]["outcome"], "completed");
        assert_eq!(waited["turn"]["replies"][0]["text"], "hello back");
    }

    #[tokio::test]
    async fn send_with_wait_keeps_waiting_until_a_turn_lands() {
        // The stub never grows a session row, so a send-then-wait must run
        // to its deadline rather than declaring the (unstarted) run done.
        let world = Arc::new(World::default());
        let tools = tools(world, Origin::default());
        let started = std::time::Instant::now();
        let sent = tools
            .call(
                "send_message",
                json!({ "chat": "alpha", "text": "hi", "wait": true, "timeout_secs": 1 }),
            )
            .await
            .unwrap();
        assert_eq!(sent["turn"]["outcome"], "timedOut");
        assert!(started.elapsed() >= Duration::from_millis(900));
    }

    #[tokio::test]
    async fn batches_dispatch_all_chats_before_waiting_and_keep_partial_results() {
        for (name, requests) in [
            (
                "create_chats",
                json!([
                    {"prompt": "first", "wait": true, "timeout_secs": 1},
                    {"harness": "not-a-harness"},
                    {"prompt": "second", "wait": true, "timeout_secs": 1},
                ]),
            ),
            (
                "send_messages",
                json!([
                    {"chat": "alpha", "text": "first", "wait": true, "timeout_secs": 1},
                    {"chat": "missing", "text": "invalid"},
                    {"chat": "beta", "text": "second", "wait": true, "timeout_secs": 1},
                ]),
            ),
        ] {
            let world = Arc::new(World {
                dispatch_barrier: Some(tokio::sync::Barrier::new(2)),
                ..Default::default()
            });
            let tools = tools(world.clone(), Origin::default());
            let reply = tokio::time::timeout(
                Duration::from_secs(3),
                crate::jsonrpc::handle_request(
                    &tools,
                    json!(42),
                    "tools/call",
                    json!({"name": name, "arguments": {"requests": requests}}),
                ),
            )
            .await
            .expect("both dispatches must proceed while other requests are waiting");
            assert_eq!(reply["result"]["isError"], false, "{reply}");
            let results = &reply["result"]["structuredContent"]["results"];
            for i in [0, 2] {
                assert_eq!(results[i]["index"], i);
                assert_eq!(results[i]["isError"], false, "{reply}");
                assert_eq!(results[i]["result"]["turn"]["outcome"], "timedOut");
            }
            assert_eq!(results[1]["index"], 1);
            assert_eq!(results[1]["isError"], true);
            let writes = world.writes.lock().unwrap();
            let dispatched: Vec<_> = writes
                .iter()
                .filter(|(method, _)| method == methods::QUEUE_COMMAND)
                .collect();
            assert_eq!(dispatched.len(), 2);
            assert_ne!(dispatched[0].1["chatId"], dispatched[1].1["chatId"]);
        }
    }

    #[tokio::test]
    async fn invalid_batch_sizes_have_no_side_effects() {
        let world = Arc::new(World::default());
        let tools = tools(world.clone(), Origin::default());
        for name in ["create_chats", "send_messages"] {
            for requests in [vec![], vec![json!({"prompt": "hello"}); MAX_BATCH + 1]] {
                assert!(
                    tools
                        .call(name, json!({"requests": requests}))
                        .await
                        .is_err()
                );
            }
        }
        assert!(world.writes.lock().unwrap().is_empty());
    }

    fn subagent(world: Arc<World>) -> Tools {
        tools(
            world,
            Origin {
                chat_id: Some("chat-beta-2".into()),
                device_id: None,
            },
        )
    }

    fn written(world: &World, method: &str) -> Vec<Value> {
        world
            .writes
            .lock()
            .unwrap()
            .iter()
            .filter(|(m, _)| m == method)
            .map(|(_, params)| params.clone())
            .collect()
    }

    #[tokio::test]
    async fn subagents_list_a_small_tool_set() {
        let world = Arc::new(World {
            beta_parent: Some("chat-alpha-1".into()),
            ..Default::default()
        });
        let names: Vec<&str> = subagent(world).list().await.iter().map(|t| t.name).collect();
        assert_eq!(names, ["whoami", "zeron_help", "get_chat", "send_message", "read_chat", "show_visual"]);
    }

    #[tokio::test]
    async fn subagent_messages_go_to_the_parents_inbox_never_a_queue_row() {
        let world = Arc::new(World {
            beta_parent: Some("chat-alpha-1".into()),
            ..Default::default()
        });
        let child = subagent(world.clone());
        // A chat argument is ignored: it always reaches the parent.
        let sent = child
            .call("send_message", json!({ "chat": "Gamma", "text": "found the bug in auth.rs" }))
            .await
            .unwrap();
        assert_eq!(sent["to"], "chat-alpha-1");
        let events = written(&world, methods::DELIVER_EVENT);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["chatId"], "chat-alpha-1");
        assert!(events[0]["text"].as_str().unwrap().ends_with("found the bug in auth.rs"));
        assert!(written(&world, methods::QUEUE_MESSAGE).is_empty());
        assert!(written(&world, methods::QUEUE_COMMAND).is_empty());

        let asked = child
            .call("send_message", json!({ "text": "v1 or v2?", "wait": true }))
            .await
            .unwrap();
        assert_eq!(asked["reply"], "use the v2 API");
    }

    #[tokio::test]
    async fn a_waiting_child_gets_the_orchestrators_text_as_its_reply() {
        let world = Arc::new(World {
            beta_parent: Some("chat-alpha-1".into()),
            child_waiting: true,
            ..Default::default()
        });
        let parent = tools(
            world.clone(),
            Origin {
                chat_id: Some("chat-alpha-1".into()),
                device_id: None,
            },
        );
        let sent = parent
            .call("send_message", json!({ "chat": "Beta", "text": "use v2" }))
            .await
            .unwrap();
        assert_eq!(sent["sent"]["delivery"], "reply");
        assert_eq!(written(&world, methods::DELIVER_AGENT_REPLY).len(), 1);
        assert!(written(&world, methods::QUEUE_COMMAND).is_empty());
    }

    #[tokio::test]
    async fn orchestrators_cannot_touch_another_orchestrators_children() {
        let world = Arc::new(World {
            beta_parent: Some("chat-other".into()),
            ..Default::default()
        });
        let alpha = tools(
            world.clone(),
            Origin {
                chat_id: Some("chat-alpha-1".into()),
                device_id: None,
            },
        );
        for (tool, args) in [
            ("send_message", json!({ "chat": "Beta", "text": "hi" })),
            ("stop_chat", json!({ "chat": "Beta" })),
            ("interrupt_chat", json!({ "chat": "Beta" })),
        ] {
            let error = alpha.call(tool, args).await.unwrap_err();
            assert!(error.contains("another orchestrator"), "{tool}: {error}");
        }
        assert!(world.writes.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn side_chats_cannot_create_chats_or_be_parents() {
        let world = Arc::new(World {
            beta_parent: Some("chat-alpha-1".into()),
            ..Default::default()
        });
        let side = tools(
            world.clone(),
            Origin {
                chat_id: Some("chat-beta-2".into()),
                device_id: None,
            },
        );
        for args in [json!({}), json!({"parent":"Alpha"}), json!({"kind":"chat"})] {
            assert!(
                side.call("create_chat", args)
                    .await
                    .unwrap_err()
                    .contains("not available to this chat")
            );
        }
        assert!(
            side.call(
                "create_chats",
                json!({"requests":[{}, {"parent":"Alpha"}, {"kind":"chat"}]}),
            )
            .await
            .unwrap_err()
            .contains("not available to this chat")
        );
        let root = tools(world.clone(), Origin::default());
        assert!(
            root.call("create_chat", json!({"parent":"Beta"}))
                .await
                .unwrap_err()
                .contains("child of a side chat")
        );
        assert!(world.writes.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn kinds_preserve_defaults_and_reject_invalid_arguments_before_writing() {
        let world = Arc::new(World::default());
        let root = tools(world.clone(), Origin::default());
        for args in [
            json!({"kind":"side"}),
            json!({"kind":"unknown"}),
            json!({"kind":"chat", "parent":"Alpha", "title":"must not write"}),
        ] {
            assert!(root.call("create_chat", args).await.is_err());
        }
        assert!(world.writes.lock().unwrap().is_empty());
        let from_chat = tools(
            world.clone(),
            Origin {
                chat_id: Some("chat-alpha-1".into()),
                device_id: None,
            },
        );
        for (args, kind, parent) in [
            (json!({"kind":"chat"}), "chat", Value::Null),
            (json!({"kind":"chat", "parent":"  "}), "chat", Value::Null),
            (json!({"kind":"side"}), "side", json!("chat-alpha-1")),
            (json!({}), "side", json!("chat-alpha-1")),
            (json!({"parent":"Beta"}), "side", json!("chat-beta-2")),
        ] {
            let created = from_chat.call("create_chat", args).await.unwrap();
            assert_eq!(created["kind"], kind);
            assert_eq!(created["parentChatId"], parent);
            let writes = world.writes.lock().unwrap();
            let row = &writes.last().unwrap().1;
            if kind == "chat" {
                assert!(row.get("parentChatId").is_none());
            } else {
                assert_eq!(row["parentChatId"], parent);
            }
        }
        assert_eq!(
            root.call("create_chat", json!({})).await.unwrap()["kind"],
            "chat"
        );
        assert_eq!(
            root.call("create_chat", json!({"kind":"side", "parent":"Alpha"}))
                .await
                .unwrap()["kind"],
            "side"
        );
    }

    #[tokio::test]
    async fn project_and_device_resolution_scopes_names_and_exact_paths() {
        let world = Arc::new(World::default());
        let tools = tools(world.clone(), Origin::default());
        for args in [
            json!({"project":"comet"}),
            json!({"project":"/repo/comet"}),
            json!({"project":"space-1", "device":"Worker"}),
            json!({"project":"missing", "device":"Worker"}),
            json!({"device":"missing"}),
        ] {
            let error = tools.call("create_chat", args).await.unwrap_err();
            assert!(world.writes.lock().unwrap().is_empty(), "{error}");
        }
        let error = tools
            .call("create_chat", json!({"project":"/repo/comet"}))
            .await
            .unwrap_err();
        for candidate in ["space-1", "space-remote", "dev-local", "dev-remote"] {
            assert!(error.contains(candidate), "{error}");
        }
        for project in ["comet", "/repo/comet", "space-remote"] {
            let created = tools
                .call("create_chat", json!({"project":project,"device":"Worker"}))
                .await
                .unwrap();
            assert_eq!(created["deviceId"], "dev-remote");
            assert_eq!(created["project"]["id"], "space-remote");
            assert_eq!(created["harness"], "codex");
        }
        let created = tools
            .call("create_chat", json!({"project":"unique"}))
            .await
            .unwrap();
        assert_eq!(created["deviceId"], "dev-remote");
        let created = tools
            .call("create_chat", json!({"device":"dev-remote"}))
            .await
            .unwrap();
        assert!(created["project"].is_null());
        assert_eq!(created["deviceId"], "dev-remote");
        let created = tools.call("create_chat", json!({})).await.unwrap();
        assert_eq!(created["deviceId"], "dev-local");
    }

    #[tokio::test]
    async fn discovery_and_creation_use_the_selected_hosts_catalogs() {
        let world = Arc::new(World::default());
        let tools = tools(world.clone(), Origin::default());
        assert_eq!(
            tools.call("list_projects", json!({})).await.unwrap()["projects"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
        let projects = tools
            .call("list_projects", json!({"device":"Worker"}))
            .await
            .unwrap();
        assert_eq!(projects["projects"].as_array().unwrap().len(), 2);
        assert!(
            projects["projects"]
                .as_array()
                .unwrap()
                .iter()
                .all(|p| p["deviceId"] == "dev-remote")
        );
        assert_eq!(
            tools.call("list_harnesses", json!({})).await.unwrap()["harnesses"][0]["id"],
            "claude-code"
        );
        assert_eq!(
            tools
                .call("list_harnesses", json!({"device":"Worker"}))
                .await
                .unwrap()["harnesses"][0]["id"],
            "codex"
        );
        assert_eq!(
            tools
                .call("list_models", json!({"harness":"codex", "device":"Worker"}))
                .await
                .unwrap()["models"][0]["id"],
            "remote-model"
        );
        assert_eq!(
            tools
                .call("list_models", json!({"harness":"claude-code"}))
                .await
                .unwrap()["models"][0]["id"],
            "opus"
        );
        for args in [
            json!({"device":"Worker", "harness":"claude-code"}),
            json!({"device":"Worker", "harness":"cursor"}),
            json!({"device":"Laptop", "harness":"codex"}),
            json!({"device":"Laptop", "harness":"mock"}),
            json!({"device":"Worker", "harness":"codex", "model":"opus"}),
        ] {
            assert!(tools.call("create_chat", args).await.is_err());
            assert!(world.writes.lock().unwrap().is_empty());
        }
        let created = tools
            .call(
                "create_chat",
                json!({"device":"Worker", "harness":"codex", "model":"remote-model"}),
            )
            .await
            .unwrap();
        assert_eq!(created["harness"], "codex");
        assert_eq!(created["model"], "remote-model");
        for method in [methods::LIST_HARNESSES, methods::LIST_MODELS] {
            let broken = Arc::new(World {
                catalog_error: Some(method),
                ..Default::default()
            });
            let tools = self::tools(broken.clone(), Origin::default());
            let error = tools
                .call(
                    "create_chat",
                    json!({"device":"Worker", "model":"remote-model"}),
                )
                .await
                .unwrap_err();
            assert!(
                error.contains("dev-remote") && error.contains("catalog unavailable"),
                "{error}"
            );
            assert!(broken.writes.lock().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn remote_send_selects_the_remote_default_harness() {
        let world = Arc::new(World {
            beta_remote: true,
            ..Default::default()
        });
        let tools = tools(world.clone(), Origin::default());
        tools
            .call("send_message", json!({"chat":"Beta", "text":"go"}))
            .await
            .unwrap();
        assert_eq!(
            world.reads.lock().unwrap().last().unwrap().1["targetDeviceId"],
            "dev-remote"
        );
        assert_eq!(
            world.writes.lock().unwrap().last().unwrap().1["command"]["request"]["harness"],
            "codex"
        );
    }

    #[tokio::test]
    async fn mixed_batch_preserves_order_and_errors_per_request() {
        let world = Arc::new(World::default());
        let tools = tools(
            world.clone(),
            Origin {
                chat_id: Some("chat-alpha-1".into()),
                device_id: None,
            },
        );
        let result = tools
            .call(
                "create_chats",
                json!({"requests":[
                    {"kind":"chat", "device":"Worker", "project":"comet"},
                    {"kind":"chat", "parent":"Alpha"},
                    {"kind":"side", "project":"space-1"},
                    {"kind":"bogus"}
                ]}),
            )
            .await
            .unwrap();
        let results = &result["results"];
        assert_eq!(results[0]["result"]["kind"], "chat");
        assert_eq!(results[0]["result"]["deviceId"], "dev-remote");
        assert_eq!(results[1]["isError"], true);
        assert_eq!(results[2]["result"]["kind"], "side");
        assert_eq!(results[2]["result"]["parentChatId"], "chat-alpha-1");
        assert_eq!(results[3]["isError"], true);
        for i in 0..4 {
            assert_eq!(results[i]["index"], i);
        }
        assert_eq!(world.writes.lock().unwrap().len(), 2);
    }

    /// Sessions and transcripts are independently delivered. The command reply
    /// is deliberately slow, so the assistant starts before QueueCommand returns.
    struct DelayedTurn {
        world: World,
        sent: Mutex<Option<(i64, Instant, String)>>,
        /// The sent user message's id; it reaches the transcript at once.
        message: Mutex<Option<String>>,
        reply_delay: Duration,
        never_reply: bool,
        /// Host clock offset: a remote engine stamps createdAt on its own clock.
        host_skew_millis: i64,
    }

    #[async_trait]
    impl RpcService for DelayedTurn {
        async fn handle(&self, method: &str, params: Value) -> Result<RpcReply, RpcError> {
            match method {
                methods::WATCH_CHATS => {
                    let reply = self.world.handle(method, params).await?;
                    let RpcReply::Stream(mut rows) = reply else {
                        unreachable!()
                    };
                    let mut rows = rows.next().await.unwrap();
                    if let Some((_, _, chat)) = self.sent.lock().unwrap().as_ref()
                        && chat != "chat-beta-2"
                    {
                        rows.as_array_mut().unwrap().push(json!({
                            "id": chat, "deviceId":"dev-remote", "archived":false,
                            "createdAt":chrono::Utc::now(), "config":{"harness":"codex", "sandbox":"workspace-write"}
                        }));
                    }
                    Ok(stream(rows))
                }
                methods::QUEUE_COMMAND => {
                    let reply = self.world.handle(method, params.clone()).await?;
                    *self.message.lock().unwrap() =
                        params["command"]["messageId"].as_str().map(str::to_owned);
                    *self.sent.lock().unwrap() = Some((
                        now_millis(),
                        Instant::now(),
                        params["chatId"].as_str().unwrap().into(),
                    ));
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    Ok(reply)
                }
                methods::WATCH_SESSIONS => {
                    let sent = self.sent.lock().unwrap().clone();
                    let Some((_, _, chat)) = sent else {
                        return Ok(stream(json!([])));
                    };
                    Ok(RpcReply::Stream(
                        futures::stream::once(async { json!([]) })
                            .chain(futures::stream::once(async move {
                                tokio::time::sleep(Duration::from_millis(50)).await;
                                json!([{"chatId":chat, "deviceId":"dev-remote", "status":"idle",
                                "updatedAt":chrono::Utc::now(), "lastCompletedTurn":"new-turn"}])
                            }))
                            .boxed(),
                    ))
                }
                methods::WATCH_DOC_MESSAGES => {
                    let sent = self.sent.lock().unwrap().clone();
                    // Only the existing chat has history. Its previous reply
                    // shares the send's timestamp, so only its id tells them apart.
                    let mut messages = json!([]);
                    if params["chatId"] == "chat-beta-2" {
                        let created_at = sent.as_ref().map_or_else(now_millis, |(t, _, _)| *t);
                        messages.as_array_mut().unwrap().push(json!({
                            "id":"previous-reply", "role":"assistant", "createdAt":created_at,
                            "deviceId":"dev-remote", "status":"complete", "parts":[{"kind":"text", "id":"t", "text":"old reply"}]
                        }));
                    }
                    if let Some((timestamp, _, chat)) = sent.as_ref()
                        && params["chatId"] == chat.as_str()
                        && let Some(id) = self.message.lock().unwrap().clone()
                    {
                        messages.as_array_mut().unwrap().push(json!({
                            "id":id, "role":"user", "createdAt":timestamp + self.host_skew_millis,
                            "deviceId":"dev-local", "parts":[{"kind":"text", "id":"t", "text":"go"}]
                        }));
                    }
                    if let Some((timestamp, started, _)) = sent
                        && started.elapsed() >= self.reply_delay
                        && !self.never_reply
                    {
                        messages.as_array_mut().unwrap().push(json!({
                            "id":"new-reply", "role":"assistant", "createdAt":timestamp + self.host_skew_millis,
                            "deviceId":"dev-remote", "status":"complete", "parts":[{"kind":"text", "id":"t", "text":"new reply"}]
                        }));
                    }
                    Ok(stream(json!({"reset":messages})))
                }
                _ => self.world.handle(method, params).await,
            }
        }
    }

    #[tokio::test]
    async fn wait_allows_a_missing_session_and_a_late_remote_transcript() {
        for create in [true, false] {
            let service = Arc::new(DelayedTurn {
                world: World {
                    beta_remote: true,
                    ..Default::default()
                },
                sent: Mutex::new(None),
                message: Mutex::new(None),
                reply_delay: Duration::from_millis(450),
                never_reply: false,
                host_skew_millis: 0,
            });
            let tools = Tools::new(Arc::new(Zeron::with_client(
                memory_client(service),
                Origin::default(),
            )));
            let (name, args) = if create {
                (
                    "create_chat",
                    json!({"kind":"chat", "device":"Worker", "prompt":"go", "wait":true, "timeout_secs":2}),
                )
            } else {
                (
                    "send_message",
                    json!({"chat":"Beta", "text":"go", "wait":true, "timeout_secs":2}),
                )
            };
            let result = tools.call(name, args).await.unwrap();
            assert_eq!(result["turn"]["outcome"], "completed", "{result}");
            assert_eq!(
                result["turn"]["replies"].as_array().unwrap().len(),
                1,
                "{result}"
            );
            assert_eq!(result["turn"]["replies"][0]["text"], "new reply");
        }
    }

    #[tokio::test]
    async fn standalone_wait_after_nonblocking_send_keeps_the_send_baseline() {
        for create in [true, false] {
            let service = Arc::new(DelayedTurn {
                world: World {
                    beta_remote: true,
                    ..Default::default()
                },
                sent: Mutex::new(None),
                message: Mutex::new(None),
                reply_delay: Duration::from_millis(450),
                never_reply: false,
                host_skew_millis: 0,
            });
            let tools = Tools::new(Arc::new(Zeron::with_client(
                memory_client(service),
                Origin::default(),
            )));
            let sent = if create {
                tools
                    .call(
                        "create_chat",
                        json!({"kind":"chat", "device":"Worker", "prompt":"go", "wait":false}),
                    )
                    .await
                    .unwrap()
            } else {
                tools
                    .call(
                        "send_message",
                        json!({"chat":"Beta", "text":"go", "wait":false}),
                    )
                    .await
                    .unwrap()
            };
            let result = tools
                .call(
                    "wait_for_turn",
                    json!({"chat":sent["chatId"], "timeout_secs":2}),
                )
                .await
                .unwrap();
            assert_eq!(result["turn"]["outcome"], "completed", "{result}");
            assert_eq!(result["turn"]["replies"].as_array().unwrap().len(), 1);
            assert_eq!(result["turn"]["replies"][0]["text"], "new reply");
        }
    }

    /// A remote host stamps replies on its own clock. One running behind the
    /// caller must not hide the reply to this send.
    #[tokio::test]
    async fn wait_returns_the_reply_from_a_host_whose_clock_is_behind() {
        for create in [true, false] {
            let service = Arc::new(DelayedTurn {
                world: World {
                    beta_remote: true,
                    ..Default::default()
                },
                sent: Mutex::new(None),
                message: Mutex::new(None),
                reply_delay: Duration::from_millis(100),
                never_reply: false,
                host_skew_millis: -30_000,
            });
            let tools = Tools::new(Arc::new(Zeron::with_client(
                memory_client(service),
                Origin::default(),
            )));
            let (name, args) = if create {
                (
                    "create_chat",
                    json!({"kind":"chat", "device":"Worker", "prompt":"go", "wait":true, "timeout_secs":2}),
                )
            } else {
                (
                    "send_message",
                    json!({"chat":"Beta", "text":"go", "wait":true, "timeout_secs":2}),
                )
            };
            let result = tools.call(name, args).await.unwrap();
            assert_eq!(result["turn"]["outcome"], "completed", "{result}");
            let replies = result["turn"]["replies"].as_array().unwrap();
            assert!(replies.iter().any(|r| r["text"] == "new reply"), "{result}");
        }
    }

    #[tokio::test]
    async fn completed_session_without_new_transcript_times_out_without_old_reply() {
        let service = Arc::new(DelayedTurn {
            world: World {
                beta_remote: true,
                ..Default::default()
            },
            sent: Mutex::new(None),
            message: Mutex::new(None),
            reply_delay: Duration::ZERO,
            never_reply: true,
            host_skew_millis: 0,
        });
        let tools = Tools::new(Arc::new(Zeron::with_client(
            memory_client(service),
            Origin::default(),
        )));
        let result = tools
            .call(
                "send_message",
                json!({"chat":"Beta", "text":"go", "wait":true, "timeout_secs":1}),
            )
            .await
            .unwrap();
        assert_eq!(result["turn"]["outcome"], "timedOut");
        assert_eq!(result["turn"]["replies"], json!([]));
    }

    #[tokio::test]
    async fn remote_replies_do_not_require_synchronized_clocks() {
        for host_skew_millis in [-1_000, 1_000] {
            for wait in [true, false] {
                let service = Arc::new(DelayedTurn {
                    world: World {
                        beta_remote: true,
                        ..Default::default()
                    },
                    sent: Mutex::new(None),
                    message: Mutex::new(None),
                    reply_delay: Duration::from_millis(450),
                    never_reply: false,
                    host_skew_millis,
                });
                let tools = Tools::new(Arc::new(Zeron::with_client(
                    memory_client(service),
                    Origin::default(),
                )));
                let sent = tools
                    .call(
                        "send_message",
                        json!({
                            "chat":"Beta", "text":"go", "wait":wait, "timeout_secs":2
                        }),
                    )
                    .await
                    .unwrap();
                let result = if wait {
                    sent
                } else {
                    tools
                        .call(
                            "wait_for_turn",
                            json!({
                                "chat":sent["chatId"], "timeout_secs":2
                            }),
                        )
                        .await
                        .unwrap()
                };
                assert_eq!(
                    result["turn"]["outcome"], "completed",
                    "offset={host_skew_millis}, wait={wait}: {result}"
                );
                assert_eq!(
                    result["turn"]["replies"].as_array().unwrap().len(),
                    1,
                    "{result}"
                );
                assert_eq!(result["turn"]["replies"][0]["id"], "new-reply");
            }
        }
    }

    #[tokio::test]
    async fn initialize_and_list_over_jsonrpc() {
        let world = Arc::new(World::default());
        let tools = tools(world, Origin::default());
        let init = crate::jsonrpc::handle_request(
            &tools,
            json!(1),
            "initialize",
            json!({ "protocolVersion": "2025-03-26" }),
        )
        .await;
        assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(init["result"]["serverInfo"]["name"], "zeron");
        let list =
            crate::jsonrpc::handle_request(&tools, json!(2), "tools/list", Value::Null).await;
        assert!(list["result"]["tools"].as_array().unwrap().len() >= 10);
        let bad = crate::jsonrpc::handle_request(
            &tools,
            json!(3),
            "tools/call",
            json!({ "name": "nope" }),
        )
        .await;
        assert_eq!(bad["error"]["code"], -32602);
        let whoami = crate::jsonrpc::handle_request(
            &tools,
            json!(4),
            "tools/call",
            json!({ "name": "whoami" }),
        )
        .await;
        assert_eq!(whoami["result"]["isError"], false);
        assert_eq!(
            whoami["result"]["structuredContent"]["localDeviceId"],
            "dev-local"
        );
    }

    #[tokio::test]
    async fn sends_register_a_turn_notice_unless_told_not_to() {
        let world = Arc::new(World::default());
        let tools = tools(
            world.clone(),
            Origin {
                chat_id: Some("chat-beta-2".into()),
                device_id: None,
            },
        );
        let sent = tools
            .call(
                "send_message",
                json!({ "chat": "alpha", "text": "review this" }),
            )
            .await
            .unwrap();
        assert!(
            sent["notice"]
                .as_str()
                .unwrap()
                .contains("automatic Zeron notice"),
            "{sent}"
        );
        {
            let writes = world.writes.lock().unwrap();
            let command = &writes
                .iter()
                .find(|(m, _)| m == methods::QUEUE_COMMAND)
                .unwrap()
                .1;
            let (method, params) = writes.last().unwrap();
            assert_eq!(method, methods::NOTIFY_TURN_END);
            assert_eq!(params["watcherChatId"], "chat-beta-2");
            assert_eq!(params["chatId"], "chat-alpha-1");
            // Anchored on the exact message that was sent.
            assert_eq!(params["messageId"], command["command"]["messageId"]);
        }

        world.writes.lock().unwrap().clear();
        let sent = tools
            .call(
                "send_message",
                json!({ "chat": "alpha", "text": "fyi: merged", "notify": false }),
            )
            .await
            .unwrap();
        let writes = world.writes.lock().unwrap();
        assert!(writes.iter().all(|(m, _)| m != methods::NOTIFY_TURN_END));
        let command = &writes
            .iter()
            .find(|(m, _)| m == methods::QUEUE_COMMAND)
            .unwrap()
            .1;
        let prompt = command["command"]["request"]["prompt"]
            .as_str()
            .unwrap();
        assert!(prompt.contains("It doesn't need a reply."), "{prompt}");
        assert_eq!(sent["notice"], "No automatic notice (notify=false).");
    }

    #[tokio::test]
    async fn a_wait_that_returns_the_reply_clears_the_notice() {
        let service = Arc::new(DelayedTurn {
            world: World {
                beta_remote: true,
                ..Default::default()
            },
            sent: Mutex::new(None),
            message: Mutex::new(None),
            reply_delay: Duration::from_millis(100),
            never_reply: false,
            host_skew_millis: 0,
        });
        let tools = Tools::new(Arc::new(Zeron::with_client(
            memory_client(service.clone()),
            Origin {
                chat_id: Some("chat-alpha-1".into()),
                device_id: None,
            },
        )));
        // A blocking send gets the reply in hand: no notice is registered.
        let result = tools
            .call(
                "send_message",
                json!({ "chat": "Beta", "text": "go", "wait": true, "timeout_secs": 3 }),
            )
            .await
            .unwrap();
        assert_eq!(result["turn"]["outcome"], "completed", "{result}");
        assert!(result.get("notice").is_none_or(Value::is_null), "{result}");
        assert!(
            service
                .world
                .writes
                .lock()
                .unwrap()
                .iter()
                .all(|(m, _)| m != methods::NOTIFY_TURN_END)
        );

        // A later wait_for_turn that collects the reply cancels the notice.
        tools
            .call("send_message", json!({ "chat": "Beta", "text": "again" }))
            .await
            .unwrap();
        let waited = tools
            .call(
                "wait_for_turn",
                json!({ "chat": "Beta", "timeout_secs": 3 }),
            )
            .await
            .unwrap();
        assert_eq!(waited["turn"]["outcome"], "completed", "{waited}");
        let writes = service.world.writes.lock().unwrap();
        let methods_seen: Vec<&str> = writes.iter().map(|(m, _)| m.as_str()).collect();
        let notify = methods_seen
            .iter()
            .position(|m| *m == methods::NOTIFY_TURN_END)
            .expect("the non-blocking send registered a notice");
        assert!(
            methods_seen[notify..].contains(&methods::CANCEL_TURN_NOTICE),
            "{methods_seen:?}"
        );
    }
}
