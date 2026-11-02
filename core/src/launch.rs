//! Launching an app over the tunnel through CoreDevice's appservice on the device, the way
//! `devicectl device process launch` does, with no CoreDevice on this side.

use idevice::{
    ReadWrite, RsdService,
    core_device::{AppServiceClient, OpenStdioSocketClient},
    rsd::RsdHandshake,
    tcp::handle::AdapterHandle,
};
use rootcause::prelude::*;

/// What a launch gave back: the new process, and its stdout/stderr when asked for.
pub struct Launched {
    pub pid: u64,
    /// The app's stdout and stderr (interleaved, as the device writes them), until it exits.
    pub console: Option<Box<dyn ReadWrite>>,
}

/// Launches `bundle_id` with `environment`, ending a running instance first. With `console`,
/// its stdio is bridged to the returned stream (devicectl's `--console`). With `suspended`, it
/// starts stopped, for a debugger to attach before it runs (what `jit` needs).
pub async fn launch(
    adapter: &mut AdapterHandle,
    handshake: &mut RsdHandshake,
    bundle_id: &str,
    arguments: &[&str],
    environment: plist::Dictionary,
    console: bool,
    suspended: bool,
) -> Result<Launched, Report> {
    let (stdio, uuid) = if console {
        let mut stdio = OpenStdioSocketClient::connect_rsd(adapter, handshake).await.context("stdio socket connect failed")?;
        let uuid = stdio.read_uuid().await.context("stdio socket gave no id")?;
        (Some(stdio.inner), Some(uuid))
    } else {
        (None, None)
    };
    let mut apps = AppServiceClient::connect_rsd(adapter, handshake).await.context("appservice connect failed")?;
    let launched = match apps.launch_application(bundle_id, arguments, true, suspended, Some(environment), None, uuid).await {
        Ok(launched) => launched,
        Err(error) => {
            let text = error.to_string();
            return Err(match device_reason(&text) {
                Some(reason) => report!("launch failed: {reason}"),
                None => report!("launch failed: {error}"),
            });
        }
    };
    Ok(Launched { pid: launched.pid as u64, console: stdio })
}

/// The innermost reason in a CoreDevice error: its nested userInfo dictionaries end in the
/// one that says why ("… because the device was not, or could not be, unlocked."), while the
/// whole thing, with its archived copies, runs to kilobytes.
fn device_reason(text: &str) -> Option<String> {
    let key = "NSLocalizedFailureReason\": String(\"";
    let start = text.rfind(key)? + key.len();
    let rest = &text[start..];
    let end = rest.find("\")")?;
    Some(rest[..end].replace("\\\"", "\""))
}

#[cfg(test)]
mod tests {
    #[test]
    fn device_reason_takes_the_innermost() {
        let text = r#"x {"NSLocalizedFailureReason": String("outer"), "u": {"NSLocalizedFailureReason": String("Unable to launch a because the device was not, or could not be, unlocked.")}}"#;
        assert_eq!(super::device_reason(text).as_deref(), Some("Unable to launch a because the device was not, or could not be, unlocked."));
        assert_eq!(super::device_reason("nothing"), None);
    }
}
