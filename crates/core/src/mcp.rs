//! BIMCraft's MCP server: JSON-RPC 2.0 over stdio, one message per line (MCP's stdio transport).
//! Its tools run the same commands as the CLI and scripts: on a headless session (`bimcraft-cli mcp`), or on the
//! desktop's own, live, through [`reply`] (`bimcraft --live` listens on [`LIVE_ADDR`], `bimcraft-cli mcp --live` relays to it).

use std::io::{BufRead, Write};

use std::net::SocketAddr;

use crate::engine::{Session, catalog};
use serde_json::{Value, json};

pub const PROTOCOL_VERSION: &str = "2025-06-18";
const SUPPORTED: &[&str] = &[PROTOCOL_VERSION, "2025-03-26", "2024-11-05"];

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

const INSTRUCTIONS: &str = "BIMCraft is a BIM modeler, a Revit-class building model. Lengths are millimetres; x east, y north, z up; \
the model starts with Level 1 (0) and Level 2 (3000). Model with execute: wall {points: [[0,0],[8000,0],[8000,6000],[0,6000]], \
closed: true}, door {at: [2000, 0]}, window {at: [8000, 3000]}, room {point: [4000, 3000], name: \"Living\"}, floor {} (open \
over a loop of walls standing free inside), \
roof {} (on Level 2, over the walls below, hipped, open over a loop of them standing free inside; gables: [1, 3] stands \
those edges upright; courtyards: [[[x,y],…]] slopes it round holes in it too), dormer {host: <roof id>, \
at: [4000, 800]} (on its slope), column {at: [4000, 3000]}, stair {start: [1000, 1000], end: [1000, 2000], \
railings: true} (with landings: [{at: 8, turn: -90}], it turns right after 8 risers), ramp {start: [-5000, 0], height: 600}, \
dimension {refs: [[0, 3000], [8000, 3000]], at: [4000, -1000]} (between the walls under those points), tag {} (every door and \
window); or send several at once with script, one `command {json}` per line. list_commands shows every command and its \
parameters. Check your work with inspect_model and query_elements, look at it with render (a PNG of a level's plan, with \
ceiling: true its reflected ceiling plan, of a section or elevation by name, or of the 3D view with view \"{3D}\"), and save \
to .bimcraft or .ifc.";

/// Where the desktop's live link listens unless told otherwise: this machine only.
pub const LIVE_ADDR: &str = "127.0.0.1:47833";

/// A live link's address: `host:port` on this machine (a loopback address), never one another machine could reach.
pub fn live_addr(text: &str) -> Result<SocketAddr, String> {
    let addr: SocketAddr = text.trim().parse().map_err(|e| format!("`{text}` is not an address such as {LIVE_ADDR}: {e}"))?;
    if addr.ip().is_loopback() {
        Ok(addr)
    } else {
        Err(format!("the live link listens on this machine only (127.0.0.1 or [::1]), not on {}", addr.ip()))
    }
}

pub struct Server {
    pub session: Session,
}

impl Default for Server {
    fn default() -> Self {
        Server::new()
    }
}

fn error(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message.into()}})
}

fn text(v: &Value) -> Value {
    json!({"content": [{"type": "text", "text": serde_json::to_string_pretty(v).unwrap_or_default()}]})
}

fn tool_error(e: &str) -> Value {
    json!({"content": [{"type": "text", "text": e}], "isError": true})
}

fn tools() -> Value {
    json!([
        {"name": "execute", "description": "Run any BIMCraft command with JSON params, e.g. wall {points: [[0,0],[5000,0]]}, door {at: [2500, 0]}, \
            room {point: [x, y], name}, modify {id, …fields}, delete {ids}, undo. list_commands shows them all with their parameters.",
         "inputSchema": {"type": "object", "properties": {"command": {"type": "string"}, "params": {"type": "object"}}, "required": ["command"]}},
        {"name": "script", "description": "Run several commands, one per line: `wall {\"points\": [[0,0],[4000,0]]}`. Stops at the first failing line and names it; the lines before it stay done.",
         "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}},
        {"name": "list_commands", "description": "Every command with its id, label, Revit-style shortcut and parameters. `filter` matches part of an id, label or shortcut.",
         "inputSchema": {"type": "object", "properties": {"filter": {"type": "string"}}}},
        {"name": "inspect_model", "description": "The model at a glance: levels, element counts, types, warnings (such as rooms no longer enclosed) and undo history.",
         "inputSchema": {"type": "object", "properties": {}}},
        {"name": "query_elements", "description": "Elements with ids, fields and derived values (wall length and height, room area_m2 and enclosed, door position `at`). Filter by category and level; page with limit and offset.",
         "inputSchema": {"type": "object", "properties": {"category": {"type": "string"}, "level": {"type": ["string", "integer"]}, "limit": {"type": "integer"}, "offset": {"type": "integer"}}}},
        {"name": "schedule", "description": "A schedule's columns, rows and totals, and the fields it could show (`available`). `category` alone reads that category's own: rooms (number, name, level, area, perimeter), doors, windows, walls, floors, levels; ceilings, roofs, columns, components, stairs, ramps and railings too. `name` and `category` make a new one. `fields` (names, or {name, formula, kind, heading, total}: a calculated one such as Area * 120, lengths in mm and areas in m²), `filters` ([{field, op: equals, greater, contains, has-value…, value}]), `sort` ([field or {field, descending}]) and `grouped` change it, each list replacing the last.",
         "inputSchema": {"type": "object", "properties": {"name": {"type": "string"}, "category": {"type": "string"}, "fields": {"type": "array"}, "filters": {"type": "array"}, "sort": {"type": "array"}, "grouped": {"type": "boolean"}}}},
        {"name": "render", "description": "Look at the model as a PNG: a level's floor plan (walls and columns with poché, doors with swings, windows, stairs, ramps and railings, roofs, room tags with areas, grids, dimensions, door and window tags), or with ceiling true its reflected ceiling plan, a section or elevation by name (the cut in poché, what's beyond, levels and grids), a sheet by number (its title block and the views on it), or with view \"{3D}\" the 3D view, seen from azimuth degrees round from east (counter-clockwise) and elevation degrees up; by default from the south-east, 30° up.",
         "inputSchema": {"type": "object", "properties": {"view": {"type": ["string", "integer"]}, "ceiling": {"type": "boolean"}, "width": {"type": "integer"}, "height": {"type": "integer"}, "azimuth": {"type": "number"}, "elevation": {"type": "number"}}}},
        {"name": "new_model", "description": "Start an empty project with Level 1 and Level 2.",
         "inputSchema": {"type": "object", "properties": {"name": {"type": "string"}}}},
        {"name": "open", "description": "Open a .bimcraft project, or import an .ifc file (IFC2x3 or IFC4, from Revit or any BIM tool): its storeys, straight and curved walls with their doors and windows, floors (level or sloped) and ceilings, with their holes, rooms, columns, grids, stairs and ramps of straight runs with landings, railings, shafts and roofs (flat, sloping all round over any footprint with gable ends, courtyards and openings, or one profile swept across) come in measured from their geometry; the reply lists what was left out and why.",
         "inputSchema": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}},
        {"name": "save", "description": "Save to .bimcraft (the project), or export by extension: .ifc (IFC4 for other BIM tools), .svg or .png (the lowest level's plan), .pdf (every sheet, a page each).",
         "inputSchema": {"type": "object", "properties": {"path": {"type": "string"}}}}
    ])
}

impl Server {
    pub fn new() -> Self {
        Server { session: Session::new() }
    }

    /// Serve until `input` closes. Logs go to stderr: stdout carries only protocol messages.
    pub fn serve(&mut self, input: impl BufRead, mut output: impl Write) -> std::io::Result<()> {
        for line in input.lines() {
            if let Some(reply) = self.handle_line(&line?) {
                writeln!(output, "{reply}")?;
                output.flush()?;
            }
        }
        Ok(())
    }

    pub fn handle_line(&mut self, line: &str) -> Option<String> {
        reply(&mut self.session, line)
    }
}

/// The reply to one line of the protocol, run on `s`: none for a notification or a blank line.
pub fn reply(s: &mut Session, line: &str) -> Option<String> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let reply = match serde_json::from_str::<Value>(line) {
        Ok(Value::Array(batch)) if batch.is_empty() => Some(error(Value::Null, INVALID_REQUEST, "empty batch")),
        Ok(Value::Array(batch)) => {
            let replies: Vec<Value> = batch.into_iter().filter_map(|m| handle(s, m)).collect();
            (!replies.is_empty()).then_some(Value::Array(replies))
        }
        Ok(msg) => handle(s, msg),
        Err(e) => Some(error(Value::Null, PARSE_ERROR, format!("parse error: {e}"))),
    };
    reply.map(|r| r.to_string())
}

fn handle(s: &mut Session, msg: Value) -> Option<Value> {
    let Some(o) = msg.as_object() else { return Some(error(Value::Null, INVALID_REQUEST, "a message must be an object")) };
    let id = o.get("id").cloned();
    let Some(method) = o.get("method").and_then(Value::as_str) else {
        // A response to us: there is nothing to answer.
        if o.contains_key("result") || o.contains_key("error") {
            return None;
        }
        return Some(error(id.unwrap_or(Value::Null), INVALID_REQUEST, "missing `method`"));
    };
    // Notifications (no id) get no reply.
    let id = id?;
    let params = o.get("params").cloned().unwrap_or(Value::Null);
    Some(match request(s, method, &params) {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err((code, message)) => error(id, code, message),
    })
}

fn request(s: &mut Session, method: &str, params: &Value) -> Result<Value, (i64, String)> {
    match method {
        "initialize" => {
            let asked = params.get("protocolVersion").and_then(Value::as_str).unwrap_or(PROTOCOL_VERSION);
            let version = if SUPPORTED.contains(&asked) { asked } else { PROTOCOL_VERSION };
            Ok(json!({
                "protocolVersion": version,
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {"name": "bimcraft", "title": "BIMCraft", "version": env!("CARGO_PKG_VERSION")},
                "instructions": INSTRUCTIONS,
            }))
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools() })),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).ok_or((INVALID_PARAMS, "missing tool `name`".to_string()))?;
            call(s, name, params.get("arguments").unwrap_or(&Value::Null))
        }
        _ => Err((METHOD_NOT_FOUND, format!("method `{method}` not found"))),
    }
}

/// A tool call. `Err` is a protocol error (unknown tool, arguments that aren't an object or lack
/// a required one); a tool that runs and fails answers with `isError`.
fn call(s: &mut Session, name: &str, args: &Value) -> Result<Value, (i64, String)> {
    let tools = tools();
    let tool = tools
        .as_array()
        .and_then(|a| a.iter().find(|t| t["name"] == name))
        .ok_or_else(|| (INVALID_PARAMS, format!("unknown tool `{name}`")))?;
    let args = match args {
        Value::Null => json!({}),
        Value::Object(_) => args.clone(),
        _ => return Err((INVALID_PARAMS, format!("the arguments of `{name}` must be an object"))),
    };
    for key in tool["inputSchema"]["required"].as_array().into_iter().flatten().filter_map(Value::as_str) {
        if args.get(key).is_none_or(Value::is_null) {
            return Err((INVALID_PARAMS, format!("`{name}` needs `{key}`")));
        }
    }
    let result = match name {
        "execute" => {
            let params = args.get("params").cloned().unwrap_or_else(|| json!({}));
            s.execute(args["command"].as_str().unwrap_or_default(), &params)
        }
        "script" => s.script(args["text"].as_str().unwrap_or_default()).map(|results| json!({ "results": results })),
        "list_commands" => {
            let filter = args["filter"].as_str().map(str::to_ascii_lowercase).unwrap_or_default();
            let has = |v: &Value| v.as_str().is_some_and(|t| t.to_ascii_lowercase().contains(&filter));
            let all = catalog();
            Ok(all
                .as_array()
                .map(|cmds| {
                    cmds.iter()
                        .filter(|c| has(&c["id"]) || has(&c["label"]) || c["aliases"].as_array().is_some_and(|a| a.iter().any(has)))
                        .cloned()
                        .collect()
                })
                .unwrap_or_default())
        }
        "inspect_model" => s.execute("inspect", &json!({})),
        "query_elements" => s.execute("elements", &args),
        "schedule" => s.execute("schedule", &args),
        "new_model" => s.execute("new", &args),
        "open" => s.execute("open", &args),
        "save" => s.execute("save", &args),
        "render" => {
            let keys = ["view", "ceiling", "width", "height", "azimuth", "elevation"];
            let view: serde_json::Map<String, Value> = keys.iter().map(|k| (k.to_string(), args[*k].clone())).collect();
            return Ok(match s.execute("render", &Value::Object(view)) {
                Ok(v) => {
                    let name = v["view"].as_str().unwrap_or("");
                    let what = match v["kind"].as_str() {
                        Some("3D view") => {
                            let deg = |k: &str| v[k].as_f64().unwrap_or_default();
                            format!("3D view from {}° round, {}° up", deg("azimuth"), deg("elevation"))
                        }
                        Some(kind) => format!("{name} ({kind})"),
                        None => name.to_string(),
                    };
                    json!({"content": [
                        {"type": "image", "data": v["pngBase64"], "mimeType": "image/png"},
                        {"type": "text", "text": format!("{what}, {} x {} px", v["width"], v["height"])},
                    ]})
                }
                Err(e) => tool_error(&e),
            });
        }
        _ => return Err((INVALID_PARAMS, format!("unknown tool `{name}`"))),
    };
    Ok(match result {
        Ok(v) => text(&v),
        Err(e) => tool_error(&e),
    })
}

/// Whether a line of the protocol asks to start another project (`new_model`, `open`, or a script or `execute` of `new` or
/// `open`): a front end holding views of the old one starts over on the session's after replying.
pub fn starts_over(line: &str) -> bool {
    fn replaces(command: &str) -> bool {
        crate::engine::find(command).is_some_and(|c| c.id == "new" || c.id == "open")
    }
    fn one(msg: &Value) -> bool {
        if msg["method"] != "tools/call" {
            return false;
        }
        let args = &msg["params"]["arguments"];
        match msg["params"]["name"].as_str() {
            Some("new_model" | "open") => true,
            Some("execute") => args["command"].as_str().is_some_and(replaces),
            Some("script") => args["text"].as_str().is_some_and(|t| t.lines().filter_map(|l| l.split_whitespace().next()).any(replaces)),
            _ => false,
        }
    }
    match serde_json::from_str::<Value>(line.trim()) {
        Ok(Value::Array(batch)) => batch.iter().any(one),
        Ok(msg) => one(&msg),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(s: &mut Server, id: u64, method: &str, params: Value) -> Value {
        let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string();
        serde_json::from_str(&s.handle_line(&line).expect("a reply")).unwrap()
    }

    /// A tool's result: its text content parsed as JSON, or its first content item.
    fn tool(s: &mut Server, name: &str, args: Value) -> Value {
        let r = call(s, 9, "tools/call", json!({"name": name, "arguments": args}));
        let content = &r["result"]["content"][0];
        assert_ne!(r["result"]["isError"], json!(true), "{name} failed: {content}");
        if content["type"] == "text" { serde_json::from_str(content["text"].as_str().unwrap()).unwrap() } else { content.clone() }
    }

    #[test]
    fn lifecycle_and_tool_list() {
        let mut s = Server::new();
        let init = call(&mut s, 1, "initialize", json!({"protocolVersion": "2025-06-18"}));
        assert_eq!(init["result"]["serverInfo"]["name"], "bimcraft");
        assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
        assert!(s.handle_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).is_none());
        let tools = call(&mut s, 2, "tools/list", json!({}));
        let names: Vec<&str> = tools["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"execute") && names.contains(&"render"), "{names:?}");
        assert_eq!(call(&mut s, 3, "nope", json!({}))["error"]["code"], -32601);
        let old = call(&mut s, 4, "initialize", json!({"protocolVersion": "1999-01-01"}));
        assert_eq!(old["result"]["protocolVersion"], PROTOCOL_VERSION);
    }

    #[test]
    fn an_agent_models_a_house_and_checks_it_through_mcp_only() {
        let mut s = Server::new();
        tool(&mut s, "new_model", json!({"name": "Cabin"}));
        tool(
            &mut s,
            "execute",
            json!({"command": "wall", "params": {"points": [[0, 0], [6000, 0], [6000, 4000], [0, 4000]], "closed": true}}),
        );
        let script =
            "door {\"at\": [3000, 0]}\nwindow {\"at\": [6000, 2000]}\nroom {\"point\": [3000, 2000], \"name\": \"Cabin\"}\nfloor {}";
        tool(&mut s, "script", json!({ "text": script }));
        let m = tool(&mut s, "inspect_model", json!({}));
        assert_eq!(m["counts"], json!({"door": 1, "floor": 1, "level": 2, "room": 1, "wall": 4, "window": 1}));
        let doors = tool(&mut s, "query_elements", json!({"category": "door"}));
        assert_eq!(doors["elements"][0]["at"], json!([3000.0, 0.0]));
        let rooms = tool(&mut s, "schedule", json!({"category": "rooms"}));
        assert_eq!(rooms["rows"][0][3], 24.0);
        let big = json!({"name": "Big Rooms", "category": "rooms", "fields": ["Name", {"name": "Cost", "formula": "Area * 100", "total": true}], "filters": [{"field": "Area", "op": "greater", "value": 20}]});
        let big = tool(&mut s, "schedule", big);
        assert_eq!((big["rows"].clone(), big["totals"].clone()), (json!([["Cabin", 2400.0]]), json!([null, 2400.0])));
        let img = tool(&mut s, "render", json!({"width": 400, "height": 300}));
        assert_eq!((img["type"].as_str(), img["mimeType"].as_str()), (Some("image"), Some("image/png")));
        let r = call(&mut s, 10, "tools/call", json!({"name": "render", "arguments": {"view": "{3D}", "azimuth": 135, "elevation": 20}}));
        assert_eq!(r["result"]["content"][1]["text"], "3D view from 135° round, 20° up, 1600 x 1000 px");
        let path = std::env::temp_dir().join(format!("bimcraft-mcp-{}.ifc", std::process::id()));
        tool(&mut s, "save", json!({ "path": path }));
        assert!(std::fs::read_to_string(&path).unwrap().contains("IFCSPACE("));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn failures_are_tool_errors_and_bad_calls_are_protocol_errors() {
        let mut s = Server::new();
        let r = call(&mut s, 1, "tools/call", json!({"name": "execute", "arguments": {"command": "door", "params": {"at": [0, 0]}}}));
        assert_eq!(r["result"]["isError"], true);
        assert!(r["result"]["content"][0]["text"].as_str().unwrap().contains("no wall"));
        assert_eq!(call(&mut s, 2, "tools/call", json!({"name": "execute", "arguments": {}}))["error"]["code"], -32602);
        assert_eq!(call(&mut s, 3, "tools/call", json!({"name": "teleport", "arguments": {}}))["error"]["code"], -32602);
        assert_eq!(call(&mut s, 4, "tools/call", json!({"name": "script", "arguments": "x"}))["error"]["code"], -32602);
        assert!(s.handle_line("{not json").unwrap().contains("-32700"));
        assert!(s.handle_line("[]").unwrap().contains("-32600"));
    }

    #[test]
    fn serve_answers_each_request_on_its_own_line() {
        let mut s = Server::new();
        let input = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n\n\
                     {\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n\
                     {\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"ping\"}\n";
        let mut out = Vec::new();
        s.serve(input.as_bytes(), &mut out).unwrap();
        let lines: Vec<Value> = String::from_utf8(out).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(lines.iter().map(|l| l["id"].as_u64()).collect::<Vec<_>>(), [Some(1), Some(2)]);
    }

    #[test]
    fn a_live_address_is_on_this_machine_only() {
        assert_eq!(live_addr(LIVE_ADDR).map(|a| a.port()), Ok(47833));
        assert!(live_addr("[::1]:9000").is_ok());
        assert!(live_addr("0.0.0.0:47833").unwrap_err().contains("this machine only"));
        assert!(live_addr("192.168.1.10:47833").is_err());
        assert!(live_addr("localhost").is_err());
    }

    #[test]
    fn reply_runs_on_the_session_it_is_given() {
        let mut session = Session::new();
        let line = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "execute",
            "arguments": {"command": "wall", "params": {"points": [[0, 0], [4000, 0]]}}}})
        .to_string();
        let r: Value = serde_json::from_str(&reply(&mut session, &line).unwrap()).unwrap();
        assert_ne!(r["result"]["isError"], json!(true), "{r}");
        assert_eq!(session.model.elements.values().filter(|e| e.category() == "wall").count(), 1);
        assert!(reply(&mut session, "  ").is_none());
    }

    #[test]
    fn starts_over_finds_new_and_open_however_they_are_asked() {
        let call = |name: &str, args: Value| {
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": args}}).to_string()
        };
        assert!(starts_over(&call("new_model", json!({}))));
        assert!(starts_over(&call("open", json!({"path": "x.bimcraft"}))));
        assert!(starts_over(&call("execute", json!({"command": "NEW"}))));
        assert!(starts_over(&call("script", json!({"text": "wall {}\nopen {\"path\": \"a.ifc\"}"}))));
        assert!(!starts_over(&call("execute", json!({"command": "wall", "params": {}}))));
        assert!(!starts_over(&call("script", json!({"text": "level {\"elevation\": 3000}"}))));
        assert!(!starts_over(&call("save", json!({"path": "x.bimcraft"}))));
        assert!(starts_over(&format!("[{}]", call("new_model", json!({})))));
        assert!(!starts_over("{not json"));
    }
}
