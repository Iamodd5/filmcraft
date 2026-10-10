//! The MCP layer: lifecycle, tool calls (arguments checked against each tool's own schema),
//! resources and prompts, over [`crate::rpc`].

use std::panic::{AssertUnwindSafe, catch_unwind};

use serde_json::{Value, json};

use crate::b64;
use crate::rpc::{self, Incoming};
use crate::schema;
use crate::tools::{self, Output, Server};

/// Revisions this server speaks, newest first.
const VERSIONS: &[&str] = &["2026-07-28", "2025-06-18", "2025-03-26", "2024-11-05"];
/// The first revision where every request carries its own protocol version and list and read
/// results need `resultType`, `ttlMs` and `cacheScope`.
const STATELESS_FROM: &str = "2026-07-28";
const VERSION_META: &str = "io.modelcontextprotocol/protocolVersion";

pub const PROTOCOL: &str = "\
FilmCraft agent editing protocol. The goal is a timeline a person only has to look at, correct every time.

1. Orient. `project` (action info, or demo/open/new), then `review` for the timeline as it is and `inspect` for clip ids.
2. Find the command. `commands` with a query (words from the menu item or button), `command_describe` for its exact parameters. Every menu item and panel control is a command.
3. Mark a way back. `checkpoint` create before any bigger piece of work.
4. Edit with `edit`, never by guessing. Put every step of one intention in ONE `edit`, state what must be true afterwards in `expect` (durationFrames, clipCount, clipsPresent, unchangedClips, onlyChanging, noGaps...). If a step fails, the edit breaks the timeline or an expectation is false, everything is undone and the timeline is verified to be what it was. Use `dry_run` first for anything you are not sure about.
5. Verify. `review` must say `clean` (or only warnings you can explain), and `frames` shows the pictures: look at them.
6. Finish. `project` save, `export` if asked. Tell the person what changed, using the `changes` the edit returned.

Time is in ticks (254016000000 per second); commands also accept `seconds`, `frame` or `timecode`. Clip and track ids come from `inspect`. Never invent an id.
With `--live`, `ui` presses any control of the running app: `ui` elements lists the clickable ids. Anything a button does is also a command, prefer `edit`.";

pub struct Mcp {
    tools: Server,
}

impl Mcp {
    pub fn new(tools: Server) -> Self {
        Self { tools }
    }

    /// The reply to one input line, or `None` for a notification.
    pub fn handle(&mut self, input: Incoming) -> Option<Value> {
        match input {
            Incoming::Notification { .. } => None,
            Incoming::Invalid { id, code, message } => Some(rpc::error(&id, code, message)),
            Incoming::Request { id, method, params } => {
                let modern = params.get("_meta").and_then(|m| m.get(VERSION_META)).and_then(Value::as_str).is_some_and(|v| v >= STATELESS_FROM);
                Some(match self.dispatch(&method, &params) {
                    Ok(result) => rpc::ok(&id, decorate(&method, modern, result)),
                    Err((code, message)) => rpc::error(&id, code, message),
                })
            }
        }
    }

    fn dispatch(&mut self, method: &str, params: &Value) -> Result<Value, (i64, String)> {
        match method {
            "initialize" => {
                let asked = params.get("protocolVersion").and_then(Value::as_str).unwrap_or("");
                let version = VERSIONS.iter().find(|v| **v == asked).or(VERSIONS.get(1)).copied().unwrap_or("2025-06-18");
                Ok(json!({"protocolVersion": version, "capabilities": capabilities(), "serverInfo": server_info(), "instructions": PROTOCOL}))
            }
            "server/discover" => {
                Ok(json!({"supportedVersions": VERSIONS, "capabilities": capabilities(), "serverInfo": server_info(), "instructions": PROTOCOL}))
            }
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": tools::definitions()})),
            "tools/call" => self.call_tool(params),
            "resources/list" => Ok(json!({"resources": [
                {"uri": "filmcraft://timeline", "name": "timeline", "title": "Timeline review", "description": "The active sequence as the `review` tool reports it", "mimeType": "application/json"},
                {"uri": "filmcraft://commands", "name": "commands", "title": "Command catalog", "description": "Every command with parameters", "mimeType": "application/json"},
                {"uri": "filmcraft://protocol", "name": "protocol", "title": "Editing protocol", "description": "How to edit with this server", "mimeType": "text/markdown"}]})),
            "resources/read" => self.read_resource(params),
            "prompts/list" => Ok(
                json!({"prompts": [{"name": "edit_protocol", "title": "Editing protocol", "description": "Orient, find the command, edit all-or-nothing, verify, finish"}]}),
            ),
            "prompts/get" => match params.get("name").and_then(Value::as_str) {
                Some("edit_protocol") => {
                    Ok(json!({"description": "FilmCraft editing protocol", "messages": [{"role": "user", "content": {"type": "text", "text": PROTOCOL}}]}))
                }
                _ => Err((rpc::INVALID_PARAMS, "unknown prompt".into())),
            },
            other => Err((rpc::METHOD_NOT_FOUND, format!("method not found: {other}"))),
        }
    }

    fn call_tool(&mut self, params: &Value) -> Result<Value, (i64, String)> {
        let name = params.get("name").and_then(Value::as_str).ok_or((rpc::INVALID_PARAMS, "tools/call needs a string `name`".to_string()))?;
        let defs = tools::definitions();
        let def = defs.iter().find(|d| d["name"] == name).ok_or_else(|| (rpc::INVALID_PARAMS, format!("unknown tool `{name}`")))?;
        let args = match params.get("arguments") {
            None | Some(Value::Null) => json!({}),
            Some(a) if a.is_object() => a.clone(),
            Some(_) => return Err((rpc::INVALID_PARAMS, "`arguments` must be an object".into())),
        };
        schema::validate(&args, &def["inputSchema"], name).map_err(|e| (rpc::INVALID_PARAMS, e))?;
        // Last-resort guard: a bug that panics inside a command (the engine already catches those)
        // or in this server becomes an error for the caller, and the session carries on.
        let out = catch_unwind(AssertUnwindSafe(|| self.tools.call(name, &args)))
            .unwrap_or_else(|_| Output::err("internal error: the tool did not complete (see the server's stderr); the session is still usable"));
        Ok(render(out))
    }

    fn read_resource(&mut self, params: &Value) -> Result<Value, (i64, String)> {
        let uri = params.get("uri").and_then(Value::as_str).ok_or((rpc::INVALID_PARAMS, "resources/read needs a `uri`".to_string()))?;
        let (mime, text) = match uri {
            "filmcraft://protocol" => ("text/markdown", PROTOCOL.to_string()),
            "filmcraft://timeline" => {
                let o = self.tools.call("review", &json!({}));
                if o.is_error {
                    return Err((rpc::INTERNAL_ERROR, o.data["error"].as_str().unwrap_or("review failed").to_string()));
                }
                ("application/json", o.data.to_string())
            }
            "filmcraft://commands" => {
                let o = self.tools.call("commands", &json!({"query": "", "limit": 1000, "enabled_only": false, "menu": ""}));
                // an empty query lists the groups, so ask for everything with a one-letter match instead
                let o = if o.data.get("commands").is_some() { o } else { self.tools.call("commands", &json!({"query": "a", "limit": 1000})) };
                ("application/json", o.data.to_string())
            }
            other => return Err((-32002, format!("resource not found: {other}"))),
        };
        Ok(json!({"contents": [{"uri": uri, "mimeType": mime, "text": text}]}))
    }
}

fn capabilities() -> Value {
    json!({"tools": {"listChanged": false}, "resources": {"listChanged": false, "subscribe": false}, "prompts": {"listChanged": false}})
}

fn server_info() -> Value {
    json!({"name": "filmcraft-agent", "title": "FilmCraft agent server", "version": env!("CARGO_PKG_VERSION")})
}

/// Per-request additions the newest revision requires.
fn decorate(method: &str, modern: bool, mut result: Value) -> Value {
    if !modern {
        return result;
    }
    if let Some(o) = result.as_object_mut() {
        o.insert("resultType".into(), json!("complete"));
        if matches!(method, "tools/list" | "resources/list" | "prompts/list" | "resources/read" | "prompts/get") {
            let ttl = if method.ends_with("/list") { 60_000 } else { 0 };
            o.insert("ttlMs".into(), json!(ttl));
            o.insert("cacheScope".into(), json!("private"));
        }
    }
    result
}

fn render(out: Output) -> Value {
    let mut content: Vec<Value> = out.texts.iter().map(|t| json!({"type": "text", "text": t})).collect();
    content.push(json!({"type": "text", "text": out.data.to_string()}));
    for png in &out.images {
        content.push(json!({"type": "image", "data": b64::encode(png), "mimeType": "image/png"}));
    }
    let mut r = json!({"content": content, "isError": out.is_error});
    if out.data.is_object()
        && let Some(o) = r.as_object_mut()
    {
        o.insert("structuredContent".into(), out.data);
    }
    r
}
