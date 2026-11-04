//! `apparition vm`: the anisette VM on this Mac (core's vm.rs), the same one the app's Companion tab sets up.

use std::path::PathBuf;

use rootcause::prelude::*;

pub const USAGE: &str = "usage: apparition vm setup [--ipsw <file>] [--keep-image] [--public-key <file>] | status [--json] | serve | quit | shutdown | force-stop | check | image [--ipsw <file>] | remove
       apparition vm screen | key <chord> | type <file> | click-text <text> [--menubar] | suspend | resume | network on|off   (for looking into a setup)
       every command takes --bundle <dir> (default ~/Library/Application Support/Apparition/anisette.vm)";

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub async fn run(args: &[String]) -> Result<(), Report> {
    use apparition_core::api::vm;

    let mut rest: Vec<&str> = Vec::new();
    let (mut bundle, mut ipsw, mut public_key, mut json, mut menubar, mut keep_image) = (vm::default_bundle(), None, None, false, false, false);
    let mut words = args.iter().map(String::as_str);
    while let Some(word) = words.next() {
        let mut path = || words.next().map(PathBuf::from).ok_or_else(|| report!("{word} needs a path"));
        match word {
            "--bundle" => bundle = path()?,
            "--ipsw" => ipsw = Some(path()?),
            "--public-key" => public_key = Some(path()?),
            "--json" => json = true,
            "--menubar" => menubar = true,
            "--keep-image" => keep_image = true,
            other => rest.push(other),
        }
    }
    let say = |progress: vm::Progress| match progress.fraction {
        Some(fraction) if fraction < 1.0 => println!("[{}] {} ({:.0}%)", progress.step, progress.text, fraction * 100.0),
        _ => println!("[{}] {}", progress.step, progress.text),
    };
    match rest.as_slice() {
        ["setup"] => {
            let options = vm::SetupOptions { bundle, ipsw, public_key, keep_image };
            let url = vm::setup(&options, &say).await?;
            println!("The anisette VM is set up and saved: its anisette is at {url}, and it resumes in a second when asked.");
            println!("Sign-ins here use it when this Mac has no anisette of its own; the app's companion offers it to devices.");
        }
        ["status"] => {
            let status = vm::status(&bundle).await;
            if json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                println!("{}", describe(&status));
            }
        }
        ["serve"] => println!("{}", vm::serve(&bundle).await?),
        ["quit"] => {
            vm::quit(&bundle).await?;
            println!("Saved and stopped; the next start resumes it.");
        }
        ["shutdown"] => vm::shutdown(&bundle).await?,
        ["force-stop"] => vm::force_stop(&bundle).await?,
        ["check"] => {
            let url = vm::serve(&bundle).await?;
            vm::check_with_apple(&url).await?;
            println!("Apple accepts the VM's anisette (a test sign-in got -20970, wrong password, rather than -3000076).");
        }
        ["image"] => println!("{}", vm::restore_image(ipsw.as_deref(), &say).await?.display()),
        ["remove"] => {
            vm::remove(&bundle).await?;
            println!("Moved {} to the Trash.", bundle.display());
        }
        ["screen"] => {
            let reply = vm::operate(&bundle, "screen", &[]).await?;
            println!("{}", reply["text"].as_str().unwrap_or_default());
            println!("(screenshot: {})", reply["path"].as_str().unwrap_or_default());
        }
        ["key", chord] => drop(vm::operate(&bundle, "key", &[("key", chord)]).await?),
        // From a file, so what is typed (a password, say) stays out of the shell's history.
        ["type", file] => drop(vm::operate(&bundle, "type", &[("text", &std::fs::read_to_string(file).context(format!("reading {file}"))?)]).await?),
        ["click-text", text] => {
            let region = if menubar { "menubar" } else { "all" };
            println!("{}", vm::operate(&bundle, "click-text", &[("text", text), ("region", region)]).await?);
        }
        [] | ["help"] => println!("{USAGE}"),
        [operation @ ("suspend" | "resume")] => println!("{}", vm::operate(&bundle, operation, &[]).await?),
        ["network", on @ ("on" | "off")] => println!("{}", vm::operate(&bundle, &format!("network-{on}"), &[]).await?),
        _ => bail!("{USAGE}"),
    }
    Ok(())
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn describe(status: &apparition_core::api::vm::Status) -> String {
    let mut lines = vec![format!("Bundle: {}", status.bundle.display())];
    let state = if let Some(why) = &status.unsupported {
        format!("Can't run here: {why}")
    } else if !status.exists {
        "Not set up (apparition vm setup)".to_string()
    } else if !status.ready {
        format!("Setting up, or stopped partway (apparition vm setup carries on){}", if status.installed { "; macOS is installed" } else { "" })
    } else {
        match &status.live {
            Some(live) if live.recovery => "In recoveryOS".to_string(),
            Some(live) => format!("Runner up, VM {}{}", live.phase, live.anisette.as_ref().map(|url| format!(", anisette at {url}")).unwrap_or_default()),
            None if status.suspended => "Saved, not running: the next request resumes it".to_string(),
            None => "Stopped: the next request boots it (about a minute)".to_string(),
        }
    };
    lines.push(state);
    lines.push(format!("Disk: {:.1} GB", status.disk_bytes as f64 / 1e9));
    lines.push(format!("{}: {}", status.image, if status.image_cached { "in the cache" } else { "not in the cache" }));
    lines.join("\n")
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
pub async fn run(_args: &[String]) -> Result<(), Report> {
    bail!("The anisette VM needs a Mac with Apple silicon")
}
