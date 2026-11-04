//! A repo's build settings: `~/.config/build-signing/global.conf`, then the repo's
//! `scripts/build-signing.conf` or `build-signing.conf`, read the way `build-and-sign` reads them.
//! They are bash (aliases branch on `$PLATFORM`, some run a command), so the system's own
//! `/bin/bash` sources them and hands back the variables; nothing here guesses at bash syntax.
//! That is macOS's bash 3.2, which every Mac has: no Homebrew bash is needed.

use std::{collections::HashMap, path::{Path, PathBuf}, process::Command};

/// The variables a conf may set that the build reads.
const NAMES: &[&str] = &[
    "PROJECT_PATH", "SCHEME_NAME", "TARGET_NAME", "PLATFORM", "DEVICE_NAME", "BUILD_BUNDLE_ID", "TEAM_ID",
    "DEV_P12_PATH", "P12_PASSWORD", "DEV_P12_PASSWORD", "DEV_PROFILE_PATH", "DEV_APPEX_PROFILE_PATH",
    "APPEX_BUNDLE_ID", "DEV_WATCH_PROFILE_PATH", "PRE_BUILD_HOOK", "EXTRA_ENTITLEMENTS_PLIST",
    "ASSOCIATED_DOMAINS", "MCP_DEFAULT", "DEBUGTRACE_UPLOAD_URL", "DEBUGTRACE_SERVER", "DEBUGTRACE_INJECT",
    "INJECT_SCOPE", "INJECT_BACKEND", "DEBUGTRACE_KEY_DIR", "DEBUGTRACE_INJECT_TOOL", "DEBUGTRACE_DSYM_DIR",
    "APPARITION_BUILD_DIR", "XDG_STATE_HOME",
];

/// Sources the confs in `bash` and writes, to fd 3, NUL-separated: each set variable of
/// `NAMES` as name, value; then `EXTRA_BUILD_SETTINGS`, its count and items; then the
/// exported environment. The confs' own output goes to stderr.
const DUMP: &str = r#"
exec 3>&1 1>&2
global="$1"; repo="$2"; shift 2
[ -f "$global" ] && source "$global"
[ -n "$repo" ] && source "$repo"
for name in "$@"; do
    if [ -n "${!name+x}" ]; then printf '%s\0%s\0' "$name" "${!name}" >&3; fi
done
printf 'EXTRA_BUILD_SETTINGS\0' >&3
if [ -n "${EXTRA_BUILD_SETTINGS+x}" ]; then
    printf '%s\0' "${#EXTRA_BUILD_SETTINGS[@]}" >&3
    for item in "${EXTRA_BUILD_SETTINGS[@]}"; do printf '%s\0' "$item" >&3; done
else
    printf '0\0' >&3
fi
/usr/bin/env -0 >&3
"#;

#[derive(Debug, Default, Clone)]
pub struct Conf {
    pub project_root: PathBuf,
    pub global: Option<PathBuf>,
    pub repo: Option<PathBuf>,
    values: HashMap<String, String>,
    pub extra_build_settings: Vec<String>,
    /// What the confs exported (`DEVELOPER_DIR`, a `PATH`), for `xcodebuild` and the tools.
    pub environment: Vec<(String, String)>,
}

impl Conf {
    /// The confs for the repo at `project_root`. Fails when neither exists, or bash does.
    /// `context` is set before sourcing (`PROJECT_ROOT`, `BUILD_DIR`), as `build-and-sign` has them.
    pub fn load(project_root: &Path, global: &Path, context: &[(&str, String)]) -> Result<Conf, String> {
        let repo = ["scripts/build-signing.conf", "build-signing.conf"].iter().map(|c| project_root.join(c)).find(|c| c.is_file());
        let global = global.is_file().then(|| global.to_path_buf());
        if global.is_none() && repo.is_none() {
            return Err(format!(
                "No config found: need {} and/or a repo build-signing.conf (looked in {}/scripts/ and {}/)",
                crate::builder::tilde(global_path_hint(global.as_deref())), project_root.display(), project_root.display()
            ));
        }
        let mut command = Command::new("/bin/bash");
        command.arg("-c").arg(DUMP).arg("build-signing")
            .arg(global.as_deref().unwrap_or(Path::new("")))
            .arg(repo.as_deref().unwrap_or(Path::new("")))
            .args(NAMES)
            .current_dir(project_root)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped());
        for (key, value) in context {
            command.env(key, value);
        }
        let output = command.output().map_err(|e| format!("couldn't run /bin/bash to read the build config: {e}"))?;
        if !output.status.success() {
            let said = String::from_utf8_lossy(&output.stderr);
            return Err(format!("reading the build config failed ({}): {}", output.status, said.trim()));
        }
        let mut conf = parse(&output.stdout)?;
        conf.project_root = project_root.to_path_buf();
        conf.global = global;
        conf.repo = repo;
        Ok(conf)
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str).filter(|v| !v.is_empty())
    }

    pub fn set(&mut self, name: &str, value: impl Into<String>) {
        self.values.insert(name.to_string(), value.into());
    }

    /// The file a missing setting should be added to, for errors.
    pub fn label(&self) -> String {
        self.repo.as_ref().or(self.global.as_ref()).map(|p| p.display().to_string()).unwrap_or_else(|| "<unknown>".into())
    }

    /// Runs `PRE_BUILD_HOOK` as `build-and-sign` does: `eval`'d in bash in the project root, after
    /// the confs, with this run's variables (`resolved`, which a flag may have changed) set
    /// after them. Its output goes to `log`.
    pub fn run_hook(&self, resolved: &[(&str, String)], log: &std::fs::File) -> Result<(), String> {
        let script = r#"
global="$1"; repo="$2"; shift 2
[ -f "$global" ] && source "$global"
[ -n "$repo" ] && source "$repo"
while [ $# -gt 0 ]; do eval "$1=\"\$2\""; shift 2; done
cd "$PROJECT_ROOT" && eval "$PRE_BUILD_HOOK"
"#;
        let mut command = Command::new("/bin/bash");
        command.arg("-c").arg(script).arg("pre-build-hook")
            .arg(self.global.as_deref().unwrap_or(Path::new("")))
            .arg(self.repo.as_deref().unwrap_or(Path::new("")));
        for (key, value) in resolved {
            command.arg(key).arg(value);
        }
        command.current_dir(&self.project_root)
            .stdin(std::process::Stdio::null())
            .stdout(log.try_clone().map_err(|e| e.to_string())?)
            .stderr(log.try_clone().map_err(|e| e.to_string())?);
        let status = command.status().map_err(|e| format!("couldn't run the pre-build hook: {e}"))?;
        if status.success() { Ok(()) } else { Err(format!("the pre-build hook failed ({status})")) }
    }
}

fn global_path_hint(global: Option<&Path>) -> PathBuf {
    global.map(Path::to_path_buf).unwrap_or_else(default_global)
}

/// `BUILD_SIGNING_GLOBAL_CONF`, else `~/.config/build-signing/global.conf`.
pub fn default_global() -> PathBuf {
    std::env::var_os("BUILD_SIGNING_GLOBAL_CONF").map(PathBuf::from)
        .unwrap_or_else(|| crate::builder::home().join(".config/build-signing/global.conf"))
}

fn parse(output: &[u8]) -> Result<Conf, String> {
    let mut fields = output.split(|b| *b == 0).map(|f| String::from_utf8_lossy(f).into_owned());
    let mut conf = Conf::default();
    loop {
        let name = fields.next().ok_or("the build config's reader stopped early")?;
        if name == "EXTRA_BUILD_SETTINGS" {
            break;
        }
        let value = fields.next().ok_or("the build config's reader stopped early")?;
        conf.values.insert(name, value);
    }
    let count: usize = fields.next().and_then(|c| c.parse().ok()).ok_or("the build config's reader stopped early")?;
    for _ in 0..count {
        conf.extra_build_settings.push(fields.next().ok_or("the build config's reader stopped early")?);
    }
    conf.environment = fields.filter_map(|entry| {
        let (key, value) = entry.split_once('=')?;
        Some((key.to_string(), value.to_string()))
    }).collect();
    Ok(conf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn the_repo_conf_wins_and_aliases_branch_on_the_platform() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global.conf");
        write(&global, "TEAM_ID=\"TEAM\"\nDEVICE_NAME=AVP\nMCP_DEFAULT=on\nexport DEVELOPER_DIR=/Applications/Xcode-beta.app\n");
        let repo = dir.path().join("repo");
        write(&repo.join("scripts/build-signing.conf"), r#"
PLATFORM="${PLATFORM:-visionOS}"
if [[ "$PLATFORM" == "iPad" ]]; then PLATFORM="iOS"; DEVICE_NAME="${DEVICE_NAME_OVERRIDE:-YPad}"; fi
SCHEME_NAME="App with spaces"
EXTRA_BUILD_SETTINGS=('A=1 2' 'B=$(inherited) C')
echo "a conf that talks"
ROOT_SEEN="$PROJECT_ROOT"
"#);
        let conf = Conf::load(&repo, &global, &[("PROJECT_ROOT", repo.display().to_string()), ("PLATFORM", "iPad".into())]).unwrap();
        assert_eq!(conf.get("PLATFORM"), Some("iOS"));
        assert_eq!(conf.get("DEVICE_NAME"), Some("YPad"));
        assert_eq!(conf.get("TEAM_ID"), Some("TEAM"));
        assert_eq!(conf.get("SCHEME_NAME"), Some("App with spaces"));
        assert_eq!(conf.extra_build_settings, ["A=1 2", "B=$(inherited) C"]);
        assert!(conf.environment.iter().any(|(k, v)| k == "DEVELOPER_DIR" && v == "/Applications/Xcode-beta.app"));
        assert_eq!(conf.get("PROJECT_PATH"), None);
        assert!(conf.label().ends_with("scripts/build-signing.conf"));

        let plain = Conf::load(&repo, &global, &[]).unwrap();
        assert_eq!(plain.get("PLATFORM"), Some("visionOS"));
        assert_eq!(plain.get("DEVICE_NAME"), Some("AVP"));
    }

    #[test]
    fn no_conf_at_all_says_where_it_looked() {
        let dir = tempfile::tempdir().unwrap();
        let error = Conf::load(dir.path(), &dir.path().join("missing.conf"), &[]).unwrap_err();
        assert!(error.contains("No config found"), "{error}");
        assert!(error.contains("/scripts/"), "{error}");
    }

    #[test]
    fn a_broken_conf_fails_with_bash_s_words() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("build-signing.conf"), "PLATFORM=visionOS\nset -e\nfalse\n");
        let error = Conf::load(dir.path(), &dir.path().join("none"), &[]).unwrap_err();
        assert!(error.contains("reading the build config failed"), "{error}");
    }

    #[test]
    fn the_hook_sees_the_conf_and_this_run_s_values() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("build-signing.conf"), "PLATFORM=visionOS\nGREETING=hello\nPRE_BUILD_HOOK='echo \"$GREETING $PLATFORM $CONFIG\" > hook.out'\n");
        let conf = Conf::load(dir.path(), &dir.path().join("none"), &[]).unwrap();
        let log = std::fs::File::create(dir.path().join("log")).unwrap();
        let resolved = [("PROJECT_ROOT", dir.path().display().to_string()), ("PLATFORM", "iOS".into()), ("CONFIG", "Debug".into())];
        conf.run_hook(&resolved, &log).unwrap();
        assert_eq!(std::fs::read_to_string(dir.path().join("hook.out")).unwrap(), "hello iOS Debug\n");

        write(&dir.path().join("build-signing.conf"), "PRE_BUILD_HOOK='exit 3'\n");
        let conf = Conf::load(dir.path(), &dir.path().join("none"), &[]).unwrap();
        let error = conf.run_hook(&[("PROJECT_ROOT", dir.path().display().to_string())], &log).unwrap_err();
        assert!(error.contains("pre-build hook failed"), "{error}");
    }
}
