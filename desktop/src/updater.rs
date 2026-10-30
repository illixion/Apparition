//! Desktop updates use the same pinned SSHSIG manifest as device installs. The package is
//! hashed before extraction or execution; OS code signing is not a substitute for that trust.

use std::{
    path::{Path, PathBuf},
    sync::Mutex,
    time::Duration,
};

use apparition_core::{api, release};
use rootcause::prelude::*;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use crate::{
    companion,
    flow::{Failure, installer_platform},
};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Finish {
    Restart,
    #[cfg(windows)]
    ExitForHelper,
}

#[derive(Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    version: Option<String>,
    phase: &'static str,
    message: String,
    can_install: bool,
}

#[derive(Default)]
pub struct Updates {
    status: Mutex<Status>,
    candidate: Mutex<Option<(release::Verified, release::Asset)>>,
    operation: tokio::sync::Mutex<()>,
}

#[derive(Clone, Debug)]
#[allow(dead_code)] // Each platform constructs only its own installation formats.
enum Installation {
    Mac(PathBuf),
    Windows,
    AppImage(PathBuf),
    Deb,
    Unsupported(&'static str),
}

impl Installation {
    fn extension(&self) -> &'static str {
        match self {
            Self::AppImage(_) => "AppImage",
            Self::Deb => "deb",
            _ if cfg!(target_os = "linux") => "AppImage",
            _ => "zip",
        }
    }

    fn supported(&self) -> bool {
        !matches!(self, Self::Unsupported(_))
    }
}

fn installation(app: &AppHandle) -> Result<Installation, Report> {
    if cfg!(debug_assertions) {
        return Ok(Installation::Unsupported(
            "Development builds are not replaced. Download a release to enable self-updates.",
        ));
    }
    let executable = tauri::process::current_binary(&app.env())?;
    #[cfg(target_os = "macos")]
    {
        let bundle = executable
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent);
        if let Some(bundle) = bundle.filter(|p| p.extension().is_some_and(|ext| ext == "app")) {
            return Ok(Installation::Mac(bundle.to_path_buf()));
        }
    }
    #[cfg(windows)]
    {
        let _ = executable;
        return Ok(Installation::Windows);
    }
    #[cfg(target_os = "linux")]
    {
        if Path::new("/.flatpak-info").exists() {
            return Ok(Installation::Unsupported(
                "The Flatpak prototype has no update repository. Install a new prototype bundle manually.",
            ));
        }
        if app.env().appimage.is_some() {
            return Ok(Installation::AppImage(executable.canonicalize()?));
        }
        let owner = std::process::Command::new("/usr/bin/dpkg-query")
            .arg("--search")
            .arg(&executable)
            .output();
        if owner.is_ok_and(|output| {
            output.status.success()
                && String::from_utf8_lossy(&output.stdout).lines().any(|line| {
                    line.split_once(": ").is_some_and(|(package, path)| {
                        package.split(':').next() == Some("apparition")
                            && Path::new(path) == executable
                    })
                })
        }) {
            return Ok(Installation::Deb);
        }
    }
    #[allow(unreachable_code)]
    Ok(Installation::Unsupported(
        "This installation is not a supported release package. Download the desktop app to update it.",
    ))
}

fn publish(app: &AppHandle, status: Status) {
    *app.state::<Updates>().status.lock().unwrap() = status.clone();
    companion::update_available(app, status.version.as_deref());
    let _ = app.emit("desktop-update", status);
}

async fn check(app: &AppHandle, manual: bool) -> Result<(), Report> {
    let state = app.state::<Updates>();
    let Ok(_guard) = state.operation.try_lock() else {
        return Ok(());
    };
    if !manual && !app.state::<std::sync::Arc<companion::CompanionState>>().auto_updates() {
        return Ok(());
    }
    let installation = installation(app)?;
    // Keep an already announced update (and its badge) on transient network failures.
    let candidate = release::latest_installer(
        release::SITE,
        installer_platform(),
        installation.extension(),
        VERSION,
    )
    .await?;
    if let Some((verified, asset)) = candidate {
        let mut status = Status {
            version: Some(verified.release.version.clone()),
            phase: "available",
            message: match &installation {
                Installation::Unsupported(reason) => reason.to_string(),
                Installation::Deb => {
                    "Installing asks for administrator authorization, then restarts Apparition."
                        .into()
                }
                _ => {
                    "Ready to download, verify and restart. The companion pauses while restarting."
                        .into()
                }
            },
            can_install: installation.supported(),
        };
        let previous = state.status.lock().unwrap().clone();
        if !manual && previous.phase == "error" && previous.version == status.version {
            status.phase = "error";
            status.message = previous.message;
        }
        *state.candidate.lock().unwrap() = Some((verified, asset));
        publish(app, status);
    } else if manual && state.candidate.lock().unwrap().is_none() {
        publish(
            app,
            Status {
                phase: "current",
                message: "Apparition is up to date.".into(),
                ..Default::default()
            },
        );
    }
    Ok(())
}

pub fn setup(app: &AppHandle) {
    #[cfg(windows)]
    if let Some(result) = take_windows_result() {
        publish(
            app,
            Status {
                version: result
                    .get("version")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                phase: "error",
                message: format!(
                    "Update failed: {}",
                    result["error"]
                        .as_str()
                        .unwrap_or("Unknown installer error")
                ),
                can_install: !cfg!(debug_assertions),
            },
        );
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            let _ = check(&app, false).await;
            tokio::time::sleep(CHECK_INTERVAL).await;
        }
    });
}

pub fn request_check(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(error) = check(&app, true).await {
            let mut status = app.state::<Updates>().status.lock().unwrap().clone();
            status.phase = "error";
            status.message = format!("Couldn't check for updates: {}", api::describe(&error));
            publish(&app, status);
        }
    });
}

pub fn request_automatic_check(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let _ = check(&app, false).await;
    });
}

#[tauri::command]
pub fn update_status(state: tauri::State<'_, Updates>) -> Status {
    state.status.lock().unwrap().clone()
}

#[tauri::command]
pub async fn check_update(app: AppHandle) -> Result<Status, Failure> {
    if let Err(error) = check(&app, true).await {
        let mut status = app.state::<Updates>().status.lock().unwrap().clone();
        status.phase = "error";
        status.message = format!("Couldn't check for updates: {}", api::describe(&error));
        publish(&app, status);
        return Err(error.into());
    }
    Ok(app.state::<Updates>().status.lock().unwrap().clone())
}

#[tauri::command]
pub async fn install_update(app: AppHandle) -> Result<(), Failure> {
    let state = app.state::<Updates>();
    let _guard = state
        .operation
        .try_lock()
        .map_err(|_| Failure::new("An update operation is already running."))?;
    let installation = installation(&app)?;
    if let Installation::Unsupported(reason) = installation {
        return Err(Failure::new(reason));
    }
    let (verified, asset) = state
        .candidate
        .lock()
        .unwrap()
        .clone()
        .ok_or_else(|| Failure::new("Check for updates first."))?;
    if !release::is_newer(&verified.release.version, VERSION) {
        return Err(Failure::new("Refusing to install an older version."));
    }
    let mut status = state.status.lock().unwrap().clone();
    status.phase = "downloading";
    status.message = "Downloading and verifying the signed update...".into();
    publish(&app, status.clone());
    let result: Result<Finish, Report> = async {
        let scratch = tempfile::Builder::new()
            .prefix("apparition-update-")
            .tempdir()?;
        let package = release::download(&verified, &asset, scratch.path(), |_, _| {}).await?;
        status.phase = "installing";
        status.message = "Signature and SHA-256 verified. Installing...".into();
        publish(&app, status.clone());
        let version = verified.release.version.clone();
        let finish = tauri::async_runtime::spawn_blocking(move || {
            let finish = install(&installation, &package, &version)?;
            #[cfg(windows)]
            if finish == Finish::ExitForHelper {
                let _ = scratch.keep();
            } else {
                drop(scratch);
            }
            #[cfg(not(windows))]
            drop(scratch);
            Ok::<_, Report>(finish)
        })
        .await
        .map_err(|error| report!("{error}"))??;
        Ok(finish)
    }
    .await;
    match result {
        Ok(Finish::Restart) => app.request_restart(),
        #[cfg(windows)]
        Ok(Finish::ExitForHelper) => app.exit(0),
        Err(error) => {
            status.phase = "error";
            status.message = format!(
                "Update failed: {}. You can retry; your settings are unchanged.",
                api::describe(&error)
            );
            publish(&app, status);
            return Err(error.into());
        }
    }
    Ok(())
}

fn install(installation: &Installation, package: &Path, _version: &str) -> Result<Finish, Report> {
    #[cfg(windows)]
    if matches!(installation, Installation::Windows) {
        return install_windows(package, _version);
    }
    match installation {
        Installation::Mac(bundle) => install_mac(package, bundle)?,
        Installation::Windows => bail!("Not running on Windows"),
        Installation::AppImage(path) => replace_file(package, path, true)?,
        Installation::Deb => {
            let output = std::process::Command::new("/usr/bin/pkexec")
                .args(["/usr/bin/dpkg", "--install"])
                .arg(package)
                .output()
                .context(
                    "Couldn't start the system package installer (pkexec and dpkg are required)",
                )?;
            if !output.status.success() {
                bail!(
                    "The system package installer failed or authorization was cancelled ({}): {}. If dpkg reported a dependency problem, repair it with your package manager",
                    output.status,
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
        }
        Installation::Unsupported(reason) => bail!("{reason}"),
    }
    Ok(Finish::Restart)
}

/// Stage on the destination filesystem, so replacement is a rename, never an in-place
/// write to a mapped executable. Original permissions survive (including AppImage +x).
fn replace_file(source: &Path, target: &Path, executable: bool) -> Result<(), Report> {
    let parent = target
        .parent()
        .ok_or_else(|| report!("No installation directory"))?;
    let mut staged = tempfile::NamedTempFile::new_in(parent)
        .context("The installation directory is not writable")?;
    std::io::copy(&mut std::fs::File::open(source)?, staged.as_file_mut())?;
    let permissions = std::fs::metadata(target)
        .or_else(|_| std::fs::metadata(source))?
        .permissions();
    #[cfg(unix)]
    let permissions = {
        use std::os::unix::fs::PermissionsExt;
        std::fs::Permissions::from_mode(permissions.mode() | if executable { 0o100 } else { 0 })
    };
    #[cfg(not(unix))]
    let _ = executable;
    staged.as_file().set_permissions(permissions)?;
    staged.as_file().sync_all()?;
    staged
        .persist(target)
        .map_err(|error| report!("Couldn't replace the installed file: {error}"))?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn install_mac(package: &Path, bundle: &Path) -> Result<(), Report> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt, process::Command};
    let parent = bundle.parent().ok_or_else(|| report!("No app directory"))?;
    let staged = tempfile::Builder::new()
        .prefix(".apparition-update-")
        .tempdir_in(parent)
        .context("Move Apparition to a writable Applications folder before updating")?;
    let status = Command::new("/usr/bin/ditto")
        .args(["-x", "-k"])
        .arg(package)
        .arg(staged.path())
        .status()?;
    if !status.success() {
        bail!("Couldn't unpack the signed macOS update");
    }
    let replacement = staged.path().join("Apparition.app");
    if !replacement.join("Contents/MacOS/Apparition").is_file() {
        bail!("The update has no Apparition.app");
    }
    // Ad-hoc signing is intentional. Publisher identity comes from the SSHSIG, but the
    // bundle's seal must still be valid for macOS (and its entitled nested helper).
    if !Command::new("/usr/bin/codesign")
        .args(["--verify", "--deep", "--strict"])
        .arg(&replacement)
        .status()?
        .success()
    {
        bail!("The updated app's code-signing seal is invalid");
    }
    let from = CString::new(replacement.as_os_str().as_bytes())?;
    let to = CString::new(bundle.as_os_str().as_bytes())?;
    if unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_SWAP) } != 0 {
        Err(std::io::Error::last_os_error()).context("Couldn't exchange the app bundles")?;
    }
    // The old bundle is now in the staging directory; never re-sign the replacement or
    // its anisette helper, whose special entitlements must remain intact.
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn install_mac(_: &Path, _: &Path) -> Result<(), Report> {
    bail!("Not running on macOS")
}

#[cfg(windows)]
fn install_windows(package: &Path, version: &str) -> Result<Finish, Report> {
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use std::{
        io::Read,
        os::windows::process::CommandExt,
        process::{Command, Stdio},
    };
    let stage = package
        .parent()
        .ok_or_else(|| report!("No staging directory"))?;
    let unpacked = stage.join("unpacked");
    std::fs::create_dir(&unpacked)?;
    let mut zip = zip::ZipArchive::new(std::fs::File::open(package)?)?;
    // PowerShell 5's filesystem provider cannot Join-Path a verbatim \\?\ path.
    let current = dunce::simplified(&tauri::utils::platform::current_exe()?).to_path_buf();
    let parent = current
        .parent()
        .ok_or_else(|| report!("No installation directory"))?;
    let app_name = current.file_name().unwrap().to_string_lossy();
    if app_name.eq_ignore_ascii_case("apparition-cli.exe")
        || app_name.eq_ignore_ascii_case("WebView2Loader.dll")
    {
        bail!("Rename the desktop executable to Apparition.exe before updating");
    }
    let mut files = Vec::new();
    // Extract only the expected files, not paths supplied by the archive.
    for name in ["WebView2Loader.dll", "apparition-cli.exe", "Apparition.exe"] {
        let mut entry = zip.by_name(name)?;
        let mut file = std::fs::File::create(unpacked.join(name))?;
        std::io::copy(&mut entry, &mut file)?;
        file.sync_all()?;
        let mut hash = Sha256::new();
        let mut file = std::fs::File::open(unpacked.join(name))?;
        let mut buffer = [0; 65536];
        loop {
            let n = file.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            hash.update(&buffer[..n]);
        }
        files.push(json!({ "source": name, "target": if name == "Apparition.exe" { app_name.as_ref() } else { name },
            "sha256": format!("{:x}", hash.finalize()) }));
    }
    let result = windows_result_path();
    std::fs::create_dir_all(result.parent().unwrap())?;
    let _ = take_windows_result();
    #[cfg(test)]
    let arguments = "/c exit 0";
    #[cfg(not(test))]
    let arguments = if std::env::args().any(|arg| arg == companion::HIDDEN) {
        companion::HIDDEN
    } else {
        ""
    };
    let plan = json!({ "pid": std::process::id(), "directory": parent, "executable": current, "arguments": arguments,
        "version": version, "files": files, "result": result });
    std::fs::write(stage.join("plan.json"), serde_json::to_vec(&plan)?)?;
    let script = stage.join("update.ps1");
    std::fs::write(&script, include_str!("update-windows.ps1"))?;
    let powershell = PathBuf::from(
        std::env::var_os("SystemRoot").ok_or_else(|| report!("SystemRoot is missing"))?,
    )
    .join("System32/WindowsPowerShell/v1.0/powershell.exe");
    let mut child = Command::new(powershell)
        .env_remove("PSModulePath")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(&script)
        .arg("-Stage")
        .arg(stage)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(0x08000000) // CREATE_NO_WINDOW
        .spawn()?;
    for _ in 0..600 {
        if stage.join("ready").exists() {
            std::fs::write(stage.join("go"), b"go")?;
            return Ok(Finish::ExitForHelper);
        }
        if child.try_wait()?.is_some() {
            let result = take_windows_result();
            bail!(
                "The Windows updater couldn't prepare the installation: {}",
                result
                    .as_ref()
                    .and_then(|v| v["error"].as_str())
                    .unwrap_or("PowerShell could not start")
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    child.kill()?;
    let _ = child.wait();
    bail!("The Windows updater did not become ready; no files were replaced")
}

#[cfg(windows)]
fn windows_result_path() -> PathBuf {
    #[cfg(test)]
    if let Some(path) = std::env::var_os("APPARITION_TEST_UPDATE_RESULT") {
        return PathBuf::from(path);
    }
    api::DeviceStore::default()
        .dir
        .join("desktop-update-result.json")
}

#[cfg(windows)]
fn take_windows_result() -> Option<serde_json::Value> {
    let path = windows_result_path();
    let bytes = std::fs::read(&path).ok()?;
    // Windows PowerShell 5 writes a UTF-8 BOM.
    let result =
        serde_json::from_slice(bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&bytes)).ok();
    let _ = std::fs::remove_file(path);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_replacement_is_complete_and_preserves_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("download");
        let target = dir.path().join("Apparition.AppImage");
        std::fs::write(&source, b"verified new image").unwrap();
        std::fs::write(&target, b"old image").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o750)).unwrap();
        }
        replace_file(&source, &target, true).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"verified new image");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
                0o750
            );
        }
        assert!(replace_file(&dir.path().join("missing"), &target, true).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"verified new image");
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore] // Installs a synthetic package only in the explicitly opted-in container.
    fn deb_updates_through_the_system_installer() {
        use std::process::Command;
        assert_eq!(
            std::env::var("APPARITION_UPDATE_CONTAINER").as_deref(),
            Ok("1")
        );
        assert!(Path::new("/.dockerenv").exists());
        let dir = tempfile::tempdir().unwrap();
        for version in ["1.0.0", "2.0.0"] {
            let root = dir.path().join(version);
            std::fs::create_dir_all(root.join("DEBIAN")).unwrap();
            std::fs::create_dir_all(root.join("usr/share/apparition-update-fixture")).unwrap();
            std::fs::write(root.join("DEBIAN/control"), format!(
                "Package: apparition-update-fixture\nVersion: {version}\nArchitecture: all\nMaintainer: Test <test@example.invalid>\nDescription: Disposable update fixture\n"
            )).unwrap();
            std::fs::write(
                root.join("usr/share/apparition-update-fixture/version"),
                version,
            )
            .unwrap();
            let package = dir.path().join(format!("{version}.deb"));
            assert!(
                Command::new("/usr/bin/dpkg-deb")
                    .arg("--build")
                    .arg(&root)
                    .arg(&package)
                    .status()
                    .unwrap()
                    .success()
            );
            install(&Installation::Deb, &package, version).unwrap();
            assert_eq!(
                std::fs::read_to_string("/usr/share/apparition-update-fixture/version").unwrap(),
                version
            );
        }
        let broken = dir.path().join("broken.deb");
        std::fs::write(&broken, b"not a Debian package").unwrap();
        assert!(install(&Installation::Deb, &broken, "3.0.0").is_err());
        assert_eq!(
            std::fs::read_to_string("/usr/share/apparition-update-fixture/version").unwrap(),
            "2.0.0"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn mac_update_replaces_the_whole_sealed_bundle_and_rejects_a_broken_seal() {
        use std::process::Command;
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let bundle = source.join("Apparition.app");
        let contents = bundle.join("Contents");
        std::fs::create_dir_all(contents.join("MacOS")).unwrap();
        std::fs::create_dir_all(contents.join("Resources")).unwrap();
        std::fs::copy("/usr/bin/true", contents.join("MacOS/Apparition")).unwrap();
        std::fs::write(
            contents.join("Info.plist"),
            br#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>Apparition</string>
<key>CFBundleIdentifier</key><string>com.illixion.apparition.update-fixture</string>
<key>CFBundlePackageType</key><string>APPL</string>
</dict></plist>"#,
        )
        .unwrap();
        std::fs::write(contents.join("Resources/version"), b"new").unwrap();
        assert!(
            Command::new("/usr/bin/codesign")
                .args(["--force", "--sign", "-"])
                .arg(&bundle)
                .status()
                .unwrap()
                .success()
        );
        let installed = dir.path().join("Apparition.app");
        std::fs::create_dir_all(&installed).unwrap();
        std::fs::write(installed.join("obsolete"), b"old resource").unwrap();
        let archive = dir.path().join("release.zip");
        let zip = || {
            let _ = std::fs::remove_file(&archive);
            assert!(
                Command::new("/usr/bin/ditto")
                    .args(["-c", "-k", "--keepParent"])
                    .arg(&bundle)
                    .arg(&archive)
                    .status()
                    .unwrap()
                    .success()
            );
        };
        zip();
        install_mac(&archive, &installed).unwrap();
        assert!(!installed.join("obsolete").exists());
        assert_eq!(
            std::fs::read(installed.join("Contents/Resources/version")).unwrap(),
            b"new"
        );
        assert!(
            Command::new(installed.join("Contents/MacOS/Apparition"))
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(contents.join("Resources/version"), b"tampered").unwrap();
        zip();
        assert!(install_mac(&archive, &installed).is_err());
        assert_eq!(
            std::fs::read(installed.join("Contents/Resources/version")).unwrap(),
            b"new"
        );
    }

    #[cfg(windows)]
    #[test]
    #[ignore] // Invoked only by the parent fixture below, in a disposable copy.
    fn windows_replacement_child() {
        install_windows(
            Path::new(&std::env::var_os("APPARITION_TEST_UPDATE").unwrap()),
            "1.0.0",
        )
        .unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_replaces_a_running_executable_and_its_cli() {
        let payload = std::fs::read(
            PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32/cmd.exe"),
        )
        .unwrap();
        windows_update_fixture(&payload, true);
    }

    #[cfg(windows)]
    #[test]
    fn windows_restores_old_files_if_the_updated_app_cannot_launch() {
        windows_update_fixture(b"not a Windows executable", false);
    }

    #[cfg(windows)]
    fn windows_update_fixture(new_exe: &[u8], success: bool) {
        use std::{
            io::Write,
            process::{Command, Stdio},
        };
        let dir = tempfile::tempdir().unwrap();
        let installed = dir.path().join("Apparition.exe");
        let current = std::env::current_exe().unwrap();
        std::fs::copy(&current, &installed).unwrap();
        let loader = current.parent().unwrap().join("WebView2Loader.dll");
        let loader = if loader.exists() {
            loader
        } else {
            current
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join("WebView2Loader.dll")
        };
        std::fs::copy(&loader, dir.path().join("WebView2Loader.dll")).unwrap();
        std::fs::write(dir.path().join("apparition-cli.exe"), b"old CLI").unwrap();
        let download = tempfile::tempdir().unwrap();
        let archive = download.path().join("update.zip");
        let mut zip = zip::ZipWriter::new(std::fs::File::create(&archive).unwrap());
        for name in ["Apparition.exe", "apparition-cli.exe"] {
            zip.start_file(name, zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(new_exe).unwrap();
        }
        zip.start_file(
            "WebView2Loader.dll",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        zip.write_all(&std::fs::read(&loader).unwrap()).unwrap();
        zip.finish().unwrap();
        assert!(
            Command::new(&installed)
                .args([
                    "--exact",
                    "updater::tests::windows_replacement_child",
                    "--ignored"
                ])
                .env("APPARITION_TEST_UPDATE", &archive)
                .env(
                    "APPARITION_TEST_UPDATE_RESULT",
                    dir.path().join("result.json")
                )
                .status()
                .unwrap()
                .success()
        );
        for _ in 0..300 {
            if !download.path().exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(!download.path().exists(), "Windows helper did not finish");
        if success {
            assert_eq!(std::fs::read(&installed).unwrap(), new_exe);
            assert_eq!(
                std::fs::read(dir.path().join("apparition-cli.exe")).unwrap(),
                new_exe
            );
            assert!(
                Command::new(&installed)
                    .args(["/c", "exit", "0"])
                    .status()
                    .unwrap()
                    .success()
            );
        } else {
            assert_eq!(
                std::fs::read(&installed).unwrap(),
                std::fs::read(&current).unwrap()
            );
            assert_eq!(
                std::fs::read(dir.path().join("apparition-cli.exe")).unwrap(),
                b"old CLI"
            );
            assert!(
                Command::new(&installed)
                    .arg("--list")
                    .stdout(Stdio::null())
                    .status()
                    .unwrap()
                    .success()
            );
        }
        // The helper waits for the copied test process, not an arbitrary sleep or PID.
        for _ in 0..300 {
            if !std::fs::read_dir(dir.path())
                .unwrap()
                .filter_map(Result::ok)
                .any(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".apparition-update-")
                })
            {
                assert_eq!(dir.path().join("result.json").exists(), !success);
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("Windows left a locked-executable cleanup helper behind");
    }
}
