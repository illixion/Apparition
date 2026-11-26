//! The device tools from the command line: what runs on a device (and ending it), a simulated
//! location, and the device's log. The same calls Apparition's Tools tab makes on the device,
//! over this computer's own pairing (see `apparition_core::tools`).

use crate::{Error, api, ask, fail, find_device, store};
use apparition_core::tools::{Coordinate, Level, LogEntry, Process, ProcessKind};
use std::io::Write;

/// `processes [--device X] [--apps] [--json]`: the device's running processes, apps first.
pub async fn processes(args: &[String]) -> Result<(), Error> {
    let usage = || fail("usage: apparition processes [--device <name or UDID>] [--apps] [--json]");
    let (mut wanted, mut apps_only, mut json) = (None, false, false);
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--device" => wanted = Some(args.next().ok_or_else(usage)?.as_str()),
            "--apps" => apps_only = true,
            "--json" => json = true,
            _ => return Err(usage()),
        }
    }
    let device = find_device(wanted)?;
    let mut tunnel = store().connect_healing(&device, String::new, &crate::say).await?;
    let mut list = tunnel.processes().await?;
    if apps_only {
        list.retain(|p| matches!(p.kind, ProcessKind::App | ProcessKind::SystemApp));
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&list)?);
        return Ok(());
    }
    for process in &list {
        println!("{:>7}  {:<10}  {}", process.pid, kind_label(process.kind), label(process));
    }
    println!("{} processes on {}.", list.len(), device.name);
    Ok(())
}

fn kind_label(kind: ProcessKind) -> &'static str {
    match kind {
        ProcessKind::App => "app",
        ProcessKind::SystemApp => "system app",
        ProcessKind::Extension => "extension",
        ProcessKind::System => "system",
    }
}

/// "UTM (com.utmapp.UTM)", "Widget, from UTM", or a daemon's executable name.
fn label(process: &Process) -> String {
    match (&process.app_name, &process.bundle_id, process.kind) {
        (Some(app), _, ProcessKind::Extension) => format!("{}, from {app}", process.name),
        (Some(app), Some(id), _) => format!("{app} ({id})"),
        _ => process.name.clone(),
    }
}

/// `kill <pid or bundle ID> [--device X] [--term]`: ends a process (SIGKILL), or asks it to end
/// (SIGTERM) with --term. A bundle ID ends that app's own process, not its extensions.
pub async fn kill(args: &[String]) -> Result<(), Error> {
    let usage = || fail("usage: apparition kill <pid or bundle ID> [--device <name or UDID>] [--term]");
    let (mut target, mut wanted, mut term) = (None, None, false);
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--device" => wanted = Some(args.next().ok_or_else(usage)?.as_str()),
            "--term" => term = true,
            other if !other.starts_with('-') && target.is_none() => target = Some(other.to_string()),
            _ => return Err(usage()),
        }
    }
    let target = target.ok_or_else(usage)?;
    let device = find_device(wanted)?;
    let mut tunnel = store().connect_healing(&device, String::new, &crate::say).await?;
    let (pid, name) = match target.parse::<u32>() {
        Ok(pid) => (pid, format!("process {pid}")),
        Err(_) => {
            let list = tunnel.processes().await?;
            let process = list
                .iter()
                .find(|p| p.bundle_id.as_deref() == Some(target.as_str()) && p.kind != ProcessKind::Extension)
                .ok_or_else(|| fail(format!("{target} isn't running on {}", device.name)))?;
            (process.pid, format!("{} (pid {})", label(process), process.pid))
        }
    };
    tunnel.signal_process(pid, if term { 15 } else { 9 }).await?;
    println!("{} {name} on {}.", if term { "Asked to end" } else { "Ended" }, device.name);
    Ok(())
}

/// `location <latitude> <longitude> [--device X]`: simulates that place for the whole device
/// while this runs; Ctrl-C puts the real location back.
pub async fn location(args: &[String]) -> Result<(), Error> {
    let usage = || fail("usage: apparition location <latitude> <longitude> [--device <name or UDID>]");
    let (mut numbers, mut wanted) = (Vec::new(), None);
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--device" => wanted = Some(args.next().ok_or_else(usage)?.as_str()),
            // A negative coordinate looks like a flag: anything that reads as a number is one.
            other => numbers.push(other.trim_end_matches(',').parse::<f64>().map_err(|_| usage())?),
        }
    }
    let [latitude, longitude] = numbers[..] else { return Err(usage()) };
    let at = Coordinate { latitude, longitude };
    at.check()?;
    let device = find_device(wanted)?;
    println!("Connecting to {}…", device.name);
    let pin = || ask("The device wants to pair again. PIN: ");
    let mut tunnel = store().connect_for_developer(&device, pin, &crate::say, &crate::ddi_consent).await?;
    const KEY: &str = "cli-location";
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            api::stop_tool(KEY);
        }
    });
    let reply = tunnel
        .simulate_location(KEY, at, &mut |set| {
            println!("{} is at {:.6}, {:.6} now. Leave this running; Ctrl-C puts its real location back.", device.name, set.latitude, set.longitude)
        })
        .await?;
    match reply["ended"].as_str() {
        Some("lost") => Err(fail(format!("The connection to {} dropped, so it's back at its real location.", device.name))),
        _ => {
            println!("{} is back at its real location.", device.name);
            Ok(())
        }
    }
}

/// `logs [--app <bundle ID> | --pid <pid>] [--level debug|info|default|error] [--device X]
/// [--json]`: the device's log as it's written, one process's or everything, until Ctrl-C.
/// Like `log stream`, it leaves out info and debug lines unless --level asks for them.
pub async fn logs(args: &[String]) -> Result<(), Error> {
    let usage = || fail("usage: apparition logs [--app <bundle ID> | --pid <pid>] [--level debug|info|default|error] [--device <name or UDID>] [--json]");
    let (mut app, mut pid, mut wanted, mut json, mut minimum) = (None, None, None, false, Level::Notice);
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--app" => app = Some(args.next().ok_or_else(usage)?.clone()),
            "--pid" => pid = Some(args.next().and_then(|a| a.parse::<u32>().ok()).ok_or_else(usage)?),
            "--device" => wanted = Some(args.next().ok_or_else(usage)?.as_str()),
            "--json" => json = true,
            "--level" => minimum = args.next().and_then(|a| Level::parse(a)).ok_or_else(usage)?,
            _ => return Err(usage()),
        }
    }
    if app.is_some() && pid.is_some() {
        return Err(usage());
    }
    let device = find_device(wanted)?;
    let pin = || ask("The device wants to pair again. PIN: ");
    let mut tunnel = store().connect_healing(&device, pin, &crate::say).await?;
    if let Some(app) = &app {
        let found = tunnel.running_pid(app).await?;
        pid = Some(found.ok_or_else(|| fail(format!("{app} isn't running on {}; open it first", device.name)))? as u32);
    }
    const KEY: &str = "cli-log";
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            api::stop_tool(KEY);
        }
    });
    if !json {
        eprintln!("{} log from {}; Ctrl-C stops.", if pid.is_some() { "The app's" } else { "The whole" }, device.name);
    }
    let reply = tunnel
        .stream_log(KEY, pid, minimum, &mut |entries| {
            let mut out = std::io::stdout().lock();
            for entry in entries {
                let line = if json { serde_json::to_string(&entry).unwrap_or_default() } else { line(&entry) };
                let _ = writeln!(out, "{line}");
            }
            let _ = out.flush();
        })
        .await?;
    if reply["ended"].as_str() == Some("lost") {
        return Err(fail(format!("{} ended the log stream (the process may have exited).", device.name)));
    }
    Ok(())
}

/// One log line the way `log stream` prints it, in this computer's time zone.
fn line(entry: &LogEntry) -> String {
    let time = chrono::DateTime::from_timestamp_millis(entry.time)
        .map(|t| t.with_timezone(&chrono::Local).format("%H:%M:%S%.3f").to_string())
        .unwrap_or_default();
    let label = match (&entry.subsystem, &entry.category) {
        (Some(subsystem), Some(category)) => format!(" [{subsystem}:{category}]"),
        (Some(subsystem), None) => format!(" [{subsystem}]"),
        _ => String::new(),
    };
    format!("{time} {}[{}] <{}>{label} {}", entry.process, entry.pid, entry.level, entry.message)
}
