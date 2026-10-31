//! One-shot developer patching. Compilation and authorization stay in DebugTrace;
//! this CLI owns the debugger for exactly the duration of the operation.
use crate::{Error, api, fail, find_device, store};
use std::path::PathBuf;

pub async fn run(args: &[String]) -> Result<(), Error> {
    let usage = || fail("usage: apparition inject <bundle-id> --device <name or UDID> [--file PATH]... [--run SWIFT]");
    if args.first().is_some_and(|s| s == "--help" || s == "-h") {
        println!("apparition inject <bundle-id> --device <name or UDID> [--file PATH]... [--run SWIFT]\nCompiles signed patches on the build Mac; device approval is still required.");
        return Ok(());
    }
    if !cfg!(target_os = "macos") { return Err(fail("Run inject on the Mac that built this app; compilation requires Xcode and its captured build products")); }
    let (mut bundle, mut device_name, mut patch_args) = (None, None, Vec::new());
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--device" => device_name = Some(args.next().ok_or_else(usage)?.clone()),
            "--file" | "--run" => { patch_args.push(arg.clone()); patch_args.push(args.next().ok_or_else(usage)?.clone()); }
            name if !name.starts_with('-') && bundle.is_none() => bundle = Some(name.to_owned()),
            _ => return Err(usage()),
        }
    }
    let bundle = bundle.ok_or_else(usage)?;
    let device = find_device(device_name.as_deref())?;
    let tool = std::env::var_os("DEBUGTRACE_MCP").map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Projects/DebugTrace/Tools/debugtrace-mcp")))
        .filter(|p| p.is_file()).ok_or_else(|| fail("Set DEBUGTRACE_MCP to DebugTrace/Tools/debugtrace-mcp on the build Mac"))?;
    let app = format!("{bundle}@{}", device.name);
    let status = tokio::time::timeout(std::time::Duration::from_secs(20),
        tokio::process::Command::new("python3").arg(&tool).args(["inject-status", "--app", &app]).kill_on_drop(true).output())
        .await.map_err(|_| fail("Timed out verifying the live DebugTrace session; allow the client on the device and retry"))??;
    if !status.status.success() { return Err(fail(format!("Could not verify the live-patching session: {}", String::from_utf8_lossy(&status.stdout)))); }
    let status: serde_json::Value = serde_json::from_slice(&status.stdout).map_err(|_| fail("Invalid DebugTrace session status"))?;
    if status["backend"] == "dynamic" {
        let result = tokio::process::Command::new("python3").arg(&tool).args(["inject", "--app", &app]).args(&patch_args).kill_on_drop(true).status().await?;
        return if result.success() { Ok(()) } else { Err(fail("Patch failed; see its hint")) };
    }
    let pid = status["pid"].as_u64().filter(|p| *p > 0).ok_or_else(|| fail("The live app did not report a valid process ID"))?;
    eprintln!("Connecting to {} to patch {bundle} (pid {pid})…", device.name);
    let mut tunnel = tokio::time::timeout(std::time::Duration::from_secs(25), store().connect(&device, || crate::ask("Pairing PIN: ")))
        .await.map_err(|_| fail("Timed out connecting to the device; check pairing and Local Network access"))??;
    let key = pid.to_string();
    let stop_key = key.clone();
    let worker = async {
        let result = tokio::process::Command::new("python3").arg(tool).arg("inject")
            .arg("--app").arg(format!("{bundle}@{}", device.name)).args(patch_args)
            .env("DEBUGTRACE_WAIT_FOR_DEBUGGER", "30").kill_on_drop(true).status().await;
        api::stop_jit(&stop_key);
        result
    };
    let debugger = async {
        tunnel.serve_jit(api::JitTarget::Attach { pid, bring_back: None }, 60, &mut |line| eprintln!("{line}")).await
    };
    // poll both futures: the worker waits until _patches confirms debugger attachment.
    let (result, debug_result) = tokio::join!(worker, debugger);
    debug_result?;
    if result?.success() { Ok(()) } else { Err(fail("Patch failed; see the reported hint. The debugger detached.")) }
}
