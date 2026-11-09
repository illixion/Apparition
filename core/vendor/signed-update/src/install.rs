//! Putting a downloaded, verified package in place of the running app.

use std::path::{Path, PathBuf};

use crate::error::{Result, fail};

/// The `.app` the running executable is inside (`Foo.app/Contents/MacOS/foo`), if it is in one: a
/// development build run from `target/` isn't, and isn't replaced.
pub fn current_bundle() -> Option<PathBuf> {
    let executable = std::env::current_exe().ok()?;
    let bundle = executable.parent()?.parent()?.parent()?;
    (cfg!(target_os = "macos") && bundle.extension().is_some_and(|e| e == "app")).then(|| bundle.to_path_buf())
}

/// Replaces `bundle` with the bundle of the same name in the zip `package` (which has `Name.app`
/// at its root, as `ditto -c -k --keepParent` makes). The zip is unpacked beside the installed app,
/// its code-signing seal checked, and the two swapped in one step (`renamex_np` with
/// `RENAME_SWAP`), so a failure at any point leaves the installed app as it was. The caller
/// restarts the app; the old bundle then sits where the new one was, and is deleted with the
/// staging folder when this returns.
#[cfg(target_os = "macos")]
pub fn install_bundle(package: &Path, bundle: &Path) -> Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::process::Command;

    let parent = bundle.parent().ok_or("no app folder")?;
    let staged = tempfile::Builder::new()
        .prefix(".update-")
        .tempdir_in(parent)
        .map_err(|_| "Move the app to a writable Applications folder before updating")?;
    let unpacked = Command::new("/usr/bin/ditto")
        .args(["-x", "-k"])
        .arg(package)
        .arg(staged.path())
        .status()
        .map_err(|e| e.to_string())?;
    if !unpacked.success() {
        fail!("Couldn't unpack the signed update");
    }
    let name = bundle.file_name().ok_or("no bundle name")?;
    let replacement = staged.path().join(name);
    if !replacement.join("Contents/Info.plist").is_file() {
        fail!("The update has no {}", name.to_string_lossy());
    }
    let sealed = Command::new("/usr/bin/codesign")
        .args(["--verify", "--deep", "--strict"])
        .arg(&replacement)
        .status()
        .map_err(|e| e.to_string())?;
    if !sealed.success() {
        fail!("The updated app's code-signing seal is invalid");
    }
    let from = CString::new(replacement.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
    let to = CString::new(bundle.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
    if unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_SWAP) } != 0 {
        fail!("Couldn't exchange the app bundles: {}", std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub fn install_bundle(_package: &Path, _bundle: &Path) -> Result<()> {
    fail!("Installing a bundle is for macOS; download the new version instead")
}
