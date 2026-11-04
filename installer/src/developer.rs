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
enum Target {
    /// The Mac this computer is paired with, through the channel.
    Paired { dir: PathBuf, name: String },
    /// This Mac's own app.
    Local { address: SocketAddr, token: String },
}

impl Target {
    fn find(prefer_local: bool) -> Result<Target, Error> {
        let dir = anisette_dir();
        let paired = paired_mac::status(&dir).filter(|p| p.developer);
        let local = local_store();
        match (paired, local) {
            (_, Some((address, token))) if prefer_local => Ok(Target::Local { address, token }),
            (Some(paired), _) => Ok(Target::Paired { dir, name: paired.mac_name }),
            (None, Some((address, token))) => Ok(Target::Local { address, token }),
            (None, None) => Err(fail(match paired_mac::status(&dir) {
                Some(p) => format!("{} paired this computer for anisette only. Pair again with `apparition mac pair {}` and allow developer access on the Mac.", p.mac_name, p.address),
                None if cfg!(target_os = "macos") => "Open Apparition on this Mac (its Store starts with it), or pair this computer with your build Mac: `apparition mac pair <address>`.".into(),
                None => "Pair this computer with your Mac first: `apparition mac pair <address>`, with Apparition open on the Mac.".into(),
            })),
        }
    }

    fn name(&self) -> String {
        match self {
            Target::Paired { name, .. } => name.clone(),
            Target::Local { .. } => "this Mac".into(),
        }
    }

    async fn request(&self, method: &str, path: &str, content_type: Option<&str>, body: Upload<'_>) -> Result<Reply, Error> {
        match self {
            Target::Paired { dir, .. } => paired_mac::developer(dir, method, path, content_type, body).await,
            Target::Local { address, token } => paired_mac::local_developer(*address, token, method, path, content_type, body).await,
        }
    }
}

/// The app's Store on this Mac, if it runs: `developer.json` in the Store's folder.
fn local_store() -> Option<(SocketAddr, String)> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let home = PathBuf::from(std::env::var_os("HOME")?);
    let file = home.join("Library/Application Support/Apparition/Store/developer.json");
    let value: Value = serde_json::from_slice(&std::fs::read(file).ok()?).ok()?;
    let address: SocketAddr = value["url"].as_str()?.strip_prefix("http://")?.parse().ok()?;
    let token = value["token"].as_str()?.to_string();
    // Left behind by an app that didn't quit cleanly: nothing answers there.
    std::net::TcpStream::connect_timeout(&address, std::time::Duration::from_millis(300)).ok()?;
    Some((address, token))
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
                println!("Apparition's Store runs on this Mac too: `apparition mcp --local` uses it.");
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
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--device" => device = Some(iter.next().ok_or_else(|| fail("--device needs a device name"))?.clone()),
            "--local" => local = true,
            other if file.is_none() && !other.starts_with("--") => file = Some(PathBuf::from(other)),
            other => return Err(fail(format!("unknown argument {other}"))),
        }
    }
    let file = file.ok_or_else(|| fail("usage: apparition deploy <app.ipa> [--device <name>] [--local]"))?;
    let target = Target::find(local)?;
    println!("Sending {} to {}…", file.display(), target.name());
    let reply = send_build(&target, &file, device.as_deref()).await?;
    println!("{}", reply["install"]["message"].as_str().unwrap_or("Sent."));
    Ok(())
}

/// Uploads `file` to the Store, as a kept build for the device's Apparition to install.
async fn send_build(target: &Target, file: &Path, device: Option<&str>) -> Result<Value, Error> {
    if !file.is_file() {
        return Err(fail(format!("{} isn't a file. Pass the .ipa your build made (xtool: xtool dev build, then the .ipa it names).", file.display())));
    }
    let name = file.file_name().and_then(|n| n.to_str()).unwrap_or("app.ipa");
    let mut path = format!("/api/service/v1/deploy?file={}", encode(name));
    if let Some(device) = device {
        path.push_str(&format!("&device={}", encode(device)));
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
        "description": "Send an app built on this computer (an .ipa from xtool, Xcode or any build) to the Mac's Apparition Store, to install on a device. The device's Apparition installs it with its own signing: the reply says to open Apparition there. Use it after building, instead of copying files to the Mac.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "The .ipa on this computer." },
                "device": { "type": "string", "description": "The device it's for, as the person calls it (shown to them; optional)." }
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
            Some(path) => match send_build(target, Path::new(path), args["device"].as_str()).await {
                Ok(reply) => tool_text(&json!({ "ok": true, "buildId": reply["buildId"], "app": reply["app"], "platform": reply["platform"],
                    "message": reply["install"]["message"], "hint": "tell the person to open Apparition on the device to finish installing" }).to_string(), false),
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
#[cfg(all(test, target_os = "macos"))]
mod end_to_end {
    use super::*;
    use std::io::Write;

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
        assert_eq!(text["message"], "Thing is on the Mac. Open Apparition on ixPhone to install it.", "{deployed}");
        let id = text["buildId"].as_str().unwrap();
        let manifest: Value = serde_json::from_slice(&std::fs::read(mac.path().join("builds").join(id).join("manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest["origin"]["computer"], apparition_core::relay::machine_name().as_str());

        // Unpaired on the Mac: the next message says so instead of reaching the Store.
        let device_id = companion.devices.list()[0].id.clone();
        assert!(companion.devices.forget(&device_id));
        let refused = answer(&target, r#"{"jsonrpc":"2.0","id":5,"method":"ping"}"#).await.unwrap();
        assert!(refused["error"]["message"].as_str().unwrap().contains("Pair with it again"), "{refused}");
        store.abort();
    }
}
