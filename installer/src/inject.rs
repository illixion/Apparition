//! One-shot developer patching. Compilation and authorization stay in DebugTrace;
//! this CLI owns the debugger for exactly the duration of the operation.
//!
//! It is the one entry point that gets a patch onto the device whatever state the app is in:
//! - running a patchable build: the patch goes straight to the app's debug server (the dynamic
//!   backend), or with this Mac attached as its debugger for the call (the Apparition backend);
//! - not running, or running a build without live patching: the app is rebuilt with `--inject`
//!   from its last launch record (same project, device, platform, backend and JIT) and relaunched,
//!   which builds the edits in, then any `--run` snippet runs against it. `--no-launch` refuses
//!   instead, for callers that promise never to build (the Store's patch route).
//!
//! The `inject` MCP tools hand off to this command when they can't patch directly, and set
//! `DEBUGTRACE_NO_DELEGATE` on the DebugTrace calls made from here so nothing loops.
use crate::{Error, api, fail, find_device, store};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;

const USAGE: &str = "apparition inject [<app>] [--device <name or UDID>] [--file PATH]... [--run SWIFT] [--allow-view-type-change] [--no-launch]";

/// Failures `debugtrace-mcp inject-status` reports that a rebuild and relaunch cure.
const RELAUNCH_REASONS: [&str; 2] = ["not-running", "not-patchable"];

struct Args {
    app: Option<String>,
    device: Option<String>,
    patch: Vec<String>,
    run: bool,
    no_launch: bool,
}

fn parse(args: &[String]) -> Result<Option<Args>, Error> {
    let usage = || fail(format!("usage: {USAGE}"));
    let mut out = Args { app: None, device: None, patch: Vec::new(), run: false, no_launch: false };
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => return Ok(None),
            "--device" => out.device = Some(args.next().ok_or_else(usage)?.clone()),
            "--file" | "--run" => {
                out.run |= arg == "--run";
                out.patch.push(arg.clone());
                out.patch.push(args.next().ok_or_else(usage)?.clone());
            }
            "--allow-view-type-change" => out.patch.push(arg.clone()),
            "--no-launch" => out.no_launch = true,
            name if !name.starts_with('-') && out.app.is_none() => out.app = Some(name.to_owned()),
            _ => return Err(usage()),
        }
    }
    // "Hypnos@AVP", as the MCP tools address an app.
    if out.device.is_none()
        && let Some((app, device)) = out.app.clone().and_then(|a| a.rsplit_once('@').map(|(a, d)| (a.to_owned(), d.to_owned()))) {
        out.app = Some(app);
        out.device = Some(device);
    }
    Ok(Some(out))
}

/// Where `apparition build --run --mcp` writes its launch records (`<bundle>@<device>.json`).
fn sessions_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("DEBUGTRACE_STATE_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(dir).join("sessions");
    }
    let state = std::env::var_os("XDG_STATE_HOME").filter(|d| !d.is_empty()).map(PathBuf::from)
        .unwrap_or_else(|| crate::builder::home().join(".local/state"));
    state.join("debugtrace/sessions")
}

fn records(dir: &Path) -> Vec<Value> {
    let mut out: Vec<Value> = std::fs::read_dir(dir).into_iter().flatten().flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| std::fs::read(e.path()).ok())
        .filter_map(|b| serde_json::from_slice(&b).ok())
        .collect();
    // Oldest first, so the last match is the latest launch ("startedAt" is a sortable UTC stamp).
    out.sort_by(|a, b| a["startedAt"].as_str().unwrap_or("").cmp(b["startedAt"].as_str().unwrap_or("")));
    out
}

/// The latest launch record for `app` (bundle id, display name or the bundle id's last part, as
/// the MCP tools match) on `device`; with no app, the latest one launched for live patching.
fn latest_record(records: &[Value], app: Option<&str>, device: Option<&str>) -> Option<Value> {
    let s = |r: &Value, k: &str| r[k].as_str().unwrap_or("").to_lowercase();
    records.iter().rev().find(|r| {
        let on_device = device.is_none_or(|d| s(r, "device") == d.to_lowercase() || s(r, "deviceId") == d.to_lowercase());
        let named = match app {
            Some(app) => {
                let (wanted, bundle) = (app.to_lowercase(), s(r, "bundleId"));
                wanted == bundle || wanted == s(r, "appName") || Some(wanted.as_str()) == bundle.rsplit('.').next()
            }
            None => r["injectDir"].as_str().is_some_and(|d| !d.is_empty()),
        };
        on_device && named
    }).cloned()
}

fn debugtrace_tool() -> Result<PathBuf, Error> {
    std::env::var_os("DEBUGTRACE_MCP").map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Projects/DebugTrace/Tools/debugtrace-mcp")))
        .filter(|p| p.is_file()).ok_or_else(|| fail("Set DEBUGTRACE_MCP to DebugTrace/Tools/debugtrace-mcp on the build Mac"))
}

fn debugtrace(tool: &Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new("python3");
    command.arg(tool).env("DEBUGTRACE_NO_DELEGATE", "1").kill_on_drop(true);
    command
}

/// `inject-status`: Ok(pid, backend) for a live patchable build, else Err(reason, message).
async fn status(tool: &Path, app: &str, wait: Duration) -> Result<Result<(u64, String), (String, String)>, Error> {
    let output = tokio::time::timeout(wait, debugtrace(tool).args(["inject-status", "--app", app]).output()).await
        .map_err(|_| fail("Timed out verifying the live DebugTrace session; allow the client on the device and retry"))??;
    let reply: Value = serde_json::from_slice(&output.stdout).map_err(|_| fail(format!(
        "Invalid DebugTrace session status: {}", String::from_utf8_lossy(&output.stdout).trim())))?;
    if output.status.success() {
        let backend = reply["backend"].as_str().unwrap_or("dynamic").to_string();
        // A dynamic patch needs no pid; the Apparition backend checks it before attaching.
        return Ok(Ok((reply["pid"].as_u64().unwrap_or(0), backend)));
    }
    let text = reply.pointer("/content/0/text").and_then(Value::as_str).unwrap_or("could not verify the live-patching session");
    Ok(Err((reply["reason"].as_str().unwrap_or("").to_string(), text.to_string())))
}

/// Rebuilds and relaunches the app as its last launch did, with live patching on. The build's
/// own output goes to stderr: stdout carries only the JSON result.
async fn relaunch(bundle: &str, device: &str, record: Option<&Value>) -> Result<(), Error> {
    let root = record.and_then(|r| r["projectRoot"].as_str()).filter(|r| !r.is_empty()).map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .ok_or_else(|| fail("No project to rebuild: run this from the app's repo"))?;
    let backend = record.and_then(|r| r["injectDir"].as_str()).filter(|d| !d.is_empty())
        .and_then(|d| std::fs::read_to_string(Path::new(d).join("modules.json")).ok())
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|m| m["backend"].as_str().map(String::from));
    let mut command = tokio::process::Command::new(std::env::current_exe()?);
    command.args(["build", "--run", "--mcp", "--inject", "--device", device]);
    if let Some(backend) = backend { command.args(["--inject-backend", &backend]); }
    if let Some(platform) = record.and_then(|r| r["platform"].as_str()).filter(|p| !p.is_empty()) { command.args(["--platform", platform]); }
    if record.is_some_and(|r| r["jit"] == json!(true)) { command.arg("--jit"); }
    eprintln!("{bundle} isn't running a patchable build on {device}: rebuilding it with live patching from {} and relaunching it…", root.display());
    let status = command.current_dir(&root)
        .stdin(std::process::Stdio::null()).stdout(std::io::stderr()).kill_on_drop(true).status().await?;
    if !status.success() {
        return Err(fail(format!("Rebuilding {bundle} for live patching failed (see the build output above); fix it, or run `apparition build --run --mcp --inject` in {}", root.display())));
    }
    Ok(())
}

/// A JSON tool result, as `debugtrace-mcp inject` prints one.
fn result(value: Value) -> Value {
    json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&value).unwrap_or_default() }] })
}

pub async fn run(args: &[String]) -> Result<(), Error> {
    let Some(args) = parse(args)? else {
        println!("{USAGE}\nPatches the running app with the Swift sources edited since its build. If it isn't running, or runs a build\nwithout live patching, rebuilds it with --inject (as its last launch did) and relaunches it first, unless --no-launch.\nCompiles signed patches on the build Mac; the device still asks to allow each patch.");
        return Ok(());
    };
    if !cfg!(target_os = "macos") { return Err(fail("Run inject on the Mac that built this app; compilation requires Xcode and its captured build products")); }
    let tool = debugtrace_tool()?;
    let records = records(&sessions_dir());
    let record = latest_record(&records, args.app.as_deref(), args.device.as_deref());
    let bundle = record.as_ref().and_then(|r| r["bundleId"].as_str().map(String::from))
        .or_else(|| args.app.clone().filter(|a| a.contains('.')))
        .ok_or_else(|| fail(match &args.app {
            Some(app) => format!("No launch of {app} on record: pass its bundle ID, or launch it once with `apparition build --run --mcp --inject`"),
            None => "No app launched for live patching on record: name one, or launch it with `apparition build --run --mcp --inject`".into(),
        }))?;
    let device = find_device(args.device.as_deref().or(record.as_ref().and_then(|r| r["device"].as_str())))?;
    let app = format!("{bundle}@{}", device.name);

    let mut relaunched = false;
    let (pid, backend) = match status(&tool, &app, Duration::from_secs(20)).await? {
        Ok(live) => live,
        Err((reason, message)) if RELAUNCH_REASONS.contains(&reason.as_str()) && !args.no_launch => {
            relaunch(&bundle, &device.name, record.as_ref()).await?;
            relaunched = true;
            // The relaunch built the edits in; a new client may need allowing on the device.
            status(&tool, &app, Duration::from_secs(60)).await?
                .map_err(|(_, after)| fail(format!("Relaunched {bundle}, but its live-patching session still isn't usable: {after}\n(before: {message})")))?
        }
        Err((_, message)) => return Err(fail(format!("Could not verify the live-patching session: {message}"))),
    };
    let relaunch_note = "the app wasn't running a patchable build, so it was rebuilt with live patching and relaunched: your edits are built in, and the app's state started over";
    if relaunched && !args.run {
        println!("{}", result(json!({ "ok": true, "relaunched": true, "applied": [], "note": relaunch_note })));
        return Ok(());
    }

    let output = if backend == "dynamic" {
        debugtrace(&tool).args(["inject", "--app", &app]).args(&args.patch).stderr(std::process::Stdio::inherit()).output().await?
    } else {
        // The Apparition backend: this Mac is the app's debugger while the patch loads.
        if pid == 0 { return Err(fail("The live app did not report a valid process ID")); }
        eprintln!("Connecting to {} to patch {bundle} (pid {pid})…", device.name);
        let mut tunnel = tokio::time::timeout(Duration::from_secs(25), store().connect(&device, || crate::ask("Pairing PIN: ")))
            .await.map_err(|_| fail("Timed out connecting to the device; check pairing and Local Network access"))??;
        let key = pid.to_string();
        let worker = async {
            let output = debugtrace(&tool).args(["inject", "--app", &app]).args(&args.patch)
                .env("DEBUGTRACE_WAIT_FOR_DEBUGGER", "30").stderr(std::process::Stdio::inherit()).output().await;
            api::stop_jit(&key);
            output
        };
        let debugger = async {
            tunnel.serve_jit(api::JitTarget::Attach { pid, bring_back: None }, 60, &mut |line| eprintln!("{line}")).await
        };
        // Poll both futures: the worker waits until _patches confirms debugger attachment.
        let (output, debug_result) = tokio::join!(worker, debugger);
        debug_result?;
        output?
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let mut reply: Value = serde_json::from_str(text.trim()).map_err(|_| fail(format!("Patch failed: {}", text.trim())))?;
    if relaunched && let Some(content) = reply["content"].as_array_mut() {
        content.push(json!({ "type": "text", "text": format!("note: {relaunch_note}") }));
    }
    println!("{reply}");
    if output.status.success() { Ok(()) } else { Err(fail("Patch failed; see its hint")) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn an_app_at_a_device_names_both() {
        let a = parse(&args(&["Hypnos@AVP", "--file", "A.swift", "--allow-view-type-change"])).unwrap().unwrap();
        assert_eq!((a.app.as_deref(), a.device.as_deref()), (Some("Hypnos"), Some("AVP")));
        assert_eq!(a.patch, args(&["--file", "A.swift", "--allow-view-type-change"]));
        assert!(!a.run && !a.no_launch);
        let a = parse(&args(&["--no-launch", "--run", "return 1"])).unwrap().unwrap();
        assert!(a.app.is_none() && a.run && a.no_launch);
        assert!(parse(&args(&["--help"])).unwrap().is_none());
        assert!(parse(&args(&["--bogus"])).is_err());
    }

    #[test]
    fn the_latest_matching_launch_record_wins() {
        let records = vec![
            json!({ "bundleId": "com.illixion.hypnos", "appName": "Convolution", "device": "AVP", "startedAt": "2026-11-01T10:00:00Z", "injectDir": "/a" }),
            json!({ "bundleId": "com.illixion.hypnos", "appName": "Convolution", "device": "ixPhone", "startedAt": "2026-11-02T10:00:00Z" }),
            json!({ "bundleId": "com.example.other", "appName": "Other", "device": "AVP", "startedAt": "2026-11-03T10:00:00Z" }),
        ];
        let device = |r: Option<Value>| r.map(|r| r["device"].as_str().unwrap().to_string());
        assert_eq!(device(latest_record(&records, Some("hypnos"), None)), Some("ixPhone".into()));
        assert_eq!(device(latest_record(&records, Some("Convolution"), Some("avp"))), Some("AVP".into()));
        // No app named: the latest launch that can be patched.
        assert_eq!(latest_record(&records, None, None).unwrap()["bundleId"], "com.illixion.hypnos");
        assert!(latest_record(&records, Some("nope"), None).is_none());
    }
}
