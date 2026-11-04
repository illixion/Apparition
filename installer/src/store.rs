//! `apparition store`: everything the Apparition Store does, from a terminal. `serve` runs the
//! Store without the app (the same Store, folder and port); every other subcommand drives the
//! Store this computer reaches, the way `deploy` and `updates` do: the app's (or `serve`'s) here,
//! else the Mac this computer is paired with for developer access (`apparition mac pair`).

use std::path::{Path, PathBuf};
use std::time::Duration;

use apparition_core::api::paired_mac::Upload;
use serde_json::{Value, json};

use crate::developer::Target;
use crate::{Error, fail};

pub const USAGE: &str = "\
apparition store: the Apparition Store from a terminal. Every subcommand but serve drives the Store
this computer reaches: the Apparition app's (or `store serve`'s) here, else the Mac paired for
developer access (`apparition mac pair`); --local insists on this computer's. --json prints the
Store's answer as it is.

  apparition store serve [--port <port>]
      Run the Store without the app, in the app's folder and on config.json's port (8484), until
      Ctrl-C or SIGTERM. What the app's Store does, it does: builds and the update queue, OTA and
      AltStore links, Mac downloads, re-signs, trace uploads, MCP at /mcp. `apparition relay`
      beside it lets paired computers reach it. Only one Store runs per folder.
  apparition store status
      Where the Store is, whether its web pages are on, its folder.
  apparition store web-ui on|off [--password]
      Turn the Store's web pages (and other devices' Apparition connecting with a password) on or
      off. --password asks for a new password (8 characters or more); turning on needs one set.
  apparition store open [debug [<app>]]
      Open the Store's page in the browser here, signed in by a one-time link (this computer only);
      debug opens its Debug console (live log, tuning panels, profile, memory), at <app> (its
      debug id, from `apparition mcp`'s apps) if given.
  apparition store projects
      The projects, their targets (platform, device, bundle ID) and branches.
  apparition store add <folder> [--app <scheme>]... [--device <name>]
      Add an Xcode project: reads its schemes and apps, and adds a target per app and platform,
      on the paired device of that platform (--device picks one), writing a build-signing.conf if
      it has none. --app keeps only the apps named.
  apparition store devices [--refresh [--force]]
      The paired devices and what each has installed of the projects' apps. --refresh asks them
      again (--force asks the ones config.json's ledgerOnlyDevices leaves alone too).
  apparition store build <project> [--device <name>]... [--platform <p>] [--branch <b>]
          [--install auto|mac|device|none] [--run [--no-mcp] [--jit]] [--inject] [--debug] [--no-follow]
      Queue a build of a project's target, as the page's Deploy does, and follow its log until it
      ends (exit 1 if it failed). Without --device, every target of the project (of --platform).
      --install: auto (the default) installs from here when this computer can reach the device,
      else the device's Apparition installs it; none only keeps it (for OTA). --run launches it,
      as a debug session unless --no-mcp; --jit serves its JIT; --inject makes it patchable.
  apparition store builds
      The latest builds, queued and running first.
  apparition store log <build id> [--follow]
      A build's log; --follow keeps printing it until the build ends.
  apparition store cancel <build id>
      Cancel a queued or running build.
  apparition store kept [download <build id> [--output <file>]]
      The kept builds (IPAs and Mac apps, with their manifests), and downloading one.
  apparition store resign <project> --device <name> --profile <id> [--platform <p>] [--branch <b>]
          [--discard-data] [--dry-run]
  apparition store resign --build <kept build id> --device <name> --profile <id> [--discard-data] [--dry-run]
  apparition store resign --profiles
      Re-sign an app on a device with another profile (from a fresh build of the project, or a
      kept build), keeping its data unless --discard-data. --profiles lists the profiles.
  apparition store ota [<bundle ID or slug>]
      The OTA installs the Store holds, or the signed itms-services link of one (an hour).
  apparition store altsource
      The AltStore source links (the combined one and one per platform).
  apparition store mac <app slug> [--branch <b>] [--build-now]
      A Mac app's download link (lasting a week; the zip builds on first fetch); --build-now builds
      it now and waits.
  apparition store traces [delete <trace id>]
      The debug traces apps uploaded. `apparition crash-reports` reads them with the crash logs.

  Links the Store makes for devices (OTA, AltStore, Mac) start with config.json's publicUrl when
  asked from this computer (\"publicUrl\": \"https://mac.your-tailnet.ts.net\", where `tailscale
  serve` points at the Store), so they work from a phone; without one they name 127.0.0.1.
";

pub async fn run(args: &[String]) -> Result<(), Error> {
    let Some(command) = args.first().map(String::as_str) else {
        print!("{USAGE}");
        return Ok(());
    };
    let rest = &args[1..];
    if command == "serve" {
        return serve(rest).await;
    }
    if matches!(command, "help" | "--help" | "-h") {
        print!("{USAGE}");
        return Ok(());
    }
    let opts = Opts::parse(rest)?;
    let store = Store::find(opts.has("--local"))?;
    match command {
        "status" => status(&store, &opts).await,
        "web-ui" => web_ui(&store, &opts).await,
        "open" => open(&store, &opts).await,
        "projects" => projects(&store, &opts).await,
        "add" => add(&store, &opts).await,
        "devices" => devices(&store, &opts).await,
        "build" => build(&store, &opts).await,
        "builds" => builds(&store, &opts).await,
        "log" => log(&store, &opts).await,
        "cancel" => cancel(&store, &opts).await,
        "kept" => kept(&store, &opts).await,
        "resign" => resign(&store, &opts).await,
        "ota" => ota(&store, &opts).await,
        "altsource" => altsource(&store, &opts).await,
        "mac" => mac(&store, &opts).await,
        "traces" => traces(&store, &opts).await,
        other => Err(fail(format!("Unknown store command: {other}. `apparition store help` lists them."))),
    }
}

// ---- options ----

/// Positional words, `--flag`s and `--key value`s, in any order.
struct Opts {
    words: Vec<String>,
    flags: Vec<String>,
    values: Vec<(String, String)>,
}

/// Options that take a value.
const VALUED: &[&str] = &["--port", "--app", "--device", "--platform", "--branch", "--install", "--profile", "--build", "--output", "-o"];

impl Opts {
    fn parse(args: &[String]) -> Result<Opts, Error> {
        let mut opts = Opts { words: Vec::new(), flags: Vec::new(), values: Vec::new() };
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            if VALUED.contains(&arg.as_str()) {
                let value = args.next().ok_or_else(|| fail(format!("{arg} needs a value")))?;
                opts.values.push((arg.clone(), value.clone()));
            } else if arg.starts_with('-') {
                opts.flags.push(arg.clone());
            } else {
                opts.words.push(arg.clone());
            }
        }
        Ok(opts)
    }

    fn has(&self, flag: &str) -> bool {
        self.flags.iter().any(|f| f == flag)
    }

    fn value(&self, key: &str) -> Option<&str> {
        self.values.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    fn all(&self, key: &str) -> Vec<String> {
        self.values.iter().filter(|(k, _)| k == key).map(|(_, v)| v.clone()).collect()
    }

    fn word(&self, at: usize) -> Option<&str> {
        self.words.get(at).map(String::as_str)
    }

    /// Refuses flags this command doesn't know, so a typo doesn't pass silently.
    fn only(&self, flags: &[&str]) -> Result<(), Error> {
        for flag in &self.flags {
            if flag != "--local" && flag != "--json" && !flags.contains(&flag.as_str()) {
                return Err(fail(format!("Unknown option: {flag}. `apparition store help` lists them.")));
            }
        }
        Ok(())
    }
}

// ---- the Store ----

struct Store {
    target: Target,
}

impl Store {
    fn find(prefer_local: bool) -> Result<Store, Error> {
        Ok(Store { target: Target::find(prefer_local)? })
    }

    /// A service API call (`/api/service/v1<path>`): the JSON answer, or the Store's error.
    async fn call(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value, Error> {
        let reply = self.raw(method, path, body).await?;
        let value: Value = serde_json::from_slice(&reply.body).unwrap_or(Value::Null);
        if reply.status == 404 && value["error"] == "Unknown service operation." {
            return Err(fail(format!("{}'s Store doesn't do that (or it's older than this command line): update the Apparition app there.", self.target.name())));
        }
        if !(200..300).contains(&reply.status) {
            let message = value["error"].as_str().or_else(|| value.pointer("/error/message").and_then(Value::as_str)).unwrap_or("");
            return Err(fail(format!("{} answered HTTP {}{}", self.target.name(), reply.status, if message.is_empty() { String::new() } else { format!(": {message}") })));
        }
        Ok(value)
    }

    async fn raw(&self, method: &str, path: &str, body: Option<Value>) -> Result<apparition_core::api::paired_mac::Reply, Error> {
        let bytes = body.map(|b| b.to_string()).unwrap_or_default();
        let upload = if bytes.is_empty() { Upload::None } else { Upload::Bytes(bytes.as_bytes()) };
        let content_type = (!bytes.is_empty()).then_some("application/json");
        self.target.request(method, &format!("/api/service/v1{path}"), content_type, upload).await
    }

    async fn get(&self, path: &str) -> Result<Value, Error> {
        self.call("GET", path, None).await
    }

    async fn post(&self, path: &str, body: Value) -> Result<Value, Error> {
        self.call("POST", path, Some(body)).await
    }
}

fn print_json(value: &Value) {
    println!("{}", serde_json::to_string_pretty(value).unwrap_or_default());
}

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap_or("")
}

/// A path segment, percent-encoded.
fn segment(text: &str) -> String {
    text.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

fn ago(ms: u64) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
    let s = now.saturating_sub(ms) / 1000;
    match s {
        0..=59 => format!("{s} s ago"),
        60..=3599 => format!("{} min ago", s / 60),
        3600..=86_399 => format!("{} h ago", s / 3600),
        _ => format!("{} d ago", s / 86_400),
    }
}

// ---- serve ----

async fn serve(args: &[String]) -> Result<(), Error> {
    let opts = Opts::parse(args)?;
    opts.only(&[])?;
    let port = opts.value("--port").map(|p| p.parse::<u16>().map_err(|_| fail("--port needs a number"))).transpose()?;
    let name = format!("Apparition Store on {}", apparition_core::relay::machine_name());
    let mut options = apparition_service::hosted::Options::new(name);
    options.port = port;
    let mut hosted = apparition_service::hosted::start(options).await.map_err(fail)?;
    if let Some(moved) = hosted.moved() {
        eprintln!("warning: {moved}");
    }
    println!("Apparition Store at http://{} (MCP at /mcp), folder {}.", hosted.route.address, hosted.dir().display());
    println!("`apparition store` and `apparition mcp` here use it; `apparition relay` lets paired computers reach it. Ctrl-C stops it.");
    let stop = async {
        #[cfg(unix)]
        {
            let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).map_err(|e| fail(e.to_string()))?;
            tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = term.recv() => {} }
        }
        #[cfg(not(unix))]
        let _ = tokio::signal::ctrl_c().await;
        Ok::<(), Error>(())
    };
    tokio::select! {
        result = stop => result?,
        () = hosted.wait() => return Err(fail("the Store stopped by itself")),
    }
    println!("Stopping: running builds are stopped and noted as failed.");
    hosted.stop();
    Ok(())
}

// ---- the Store itself ----

async fn status(store: &Store, opts: &Opts) -> Result<(), Error> {
    opts.only(&[])?;
    let host = store.get("/host").await;
    let capabilities = store.get("").await?;
    if opts.has("--json") {
        print_json(&json!({ "store": store.target.name(), "capabilities": capabilities, "host": host.ok() }));
        return Ok(());
    }
    println!("The Store on {} ({}), service API v{}.", store.target.name(), text(&capabilities, "hostPlatform"), capabilities["version"]);
    match host {
        Ok(host) => {
            println!("Web pages: {}{}.", if host["webUiOn"] == true { "on" } else { "off" },
                if host["passwordSet"] == true { ", password set" } else { ", no password set" });
            println!("Folder: {}", host["configPath"].as_str().and_then(|p| Path::new(p).parent()).map(|p| p.display().to_string()).unwrap_or_default());
            if let Some(port) = host["port"].as_u64() {
                println!("Port: {port} (config.json)");
            }
        }
        Err(_) => println!("Its settings are its owner's: run `apparition store status` on that Mac to see them."),
    }
    Ok(())
}

async fn web_ui(store: &Store, opts: &Opts) -> Result<(), Error> {
    opts.only(&["--password"])?;
    let enabled = match opts.word(0) {
        Some("on") => true,
        Some("off") => false,
        _ => return Err(fail("usage: apparition store web-ui on|off [--password]")),
    };
    let password = if opts.has("--password") {
        let first = crate::ask_secret("New password for the Store's web pages: ");
        if crate::ask_secret("Again: ") != first {
            return Err(fail("The passwords don't match."));
        }
        Some(first)
    } else {
        None
    };
    let host = store.post("/host/web-ui", json!({ "enabled": enabled, "password": password })).await?;
    println!("The Store's web pages are {}.", if host["webUiOn"] == true { "on: other devices sign in with the password" } else { "off" });
    Ok(())
}

async fn open(store: &Store, opts: &Opts) -> Result<(), Error> {
    opts.only(&[])?;
    let Target::Local { address, .. } = &store.target else {
        return Err(fail("`store open` opens this computer's Store; on another computer, turn its web pages on (`store web-ui on`) and open its address."));
    };
    let link = store.post("/host/web-link", json!({})).await?;
    let path = link["path"].as_str().filter(|p| p.starts_with("/host-login?code=")).ok_or_else(|| fail("The Store didn't give a sign-in link."))?;
    let next = match (opts.word(0), opts.word(1)) {
        (None, _) => String::new(),
        (Some("debug"), None) => "&next=/%23debug".to_string(),
        (Some("debug"), Some(app)) if !app.is_empty() && app.len() <= 64 && app.bytes().all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c)) => format!("&next=/%23debug/{app}"),
        _ => return Err(fail("usage: apparition store open [debug [<app>]]")),
    };
    let url = format!("http://{address}{path}{next}");
    let opener = if cfg!(target_os = "macos") { "open" } else if cfg!(windows) { "explorer" } else { "xdg-open" };
    match std::process::Command::new(opener).arg(&url).status() {
        Ok(status) if status.success() || cfg!(windows) => println!("Opened the Store's page (the link works once, for a minute)."),
        _ => println!("Open this in a browser here within a minute (it works once):\n{url}"),
    }
    Ok(())
}

// ---- projects and devices ----

async fn projects(store: &Store, opts: &Opts) -> Result<(), Error> {
    opts.only(&[])?;
    let state = store.get("/state").await?;
    let repos = state["repos"].as_array().cloned().unwrap_or_default();
    if opts.has("--json") {
        print_json(&json!(repos));
        return Ok(());
    }
    if repos.is_empty() {
        println!("No projects yet: `apparition store add <folder>`.");
    }
    for repo in &repos {
        let git = &repo["git"];
        let branches: Vec<&str> = repo["branches"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        println!("{}  [{}]{}", text(repo, "name"), branches.join(", "),
            git["branch"].as_str().map(|b| format!("  checkout on {b}{}", if git["dirty"] == true { ", uncommitted changes" } else { "" })).unwrap_or_default());
        for row in repo["platforms"].as_array().into_iter().flatten() {
            println!("    {:<10} {:<24} {}", text(row, "platform"), text(row, "device"), text(row, "bundleId"));
        }
        for app in repo["macApps"].as_array().into_iter().flatten() {
            println!("    Mac app    {} (`apparition store mac {}`)", text(app, "name"), text(app, "slug"));
        }
    }
    Ok(())
}

async fn add(store: &Store, opts: &Opts) -> Result<(), Error> {
    opts.only(&[])?;
    let folder = opts.word(0).ok_or_else(|| fail("usage: apparition store add <folder> [--app <scheme>]... [--device <name>]"))?;
    // A folder here is named as this computer sees it; one on the paired Mac, as the Mac does.
    let path = match &store.target {
        Target::Local { .. } => std::fs::canonicalize(folder).map_err(|e| fail(format!("{folder}: {e}")))?.display().to_string(),
        Target::Paired { .. } => folder.to_string(),
    };
    println!("Reading the project (Xcode is asked for its schemes, which can take a moment)...");
    let found = store.post("/projects/discover", json!({ "path": path })).await?["discovery"].clone();
    if opts.has("--json") && found["registered"] == true {
        print_json(&found);
        return Ok(());
    }
    if found["registered"] == true {
        println!("{} is already added.", text(&found, "name"));
        return Ok(());
    }
    if let Some(note) = found["note"].as_str() {
        return Err(fail(note.to_string()));
    }
    let wanted = opts.all("--app");
    let apps: Vec<&Value> = found["apps"].as_array().into_iter().flatten()
        .filter(|a| wanted.is_empty() || wanted.iter().any(|w| w.eq_ignore_ascii_case(text(a, "scheme")))).collect();
    if apps.is_empty() {
        let schemes: Vec<&str> = found["apps"].as_array().into_iter().flatten().map(|a| text(a, "scheme")).collect();
        return Err(fail(format!("No app to add{}. Its apps: {}", if wanted.is_empty() { "" } else { " by those names" }, schemes.join(", "))));
    }
    let state = store.get("/state").await?;
    let paired: Vec<(String, String)> = state.pointer("/devices/list").and_then(Value::as_array).into_iter().flatten()
        .map(|d| (text(d, "name").to_string(), text(d, "platform").to_string())).collect();
    let chosen = opts.value("--device");
    let mut platforms = Vec::new();
    for app in &apps {
        let list: Vec<String> = app["platforms"].as_array().into_iter().flatten().filter_map(Value::as_str).map(String::from).collect();
        for platform in if list.is_empty() { vec!["iOS".to_string()] } else { list } {
            let device = match chosen {
                Some(name) => name.to_string(),
                None => {
                    let of: Vec<&String> = paired.iter().filter(|(_, p)| p.eq_ignore_ascii_case(&platform)).map(|(n, _)| n).collect();
                    match of.as_slice() {
                        [one] => one.to_string(),
                        [] => { println!("    skipped {} on {platform}: no paired {platform} device", text(app, "scheme")); continue }
                        many => return Err(fail(format!("Several {platform} devices are paired ({}): choose one with --device.", many.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")))),
                    }
                }
            };
            platforms.push(json!({ "platform": platform, "device": device, "bundleId": text(app, "bundleId") }));
        }
    }
    if platforms.is_empty() {
        return Err(fail("No target to add: pair the device with Apparition on that computer first (`apparition pair`), or name it with --device."));
    }
    let mut body = json!({ "name": found["name"], "path": found["path"], "platforms": platforms });
    if let Some(branch) = found["currentBranch"].as_str() {
        body["branches"] = json!([branch]);
    }
    if found["hasConf"] != true {
        body["generate"] = json!({ "container": found["container"], "scheme": text(apps[0], "scheme") });
    }
    let reply = store.post("/projects", body).await?;
    if opts.has("--json") {
        print_json(&reply);
        return Ok(());
    }
    let project = &reply["project"];
    println!("Added {}:", text(project, "name"));
    for row in project["platforms"].as_array().into_iter().flatten() {
        println!("    {:<10} {:<24} {}", text(row, "platform"), text(row, "device"), text(row, "bundleId"));
    }
    if found["hasConf"] != true {
        println!("A build-signing.conf was written in the project; check its signing settings before the first build.");
    }
    Ok(())
}

async fn devices(store: &Store, opts: &Opts) -> Result<(), Error> {
    opts.only(&["--refresh", "--force"])?;
    if opts.has("--refresh") {
        let reply = store.post("/refresh-devices", json!({ "force": opts.has("--force") })).await?;
        let skipped: Vec<&str> = reply["skipped"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        println!("Asking the devices{}...", if skipped.is_empty() { String::new() } else { format!(" (not {}: --force asks them too)", skipped.join(", ")) });
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
    let state = store.get("/state").await?;
    if opts.has("--json") {
        print_json(&json!({ "devices": state["devices"], "deviceApps": state["deviceApps"], "installs": state["installs"], "appsOpen": state["appsOpen"] }));
        return Ok(());
    }
    let list = state.pointer("/devices/list").and_then(Value::as_array).cloned().unwrap_or_default();
    if list.is_empty() {
        println!("No paired devices: `apparition pair` pairs one.");
    }
    for device in &list {
        let name = text(device, "name");
        let apps = &state["deviceApps"][name];
        let open = state["appsOpen"].as_array().into_iter().flatten().any(|d| d.as_str() == Some(name));
        let model = text(device, "model");
        println!("{name} ({}{}){}", text(device, "platform"), if model.is_empty() { String::new() } else { format!(", {model}") }, if open { " — Apparition open there" } else { "" });
        if let Some(error) = apps["error"].as_str() {
            println!("    couldn't ask it: {error}");
        }
        if let Some(at) = apps["fetchedAt"].as_u64() {
            println!("    asked {}", ago(at));
        }
        for (bundle, app) in apps["apps"].as_object().into_iter().flatten() {
            let installed = &state["installs"][name][bundle];
            let from = installed["commit"].as_str().map(|c| format!("  ({}@{c})", text(installed, "branch"))).unwrap_or_default();
            println!("    {bundle:<40} {} ({}){from}", text(app, "version"), text(app, "buildNo"));
        }
    }
    Ok(())
}

// ---- builds ----

async fn build(store: &Store, opts: &Opts) -> Result<(), Error> {
    opts.only(&["--run", "--no-mcp", "--jit", "--inject", "--debug", "--no-follow"])?;
    let project = opts.word(0).ok_or_else(|| fail("usage: apparition store build <project> [--device <name>]... (`apparition store help`)"))?;
    let state = store.get("/state").await?;
    let repo = state["repos"].as_array().into_iter().flatten().find(|r| text(r, "name").eq_ignore_ascii_case(project))
        .ok_or_else(|| fail(format!("No project {project}. `apparition store projects` lists them.")))?.clone();
    let devices = opts.all("--device");
    let platform = opts.value("--platform");
    let rows: Vec<&Value> = repo["platforms"].as_array().into_iter().flatten()
        .filter(|r| devices.is_empty() || devices.iter().any(|d| d.eq_ignore_ascii_case(text(r, "device"))))
        .filter(|r| platform.is_none_or(|p| p.eq_ignore_ascii_case(text(r, "platform"))))
        .filter(|r| !text(r, "platform").eq_ignore_ascii_case("watchOS"))
        .collect();
    if rows.is_empty() {
        return Err(fail(format!("{} has no target{}{}.", text(&repo, "name"),
            if devices.is_empty() { String::new() } else { format!(" on {}", devices.join(", ")) },
            platform.map(|p| format!(" for {p}")).unwrap_or_default())));
    }
    let run = opts.has("--run") || opts.has("--jit");
    let mut ids = Vec::new();
    for row in rows {
        let mut body = json!({
            "repo": repo["name"], "device": row["device"], "targetPlatform": row["platform"],
            "run": run, "mcp": run && !opts.has("--no-mcp"), "jit": opts.has("--jit"),
            "inject": opts.has("--inject"), "buildDebug": opts.has("--debug"),
        });
        if let Some(branch) = opts.value("--branch") {
            body["branch"] = json!(branch);
        }
        if let Some(install) = opts.value("--install") {
            body["install"] = json!(install);
        }
        let reply = store.post("/build", body).await?;
        for job in reply["jobs"].as_array().into_iter().flatten() {
            println!("#{} {} [{}] for {} on {}: installed {}", job["id"], text(job, "repo"), text(job, "branch"), text(job, "device"), text(job, "platform"),
                match text(job, "installVia") { "mac" => "from here", "device" => "by the device's Apparition", "none" => "nowhere (kept for OTA)", other => other });
            ids.push(job["id"].as_u64().unwrap_or(0));
        }
    }
    if opts.has("--no-follow") {
        return Ok(());
    }
    let mut failed = Vec::new();
    let many = ids.len() > 1;
    for id in ids {
        if many {
            println!("\n==> #{id}");
        }
        if follow(store, id).await? != "success" {
            failed.push(id);
        }
    }
    if failed.is_empty() { Ok(()) } else { Err(fail(format!("Build {} didn't succeed.", failed.iter().map(|i| format!("#{i}")).collect::<Vec<_>>().join(", ")))) }
}

/// Prints the build's log as it grows, until it ends; its last status.
async fn follow(store: &Store, id: u64) -> Result<String, Error> {
    let mut offset = 0u64;
    loop {
        let reply = store.get(&format!("/builds/{id}/log?offset={offset}")).await?;
        if let Some(chunk) = reply["chunk"].as_str().filter(|c| !c.is_empty()) {
            print!("{chunk}");
            use std::io::Write;
            let _ = std::io::stdout().flush();
        }
        offset = reply["nextOffset"].as_u64().unwrap_or(offset);
        let status = text(&reply, "status").to_string();
        if !matches!(status.as_str(), "queued" | "running") {
            println!("\n==> #{id}: {status}");
            return Ok(status);
        }
        tokio::time::sleep(Duration::from_millis(1000)).await;
    }
}

fn job_id(opts: &Opts, usage: &str) -> Result<u64, Error> {
    opts.word(0).and_then(|w| w.trim_start_matches('#').parse().ok()).ok_or_else(|| fail(usage.to_string()))
}

async fn builds(store: &Store, opts: &Opts) -> Result<(), Error> {
    opts.only(&[])?;
    let state = store.get("/state").await?;
    let list = state["builds"].as_array().cloned().unwrap_or_default();
    if opts.has("--json") {
        print_json(&json!(list));
        return Ok(());
    }
    if list.is_empty() {
        println!("No builds yet.");
    }
    let mut list = list;
    list.sort_by_key(|b| !matches!(text(b, "status"), "queued" | "running"));
    for b in &list {
        let devices: Vec<&str> = b["devices"].as_array().map(|d| d.iter().filter_map(Value::as_str).collect()).filter(|d: &Vec<&str>| !d.is_empty())
            .unwrap_or_else(|| vec![text(b, "device")]);
        let at = b["finishedAt"].as_u64().or(b["startedAt"].as_u64()).or(b["queuedAt"].as_u64()).map(ago).unwrap_or_default();
        let commit = b["commit"].as_str().map(|c| format!("@{c}")).unwrap_or_default();
        println!("#{:<8} {:<10} {:<9} {} [{}{commit}] {}  {at}{}", b["id"], text(b, "status"), b["kind"].as_str().unwrap_or("deploy"), text(b, "repo"), text(b, "branch"),
            devices.join(", "), b["error"].as_str().map(|e| format!("\n          {e}")).unwrap_or_default());
    }
    Ok(())
}

async fn log(store: &Store, opts: &Opts) -> Result<(), Error> {
    opts.only(&["--follow", "-f"])?;
    let id = job_id(opts, "usage: apparition store log <build id> [--follow]")?;
    if opts.has("--follow") || opts.has("-f") {
        follow(store, id).await?;
        return Ok(());
    }
    let reply = store.get(&format!("/builds/{id}/log?offset=0")).await?;
    print!("{}", text(&reply, "chunk"));
    println!("\n==> #{id}: {}", text(&reply, "status"));
    Ok(())
}

async fn cancel(store: &Store, opts: &Opts) -> Result<(), Error> {
    opts.only(&[])?;
    let id = job_id(opts, "usage: apparition store cancel <build id>")?;
    let reply = store.post(&format!("/builds/{id}/cancel"), json!({})).await?;
    match reply["job"]["status"].as_str() {
        Some("cancelled") | None => println!("#{id}: cancelled"),
        Some(status) => println!("#{id} had already ended: {status}"),
    }
    Ok(())
}

async fn kept(store: &Store, opts: &Opts) -> Result<(), Error> {
    opts.only(&[])?;
    if opts.word(0) == Some("download") {
        let id = opts.word(1).ok_or_else(|| fail("usage: apparition store kept download <build id> [--output <file>]"))?;
        let manifest = store.get(&format!("/artifacts/{}/manifest", segment(id))).await?;
        let reply = store.raw("GET", &format!("/artifacts/{}/download", segment(id)), None).await?;
        if reply.status != 200 {
            return Err(fail(format!("{} answered HTTP {}", store.target.name(), reply.status)));
        }
        let artifact = &manifest["artifact"];
        if artifact["sha256"].as_str().is_some_and(|want| want != sha256(&reply.body)) {
            return Err(fail("The download doesn't match the build's manifest (sha256); nothing was saved."));
        }
        let name = opts.value("--output").or(opts.value("-o")).map(PathBuf::from).unwrap_or_else(|| {
            let app = manifest.pointer("/app/name").and_then(Value::as_str).unwrap_or("build");
            let ext = if artifact["kind"] == "ipa" { "ipa" } else { "zip" };
            PathBuf::from(format!("{}-{}.{ext}", app.replace(['/', ' '], "-"), manifest.pointer("/app/build").and_then(Value::as_str).unwrap_or(id)))
        });
        std::fs::write(&name, &reply.body).map_err(|e| fail(format!("{}: {e}", name.display())))?;
        println!("Saved {} ({} MB, checked against its manifest).", name.display(), reply.body.len() / 1_000_000);
        return Ok(());
    }
    let reply = store.get("/artifacts").await?;
    if opts.has("--json") {
        print_json(&reply);
        return Ok(());
    }
    let list = reply["builds"].as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        println!("No kept builds.");
    }
    for b in &list {
        let app = &b["app"];
        println!("{}  {} {} ({}) {}  {}", text(b, "buildId"), text(app, "name"), text(app, "version"), text(app, "build"),
            b.pointer("/artifact/kind").and_then(Value::as_str).unwrap_or(""), text(b, "createdAt"));
    }
    Ok(())
}

fn sha256(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes).as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

async fn resign(store: &Store, opts: &Opts) -> Result<(), Error> {
    opts.only(&["--profiles", "--discard-data", "--dry-run"])?;
    if opts.has("--profiles") {
        let reply = store.get("/resign/options").await?;
        if opts.has("--json") {
            print_json(&reply);
            return Ok(());
        }
        let profiles = reply["profiles"].as_array().cloned().unwrap_or_default();
        if profiles.is_empty() {
            println!("No profiles in the build-signing folder (~/.config/build-signing) on {}.", store.target.name());
        }
        for p in &profiles {
            println!("{}  {} [{}]{}", text(p, "id"), text(p, "name"), text(p, "appId"),
                p["expires"].as_str().map(|e| format!(", until {e}")).unwrap_or_default());
        }
        return Ok(());
    }
    let usage = "usage: apparition store resign <project> | --build <id>, --device <name> --profile <id> (`apparition store help`)";
    let mut body = json!({
        "device": opts.value("--device").ok_or_else(|| fail(usage))?,
        "profile": opts.value("--profile").ok_or_else(|| fail(usage))?,
        "dataMode": if opts.has("--discard-data") { "discard" } else { "keep" },
        "dryRun": opts.has("--dry-run"),
    });
    match (opts.value("--build"), opts.word(0)) {
        (Some(build), None) => body["buildId"] = json!(build),
        (None, Some(project)) => {
            body["repo"] = json!(project);
            if let Some(p) = opts.value("--platform") { body["platform"] = json!(p); }
            if let Some(b) = opts.value("--branch") { body["branch"] = json!(b); }
        }
        _ => return Err(fail(usage)),
    }
    let reply = store.post("/resign", body).await?;
    if opts.has("--json") {
        print_json(&reply);
        return Ok(());
    }
    let jobs = reply["jobs"].as_array().cloned().unwrap_or_default();
    let mut last = "success".to_string();
    for job in &jobs {
        println!("#{} {} for {}", job["id"], text(job, "kind"), text(job, "device"));
    }
    for job in &jobs {
        last = follow(store, job["id"].as_u64().unwrap_or(0)).await?;
    }
    if last == "success" { Ok(()) } else { Err(fail("The re-sign didn't succeed (the log above says why).")) }
}

// ---- delivery ----

async fn ota(store: &Store, opts: &Opts) -> Result<(), Error> {
    opts.only(&[])?;
    let state = store.get("/state").await?;
    let entries: Vec<(String, Value)> = state["ota"].as_object().into_iter().flatten().map(|(k, v)| (k.clone(), v.clone())).collect();
    let Some(wanted) = opts.word(0) else {
        if opts.has("--json") {
            print_json(&state["ota"]);
            return Ok(());
        }
        if entries.is_empty() {
            println!("No OTA installs yet: every build the Store makes for an iPhone, iPad or Vision Pro is one.");
        }
        for (key, e) in &entries {
            println!("{:<44} {} {} ({}) {}@{}  {}", text(e, "slug"), text(e, "name"), text(e, "version"), text(e, "buildNo"), text(e, "branch"), text(e, "commit"),
                key.split('|').nth(1).unwrap_or(""));
        }
        return Ok(());
    };
    let matching: Vec<&Value> = entries.iter().map(|(_, e)| e).filter(|e| text(e, "slug") == wanted || text(e, "bundleId") == wanted).collect();
    let entry = match matching.as_slice() {
        [one] => *one,
        [] => return Err(fail(format!("No OTA install of {wanted}. `apparition store ota` lists them."))),
        _ => return Err(fail(format!("{wanted} has one per platform: name its slug (`apparition store ota` lists them)."))),
    };
    let link = store.get(&format!("/ota/{}/link", segment(text(entry, "slug")))).await?;
    let manifest = text(&link, "manifestUrl");
    if opts.has("--json") {
        print_json(&link);
        return Ok(());
    }
    if !manifest.starts_with("https://") {
        eprintln!("note: iOS installs only from https. Set publicUrl in the Store's config.json to its https address (`tailscale serve`).");
    }
    println!("itms-services://?action=download-manifest&url={}", segment(manifest));
    println!("(Open it on the device within an hour. Manifest: {manifest})");
    Ok(())
}

async fn altsource(store: &Store, opts: &Opts) -> Result<(), Error> {
    opts.only(&[])?;
    let links = store.get("/altsource").await?;
    if opts.has("--json") {
        print_json(&links);
        return Ok(());
    }
    if links["enabled"] != true {
        println!("The AltStore source is off: set \"altSource\": {{\"enabled\": true}} in the Store's config.json.");
        return Ok(());
    }
    println!("All apps: {}", links["combined"]["url"].as_str().unwrap_or(""));
    for link in links["sources"].as_array().into_iter().flatten() {
        println!("{}: {}", text(link, "platform"), text(link, "url"));
    }
    println!("(In AltStore or SideStore on the device: {})", links["combined"]["altstore"].as_str().unwrap_or(""));
    for b in links["building"].as_array().into_iter().flatten() {
        println!("building {} (#{})", text(b, "key"), b["jobId"]);
    }
    println!("Add a link in AltStore (Sources, +). Downloads build the commit they name, the first time.");
    Ok(())
}

async fn mac(store: &Store, opts: &Opts) -> Result<(), Error> {
    opts.only(&["--build-now"])?;
    let slug = opts.word(0).ok_or_else(|| fail("usage: apparition store mac <app slug> [--branch <b>] [--build-now]  (slugs: `apparition store projects`)"))?;
    let branch = opts.value("--branch");
    if opts.has("--build-now") {
        let reply = store.post(&format!("/mac/{}/build", segment(slug)), json!({ "branch": branch })).await?;
        if reply["ready"] == true {
            println!("Already built: {}", text(&reply, "url"));
            return Ok(());
        }
        println!("Building {} [{}] as #{}.", &text(&reply, "commit")[..text(&reply, "commit").len().min(10)], text(&reply, "branch"), reply["jobId"]);
        let status = follow(store, reply["jobId"].as_u64().unwrap_or(0)).await?;
        if status != "success" {
            return Err(fail("The Mac build didn't succeed."));
        }
    }
    let path = match branch { Some(b) => format!("/mac/{}/link?branch={}", segment(slug), segment(b)), None => format!("/mac/{}/link", segment(slug)) };
    let link = store.get(&path).await?;
    if opts.has("--json") {
        print_json(&link);
        return Ok(());
    }
    println!("{}", text(&link, "url"));
    println!("(Works for {} days; the latest build of the branch, built on first fetch.) To fetch and unpack:\n{}", link["days"], text(&link, "curl"));
    Ok(())
}

async fn traces(store: &Store, opts: &Opts) -> Result<(), Error> {
    opts.only(&[])?;
    if opts.word(0) == Some("delete") {
        let id = opts.word(1).ok_or_else(|| fail("usage: apparition store traces delete <trace id>"))?;
        store.post(&format!("/traces/{}/delete", segment(id)), json!({})).await?;
        println!("Deleted trace {id}.");
        return Ok(());
    }
    let reply = store.get("/traces").await?;
    if opts.has("--json") {
        print_json(&reply);
        return Ok(());
    }
    let list = reply["traces"].as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        println!("No traces uploaded.");
    }
    for t in &list {
        println!("{}  {} {}  {}  {}", text(t, "id"), t.pointer("/app/name").and_then(Value::as_str).unwrap_or(""),
            t.pointer("/app/version").and_then(Value::as_str).unwrap_or(""), t.pointer("/device/name").and_then(Value::as_str).unwrap_or(""),
            t["receivedAt"].as_u64().map(ago).or_else(|| t["receivedAt"].as_str().map(String::from)).unwrap_or_default());
    }
    println!("`apparition crash-reports show <id>` reads one.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_in_any_order() {
        let args: Vec<String> = ["App", "--device", "AVP", "--run", "--device", "Phone", "--local"].iter().map(|s| s.to_string()).collect();
        let opts = Opts::parse(&args).unwrap();
        assert_eq!(opts.word(0), Some("App"));
        assert_eq!(opts.all("--device"), ["AVP", "Phone"]);
        assert!(opts.has("--run") && opts.has("--local"));
        assert!(opts.only(&["--run"]).is_ok());
        assert!(opts.only(&[]).is_err());
        assert!(Opts::parse(&["--device".to_string()]).is_err());
    }

    #[test]
    fn segments_are_encoded() {
        assert_eq!(segment("com.example.app-iOS"), "com.example.app-iOS");
        assert_eq!(segment("a b/c"), "a%20b%2Fc");
    }
}
