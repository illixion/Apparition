//! Microsoft Edge WebView2, which draws the window on Windows. Windows 10 and 11 normally have it,
//! but some "debloat" tools that remove Edge remove it too, and then the window can't open at all.
//! A program with no console shows no panic, so without this the app would just not start, as far
//! as the person can tell. Checked before anything else: it is said in a plain Windows message box,
//! with the offer to fetch Microsoft's own installer (the Evergreen bootstrapper, the download
//! Microsoft links for this), check that Microsoft signed it, and run it.
//!
//! The command line (`apparition-cli.exe`) and the Windows service don't use WebView2.

use std::ffi::c_void;
use std::path::Path;
use std::process::Command;

use rootcause::prelude::*;

/// Microsoft's WebView2 Runtime bootstrapper, for x64 and ARM64 alike (it installs the right one).
const BOOTSTRAPPER: &str = "https://go.microsoft.com/fwlink/p/?LinkId=2124703";
/// Where a person finds it by hand.
const PAGE: &str = "https://developer.microsoft.com/microsoft-edge/webview2/";
/// The bootstrapper is about 2 MB.
const MAX_SIZE: usize = 32 << 20;

const MB_OK: u32 = 0x0;
const MB_YESNO: u32 = 0x4;
const MB_ICONERROR: u32 = 0x10;
const MB_ICONWARNING: u32 = 0x30;
const MB_SETFOREGROUND: u32 = 0x10000;
const IDYES: i32 = 6;

#[link(name = "user32")]
unsafe extern "system" {
    fn MessageBoxW(window: *mut c_void, text: *const u16, caption: *const u16, kind: u32) -> i32;
}

fn message(text: &str, kind: u32) -> i32 {
    let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    let (text, caption) = (wide(text), wide("Apparition"));
    // SAFETY: both strings are NUL-terminated UTF-16 that outlive the call; no owner window.
    unsafe { MessageBoxW(std::ptr::null_mut(), text.as_ptr(), caption.as_ptr(), kind | MB_SETFOREGROUND) }
}

fn installed() -> bool {
    // A debug build can pretend it isn't, to try the dialog on a PC that has it.
    if cfg!(debug_assertions) && std::env::var_os("APPARITION_PRETEND_NO_WEBVIEW2").is_some() {
        return false;
    }
    tauri::webview_version().is_ok()
}

/// Whether the window can open: WebView2 is there, or the person had Microsoft's installer put it
/// there just now. When this says no, the app quits.
pub fn ensure(runtime: &tokio::runtime::Runtime) -> bool {
    if installed() {
        return true;
    }
    let ask = "Apparition's window is drawn by Microsoft Edge WebView2, and this PC doesn't have it. Tools that remove Edge often remove it too.\n\n\
        Download Microsoft's WebView2 installer and run it now? Apparition checks that Microsoft signed it first. It then downloads the rest, about 150 MB.\n\n\
        Without it, apparition-cli.exe in Apparition's folder does everything from a terminal (apparition-cli help), and the background service doesn't need it.";
    if message(ask, MB_YESNO | MB_ICONWARNING) != IDYES {
        return false;
    }
    match runtime.block_on(install()) {
        Ok(()) if installed() => true,
        Ok(()) => {
            message(&format!("WebView2 still isn't there, so Apparition can't open its window. Microsoft's page for it: {PAGE}"), MB_OK | MB_ICONERROR);
            false
        }
        Err(error) => {
            message(&format!("Couldn't install WebView2: {error}\n\nMicrosoft's page for it: {PAGE}"), MB_OK | MB_ICONERROR);
            false
        }
    }
}

async fn install() -> Result<(), Report> {
    let response = reqwest::Client::builder()
        .user_agent(concat!("Apparition/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(120))
        .build()?
        .get(BOOTSTRAPPER)
        .send()
        .await
        .context("Can't reach Microsoft's download server")?
        .error_for_status()?;
    let bytes = response.bytes().await?;
    if bytes.len() > MAX_SIZE {
        bail!("Microsoft's server sent something too large to be the WebView2 installer");
    }
    let folder = tempfile::tempdir()?;
    let setup = folder.path().join("MicrosoftEdgeWebview2Setup.exe");
    std::fs::write(&setup, &bytes)?;
    if !signed_by_microsoft(&setup)? {
        bail!("the downloaded installer isn't signed by Microsoft, so it wasn't run");
    }
    // Through ShellExecute (Start-Process), which raises the administrator prompt the installer
    // asks for; waits until it is done.
    let status = powershell(&format!("Start-Process -FilePath '{}' -ArgumentList '/install' -Wait", quoted(&setup)?))?;
    if !status {
        bail!("Microsoft's installer didn't finish (it may have been cancelled)");
    }
    Ok(())
}

/// Windows' own Authenticode check of `file`: a valid signature, by Microsoft Corporation (whose
/// certificate signs Microsoft's downloads; Windows' own files are "Microsoft Windows").
fn signed_by_microsoft(file: &Path) -> Result<bool, Report> {
    signed_by(file, "Microsoft Corporation")
}

fn signed_by(file: &Path, signer: &str) -> Result<bool, Report> {
    powershell(&format!(
        "$s = Get-AuthenticodeSignature -LiteralPath '{}'; if ($s.Status -eq 'Valid' -and $s.SignerCertificate.Subject -like 'CN={signer},*') {{ exit 0 }} else {{ exit 1 }}",
        quoted(file)?
    ))
}

/// `path` inside a single-quoted PowerShell string.
fn quoted(path: &Path) -> Result<String, Report> {
    Ok(path.to_str().ok_or_else(|| report!("the temporary folder's path isn't valid Unicode"))?.replace('\'', "''"))
}

/// Runs `script` in Windows PowerShell, as an encoded command (which an execution policy for
/// script files doesn't block, as in updater.rs). True when it exits 0.
fn powershell(script: &str) -> Result<bool, Report> {
    use base64::Engine as _;
    use std::os::windows::process::CommandExt as _;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let encoded = base64::engine::general_purpose::STANDARD.encode(script.encode_utf16().flat_map(u16::to_le_bytes).collect::<Vec<u8>>());
    let root = std::env::var_os("SystemRoot").ok_or_else(|| report!("SystemRoot is missing"))?;
    let output = Command::new(Path::new(&root).join("System32/WindowsPowerShell/v1.0/powershell.exe"))
        .args(["-NoProfile", "-NonInteractive", "-EncodedCommand", &encoded])
        // PowerShell 7's module path, when the app was started from it, keeps Windows PowerShell
        // from loading its own modules (Get-AuthenticodeSignature's among them).
        .env_remove("PSModulePath")
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .context("Can't run Windows PowerShell")?;
    #[cfg(test)]
    if !output.status.success() {
        eprintln!("powershell: {}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    }
    Ok(output.status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file nobody signed isn't taken for Microsoft's.
    #[test]
    fn an_unsigned_file_is_refused() {
        let folder = tempfile::tempdir().unwrap();
        let file = folder.path().join("it's unsigned.exe");
        std::fs::write(&file, b"MZ not a program").unwrap();
        assert!(!signed_by_microsoft(&file).unwrap());
    }

    /// Windows' own PowerShell is signed (a catalog signature, which Get-AuthenticodeSignature
    /// reads too), by Microsoft Windows, which isn't the signer the download has to have.
    #[test]
    fn the_signer_is_checked() {
        let root = std::env::var_os("SystemRoot").unwrap();
        let powershell = Path::new(&root).join("System32/WindowsPowerShell/v1.0/powershell.exe");
        assert!(signed_by(&powershell, "Microsoft Windows").unwrap());
        assert!(!signed_by_microsoft(&powershell).unwrap());
    }

    /// The real download, checked and not run. Needs the internet: `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn microsofts_bootstrapper_is_signed_by_microsoft() {
        apparition_core::api::init();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let bytes = runtime.block_on(async { reqwest::get(BOOTSTRAPPER).await.unwrap().bytes().await.unwrap() });
        let folder = tempfile::tempdir().unwrap();
        let file = folder.path().join("MicrosoftEdgeWebview2Setup.exe");
        std::fs::write(&file, &bytes).unwrap();
        assert!(signed_by_microsoft(&file).unwrap());
    }
}
