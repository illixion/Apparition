//! The MCP server for running apps: `DebugTrace/Tools/debugtrace-mcp` in Rust, served by the
//! process that holds the Mac's Local Network permission, so an agent anywhere that can reach the
//! Store (a Windows machine with this Mac as its build and deploy server) gets the same tools.
//!
//! Tools: `apps`, `help`, `call`, `trace`, `panel`. Their schemas and the instructions are the
//! Python tool's own, embedded from `mcp_tools.json` (a test compares them with the Python source).
//! `inject` is listed but answers that it is not in this server yet.

use crate::debug::{CALL_TIMEOUT, Debug, Session};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Map, Value, json};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

const TOOLS_JSON: &str = include_str!("mcp_tools.json");
const PROTOCOL_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];
/// Past this a reply is more than an agent's tool result holds, so it goes to a file and the
/// reply says where, what it held, and the newest lines.
const MAX_INLINE_REPLY: usize = 60_000;
const SPILL_TAIL_LINES: usize = 40;
const CONTROL_KINDS: [&str; 5] = ["slider", "toggle", "choice", "button", "readout"];

/// A failure the model should see, with what to do about it.
struct ToolError(String, Option<String>);

fn err(message: impl Into<String>) -> ToolError {
    ToolError(message.into(), None)
}

fn err_hint(message: impl Into<String>, hint: impl Into<String>) -> ToolError {
    ToolError(message.into(), Some(hint.into()))
}

impl ToolError {
    fn text(&self) -> String {
        match &self.1 {
            Some(h) => format!("{}\nhint: {h}", self.0),
            None => self.0.clone(),
        }
    }
}

fn text_result(value: impl Into<TextOrJson>, is_error: bool) -> Value {
    let text = match value.into() {
        TextOrJson::Text(t) => t,
        TextOrJson::Json(v) => serde_json::to_string_pretty(&v).unwrap_or_default(),
    };
    let mut result = json!({ "content": [{ "type": "text", "text": text }] });
    if is_error {
        result["isError"] = Value::Bool(true);
    }
    result
}

enum TextOrJson {
    Text(String),
    Json(Value),
}
impl From<String> for TextOrJson { fn from(s: String) -> Self { TextOrJson::Text(s) } }
impl From<&str> for TextOrJson { fn from(s: &str) -> Self { TextOrJson::Text(s.into()) } }
impl From<Value> for TextOrJson { fn from(v: Value) -> Self { TextOrJson::Json(v) } }

fn s<'a>(session: &'a Session, key: &str) -> Option<&'a str> {
    session.get(key).and_then(Value::as_str)
}

pub struct Mcp {
    debug: Arc<Debug>,
    spec: Value,
    /// Each tool's parameter names in the order its schema lists them (a `Value` sorts keys).
    params: std::collections::HashMap<String, Vec<String>>,
    verified: Mutex<HashSet<(String, String)>>,
    /// Where a reply too big for a tool result, or a file an app returns, is saved when the app's
    /// project root is unknown.
    downloads: PathBuf,
}

impl Mcp {
    pub fn new(debug: Arc<Debug>) -> Mcp {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
        Mcp { debug, spec: serde_json::from_str(TOOLS_JSON).expect("mcp_tools.json"), params: parameter_order(), verified: Mutex::default(), downloads: home.join("Downloads") }
    }

    pub fn instructions(&self) -> &str {
        self.spec["instructions"].as_str().unwrap_or("")
    }

    pub fn tools(&self) -> &Value {
        &self.spec["tools"]
    }

    /// One JSON-RPC message; None for a notification.
    pub async fn handle(&self, message: &Value) -> Option<Value> {
        let id = message.get("id").filter(|i| !i.is_null())?.clone();
        let method = message["method"].as_str().unwrap_or("");
        let reply = |result: Value| Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }));
        let fail = |code: i64, message: String| Some(json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }));
        match method {
            "initialize" => {
                let asked = message.pointer("/params/protocolVersion").and_then(Value::as_str).unwrap_or("");
                let version = PROTOCOL_VERSIONS.iter().find(|v| **v == asked).unwrap_or(&PROTOCOL_VERSIONS[0]);
                reply(json!({ "protocolVersion": version, "capabilities": { "tools": {} },
                    "serverInfo": { "name": "debugtrace-apps", "version": "1" }, "instructions": self.instructions() }))
            }
            "ping" => reply(json!({})),
            "tools/list" => reply(json!({ "tools": self.tools() })),
            "tools/call" => {
                let params = message.get("params").cloned().unwrap_or(Value::Null);
                let name = params["name"].as_str().unwrap_or("").to_string();
                if !["panel", "apps", "help", "call", "inject", "trace"].contains(&name.as_str()) {
                    return fail(-32602, format!("unknown tool {name:?}; tools: panel, apps, help, call, inject, trace"));
                }
                let result = match self.run_tool(&name, params.get("arguments")).await {
                    Ok(r) => r,
                    Err(e) => text_result(e.text(), true),
                };
                reply(result)
            }
            other => fail(-32601, format!("method not found: {other}")),
        }
    }

    async fn run_tool(&self, name: &str, raw: Option<&Value>) -> Result<Value, ToolError> {
        let raw = match raw {
            None | Some(Value::Null) => Map::new(),
            Some(Value::Object(m)) => m.clone(),
            Some(_) => return Err(err("tool arguments must be an object")),
        };
        let (args, notes) = self.normalize(name, raw)?;
        let mut result = match name {
            "apps" => self.tool_apps().await,
            "help" => self.tool_help(&args).await?,
            "call" => self.tool_call(&args).await?,
            "trace" => self.tool_trace(&args).await?,
            "panel" => self.tool_panel(&args).await?,
            _ => return Err(err_hint("inject is not available in the Rust service yet", "use the Python debugtrace-mcp or `apparition inject` on the build Mac")),
        };
        if !notes.is_empty() {
            result["content"].as_array_mut().unwrap().push(json!({ "type": "text", "text": format!("note: {}", notes.join("; ")) }));
        }
        Ok(result)
    }

    /// The main client is a language model, and models guess parameter names: `args` for
    /// `arguments`, `name` for `endpoint`, or the endpoint's own arguments flattened next to
    /// `endpoint`. Each tool accepts the common variants, says which it accepted, and rejects any
    /// other name with the valid ones listed.
    fn normalize(&self, tool: &str, raw: Map<String, Value>) -> Result<(Map<String, Value>, Vec<String>), ToolError> {
        let aliases: &[(&str, &[&str])] = &[
            ("app", &["bundleId", "bundle_id", "bundle", "appName", "application"]),
            ("endpoint", &["name", "endpointName", "endpoint_name", "tool", "method"]),
            ("arguments", &["args", "params", "parameters", "input", "payload", "body"]),
            ("files", &["file", "paths", "path", "sources"]),
            ("run", &["code", "snippet", "eval", "script", "swift", "expression"]),
            ("allowViewTypeChange", &["allow_view_type_change", "allowTypeChange", "allowViewTypeChanges"]),
        ];
        let known: Vec<String> = self.params.get(tool).cloned().unwrap_or_default();
        let mut taken: Vec<(String, String)> = Vec::new(); // (canonical, given key)
        let (mut notes, mut extras) = (Vec::new(), Map::new());
        for (key, value) in &raw {
            let canonical = if known.contains(key) { Some(key.clone()) } else {
                known.iter().find(|n| aliases.iter().any(|(a, alts)| a == n && alts.contains(&key.as_str()))).cloned()
            };
            let Some(canonical) = canonical else { extras.insert(key.clone(), value.clone()); continue };
            if let Some((_, first)) = taken.iter().find(|(c, _)| *c == canonical) {
                return Err(err_hint(format!("`{canonical}` was given twice (as `{first}` and `{key}`)"), format!("pass it once, as `{canonical}`")));
            }
            if *key != canonical {
                notes.push(format!("`{key}` was accepted as `{canonical}`; use `{canonical}` next time"));
            }
            taken.push((canonical, key.clone()));
        }
        let mut result: Map<String, Value> = taken.iter().map(|(c, k)| (c.clone(), raw[k].clone())).collect();
        if tool == "call" {
            // `arguments` as a JSON string is a common slip; parse it.
            if let Some(Value::String(text)) = result.get("arguments").cloned() {
                let parsed = serde_json::from_str(&text).map_err(|_| err_hint("`arguments` is not valid JSON", "pass an object, e.g. {\"level\": \"error\"}"))?;
                result.insert("arguments".into(), parsed);
                notes.push("`arguments` was a JSON string; pass it as an object".into());
            }
            if result.get("arguments").is_some_and(|a| !a.is_object()) {
                return Err(err_hint("`arguments` must be an object keyed by the endpoint's parameter names", "call `help` for the endpoint's parameters"));
            }
            // Arguments flattened next to `endpoint` are the endpoint's own.
            if !extras.is_empty() {
                let names = extras.keys().map(|k| format!("`{k}`")).collect::<Vec<_>>().join(", ");
                if result.contains_key("arguments") {
                    return Err(err_hint(format!("unexpected top-level {names} alongside `arguments`"), "put the endpoint's parameters inside `arguments`"));
                }
                result.insert("arguments".into(), Value::Object(extras));
                notes.push(format!("{names} were passed at the top level and treated as the endpoint's arguments; nest them under `arguments` next time"));
            }
        } else if let Some(key) = extras.keys().next() {
            let close = known.iter().filter(|k| edit_distance(k, key) <= 3).min_by_key(|k| edit_distance(k, key));
            return Err(err_hint(format!("unknown parameter `{key}` for `{tool}`"),
                format!("{}valid parameters: {}", close.map(|c| format!("did you mean `{c}`? ")).unwrap_or_default(), if known.is_empty() { "none".into() } else { known.join(", ") })));
        }
        Ok((result, notes))
    }

    fn describe(&self, session: &Session) -> Value {
        let mut out = Map::new();
        for key in ["appName", "bundleId", "build", "device", "url", "startedAt", "consoleLog", "source", "attachable", "reason"] {
            if let Some(v) = session.get(key).filter(|v| !v.is_null()) {
                out.insert(key.into(), v.clone());
            }
        }
        // From the session record alone: listing apps sends no request to any of them.
        let inject = if let Some(dir) = s(session, "injectDir").filter(|d| !d.is_empty()) {
            let mut i = json!({ "ready": true, "note": "launched with --inject: edit Swift files, then call `inject`" });
            if let Some(backend) = std::fs::read_to_string(Path::new(dir).join("modules.json")).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).map(|m| m["backend"].as_str().unwrap_or("dynamic").to_string()) {
                if backend == "apparition" {
                    i["note"] = json!("native entry boundaries; use apparition inject on the build Mac, or attach Apparition JIT before MCP inject");
                }
                i["backend"] = json!(backend);
            }
            i
        } else if s(session, "source") == Some("bonjour") {
            json!({ "ready": "unknown", "note": "attached without a launch record; `inject` finds out when called, or relaunch with `build-and-sign --run --mcp --inject`" })
        } else {
            json!({ "ready": false, "note": "not launched with --inject; `inject` needs `build-and-sign --run --mcp --inject` from the app's repo" })
        };
        out.insert("inject".into(), inject);
        Value::Object(out)
    }

    async fn tool_apps(&self) -> Value {
        let sessions = self.debug.all_sessions().await;
        if sessions.is_empty() {
            return text_result(json!({ "apps": [], "hint": "open a dev build signed by build-and-sign on a device on this network, or launch one with `build-and-sign --run --mcp`" }), false);
        }
        text_result(json!({ "apps": sessions.iter().map(|s| self.describe(s)).collect::<Vec<_>>() }), false)
    }

    /// The running app an agent means: by name, bundle id or its last segment, `@device` when it
    /// runs on several, and the only one when none is named.
    async fn resolve(&self, app: Option<&str>) -> Result<Session, ToolError> {
        let mut sessions = self.debug.live_sessions().await;
        let name_of = |x: &Session| s(x, "appName").or(s(x, "bundleId")).unwrap_or("").to_string();
        let Some(app) = app.map(str::trim).filter(|a| !a.is_empty()) else {
            return match sessions.len() {
                0 => Err(err_hint("no app is running with its debug server", "open a dev build signed by build-and-sign on a device on this network, or launch one with `build-and-sign --run --mcp` (from the app's repo), then call `apps`")),
                1 => Ok(sessions.remove(0)),
                n => Err(err_hint(format!("{n} apps are running ({})", sessions.iter().map(name_of).collect::<Vec<_>>().join(", ")), "pass `app` with one of those names")),
            };
        };
        let mut wanted = app.to_lowercase();
        let keys = |x: &Session| -> Vec<String> {
            let bundle = s(x, "bundleId").unwrap_or("").to_lowercase();
            vec![bundle.clone(), s(x, "appName").unwrap_or("").to_lowercase(), bundle.rsplit('.').next().unwrap_or("").to_string()]
        };
        if let Some((w, device)) = wanted.clone().rsplit_once('@') {
            let (w, device) = (w.trim().to_string(), device.trim().to_string());
            wanted = w;
            let on_device = |sessions: &[Session]| sessions.iter().filter(|x| s(x, "device").unwrap_or("").to_lowercase() == device).cloned().collect::<Vec<_>>();
            let mut found = on_device(&sessions);
            // The device may be serving normally while one browse misses its advertisement:
            // ask again a few times before saying it has no running apps.
            for _ in 0..4 {
                if !found.is_empty() { break; }
                tokio::time::sleep(std::time::Duration::from_millis(400)).await;
                self.debug.refresh_discovery().await;
                sessions = self.debug.live_sessions().await;
                found = on_device(&sessions);
            }
            if found.is_empty() {
                // Bonjour can omit a live service; an explicitly named target falls back to its
                // private launch record, and `verified` authenticates before any endpoint runs.
                let recorded: Vec<Session> = self.debug.recorded_sessions().await.into_iter().filter(|x| {
                    s(x, "device").unwrap_or("").to_lowercase() == device && keys(x).contains(&wanted)
                        && s(x, "token").is_some_and(|t| !t.is_empty()) && s(x, "url").is_some_and(|u| !u.is_empty())
                }).collect();
                match recorded.last() {
                    Some(r) => sessions = vec![r.clone()],
                    None => {
                        let mut devices: Vec<String> = sessions.iter().filter_map(|x| s(x, "device").filter(|d| !d.is_empty()).map(String::from)).collect();
                        devices.sort();
                        devices.dedup();
                        return Err(err_hint(format!("no running app is on a device named '{device}'"), format!("devices with apps running: {}", if devices.is_empty() { "none".into() } else { devices.join(", ") })));
                    }
                }
            } else {
                sessions = found;
            }
        } else if sessions.is_empty() {
            return Err(err_hint("no app is running with its debug server", "open a dev build signed by build-and-sign on a device on this network, or launch one with `build-and-sign --run --mcp` (from the app's repo), then call `apps`"));
        }
        let exact: Vec<&Session> = sessions.iter().filter(|x| keys(x).contains(&wanted)).collect();
        let matches: Vec<&Session> = if exact.is_empty() { sessions.iter().filter(|x| keys(x).iter().any(|k| !k.is_empty() && k.contains(&wanted))).collect() } else { exact };
        if matches.len() == 1 {
            return Ok(matches[0].clone());
        }
        if !matches.is_empty() {
            let mut devices: Vec<&str> = matches.iter().filter_map(|m| s(m, "device").filter(|d| !d.is_empty())).collect();
            devices.sort();
            devices.dedup();
            let bundles: HashSet<&str> = matches.iter().filter_map(|m| s(m, "bundleId")).collect();
            if bundles.len() == 1 && devices.len() > 1 {
                let bundle = s(matches[0], "bundleId").unwrap_or("");
                return Err(err_hint(format!("'{app}' is running on several devices ({})", devices.join(", ")), format!("add the device: {}", devices.iter().map(|d| format!("{bundle}@{d}")).collect::<Vec<_>>().join(" or "))));
            }
            return Err(err_hint(format!("'{app}' matches several running apps ({})", matches.iter().map(|m| name_of(m)).collect::<Vec<_>>().join(", ")), "pass the full bundle id"));
        }
        Err(err_hint(format!("no running app matches '{app}'"), format!("running now: {}; call `apps` for details", sessions.iter().map(name_of).collect::<Vec<_>>().join(", "))))
    }

    async fn request(&self, session: &Session, post: Option<Value>, path: &str) -> Result<(u16, String, Vec<u8>), ToolError> {
        match self.debug.app_request(session, post, path, CALL_TIMEOUT).await {
            Ok(r) => Ok((r.status, r.content_type, r.raw)),
            Err(e) => Err(err_hint(e, "the app may be suspended or the device asleep; bring it to the foreground, or relaunch with `build-and-sign --run --mcp`")),
        }
    }

    /// The session file says which app is on this port; make sure it still is.
    async fn verified(&self, app: Option<&str>) -> Result<Session, ToolError> {
        let session = self.resolve(app).await?;
        let key = (s(&session, "url").unwrap_or("").to_string(), session.get("startedAt").map(Value::to_string).unwrap_or_default());
        if self.verified.lock().unwrap().contains(&key) {
            return Ok(session);
        }
        let (status, _, raw) = self.request(&session, None, "/_info").await?;
        let info: Value = serde_json::from_slice(&raw).unwrap_or(Value::Null);
        let bundle = info.pointer("/data/app/bundleId").and_then(Value::as_str);
        let name = s(&session, "appName").unwrap_or("The app");
        if status == 401 {
            return Err(err_hint(format!("{name} refused the token"), "the build on the device is not the one the ledger describes; reinstall it with build-and-sign"));
        }
        if let Some(b) = bundle.filter(|b| Some(*b) != s(&session, "bundleId")) {
            return Err(err_hint(format!("{} is now served by {b}, not {}", key.0, s(&session, "bundleId").unwrap_or("")), "relaunch the app with `build-and-sign --run --mcp` to refresh its session"));
        }
        self.verified.lock().unwrap().insert(key);
        Ok(session)
    }

    /// Saves under the project's `build/debugtrace`. `relative` is built from names the app
    /// chose (an endpoint, a trace's file name), so it must stay a plain relative path.
    async fn save_file(&self, session: &Session, relative: &str, data: &[u8]) -> Result<String, ToolError> {
        let plain = !relative.is_empty() && !relative.starts_with('/') && !relative.contains('\\')
            && relative.split('/').all(|part| !part.is_empty() && part != "." && part != ".." && !part.starts_with('.'));
        if !plain {
            return Err(err(format!("refusing to save the app's reply as {relative}")));
        }
        let root = s(session, "projectRoot").filter(|r| !r.is_empty()).map(PathBuf::from).unwrap_or_else(|| self.downloads.clone());
        let path = root.join("build/debugtrace").join(relative);
        let parent = path.parent().map(PathBuf::from).unwrap_or_default();
        let saved = async { tokio::fs::create_dir_all(&parent).await?; tokio::fs::write(&path, data).await };
        saved.await.map_err(|e| err(format!("could not save {}: {e}", path.display())))?;
        Ok(path.display().to_string())
    }

    async fn tool_help(&self, args: &Map<String, Value>) -> Result<Value, ToolError> {
        let session = self.verified(args.get("app").and_then(Value::as_str)).await?;
        let path = match args.get("endpoint").and_then(Value::as_str).filter(|e| !e.is_empty()) {
            Some(e) => format!("/?endpoint={}", quote(e)),
            None => "/".into(),
        };
        let (status, _, raw) = self.request(&session, None, &path).await?;
        Ok(text_result(String::from_utf8_lossy(&raw).into_owned(), status >= 400))
    }

    async fn tool_call(&self, args: &Map<String, Value>) -> Result<Value, ToolError> {
        let session = self.verified(args.get("app").and_then(Value::as_str)).await?;
        let endpoint = match args.get("endpoint") {
            None | Some(Value::Null) => return Err(err_hint("`endpoint` is required", "call `help` to list the app's endpoints")),
            Some(Value::String(e)) if !e.is_empty() => e.clone(),
            Some(Value::String(_)) => return Err(err_hint("`endpoint` is required", "call `help` to list the app's endpoints")),
            Some(_) => return Err(err_hint("`endpoint` must be a string", "call `help` to list the app's endpoints")),
        };
        if !crate::debug::valid_endpoint(&endpoint) {
            return Err(err_hint(format!("`{endpoint}` isn't an endpoint name"), "call `help` to list the app's endpoints"));
        }
        let arguments = args.get("arguments").filter(|a| a.is_object()).cloned().unwrap_or_else(|| json!({}));
        let (status, content_type, raw) = self.request(&session, Some(json!({ "name": endpoint, "arguments": arguments })), "/_call").await?;
        if content_type.starts_with("application/json") {
            if status < 400 && raw.len() > MAX_INLINE_REPLY {
                return self.spilled_reply(&session, &endpoint, &raw).await;
            }
            return Ok(text_result(String::from_utf8_lossy(&raw).into_owned(), status >= 400));
        }
        if content_type.starts_with("image/") {
            return Ok(json!({ "content": [{ "type": "image", "data": STANDARD.encode(&raw), "mimeType": content_type.split(';').next().unwrap_or("") }] }));
        }
        let extension = content_type.rsplit('/').next().unwrap_or("").split(';').next().map(str::trim)
            .filter(|e| !e.is_empty() && e.len() <= 8 && e.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()))
            .unwrap_or("bin").to_string();
        let path = self.save_file(&session, &format!("{endpoint}.{extension}"), &raw).await?;
        Ok(text_result(json!({ "ok": true, "endpoint": endpoint, "savedTo": path, "contentType": content_type, "bytes": raw.len() }), false))
    }

    /// A reply too big for a tool result: the whole of it goes to a file, and the answer says
    /// where, what it held, and the newest lines.
    async fn spilled_reply(&self, session: &Session, endpoint: &str, raw: &[u8]) -> Result<Value, ToolError> {
        let stamp = stamp();
        let reply: Value = serde_json::from_slice(raw).unwrap_or(Value::Null);
        let data = reply.get("data");
        if let Some(entries) = data.and_then(|d| d["entries"].as_array()).filter(|e| e.iter().all(Value::is_string)) {
            // `_logs` in lines format: one line per entry, so it greps as a log.
            let lines: Vec<&str> = entries.iter().filter_map(Value::as_str).collect();
            let path = self.save_file(session, &format!("{endpoint}-{stamp}.log"), (lines.join("\n") + "\n").as_bytes()).await?;
            let mut summary = json!({ "ok": true, "endpoint": endpoint, "savedTo": path, "bytes": raw.len(),
                "note": format!("{} lines, too many to return inline: the whole reply is in savedTo, one entry per line. The newest {} follow.", lines.len(), lines.len().min(SPILL_TAIL_LINES)),
                "hint": "grep the file, or narrow the call: excludeCategory for the chatty categories in `categories`, or category/contains/level" });
            for k in ["source", "returned", "matched", "truncated", "lastSequence", "categories", "buffer"] {
                if let Some(v) = data.and_then(|d| d.get(k)) { summary[k] = v.clone(); }
            }
            summary["newest"] = json!(lines[lines.len().saturating_sub(SPILL_TAIL_LINES)..]);
            return Ok(text_result(summary, false));
        }
        let path = self.save_file(session, &format!("{endpoint}-{stamp}.json"), raw).await?;
        let mut keys: Option<Vec<String>> = data.and_then(Value::as_object).map(|o| o.keys().cloned().collect());
        if let Some(k) = keys.as_mut() { k.sort(); }
        Ok(text_result(json!({ "ok": true, "endpoint": endpoint, "savedTo": path, "bytes": raw.len(),
            "note": "the reply is too large to return inline; it is saved whole in savedTo", "dataKeys": keys,
            "hint": "read savedTo with jq, or narrow the call's arguments" }), false))
    }

    async fn tool_trace(&self, args: &Map<String, Value>) -> Result<Value, ToolError> {
        let session = self.verified(args.get("app").and_then(Value::as_str)).await?;
        let mut call_args = Map::new();
        for k in ["note", "logMinutes"] {
            if let Some(v) = args.get(k) { call_args.insert(k.into(), v.clone()); }
        }
        let (status, _, raw) = self.request(&session, Some(json!({ "name": "_trace", "arguments": call_args })), "/_call").await?;
        let Ok(reply) = serde_json::from_slice::<Value>(&raw) else { return Ok(text_result(String::from_utf8_lossy(&raw).into_owned(), true)) };
        if status >= 400 || reply["ok"].as_bool() != Some(true) {
            return Ok(text_result(reply, true));
        }
        let data = &reply["data"];
        let (status, _, archive) = self.request(&session, None, data["download"].as_str().unwrap_or("")).await?;
        if status >= 400 {
            return Ok(text_result(String::from_utf8_lossy(&archive).into_owned(), true));
        }
        let filename = data["filename"].as_str().unwrap_or("trace.zip");
        if filename.contains(['/', '\\']) || filename.starts_with('.') {
            return Err(err("the app named its trace file with a path; not saving it"));
        }
        let path = self.save_file(&session, &format!("traces/{filename}"), &archive).await?;
        let files: Vec<&str> = data["files"].as_array().into_iter().flatten().filter_map(|f| f["path"].as_str()).collect();
        Ok(text_result(json!({ "savedTo": path, "traceId": data["traceId"], "signed": data["signed"], "bytes": archive.len(), "files": files }), false))
    }

    async fn panel_bundle(&self, app: Option<&str>) -> Result<String, ToolError> {
        match self.resolve(app).await {
            Ok(session) => Ok(s(&session, "bundleId").unwrap_or("").to_string()),
            Err(e) => {
                let name = app.unwrap_or("").split('@').next().unwrap_or("");
                let parts: Vec<&str> = name.split('.').collect();
                let ok = parts.len() >= 2 && parts.iter().all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'));
                if ok { Ok(name.to_string()) } else { Err(e) }
            }
        }
    }

    async fn tool_panel(&self, args: &Map<String, Value>) -> Result<Value, ToolError> {
        let bundle = self.panel_bundle(args.get("app").and_then(Value::as_str)).await?;
        if !crate::debug::valid_bundle_id(&bundle) {
            return Err(err(format!("the app's bundle id {bundle:?} can't name a panel folder")));
        }
        let dir = self.debug.panels_root().join(&bundle);
        let remove = args.get("remove").and_then(Value::as_bool).unwrap_or(false);
        let controls = args.get("controls").filter(|c| !c.is_null());
        if controls.is_none() && !remove {
            let read = dir.clone();
            let panels: Vec<Value> = tokio::task::spawn_blocking(move || {
                let mut files: Vec<PathBuf> = std::fs::read_dir(&read).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "json")).collect();
                files.sort();
                files.iter().filter_map(|f| serde_json::from_str(&std::fs::read_to_string(f).ok()?).ok()).collect()
            }).await.unwrap_or_default();
            return Ok(text_result(json!({ "bundleId": bundle, "panels": panels, "hint": "pass `name` and `controls` to create one; tunables already get an automatic panel" }), false));
        }
        let name = args.get("name").and_then(Value::as_str).filter(|n| valid_panel_name(n)).ok_or_else(|| err("`name` must be lowercase letters, digits and dashes (e.g. hdr-tuning)"))?;
        let path = dir.join(format!("{name}.json"));
        if remove {
            let existed = tokio::fs::remove_file(&path).await.is_ok();
            return Ok(text_result(json!({ "ok": true, "removed": existed, "bundleId": bundle, "name": name }), false));
        }
        let controls = controls.and_then(Value::as_array).filter(|c| !c.is_empty()).ok_or_else(|| err("`controls` must be a non-empty array"))?;
        for (i, c) in controls.iter().enumerate() {
            let c = c.as_object().ok_or_else(|| err(format!("controls[{i}] must be an object")))?;
            check_control(i, c)?;
        }
        let mut panel = json!({ "name": name, "title": args.get("title").and_then(Value::as_str).filter(|t| !t.is_empty()).unwrap_or(name),
            "bundleId": bundle, "updatedAt": iso_now(), "controls": controls });
        if let Some(note) = args.get("note").and_then(Value::as_str).filter(|n| !n.is_empty()) {
            panel["note"] = json!(note);
        }
        let tmp = path.with_extension(format!("json.tmp-{}", crate::auth::new_token()));
        let text = serde_json::to_string_pretty(&panel).unwrap_or_default();
        let written = async {
            tokio::fs::create_dir_all(&dir).await?;
            tokio::fs::write(&tmp, text).await?;
            tokio::fs::rename(&tmp, &path).await
        };
        written.await.map_err(|e| err(e.to_string()))?;
        Ok(text_result(json!({ "ok": true, "bundleId": bundle, "name": name, "controls": controls.len(), "path": path.display().to_string(),
            "hint": "the user opens it in the Apparition Store: Debug › the app › Panels (it refreshes on its own)" }), false))
    }
}

/// Keys of a JSON object in file order, ignoring the values.
struct Keys(Vec<String>);

impl<'de> serde::Deserialize<'de> for Keys {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Keys, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = Keys;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result { f.write_str("an object") }
            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Keys, A::Error> {
                let mut keys = Vec::new();
                while let Some((k, _)) = map.next_entry::<String, serde::de::IgnoredAny>()? {
                    keys.push(k);
                }
                Ok(Keys(keys))
            }
        }
        d.deserialize_map(V)
    }
}

fn parameter_order() -> std::collections::HashMap<String, Vec<String>> {
    #[derive(serde::Deserialize)]
    struct Schema { properties: Keys }
    #[derive(serde::Deserialize)]
    struct Tool { name: String, #[serde(rename = "inputSchema")] schema: Schema }
    #[derive(serde::Deserialize)]
    struct File { tools: Vec<Tool> }
    let file: File = serde_json::from_str(TOOLS_JSON).expect("mcp_tools.json");
    file.tools.into_iter().map(|t| (t.name, t.schema.properties.0)).collect()
}

fn valid_panel_name(n: &str) -> bool {
    let mut c = n.chars();
    !n.is_empty() && n.len() <= 48 && c.next().is_some_and(|f| f.is_ascii_lowercase() || f.is_ascii_digit()) && c.all(|x| x.is_ascii_lowercase() || x.is_ascii_digit() || x == '-')
}

fn check_control(index: usize, c: &Map<String, Value>) -> Result<(), ToolError> {
    let at = format!("controls[{index}]");
    let kind = c.get("kind").and_then(Value::as_str).unwrap_or("");
    if !CONTROL_KINDS.contains(&kind) {
        let mut kinds = CONTROL_KINDS.to_vec();
        kinds.sort();
        return Err(err(format!("{at}.kind must be one of {}", kinds.join(", "))));
    }
    let truthy = |k: &str| c.get(k).is_some_and(|v| !v.is_null() && v.as_str() != Some("") && v.as_bool() != Some(false));
    let (tunable, endpoint) = (truthy("tunable"), truthy("endpoint"));
    if tunable == endpoint {
        return Err(err_hint(format!("{at} needs exactly one of `tunable` or `endpoint`"), "a tunable is a DebugTunables key from `_tunables`; anything else goes through an endpoint"));
    }
    if tunable && (kind == "button" || kind == "readout") {
        return Err(err(format!("{at}: a {kind} drives an endpoint, not a tunable")));
    }
    if endpoint && ["slider", "toggle", "choice"].contains(&kind) && !truthy("argument") {
        return Err(err(format!("{at}: a {kind} on an endpoint needs `argument`, the name its value is sent as")));
    }
    if endpoint && kind == "slider" && !["min", "max"].iter().all(|k| c.get(*k).is_some_and(Value::is_number)) {
        return Err(err(format!("{at}: a slider on an endpoint needs numeric `min` and `max`")));
    }
    if endpoint && kind == "choice" && !c.get("choices").is_some_and(|v| v.as_array().is_some_and(|a| !a.is_empty())) {
        return Err(err(format!("{at}: a choice on an endpoint needs `choices`")));
    }
    if c.get("arguments").is_some_and(|a| !a.is_object()) {
        return Err(err(format!("{at}.arguments must be an object")));
    }
    Ok(())
}

fn quote(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"_.-~/".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

fn edit_distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut row = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            row.push((prev[j] + usize::from(ca != cb)).min(prev[j + 1] + 1).min(row[j] + 1));
        }
        prev = row;
    }
    prev[b.len()]
}

fn civil(ms: u64) -> (i64, u32, u32, u64, u64, u64) {
    let secs = (ms / 1000) as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400) as u64;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day, rem / 3600, rem % 3600 / 60, rem % 60)
}

fn stamp() -> String {
    let (y, mo, d, h, mi, s) = civil(crate::auth::system_ms());
    format!("{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}")
}

fn iso_now() -> String {
    let (y, mo, d, h, mi, s) = civil(crate::auth::system_ms());
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}+0000")
}
