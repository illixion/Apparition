//! Driving a Mac's Apparition Store from here: `apparition mac`, `apparition deploy` and
//! `apparition mcp`.
//!
//! On a computer paired with a Mac for developer access (`apparition mac pair`, or the app's
//! Pair with a Mac), requests go over the companion's encrypted channel to that Mac, at home
//! over the LAN and away over the tailnet. No password and no Store token live here: the pairing
//! is the credential, the Mac's person approved it, and unpairing there revokes it.
//!
//! On the Mac the app runs on, with nothing paired, they go to the app's own Store on loopback,
//! with the token it leaves in `developer.json` for this user.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use apparition_core::api::paired_mac::{self, Access, Reply, Upload};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

use crate::{Error, anisette_dir, fail};

/// Where requests go.
pub(crate) enum Target {
    /// Another computer's Store this one signed in to with its password (`apparition store login`).
    Remote(crate::login::Login),
    /// The Mac this computer is paired with, through the channel.
    Paired { dir: PathBuf, name: String },
    /// This Mac's own app.
    Local { address: SocketAddr, token: String },
}

impl Target {
    pub(crate) fn find(prefer_local: bool) -> Result<Target, Error> {
        let dir = anisette_dir();
        let paired = paired_mac::status(&dir).filter(|p| p.developer);
        let local = local_store();
        // A sign-in is something the person did on purpose, and is the newer of the two ways to
        // reach another computer; `--local` still means this one, and `store logout` forgets it.
        let signed_in = if prefer_local && local.is_some() { None } else { crate::login::load() };
        if let Some(login) = signed_in {
            return Ok(Target::Remote(login));
        }
        match (paired, local) {
            (_, Some((address, token))) if prefer_local => Ok(Target::Local { address, token }),
            (Some(paired), _) => Ok(Target::Paired { dir, name: paired.mac_name }),
            (None, Some((address, token))) => Ok(Target::Local { address, token }),
            (None, None) => Err(fail(match paired_mac::status(&dir) {
                Some(p) => format!("{} paired this computer for anisette only. Pair again with `apparition mac pair {}` and allow developer access on the Mac.", p.mac_name, p.address),
                None => "Open the Apparition app on this computer (its Store starts with it), run `apparition store serve`, sign in to a Store with its password (`apparition store login <address>`), or pair this computer with your build Mac: `apparition mac pair <address>`.".into(),
            })),
        }
    }

    pub(crate) fn name(&self) -> String {
        match self {
            Target::Paired { name, .. } => name.clone(),
            Target::Local { .. } => "this computer".into(),
            Target::Remote(login) => login.name(),
        }
    }

    /// How this computer reaches it, for `apparition store status`.
    pub(crate) fn route(&self) -> String {
        match self {
            Target::Paired { name, .. } => format!("paired with {name} (apparition mac pair)"),
            Target::Local { .. } => "running on this computer".into(),
            Target::Remote(login) => format!("signed in at {} with its password (apparition store login)", login.url),
        }
    }

    pub(crate) async fn request(&self, method: &str, path: &str, content_type: Option<&str>, body: Upload<'_>) -> Result<Reply, Error> {
        match self {
            Target::Paired { dir, .. } => paired_mac::developer(dir, method, path, content_type, body).await,
            Target::Local { address, token } => paired_mac::local_developer(*address, token, method, path, content_type, body).await,
            Target::Remote(login) => login.request(method, path, content_type, body).await,
        }
    }
}

/// The Store running on this computer (the app's, or `apparition store serve`), if one does.
fn local_store() -> Option<(SocketAddr, String)> {
    let (route, _) = apparition_service::hosted::running(&apparition_service::hosted::store_dir())?;
    Some((route.address, route.token))
}

pub async fn mac(args: &[String]) -> Result<(), Error> {
    let dir = anisette_dir();
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["pair", address, rest @ ..] => {
            let access = match rest {
                [] => Access::Both,
                ["--developer-only"] => Access::Developer,
                ["--anisette-only"] => Access::Anisette,
                _ => return Err(fail("usage: apparition mac pair <address> [--developer-only | --anisette-only]")),
            };
            println!("Asking {address} to pair…");
            let paired = paired_mac::pair_for(&dir, address, access, |code| {
                println!("Code: {code}\nOn the Mac, allow this computer if it shows the same code.");
            })
            .await?;
            println!("Paired with {}.", paired.mac_name);
            describe(&paired);
            Ok(())
        }
        ["status"] => {
            match paired_mac::status(&dir) {
                Some(paired) => {
                    println!("Paired with {}, at {}.", paired.mac_name, paired.address);
                    describe(&paired);
                }
                None => println!("This computer isn't paired with a Mac."),
            }
            if local_store().is_some() {
                println!("Apparition's Store runs on this computer too: `apparition mcp --local` uses it.");
            }
            Ok(())
        }
        ["forget"] => {
            if paired_mac::forget(&dir) {
                println!("Forgot the paired Mac. Unpair this computer there too (its Apparition's paired list).");
            } else {
                println!("This computer isn't paired with a Mac.");
            }
            Ok(())
        }
        _ => Err(fail("usage: apparition mac pair <address> [--developer-only | --anisette-only] | status | forget")),
    }
}

fn describe(paired: &paired_mac::Summary) {
    if paired.anisette {
        println!("- Sign-ins here use its anisette.");
    }
    if paired.developer {
        println!("- `apparition deploy <app.ipa>` sends builds to it, and `apparition mcp` gives agents here its apps' debug servers.");
    } else {
        println!("- No developer access: pair again without --anisette-only to deploy and debug through it.");
    }
}

pub async fn deploy(args: &[String]) -> Result<(), Error> {
    let mut file = None;
    let mut device = None;
    let mut local = false;
    let mut keep_only = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--device" => device = Some(iter.next().ok_or_else(|| fail("--device needs a device name"))?.clone()),
            "--local" => local = true,
            "--keep" => keep_only = true,
            other if file.is_none() && !other.starts_with("--") => file = Some(PathBuf::from(other)),
            other => return Err(fail(format!("unknown argument {other}"))),
        }
    }
    let file = file.ok_or_else(|| fail("usage: apparition deploy <app.ipa> [--device <name>] [--keep] [--local]"))?;
    let target = Target::find(local)?;
    println!("Sending {} to {}…", file.display(), target.name());
    let reply = send_build(&target, &file, device.as_deref(), !keep_only).await?;
    println!("{}", reply["install"]["message"].as_str().unwrap_or("Sent."));
    Ok(())
}

/// Uploads `file` to the Store, as a kept build. With a device named and `install` set, the Mac
/// also tries to install it itself through its own pairing (and signing), and falls back to
/// leaving it for the device's Apparition when it can't; the reply's `install` says which.
async fn send_build(target: &Target, file: &Path, device: Option<&str>, install: bool) -> Result<Value, Error> {
    if !file.is_file() {
        return Err(fail(format!("{} isn't a file. Pass the .ipa your build made (xtool: xtool dev build, then the .ipa it names).", file.display())));
    }
    let name = file.file_name().and_then(|n| n.to_str()).unwrap_or("app.ipa");
    let mut path = format!("/api/service/v1/deploy?file={}", encode(name));
    if let Some(device) = device {
        path.push_str(&format!("&device={}", encode(device)));
        if install {
            path.push_str("&install=auto");
        }
    }
    let reply = target.request("POST", &path, Some("application/octet-stream"), Upload::File(file)).await?;
    let body: Value = serde_json::from_slice(&reply.body).unwrap_or(Value::Null);
    if reply.status != 201 {
        let why = body["error"].as_str().map(String::from).unwrap_or_else(|| String::from_utf8_lossy(&reply.body).trim().to_string());
        return Err(fail(format!("{} refused the build (HTTP {}): {why}", target.name(), reply.status)));
    }
    Ok(body)
}

fn encode(text: &str) -> String {
    text.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

/// The tool the bridge adds: the file is on this computer, so the Mac can't read it itself.
fn deploy_tool() -> Value {
    json!({
        "name": "deploy",
        "description": "Send an app built on this computer (an .ipa from xtool, Xcode or any build) to the Mac's Apparition Store, to install on a device. With `device` named, the Mac installs it itself when it is paired with that device and has a certificate for it (signing the app there); otherwise the build waits on the Mac and the reply says to open Apparition on the device, which installs it with its own signing. Use it after building, instead of copying files to the Mac.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "The .ipa on this computer." },
                "device": { "type": "string", "description": "The device it's for, as the person calls it. The Mac installs on it when it can; optional." },
                "install": { "type": "boolean", "description": "Set false to only keep the build on the Mac and not try to install it (default true when a device is named)." }
            },
            "required": ["path"]
        }
    })
}

/// `apparition mcp`: an MCP server on stdin and stdout for an agent on this computer, carrying
/// each message to the Mac's Store (`/mcp`), plus `deploy` for builds made here.
pub async fn mcp(args: &[String]) -> Result<(), Error> {
    let local = match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        [] => false,
        ["--local"] => true,
        _ => return Err(fail("usage: apparition mcp [--local]")),
    };
    let target = Arc::new(Target::find(local)?);
    eprintln!("apparition mcp: the apps' debug servers through {}", target.name());
    let stdout = Arc::new(tokio::sync::Mutex::new(tokio::io::stdout()));
    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    let mut tasks = tokio::task::JoinSet::new();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let (target, stdout) = (target.clone(), stdout.clone());
        // Each message on its own, so a slow call doesn't hold up a quick one.
        tasks.spawn(async move {
            if let Some(reply) = answer(&target, &line).await {
                let mut out = stdout.lock().await;
                let _ = out.write_all(format!("{reply}\n").as_bytes()).await;
                let _ = out.flush().await;
            }
        });
    }
    while tasks.join_next().await.is_some() {}
    Ok(())
}

/// The reply to one message, or `None` for a notification.
async fn answer(target: &Target, line: &str) -> Option<Value> {
    let Ok(message) = serde_json::from_str::<Value>(line) else {
        return Some(json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": "parse error" } }));
    };
    let id = message.get("id").cloned();
    if message["method"] == "tools/call" && message["params"]["name"] == "deploy" {
        let args = &message["params"]["arguments"];
        let result = match args["path"].as_str() {
            None => tool_text("`path` is required: the .ipa on this computer", true),
            Some(path) => match send_build(target, Path::new(path), args["device"].as_str(), args["install"].as_bool() != Some(false)).await {
                Ok(reply) => {
                    let mut out = json!({ "ok": true, "buildId": reply["buildId"], "app": reply["app"], "platform": reply["platform"],
                        "installed": reply["install"]["installed"].as_bool() == Some(true), "message": reply["install"]["message"] });
                    if out["installed"] != true {
                        out["hint"] = json!("tell the person to open Apparition on the device to finish installing");
                    }
                    tool_text(&out.to_string(), false)
                }
                Err(error) => tool_text(&error.to_string(), true),
            },
        };
        return Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }));
    }
    let reply = match target.request("POST", "/mcp", Some("application/json"), Upload::Bytes(line.as_bytes())).await {
        Ok(reply) => reply,
        Err(error) => {
            return id.map(|id| json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32000,
                "message": format!("couldn't reach {}'s Apparition Store: {error}", target.name()) } }));
        }
    };
    if reply.status == 202 || reply.body.is_empty() {
        return None;
    }
    let mut value: Value = match serde_json::from_slice(&reply.body) {
        Ok(value) => value,
        Err(_) => return id.map(|id| json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32000,
            "message": format!("{} answered HTTP {}: {}", target.name(), reply.status, String::from_utf8_lossy(&reply.body).trim()) } })),
    };
    if message["method"] == "tools/list" {
        if let Some(tools) = value.pointer_mut("/result/tools").and_then(Value::as_array_mut) {
            tools.push(deploy_tool());
        }
    }
    Some(value)
}

fn tool_text(text: &str, error: bool) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": error })
}

#[cfg(test)]
mod tests {
    #[test]
    fn names_are_encoded_for_the_query() {
        assert_eq!(super::encode("My App 2.ipa"), "My%20App%202.ipa");
        assert_eq!(super::encode("a&b=c"), "a%26b%3Dc");
    }
}

/// The whole path on real sockets: a Mac's companion offering its Store, a computer pairing for
/// developer access, then the agent's MCP messages and a deploy through the encrypted channel.
#[cfg(test)]
mod end_to_end {
    use super::*;
    use apparition_service::devices::{App, Device, Devices, Fut, Installed};
    use std::io::Write;

    /// A Mac paired with one phone, which installs whatever it is given.
    struct Phone(std::sync::Mutex<Vec<String>>);

    impl Devices for Phone {
        fn list(&self) -> Fut<'_, Vec<Device>> {
            Box::pin(async { Ok(vec![Device { udid: "AAAA-1".into(), name: "ixPhone".into(), platform: "iOS".into(), model: "iPhone17,1".into() }]) })
        }
        fn apps(&self, _: String) -> Fut<'_, Vec<App>> {
            Box::pin(async { Ok(Vec::new()) })
        }
        fn install(&self, device: String, ipa: PathBuf) -> Fut<'_, Installed> {
            Box::pin(async move {
                assert!(ipa.is_file());
                self.0.lock().unwrap().push(device);
                Ok(Installed { bundle_id: "com.example.thing".into(), device: "ixPhone".into(), seconds: 0.5, queued: false, signed_with: None })
            })
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_agent_elsewhere_drives_the_macs_store_and_deploys() {
        let mac = tempfile::tempdir().unwrap();
        let computer = tempfile::tempdir().unwrap();
        // The Mac: its Store on loopback, and its companion on free ports (relay, +1, +2).
        std::fs::write(mac.path().join("config.json"), json!({ "port": 1, "buildManifestDir": "builds", "repos": [] }).to_string()).unwrap();
        let mut settings = apparition_service::server::Settings::new(mac.path().join("config.json"), mac.path().join("data"), mac.path().join("public"));
        settings.public_dir = None;
        settings.host_token = Some("store-token-store-token-store-token-store-t".into());
        settings.key_dir = mac.path().join("keys");
        settings.debug_state_dir = mac.path().join("debugtrace");
        settings.browser = Arc::new(apparition_service::debug::Fixed(vec![]));
        let phone = Arc::new(Phone(Default::default()));
        settings.devices = Some(phone.clone());
        let (address, store) = apparition_service::server::serve_loopback(settings).await.unwrap();
        let relay_port = (0..50).map(|i| 40_000 + (std::process::id() as u16 % 5_000) * 3 + i * 3)
            .find(|p| (0..3).all(|k| std::net::TcpListener::bind(("0.0.0.0", p + k)).is_ok())).unwrap();
        let log: apparition_core::relay::Log = Arc::new(|_: &str| {});
        let companion = Arc::new(apparition_core::companion::Companion::start(relay_port, 1, false, mac.path(), log).await.unwrap());
        let devices = companion.devices.clone();
        companion.devices.on_pairing(Arc::new(move |id, request| {
            assert_eq!(request.grants, ["developer"]);
            devices.decide(id, true);
        }));
        assert!(paired_mac::pair_for(computer.path(), &format!("127.0.0.1:{relay_port}"), Access::Both, |_| {}).await.is_err(),
            "nothing is offered until the Store is");
        companion.set_developer(Some(apparition_core::companion_channel::DeveloperRoute {
            address, token: "store-token-store-token-store-token-store-t".into(),
        }));
        let paired = paired_mac::pair_for(computer.path(), &format!("127.0.0.1:{relay_port}"), Access::Both, |_| {}).await.unwrap();
        assert!(paired.developer && !paired.anisette);
        let target = Target::Paired { dir: computer.path().to_path_buf(), name: paired.mac_name };

        let init = answer(&target, r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#).await.unwrap();
        assert_eq!(init["result"]["serverInfo"]["name"], "debugtrace-apps", "{init}");
        assert!(answer(&target, r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).await.is_none());
        let tools = answer(&target, r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#).await.unwrap();
        let names: Vec<&str> = tools["result"]["tools"].as_array().unwrap().iter().filter_map(|t| t["name"].as_str()).collect();
        assert!(names.contains(&"apps") && names.contains(&"call") && names.last() == Some(&"deploy"), "{names:?}");
        let apps = answer(&target, r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"apps","arguments":{}}}"#).await.unwrap();
        assert!(apps["result"]["content"][0]["text"].is_string(), "{apps}");

        let ipa = computer.path().join("Thing.ipa");
        let mut zip = zip::ZipWriter::new(std::fs::File::create(&ipa).unwrap());
        zip.start_file("Payload/Thing.app/Info.plist", zip::write::SimpleFileOptions::default()).unwrap();
        zip.write_all(br#"<?xml version="1.0"?><plist version="1.0"><dict><key>CFBundleIdentifier</key><string>com.example.thing</string><key>CFBundleName</key><string>Thing</string><key>CFBundleSupportedPlatforms</key><array><string>iPhoneOS</string></array></dict></plist>"#).unwrap();
        zip.finish().unwrap();
        let message = json!({ "jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": { "name": "deploy", "arguments": { "path": ipa, "device": "ixPhone" } } });
        let deployed = answer(&target, &message.to_string()).await.unwrap();
        let text: Value = serde_json::from_str(deployed["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        // A device named: the Mac installs it itself, through its own pairing.
        assert_eq!((text["installed"].as_bool(), text["message"].as_str()), (Some(true), Some("Thing is installed on ixPhone.")), "{deployed}");
        assert!(text.get("hint").is_none());
        assert_eq!(*phone.0.lock().unwrap(), ["AAAA-1"]);
        // install: false only keeps it, for the device's Apparition.
        let message = json!({ "jsonrpc": "2.0", "id": 6, "method": "tools/call", "params": { "name": "deploy", "arguments": { "path": ipa, "device": "ixPhone", "install": false } } });
        let kept = answer(&target, &message.to_string()).await.unwrap();
        let kept: Value = serde_json::from_str(kept["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        let expected = format!("Thing is on {}. Open Apparition on ixPhone to install it.", apparition_service::HERE);
        assert_eq!((kept["installed"].as_bool(), kept["message"].as_str()), (Some(false), Some(expected.as_str())));
        assert_eq!(phone.0.lock().unwrap().len(), 1, "nothing more was installed");
        let id = text["buildId"].as_str().unwrap();
        let manifest: Value = serde_json::from_slice(&std::fs::read(mac.path().join("builds").join(id).join("manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest["origin"]["computer"], apparition_core::relay::machine_name().as_str());

        // Unpaired on the Mac: the next message says so instead of reaching the Store.
        let device_id = companion.devices.list()[0].id.clone();
        assert!(companion.devices.forget(&device_id));
        let refused = answer(&target, r#"{"jsonrpc":"2.0","id":5,"method":"ping"}"#).await.unwrap();
        assert!(refused["error"]["message"].as_str().unwrap().contains("Pair with it again"), "{refused}");
        store.stop();
    }
}

/// `apparition deploy-signing import | list`: the certificate and profiles this computer's Store signs sent
/// builds with, kept in the device store's `signing` folder (the Apparition app reads the same).
pub fn deploy_signing(args: &[String]) -> Result<(), Error> {
    let store = apparition_core::api::DeviceStore::default();
    let dir = store.dir.join("signing");
    let usage = || fail("usage: apparition deploy-signing import <cert.p12> --profile <file>... [--device <name or UDID>] | list");
    match args.first().map(String::as_str) {
        Some("list") if args.len() == 1 => {
            let inventory = apparition_core::api::signing_inventory(&dir, None);
            let identities = inventory["identities"].as_array().cloned().unwrap_or_default();
            if identities.is_empty() {
                println!("No certificate is imported on this computer. `apparition deploy-signing import <cert.p12> --profile <file>...` adds one.");
            }
            for identity in &identities {
                println!("{} ({}), expires {}{}", identity["name"].as_str().unwrap_or("?"), identity["teamName"].as_str().unwrap_or("?"),
                    identity["expires"].as_str().unwrap_or("?"), if identity["expired"] == true { " (expired)" } else { "" });
            }
            for profile in inventory["profiles"].as_array().into_iter().flatten() {
                println!("  profile {} for {}, {} devices, expires {}", profile["name"].as_str().unwrap_or("?"), profile["appIdPattern"].as_str().unwrap_or("?"),
                    profile["deviceCount"], profile["expires"].as_str().unwrap_or("?"));
            }
            Ok(())
        }
        Some("import") => {
            let (mut p12, mut profiles, mut wanted) = (None, Vec::new(), None);
            let mut iter = args[1..].iter();
            while let Some(arg) = iter.next() {
                match arg.as_str() {
                    "--profile" => profiles.push(PathBuf::from(iter.next().ok_or_else(usage)?)),
                    "--device" => wanted = Some(iter.next().ok_or_else(usage)?.clone()),
                    other if p12.is_none() && !other.starts_with("--") => p12 = Some(PathBuf::from(other)),
                    _ => return Err(usage()),
                }
            }
            let p12 = p12.ok_or_else(usage)?;
            if profiles.is_empty() {
                return Err(fail("add at least one provisioning profile with --profile <file>; it must list the device"));
            }
            // A profile's device list is checked against this device, so a mismatch says so now.
            let udid = match wanted {
                Some(w) => {
                    let known = store.devices();
                    Some(known.iter().find(|d| d.udid.eq_ignore_ascii_case(&w) || d.name.eq_ignore_ascii_case(&w)).map(|d| d.udid.clone())
                        .ok_or_else(|| fail(format!("this computer is not paired with '{w}'")))?)
                }
                None => None,
            };
            std::fs::create_dir_all(&dir)?;
            let password = rpassword::prompt_password(format!("Password for {}: ", p12.display())).unwrap_or_default();
            let identity = apparition_core::api::import_identity(&dir, &std::fs::read(&p12)?, password)?;
            println!("Imported {}.", identity["identity"]["name"].as_str().unwrap_or("the certificate"));
            for profile in profiles {
                let imported = apparition_core::api::import_profile(&dir, std::fs::read(&profile)?, udid.as_deref())?;
                let profile = &imported["profile"];
                let covers = match profile["includesDevice"].as_bool() { Some(true) => ", lists the device", Some(false) => ", does NOT list the device", None => "" };
                println!("Imported profile {} for {}{covers}.", profile["name"].as_str().unwrap_or("?"), profile["appIdPattern"].as_str().unwrap_or("?"));
            }
            println!("The Apparition app here signs with it when a build is sent here for a device a profile lists.");
            Ok(())
        }
        _ => Err(usage()),
    }
}

/// `apparition updates`: which installed apps are behind their branch or a local package, from the
/// Mac's Store, and (`--run`) one build each that brings them up to date.
pub async fn updates(args: &[String]) -> Result<(), Error> {
    let has = |flag: &str| args.iter().any(|a| a == flag);
    let mut auto = None;
    let mut only: Vec<String> = Vec::new();
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--run" | "--check" | "--json" | "--local" => {}
            "--auto" => auto = Some(match rest.next().map(String::as_str) {
                Some("on") => true,
                Some("off") => false,
                _ => return Err(fail("usage: apparition updates --auto on|off")),
            }),
            "--only" => only.push(rest.next().ok_or_else(|| fail("--only needs <device>|<bundle id>"))?.clone()),
            other => return Err(fail(format!("Unknown option: {other}\nusage: apparition updates [--check] [--run [--only <device>|<bundle id>]...] [--auto on|off] [--json] [--local]"))),
        }
    }
    let target = Target::find(has("--local"))?;
    let call = |method: &'static str, path: &'static str, body: Value| {
        let target = &target;
        async move {
            let bytes = body.to_string();
            let reply = target.request(method, path, Some("application/json"), Upload::Bytes(if method == "GET" { &[] } else { bytes.as_bytes() })).await?;
            let value: Value = serde_json::from_slice(&reply.body).unwrap_or(Value::Null);
            if reply.status == 501 || (reply.status == 404 && value["error"] == "Unknown service operation.") {
                return Err(fail(format!("{}'s Store doesn't plan updates yet: update the Apparition app there.", target.name())));
            }
            if reply.status != 200 {
                return Err(fail(format!("{} answered HTTP {}: {}", target.name(), reply.status, value["error"].as_str().unwrap_or(""))));
            }
            Ok::<Value, Error>(value)
        }
    };
    if let Some(enabled) = auto {
        let reply = call("POST", "/api/service/v1/updates/auto", json!({ "enabled": enabled })).await?;
        println!("Automatic updates are {}.", if reply["auto"]["enabled"] == true {
            format!("on: apps update once their sources have been still for {} minutes", reply["auto"]["settleMinutes"])
        } else { "off".to_string() });
        return Ok(());
    }
    let report = if has("--check") { call("POST", "/api/service/v1/updates/check", Value::Null).await? } else { call("GET", "/api/service/v1/updates", Value::Null).await? };
    if has("--run") {
        let body = if only.is_empty() { json!({}) } else { json!({ "only": only }) };
        let reply = call("POST", "/api/service/v1/updates/run", body).await?;
        if has("--json") {
            println!("{}", serde_json::to_string_pretty(&reply).unwrap_or_default());
            return Ok(());
        }
        let jobs = reply["jobs"].as_array().cloned().unwrap_or_default();
        if jobs.is_empty() {
            println!("Nothing to start: everything is up to date, or its builds are already queued.");
        }
        for job in jobs {
            println!("#{} {} [{}] for {}", job["id"], job["repo"].as_str().unwrap_or(""), job["branch"].as_str().unwrap_or(""),
                job["devices"].as_array().into_iter().flatten().filter_map(Value::as_str).collect::<Vec<_>>().join(", "));
        }
        println!("Follow them on the Store page's Updates view, or with `apparition updates`.");
        return Ok(());
    }
    if has("--json") {
        println!("{}", serde_json::to_string_pretty(&report).unwrap_or_default());
        return Ok(());
    }
    let rows = report["rows"].as_array().cloned().unwrap_or_default();
    let behind: Vec<&Value> = rows.iter().filter(|r| r["status"] == "behind").collect();
    for r in &behind {
        println!("{} on {} ({}), {}: {} → {}", r["repo"].as_str().unwrap_or(""), r["device"].as_str().unwrap_or(""), r["platform"].as_str().unwrap_or(""),
            r["branch"].as_str().unwrap_or(""), r["installed"]["buildNo"].as_str().unwrap_or("?"), r["head"].as_str().unwrap_or(""));
        for reason in r["reasons"].as_array().into_iter().flatten() {
            let commits = reason["commits"].as_u64().unwrap_or(0);
            let what = if reason["rewritten"] == true { "history rewritten".to_string() }
                else if commits == 1 { reason["latest"].as_str().unwrap_or("").to_string() }
                else { format!("{commits} commits, latest: {}", reason["latest"].as_str().unwrap_or("")) };
            println!("    {}{}: {what}", reason["source"].as_str().unwrap_or(""), if reason["package"] == true { " (package)" } else { "" });
        }
    }
    let unknown = rows.iter().filter(|r| r["status"] == "unknown").count();
    let groups = report["groups"].as_array().map(Vec::len).unwrap_or(0);
    for job in report["builds"].as_array().into_iter().flatten().filter(|b| b["status"] == "running" || b["status"] == "queued") {
        println!("#{} {} for {}: {}", job["id"], job["repo"].as_str().unwrap_or(""),
            job["devices"].as_array().into_iter().flatten().filter_map(Value::as_str).collect::<Vec<_>>().join(", "), job["status"].as_str().unwrap_or(""));
    }
    println!("{} behind, {} up to date{} — {groups} build{} would update them{}. Automatic updates are {}.",
        behind.len(), rows.iter().filter(|r| r["status"] == "current").count(),
        if unknown > 0 { format!(", {unknown} can't tell") } else { String::new() },
        if groups == 1 { "" } else { "s" }, if groups > 0 { " (`apparition updates --run`)" } else { "" },
        if report["auto"]["enabled"] == true { "on" } else { "off" });
    Ok(())
}
