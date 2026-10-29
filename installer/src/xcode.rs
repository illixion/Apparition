//! Local Xcode handoff to the same tools the Mac service uses. Registration
//! preserves the product; no Apple ID, provisioning, rebuild or signing step.
use crate::{Error, fail};
use std::path::{Path, PathBuf};

const USAGE: &str = "\
Apparition Xcode integration (macOS with Xcode and the build tools):

  apparition xcode register --app PRODUCT.app [--project DIR] [--symbols PRODUCT.app.dSYM]
      Retain an existing product and its symbols without rebuilding or re-signing it.
  apparition xcode configure --spec project.yml --target TARGET [--output FILE]
      Generate an optional XcodeGen spec and scheme, preserving the original spec.
  apparition xcode snapshot --project DIR --output FILE
      Capture a source context for the optional scheme's pre-build action.

The build tools are found beside the packaged CLI or through build-and-sign on PATH.
APPARITION_BUILD_TOOLS overrides their root. Registration never launches or attaches
a debugger. Stop Xcode debugging before separately choosing an Apparition JIT session.
";

fn is_tools(root: &Path) -> bool {
    root.join("bin/xcode-handoff").is_file() && root.join("lib/xcode.js").is_file()
}

fn from_build_script(script: &Path) -> Option<PathBuf> {
    let canonical = script.canonicalize().ok()?;
    let root = canonical.parent()?.parent()?;
    is_tools(root).then(|| root.to_path_buf())
}

fn tools() -> Result<PathBuf, Error> {
    if let Some(root) = std::env::var_os("APPARITION_BUILD_TOOLS") {
        let root = PathBuf::from(root);
        return if is_tools(&root) {
            Ok(root)
        } else {
            Err(fail(
                "APPARITION_BUILD_TOOLS must name the complete build tools checkout",
            ))
        };
    }
    if let Ok(executable) = std::env::current_exe() {
        if let Some(directory) = executable.parent() {
            for root in [
                directory.join("build-tools"),
                directory.join("../Resources/build-tools"),
                directory.join("../share/apparition/build-tools"),
            ] {
                if is_tools(&root) {
                    return Ok(root);
                }
            }
        }
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&paths) {
            if let Some(root) = from_build_script(&directory.join("build-and-sign")) {
                return Ok(root);
            }
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        if let Some(root) = from_build_script(&PathBuf::from(home).join("bin/build-and-sign")) {
            return Ok(root);
        }
    }
    Err(fail(
        "Install the complete Mac build tools or set APPARITION_BUILD_TOOLS to their checkout",
    ))
}

pub async fn run(args: &[String]) -> Result<(), Error> {
    if args.is_empty() || matches!(args[0].as_str(), "--help" | "-h" | "help") {
        print!("{USAGE}");
        return Ok(());
    }
    if !cfg!(target_os = "macos") {
        return Err(fail(
            "Xcode handoff runs on the Mac with Xcode; connect to its build service from other platforms",
        ));
    }
    let root = tools()?;
    let args = args.to_vec();
    let status = tokio::task::spawn_blocking(move || {
        std::process::Command::new("node")
            .arg(root.join("bin/xcode-handoff"))
            .args(args)
            .status()
    })
    .await
    .map_err(|_| fail("Couldn't start Xcode handoff"))?
    .map_err(|_| fail("Xcode handoff requires Node 18+ on PATH"))?;
    if status.success() {
        Ok(())
    } else {
        Err(fail("Xcode handoff failed; see the message above"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn script_resolution_requires_the_complete_tooling_tree() {
        let root = std::env::temp_dir().join(format!(
            "apparition-xcode-tools-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::write(root.join("bin/build-and-sign"), "fixture").unwrap();
        assert!(from_build_script(&root.join("bin/build-and-sign")).is_none());
        std::fs::create_dir(root.join("lib")).unwrap();
        std::fs::write(root.join("bin/xcode-handoff"), "fixture").unwrap();
        std::fs::write(root.join("lib/xcode.js"), "fixture").unwrap();
        assert_eq!(
            from_build_script(&root.join("bin/build-and-sign")).unwrap(),
            root.canonicalize().unwrap()
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
