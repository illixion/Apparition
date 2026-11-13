//! `apparition crash-reports`: what the Apparition Store holds about crashes, the crash logs pulled
//! off the devices and the debug traces apps upload (the port of `store/bin/crash-reports`). Look
//! here before asking the user or rebuilding with extra logging.
//!
//! It reads the Store's data folder directly (`STORE_DATA`, default `<Store folder>/data`), so it
//! needs no login and works while the Store is stopped. The layout is the service's:
//! `crashlogs/index.json` (`entries`, each with `id`, `file`, `symbolicated`, `traceId`, ...) and
//! `traces/<id>/meta.json`. `done` moves files to the Trash, never deletes: the Store forgets a
//! crash log whose files are gone.
//!
//! `add` is the one command that writes a trace: it opens an encrypted problem report (the
//! `.trace` file a public copy's Report a Problem makes) with this Mac's report key and stores
//! the trace inside, as dropping the file on the Store's page does. `import-dsyms` puts a
//! release's dSYMs where the symbolicator looks, so those reports' crashes resolve.

use std::{
    io::Write,
    path::{Component, Path, PathBuf},
    process::Command,
};

use rootcause::prelude::*;
use serde_json::Value;

pub const USAGE: &str = "usage: apparition crash-reports list [--app TEXT] [--device NAME]   crash logs, then traces, newest first
       apparition crash-reports show <id>                           a crash log's symbolicated stack, or a trace's MetricKit reports
       apparition crash-reports done <id>... [--dry-run]            investigated and fixed: move it to the Trash
       apparition crash-reports add <file.trace.zip or .trace>...   open an encrypted problem report and keep its trace
       apparition crash-reports import-dsyms <dSYMs.zip>...         keep a release's symbols, so its crash reports symbolicate
       an id is any unambiguous prefix of what `list` prints; STORE_DATA names another data folder";

/// Where the Store keeps its data, and where the Trash is.
struct Folders {
    data: PathBuf,
    config: PathBuf,
    /// The key ledger `apparition build` writes, and the keys that open encrypted reports.
    ledger: PathBuf,
    report_keys: PathBuf,
    /// Overrides the platform's Trash (tests, mostly).
    trash: Option<Trash>,
}

impl Folders {
    fn from_environment() -> Folders {
        let store = apparition_core::api::store_dir();
        let default_data = store.join("data");
        let data = std::env::var_os("STORE_DATA").map(PathBuf::from).unwrap_or_else(|| default_data.clone());
        let config = std::env::var_os("STORE_CONFIG").map(PathBuf::from).unwrap_or_else(|| {
            if data == default_data { store.join("config.json") } else { data.parent().unwrap_or(&data).join("config.json") }
        });
        Folders {
            data,
            config,
            ledger: apparition_service::traces::default_key_dir(),
            report_keys: apparition_service::reports::default_key_dir(),
            trash: None,
        }
    }

    fn crash_dir(&self) -> PathBuf {
        self.data.join("crashlogs")
    }

    fn trace_dir(&self) -> PathBuf {
        self.data.join("traces")
    }

    fn crash_logs(&self) -> Vec<Value> {
        let dir = self.crash_dir();
        let mut entries: Vec<Value> = read_json(&dir.join("index.json"))
            .and_then(|index| index["entries"].as_array().cloned())
            .unwrap_or_default()
            .into_iter()
            .filter(|e| {
                e["id"].as_str().is_some()
                    && e["file"].as_str().is_some_and(|f| relative(f) && dir.join(f).exists())
                    && e["symbolicated"].as_str().is_none_or(relative)
            })
            .collect();
        entries.sort_by(|a, b| number(&b["capturedAt"]).total_cmp(&number(&a["capturedAt"])));
        entries
    }

    fn traces(&self) -> Vec<Value> {
        let dir = self.trace_dir();
        let mut traces: Vec<Value> = std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| read_json(&e.path().join("meta.json")))
            .filter(|meta| meta["id"].as_str().is_some_and(|id| relative(id) && !id.contains(['/', '\\'])))
            .collect();
        traces.sort_by(|a, b| number(&b["receivedAt"]).total_cmp(&number(&a["receivedAt"])));
        traces
    }
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// A path from the index or a meta file stays inside its folder: no root, no `..`.
fn relative(path: &str) -> bool {
    !path.is_empty() && Path::new(path).components().all(|c| matches!(c, Component::Normal(_)))
}

fn number(v: &Value) -> f64 {
    v.as_f64().unwrap_or(0.0)
}

/// A non-empty string, the way JavaScript's `||` chain treats it.
fn truthy(v: &Value) -> Option<&str> {
    v.as_str().filter(|s| !s.is_empty())
}

/// `String(v)` as JavaScript would print a possibly missing value.
fn js_string(v: &Value, missing: &str) -> String {
    match v {
        Value::Null => missing.to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn str_of<'a>(v: &'a Value) -> &'a str {
    v.as_str().unwrap_or("")
}

// MARK: Times

/// `1970-01-01 00:00` civil date and time of `ms` since the epoch in the zone `offset_seconds` east of UTC.
fn civil(ms: i64, offset_seconds: i64) -> (i64, i64, i64, i64, i64, i64) {
    let secs = ms.div_euclid(1000) + offset_seconds;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day, rem / 3600, rem % 3600 / 60, rem % 60)
}

/// Seconds east of UTC that apply at `ms`, from the computer's time zone (UTC where unknown).
fn local_offset(ms: i64) -> i64 {
    #[cfg(unix)]
    {
        let t = ms.div_euclid(1000) as libc::time_t;
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() { 0 } else { tm.tm_gmtoff as i64 }
    }
    #[cfg(not(unix))]
    {
        let _ = ms;
        0
    }
}

/// `2026-10-09 10:00` in local time, as `toLocaleString('sv-SE')` prints it.
fn when(ms: f64) -> String {
    let ms = ms as i64;
    let (y, mo, d, h, mi, _) = civil(ms, local_offset(ms));
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}")
}

// MARK: Commands

/// One of `list`, `show`, `done`, `add` and `import-dsyms`. Returns the exit code to leave with, which
/// is the symbolicator's own for `show` and `import-dsyms`.
fn execute(folders: &Folders, args: &[String], out: &mut dyn Write) -> Result<i32, String> {
    match args.first().map(String::as_str) {
        Some("list") => list(folders, &args[1..], out).map(|()| 0),
        Some("show") => show(folders, args.get(1).map(String::as_str), out),
        Some("done") => done(folders, &args[1..], out).map(|()| 0),
        Some("add") => add(folders, &args[1..], out).map(|()| 0),
        Some("import-dsyms") => import_dsyms(folders, &args[1..], out),
        _ => Err(USAGE.to_string()),
    }
}

fn write_line(out: &mut dyn Write, line: String) -> Result<(), String> {
    match writeln!(out, "{line}") {
        // `| head` closed the pipe: stop quietly, as the script did.
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        other => other.map_err(|e| e.to_string()),
    }
}

fn list(folders: &Folders, args: &[String], out: &mut dyn Write) -> Result<(), String> {
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let app = opt("--app").unwrap_or_default().to_lowercase();
    let device = opt("--device");
    let matches_app = |name: &Value, bundle: &Value| {
        app.is_empty() || format!("{} {}", js_string(name, "undefined"), js_string(bundle, "undefined")).to_lowercase().contains(&app)
    };

    let logs: Vec<Value> = folders
        .crash_logs()
        .into_iter()
        .filter(|e| matches_app(&e["appName"], &e["bundleId"]) && device.as_deref().is_none_or(|d| e["device"].as_str() == Some(d)))
        .collect();
    write_line(out, format!("Crash logs from devices ({}):", logs.len()))?;
    for e in &logs {
        let name = truthy(&e["appName"]).or(truthy(&e["bundleId"])).unwrap_or("");
        let what = truthy(&e["exception"]).or(truthy(&e["termination"])).unwrap_or("");
        let trace = truthy(&e["traceId"]).map(|t| format!("  trace {}", head(t))).unwrap_or_default();
        write_line(
            out,
            format!(
                "  {}  {}  {:<8} {:<16} {:<14} {what}{trace}",
                head(str_of(&e["id"])),
                when(number(&e["capturedAt"])),
                str_of(&e["device"]),
                name,
                js_string(&e["build"], "undefined"),
            ),
        )?;
    }

    let traces: Vec<Value> = folders
        .traces()
        .into_iter()
        .filter(|t| matches_app(&t["app"]["name"], &t["app"]["bundleId"]) && device.as_deref().is_none_or(|d| t["build"]["installedTo"].as_str() == Some(d)))
        .collect();
    write_line(out, format!("Traces ({}):", traces.len()))?;
    for t in &traces {
        let reports = t["files"].as_array().into_iter().flatten().filter(|f| str_of(&f["path"]).starts_with("crashes/")).count();
        let reports = if reports > 0 { format!("{reports} MetricKit report(s)  ") } else { String::new() };
        // A problem report has no device this Mac installed to. Unsigned, its app and build are
        // only what the sender's copy said.
        let report = t.get("report").filter(|r| r.is_object());
        let unsigned = if report.is_some_and(|r| r["signed"] != true) { "unsigned  " } else { "" };
        write_line(
            out,
            format!(
                "  {}  {}  {:<8} {:<16} {:<14} {unsigned}{reports}{}",
                head(str_of(&t["id"])),
                when(number(&t["receivedAt"])),
                if report.is_some() { "report" } else { truthy(&t["build"]["installedTo"]).unwrap_or("") },
                js_string(&t["app"]["name"], "undefined"),
                js_string(&t["app"]["build"], "undefined"),
                truthy(&t["note"]).unwrap_or(""),
            ),
        )?;
    }
    if logs.is_empty() && traces.is_empty() {
        write_line(out, "  nothing: no crash has been pulled or uploaded (the poller runs every few minutes; the Traces page has \"Pull now\")".into())?;
    }
    Ok(())
}

/// The first eight characters of an id, which is what `list` prints.
fn head(id: &str) -> String {
    id.chars().take(8).collect()
}

/// The one entry whose id starts with `prefix`; an error when several do.
fn pick<'a>(list: &'a [Value], prefix: &str, what: &str) -> Result<Option<&'a Value>, String> {
    let prefix_lower = prefix.to_lowercase();
    let hits: Vec<&Value> = list.iter().filter(|x| str_of(&x["id"]).starts_with(&prefix_lower)).collect();
    if hits.len() > 1 {
        return Err(format!("{prefix} matches {} {what}s; give more of the id", hits.len()));
    }
    Ok(hits.first().copied())
}

fn show(folders: &Folders, id: Option<&str>, out: &mut dyn Write) -> Result<i32, String> {
    let id = id.filter(|id| !id.is_empty()).ok_or("usage: apparition crash-reports show <id>")?;
    let logs = folders.crash_logs();
    if let Some(log) = pick(&logs, id, "crash log")? {
        let text = truthy(&log["symbolicated"]).map(|s| folders.crash_dir().join(s)).filter(|p| p.exists());
        if let Some(text) = text {
            out.write_all(&std::fs::read(&text).map_err(|e| format!("{}: {e}", text.display()))?).map_err(|e| e.to_string())?;
            return Ok(0);
        }
        let ips = folders.crash_dir().join(str_of(&log["file"]));
        return symbolicate(folders, &ips.to_string_lossy(), out);
    }
    let traces = folders.traces();
    let Some(trace) = pick(&traces, id, "trace")? else {
        return Err(format!("no crash log or trace starts with {id}"));
    };
    let trace_id = str_of(&trace["id"]);
    let files = folders.trace_dir().join(trace_id).join("files");
    if !files.join("crashes").exists() {
        return Err(format!("trace {trace_id} has no MetricKit reports; its files are in {}", files.display()));
    }
    symbolicate(folders, trace_id, out)
}

/// DebugTrace's `debugtrace-symbolicate`, the same tool the Store's Node `crash-reports` ran: named
/// by `symbolicateTool` in the Store's config.json, else next to `debugtraceTool`, else in the
/// DebugTrace checkout.
fn symbolicate_tool(folders: &Folders) -> Result<PathBuf, String> {
    let config = read_json(&folders.config).unwrap_or(Value::Null);
    let tool = truthy(&config["symbolicateTool"])
        .map(PathBuf::from)
        .or_else(|| truthy(&config["debugtraceTool"]).and_then(|t| Path::new(t).parent().map(|p| p.join("debugtrace-symbolicate"))))
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from).unwrap_or_default();
            home.join("Projects/DebugTrace/Tools/debugtrace-symbolicate")
        });
    if tool.exists() { Ok(tool) } else { Err(format!("debugtrace-symbolicate not found at {}; set symbolicateTool in config.json", tool.display())) }
}

/// Runs the symbolicator on `target` (an .ips path or a trace id) with its output on ours.
fn symbolicate(folders: &Folders, target: &str, out: &mut dyn Write) -> Result<i32, String> {
    let tool = symbolicate_tool(folders)?;
    out.flush().map_err(|e| e.to_string())?;
    let python = if cfg!(target_os = "macos") { "/usr/bin/python3" } else if cfg!(windows) { "python" } else { "python3" };
    let status = Command::new(python)
        .arg("-I")
        .arg(&tool)
        .arg(target)
        .env("STORE_DATA", &folders.data)
        .status()
        .map_err(|e| format!("could not run {python}: {e}"))?;
    Ok(status.code().unwrap_or(1))
}

/// Opens each encrypted report with the report key it names and stores the trace inside. The
/// same intake as the Store page's drop (`traces::receive_report`): everything is checked but the
/// signature, which a public copy cannot make.
fn add(folders: &Folders, args: &[String], out: &mut dyn Write) -> Result<(), String> {
    if args.is_empty() || args.iter().any(|a| a.starts_with("--")) {
        return Err("usage: apparition crash-reports add <file.trace.zip or file.trace>...".into());
    }
    let config = read_json(&folders.config).unwrap_or(Value::Null);
    let repo_for_bundle = |bundle: &str| -> Option<String> {
        config["repos"].as_array()?.iter().find(|r| {
            ["platforms", "macApps"].iter().filter_map(|k| r.get(k)?.as_array()).flatten().any(|p| p["bundleId"].as_str() == Some(bundle))
        })?["name"].as_str().map(String::from)
    };
    let dir = folders.trace_dir();
    for path in args {
        let sealed = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
        let upload = apparition_service::traces::Upload {
            dir: &dir, key_dir: &folders.ledger, now_ms: now_ms() as u64, via_web: false, uploader: None, repo_for_bundle: &repo_for_bundle,
        };
        let (status, meta) = apparition_service::traces::receive_report(&sealed, &upload, &folders.report_keys)
            .map_err(|apparition_service::traces::TraceError(_, message)| format!("{path}: {message}"))?;
        let app = &meta["app"];
        let what = format!(
            "{}{} ({})",
            truthy(&app["name"]).unwrap_or("an app"),
            truthy(&app["version"]).map(|v| format!(" {v}")).unwrap_or_default(),
            truthy(&app["build"]).unwrap_or("?")
        );
        let id = head(str_of(&meta["id"]));
        let line = if status == 200 {
            format!("already have {id}  {what}")
        } else if meta["report"]["signed"] == true {
            format!("added {id}  {what}, a build this Mac made")
        } else {
            format!("added {id}  {what}  unsigned: the app, build and device are what the sender's copy said")
        };
        write_line(out, line)?;
        write_line(out, format!("  files: {}", dir.join(str_of(&meta["id"])).join("files").display()))?;
    }
    Ok(())
}

/// Hands release dSYM zips to the symbolicator's `import-dsyms`, which keeps them in the symbol
/// archive it reads, indexed by binary UUID.
fn import_dsyms(folders: &Folders, args: &[String], out: &mut dyn Write) -> Result<i32, String> {
    if args.is_empty() {
        return Err("usage: apparition crash-reports import-dsyms <dSYMs.zip>...".into());
    }
    let tool = symbolicate_tool(folders)?;
    out.flush().map_err(|e| e.to_string())?;
    let python = if cfg!(target_os = "macos") { "/usr/bin/python3" } else if cfg!(windows) { "python" } else { "python3" };
    let status = Command::new(python).arg("-I").arg(&tool).arg("import-dsyms").args(args).status()
        .map_err(|e| format!("could not run {python}: {e}"))?;
    Ok(status.code().unwrap_or(1))
}

fn done(folders: &Folders, args: &[String], out: &mut dyn Write) -> Result<(), String> {
    let dry = args.iter().any(|a| a == "--dry-run");
    let ids: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();
    if ids.is_empty() {
        return Err("usage: apparition crash-reports done <id>... [--dry-run]".into());
    }
    let logs = folders.crash_logs();
    let traces = folders.traces();
    let (crash_dir, trace_dir) = (folders.crash_dir(), folders.trace_dir());
    let mut files: Vec<PathBuf> = Vec::new();
    let mut add = |file: PathBuf| {
        if !files.contains(&file) {
            files.push(file);
        }
    };
    let add_log = |add: &mut dyn FnMut(PathBuf), e: &Value| {
        add(crash_dir.join(str_of(&e["file"])));
        if let Some(s) = truthy(&e["symbolicated"]) {
            add(crash_dir.join(s));
        }
    };
    for id in ids {
        let log = pick(&logs, id, "crash log")?;
        let trace = if log.is_some() { None } else { pick(&traces, id, "trace")? };
        match (log, trace) {
            (Some(log), _) => {
                add_log(&mut add, log);
                if let Some(linked) = truthy(&log["traceId"]).and_then(|t| traces.iter().find(|x| x["id"].as_str() == Some(t))) {
                    add(trace_dir.join(str_of(&linked["id"])));
                }
            }
            (None, Some(trace)) => {
                add(trace_dir.join(str_of(&trace["id"])));
                for e in logs.iter().filter(|e| e["traceId"] == trace["id"]) {
                    add_log(&mut add, e);
                }
            }
            (None, None) => return Err(format!("no crash log or trace starts with {id} (see: apparition crash-reports list)")),
        }
    }
    let existing: Vec<PathBuf> = files.into_iter().filter(|f| f.exists()).collect();
    for f in &existing {
        let shown = f.strip_prefix(&folders.data).unwrap_or(f);
        write_line(out, format!("{} to Trash: {}", if dry { "would move" } else { "moving" }, shown.display()))?;
    }
    if dry || existing.is_empty() {
        return Ok(());
    }
    move_to_trash(&existing, folders.trash.as_ref()).map_err(|e| format!("trash failed ({e}); nothing more was deleted"))?;
    write_line(out, "Done. The Store forgets a crash log once its files are in the Trash.".into())
}

// MARK: The Trash

/// A Trash folder: files go to `files`, and where `info` is set (the XDG layout) each gets a
/// `.trashinfo` there saying where it came from.
struct Trash {
    files: PathBuf,
    info: Option<PathBuf>,
}

impl Trash {
    /// The user's own Trash: `~/.Trash` on a Mac, the XDG one elsewhere on Unix.
    fn platform() -> Result<Trash, String> {
        let home = || std::env::var_os("HOME").map(PathBuf::from).ok_or("HOME is not set");
        if cfg!(target_os = "macos") {
            Ok(Trash { files: home()?.join(".Trash"), info: None })
        } else if cfg!(unix) {
            let share = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).filter(|p| p.is_absolute()).map_or_else(|| home().map(|h| h.join(".local/share")), Ok)?;
            Ok(Trash { files: share.join("Trash/files"), info: Some(share.join("Trash/info")) })
        } else {
            Err("moving files to the Trash is not supported on this platform; remove them by hand".into())
        }
    }
}

/// Moves each file or folder into the Trash, keeping it recoverable. A Mac's `/usr/bin/trash` is
/// used when there is one, as `crash-reports` always did, so Finder can "Put Back".
fn move_to_trash(files: &[PathBuf], trash: Option<&Trash>) -> Result<(), String> {
    if trash.is_none() && cfg!(target_os = "macos") && Path::new("/usr/bin/trash").exists() {
        let status = Command::new("/usr/bin/trash").args(files).status().map_err(|e| e.to_string())?;
        return if status.success() { Ok(()) } else { Err("/usr/bin/trash failed".into()) };
    }
    let platform;
    let trash = match trash {
        Some(t) => t,
        None => {
            platform = Trash::platform()?;
            &platform
        }
    };
    std::fs::create_dir_all(&trash.files).map_err(|e| format!("{}: {e}", trash.files.display()))?;
    if let Some(info) = &trash.info {
        std::fs::create_dir_all(info).map_err(|e| format!("{}: {e}", info.display()))?;
    }
    for file in files {
        let name = file.file_name().and_then(|n| n.to_str()).ok_or_else(|| format!("{} has no name", file.display()))?;
        let target = free_name(&trash.files, name);
        let target_name = target.file_name().and_then(|n| n.to_str()).unwrap_or(name).to_string();
        if let Some(info) = &trash.info {
            let now = now_ms();
            let (y, mo, d, h, mi, s) = civil(now, local_offset(now));
            let absolute = std::fs::canonicalize(file).unwrap_or_else(|_| file.clone());
            let text = format!("[Trash Info]\nPath={}\nDeletionDate={y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}\n", percent_encode(&absolute.to_string_lossy()));
            std::fs::write(info.join(format!("{target_name}.trashinfo")), text).map_err(|e| e.to_string())?;
        }
        std::fs::rename(file, &target).map_err(|e| format!("{}: {e}", file.display()))?;
    }
    Ok(())
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

/// `name` in `dir`, or `name 2`, `name 3`... (before the extension) when taken.
fn free_name(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if std::fs::symlink_metadata(&first).is_err() && !dir.join(format!("{name}.trashinfo")).exists() {
        return first;
    }
    let (stem, ext) = match name.rfind('.').filter(|i| *i > 0) {
        Some(i) => (&name[..i], &name[i..]),
        None => (name, ""),
    };
    (2..)
        .map(|n| dir.join(format!("{stem} {n}{ext}")))
        .find(|p| std::fs::symlink_metadata(p).is_err())
        .expect("an unbounded range yields a free name")
}

/// The path as a `.trashinfo` writes it: bytes outside `A-Za-z0-9/._~-` as `%XX`.
fn percent_encode(path: &str) -> String {
    path.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"/._~-".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}

// MARK: Entry point

pub async fn run(args: &[String]) -> Result<(), Report> {
    let folders = Folders::from_environment();
    let mut stdout = std::io::stdout().lock();
    let code = execute(&folders, args, &mut stdout).map_err(|message| report!("{message}"))?;
    let _ = stdout.flush();
    if code != 0 {
        // The symbolicator already said what went wrong; leave with its status, as the script did.
        drop(stdout);
        std::process::exit(code);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// A data folder with one crash log (and its text) linked to one trace holding a MetricKit report.
    fn data() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("data");
        write(&root.join("crashlogs/AVP/App-1.ips"), "x");
        write(&root.join("crashlogs/AVP/App-1.txt"), "symbolicated stack\n");
        write(
            &root.join("crashlogs/index.json"),
            &json!({ "entries": [{
                "id": "aaaa1111-0000", "device": "AVP", "bundleId": "com.example.app", "appName": "App", "build": "abc",
                "exception": "EXC_BREAKPOINT SIGTRAP", "capturedAt": 1_791_532_800_000_i64,
                "file": "AVP/App-1.ips", "symbolicated": "AVP/App-1.txt", "traceId": "bbbb2222-0000" }] })
            .to_string(),
        );
        write(
            &root.join("traces/bbbb2222-0000/meta.json"),
            &json!({ "id": "bbbb2222-0000", "receivedAt": 1, "app": { "name": "App", "build": "abc" },
                "files": [{ "path": "crashes/r.json" }], "build": { "installedTo": "AVP" } })
            .to_string(),
        );
        std::fs::create_dir_all(root.join("traces/bbbb2222-0000/files/crashes")).unwrap();
        dir
    }

    fn folders(dir: &tempfile::TempDir) -> Folders {
        Folders {
            data: dir.path().join("data"),
            config: dir.path().join("config.json"),
            ledger: dir.path().join("keys"),
            report_keys: dir.path().join("report-keys"),
            trash: Some(Trash { files: dir.path().join("trash/files"), info: None }),
        }
    }

    fn cli(f: &Folders, args: &[&str]) -> (Result<i32, String>, String) {
        let mut out = Vec::new();
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let result = execute(f, &args, &mut out);
        (result, String::from_utf8(out).unwrap())
    }

    #[test]
    fn list_shows_the_crash_log_and_the_trace_it_links_to() {
        let dir = data();
        let (result, out) = cli(&folders(&dir), &["list"]);
        assert_eq!(result, Ok(0));
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "Crash logs from devices (1):");
        assert_eq!(
            lines[1],
            format!("  aaaa1111  {}  AVP      App              abc            EXC_BREAKPOINT SIGTRAP  trace bbbb2222", when(1_791_532_800_000.0))
        );
        assert_eq!(lines[2], "Traces (1):");
        assert_eq!(lines[3], format!("  bbbb2222  {}  AVP      App              abc            1 MetricKit report(s)  ", when(1.0)));
        assert_eq!(lines.len(), 4);
    }

    /// The CryptoKit-sealed report and its throwaway key from the service's fixtures.
    fn report_fixture() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../service/tests/fixtures/report")
    }

    #[test]
    fn add_opens_a_report_and_list_marks_it_unsigned() {
        let dir = data();
        let mut f = folders(&dir);
        f.report_keys = report_fixture().join("keys");
        let file = report_fixture().join("cryptokit.trace");
        let file = file.to_str().unwrap();

        let (result, out) = cli(&f, &["add", file]);
        assert_eq!(result, Ok(0), "{out}");
        let stored = dir.path().join("data/traces/aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee");
        assert_eq!(out, format!(
            "added aaaaaaaa  Public App 1.0.0 (298)  unsigned: the app, build and device are what the sender's copy said\n  files: {}\n",
            stored.join("files").display()));
        assert!(std::fs::read_to_string(stored.join("files/logs.txt")).unwrap().starts_with("# the app's own log"));
        let meta = read_json(&stored.join("meta.json")).unwrap();
        assert_eq!((&meta["via"], &meta["report"]), (&json!("report"), &json!({ "keyId": "a5d2ee9eadd96f4a", "signed": false })));

        let (result, out) = cli(&f, &["add", file]);
        assert_eq!(result, Ok(0));
        assert!(out.starts_with("already have aaaaaaaa  Public App 1.0.0 (298)\n"), "{out}");

        let (_, out) = cli(&f, &["list", "--app", "public"]);
        assert_eq!(out.lines().nth(2).unwrap(), format!(
            "  aaaaaaaa  {}  report   Public App       298            unsigned  It stopped refreshing after the update.", when(number(&meta["receivedAt"]))));
        // A report was installed nowhere by this Mac, so no device filter finds it.
        let (_, out) = cli(&f, &["list", "--device", "report"]);
        assert!(out.contains("Traces (0):"), "{out}");
    }

    #[test]
    fn add_says_which_key_a_report_needs_and_takes_nothing_else() {
        let dir = data();
        let f = folders(&dir);
        let file = report_fixture().join("cryptokit.trace");
        let error = cli(&f, &["add", file.to_str().unwrap()]).0.unwrap_err();
        assert!(error.contains("cryptokit.trace: this report is encrypted to key a5d2ee9eadd96f4a, which is not in "), "{error}");
        let zip = report_fixture().join("report.zip");
        let error = cli(&f, &["add", zip.to_str().unwrap()]).0.unwrap_err();
        assert!(error.contains("report.zip: not an encrypted report: no magic."), "{error}");
        assert_eq!(cli(&f, &["add"]).0, Err("usage: apparition crash-reports add <file.trace.zip or file.trace>...".into()));
        assert_eq!(cli(&f, &["import-dsyms"]).0, Err("usage: apparition crash-reports import-dsyms <dSYMs.zip>...".into()));
        assert!(!dir.path().join("data/traces/aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee").exists());
    }

    #[test]
    fn list_filters_by_app_and_device() {
        let dir = data();
        let f = folders(&dir);
        let (_, out) = cli(&f, &["list", "--app", "nothing"]);
        assert!(out.starts_with("Crash logs from devices (0):\nTraces (0):\n  nothing: no crash has been pulled or uploaded"), "{out}");
        let (_, out) = cli(&f, &["list", "--app", "EXAMPLE"]);
        assert!(out.contains("Crash logs from devices (1):"), "{out}");
        // The trace has no bundle id, but its name matches.
        let (_, out) = cli(&f, &["list", "--app", "app", "--device", "Phone"]);
        assert!(out.contains("Crash logs from devices (0):") && out.contains("Traces (0):"), "{out}");
        let (_, out) = cli(&f, &["list", "--device", "AVP"]);
        assert!(out.contains("Crash logs from devices (1):") && out.contains("Traces (1):"), "{out}");
    }

    #[test]
    fn a_crash_log_whose_ips_is_gone_is_not_listed() {
        let dir = data();
        std::fs::remove_file(dir.path().join("data/crashlogs/AVP/App-1.ips")).unwrap();
        let (_, out) = cli(&folders(&dir), &["list"]);
        assert!(out.starts_with("Crash logs from devices (0):"), "{out}");
    }

    #[test]
    fn show_prints_the_symbolicated_text_of_a_crash_log() {
        let dir = data();
        let (result, out) = cli(&folders(&dir), &["show", "aaaa"]);
        assert_eq!(result, Ok(0));
        assert_eq!(out, "symbolicated stack\n");
    }

    #[test]
    fn show_needs_an_id_and_a_known_one() {
        let dir = data();
        let f = folders(&dir);
        assert_eq!(cli(&f, &["show"]).0, Err("usage: apparition crash-reports show <id>".into()));
        assert_eq!(cli(&f, &["show", "zzzz"]).0, Err("no crash log or trace starts with zzzz".into()));
    }

    #[test]
    fn show_of_a_trace_without_reports_names_its_files() {
        let dir = data();
        std::fs::remove_dir(dir.path().join("data/traces/bbbb2222-0000/files/crashes")).unwrap();
        let error = cli(&folders(&dir), &["show", "bbbb"]).0.unwrap_err();
        assert!(error.starts_with("trace bbbb2222-0000 has no MetricKit reports; its files are in "), "{error}");
    }

    #[test]
    fn show_without_a_symbolicator_says_where_it_looked() {
        let dir = data();
        std::fs::remove_file(dir.path().join("data/crashlogs/AVP/App-1.txt")).unwrap();
        let f = folders(&dir);
        write(&f.config, &json!({ "symbolicateTool": dir.path().join("missing-tool") }).to_string());
        let error = cli(&f, &["show", "aaaa"]).0.unwrap_err();
        assert!(error.contains("debugtrace-symbolicate not found at") && error.contains("missing-tool") && error.contains("symbolicateTool in config.json"), "{error}");
    }

    #[test]
    fn done_dry_run_names_the_crash_log_its_text_and_its_trace_and_moves_nothing() {
        let dir = data();
        let (result, out) = cli(&folders(&dir), &["done", "aaaa1111", "--dry-run"]);
        assert_eq!(result, Ok(0));
        assert_eq!(
            out,
            "would move to Trash: crashlogs/AVP/App-1.ips\nwould move to Trash: crashlogs/AVP/App-1.txt\nwould move to Trash: traces/bbbb2222-0000\n"
        );
        assert!(dir.path().join("data/crashlogs/AVP/App-1.ips").exists());
    }

    #[test]
    fn done_on_a_trace_also_names_the_crash_logs_linked_to_it() {
        let dir = data();
        let (_, out) = cli(&folders(&dir), &["done", "bbbb2222", "--dry-run"]);
        assert!(out.contains("crashlogs/AVP/App-1.ips") && out.contains("crashlogs/AVP/App-1.txt"), "{out}");
        // A trace and a crash log naming each other are moved once.
        let (_, out) = cli(&folders(&dir), &["done", "bbbb2222", "aaaa", "--dry-run"]);
        assert_eq!(out.lines().count(), 3, "{out}");
    }

    #[test]
    fn done_moves_the_files_to_the_trash() {
        let dir = data();
        let f = folders(&dir);
        let (result, out) = cli(&f, &["done", "aaaa"]);
        assert_eq!(result, Ok(0));
        assert!(out.starts_with("moving to Trash: crashlogs/AVP/App-1.ips\n") && out.ends_with("Done. The Store forgets a crash log once its files are in the Trash.\n"), "{out}");
        let data = dir.path().join("data");
        assert!(!data.join("crashlogs/AVP/App-1.ips").exists() && !data.join("traces/bbbb2222-0000").exists());
        let trash = dir.path().join("trash/files");
        assert_eq!(std::fs::read_to_string(trash.join("App-1.txt")).unwrap(), "symbolicated stack\n");
        assert!(trash.join("bbbb2222-0000/meta.json").exists());
        // The crash log is gone from the list now, and a second `done` finds nothing to do.
        assert!(cli(&f, &["list"]).1.starts_with("Crash logs from devices (0):"));
        assert!(cli(&f, &["done", "aaaa"]).0.is_err());
    }

    #[test]
    fn the_trash_never_overwrites_and_the_xdg_one_records_where_a_file_came_from() {
        let dir = tempfile::tempdir().unwrap();
        let trash = Trash { files: dir.path().join("Trash/files"), info: Some(dir.path().join("Trash/info")) };
        for round in 0..2 {
            let file = dir.path().join("my crash.ips");
            write(&file, &round.to_string());
            move_to_trash(std::slice::from_ref(&file), Some(&trash)).unwrap();
            assert!(!file.exists());
        }
        assert_eq!(std::fs::read_to_string(trash.files.join("my crash.ips")).unwrap(), "0");
        assert_eq!(std::fs::read_to_string(trash.files.join("my crash 2.ips")).unwrap(), "1");
        let info = std::fs::read_to_string(dir.path().join("Trash/info/my crash 2.ips.trashinfo")).unwrap();
        assert!(info.starts_with("[Trash Info]\nPath=") && info.contains("/my%20crash.ips\nDeletionDate="), "{info}");
    }

    #[test]
    fn an_unknown_id_is_an_error_and_nothing_is_touched() {
        let dir = data();
        let (result, _) = cli(&folders(&dir), &["done", "zzzz"]);
        assert_eq!(result, Err("no crash log or trace starts with zzzz (see: apparition crash-reports list)".into()));
        assert!(dir.path().join("data/crashlogs/AVP/App-1.ips").exists());
    }

    #[test]
    fn an_ambiguous_prefix_asks_for_more_of_the_id() {
        let dir = data();
        write(
            &dir.path().join("data/traces/bbbb3333/meta.json"),
            &json!({ "id": "bbbb3333-0000", "receivedAt": 2, "app": { "name": "Other" } }).to_string(),
        );
        let (result, _) = cli(&folders(&dir), &["done", "bbbb", "--dry-run"]);
        assert_eq!(result, Err("bbbb matches 2 traces; give more of the id".into()));
    }

    #[test]
    fn done_needs_an_id_and_the_command_a_known_word() {
        let dir = data();
        let f = folders(&dir);
        assert_eq!(cli(&f, &["done", "--dry-run"]).0, Err("usage: apparition crash-reports done <id>... [--dry-run]".into()));
        assert_eq!(cli(&f, &[]).0, Err(USAGE.into()));
    }

    #[test]
    fn an_index_cannot_point_outside_the_data_folder() {
        let dir = data();
        write(&dir.path().join("outside.ips"), "x");
        write(
            &dir.path().join("data/crashlogs/index.json"),
            &json!({ "entries": [{ "id": "cccc", "file": "../../outside.ips", "capturedAt": 1 }] }).to_string(),
        );
        assert!(cli(&folders(&dir), &["list"]).1.starts_with("Crash logs from devices (0):"));
    }

    #[test]
    fn civil_time_matches_known_dates() {
        assert_eq!(civil(0, 0), (1970, 1, 1, 0, 0, 0));
        assert_eq!(civil(1_791_532_800_000, 0), (2026, 10, 9, 8, 0, 0));
        assert_eq!(civil(1_791_532_800_000, 2 * 3600 + 1800), (2026, 10, 9, 10, 30, 0));
        assert_eq!(civil(-1000, 0), (1969, 12, 31, 23, 59, 59));
    }
}
