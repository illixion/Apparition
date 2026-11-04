//! The native clients' operations that change things: build, patch, report an install, add a
//! project. Part of `server`, so they share its request and reply helpers.

use super::*;

const PATCH_HINT: &str = "Launch a live-patching build and install the current Apparition CLI on the build Mac, then retry.";

impl App {
    /// `POST /api/service/v1/build`: a configured project, target and branch, with boolean options.
    /// It is the page's deploy for one target.
    pub(super) fn service_build(&self, req: &Req) -> Response {
        let body = req.json_body();
        let config = self.config();
        let repo = config.raw.get("repos").and_then(Value::as_array).into_iter().flatten().find(|r| r["name"] == body["repo"]);
        let target = repo.and_then(|r| r["platforms"].as_array()).into_iter().flatten()
            .find(|p| p["device"] == body["device"] && p["platform"] == body["targetPlatform"]);
        let branch = body.get("branch").and_then(Value::as_str).filter(|b| !b.is_empty()).unwrap_or("current");
        let offered = repo.and_then(|r| r["branches"].as_array()).is_some_and(|b| b.iter().any(|x| x.as_str() == Some(branch)));
        let bad_flag = ["run", "mcp", "inject", "buildDebug", "deploy"].iter().any(|k| body.get(k).is_some_and(|v| !v.is_boolean()));
        let bad_backend = body.get("injectBackend").is_some_and(|v| !matches!(v.as_str(), Some("dynamic" | "apparition")));
        let bad_install = body.get("install").is_some_and(|v| !matches!(v.as_str(), Some("auto" | "mac" | "device" | "none")));
        let (Some(repo), Some(target), false) = (repo, target, bad_backend || bad_flag || bad_install || (branch != "current" && !offered)) else {
            return json_reply(400, json!({ "error": "Select a configured project, target and branch, with boolean build options and install auto, mac, device or none." }));
        };
        let mut forward = json!({
            "repo": repo["name"],
            "targets": [{ "device": target["device"], "platform": target["platform"], "branch": branch }],
        });
        for key in ["run", "mcp", "jit", "inject", "injectBackend", "buildDebug", "deploy", "install"] {
            if let Some(v) = body.get(key) {
                forward[key] = v.clone();
            }
        }
        match self.queue.enqueue_deploy(&self.queue_context(), &forward) {
            Ok(jobs) => json_reply(200, json!({ "jobs": jobs })),
            Err(error) => json_reply(400, json!({ "error": error })),
        }
    }

    /// `POST /api/service/v1/inject`, checked: the app's project root, bundle, device and sources.
    pub(super) fn service_inject(&self, req: &Req) -> Response {
        let body = req.json_body();
        let config = self.config();
        let repo = config.raw.get("repos").and_then(Value::as_array).into_iter().flatten().find(|r| r["name"] == body["repo"]);
        let target = repo.and_then(|r| r["platforms"].as_array()).into_iter().flatten()
            .find(|p| p["device"] == body["device"] && p["platform"] == body["targetPlatform"]);
        let files_ok = match body.get("files") {
            None | Some(Value::Null) => true,
            Some(Value::Array(files)) => files.len() <= 64 && files.iter().all(|f| f.as_str().is_some_and(|f| f.len() <= 4096)),
            Some(_) => false,
        };
        let (Some(repo), Some(target), true) = (repo, target, files_ok) else {
            return json_reply(400, json!({ "ok": false, "error": "Select a configured project and target, with an optional list of source paths.",
                "hint": "Use the same target as the running live-patching build." }));
        };
        let (Some(path), Some(bundle), Some(device)) = (repo["path"].as_str(), target["bundleId"].as_str(), target["device"].as_str()) else {
            return json_reply(400, json!({ "ok": false, "error": "Select a configured project and target, with an optional list of source paths.",
                "hint": "Use the same target as the running live-patching build." }));
        };
        defer(Deferred::Inject(json!({ "path": path, "bundleId": bundle, "device": device, "files": body.get("files").cloned().unwrap_or_else(|| json!([])) })))
    }

    /// Runs `apparition inject`, one patch at a time per app and device.
    pub(super) async fn inject_run(self: &Arc<Self>, job: Value) -> Response {
        let refuse = |error: String| json_reply(409, json!({ "ok": false, "error": error, "hint": PATCH_HINT }));
        let (path, bundle, device) = (job["path"].as_str().unwrap_or(""), job["bundleId"].as_str().unwrap_or(""), job["device"].as_str().unwrap_or(""));
        let key = format!("{bundle}@{device}");
        if !self.patching.lock().unwrap().insert(key.clone()) {
            return refuse("A patch is already in progress for this app and device.".into());
        }
        let config = self.config();
        let mut command = tokio::process::Command::new(queue::cli(&config));
        command.arg("inject").arg(bundle).arg("--device").arg(device);
        for file in job["files"].as_array().into_iter().flatten().filter_map(Value::as_str) {
            command.arg("--file").arg(file);
        }
        command.current_dir(path).stdin(std::process::Stdio::null()).kill_on_drop(true);
        for (var, key) in [("DEBUGTRACE_MCP", "debugtraceTool"), ("DEBUGTRACE_INJECT_TOOL", "debugtraceInjectTool")] {
            if let Some(tool) = config.raw.get(key).and_then(Value::as_str).filter(|t| !t.is_empty()) {
                command.env(var, tool);
            }
        }
        let output = tokio::time::timeout(std::time::Duration::from_secs(90), command.output()).await;
        self.patching.lock().unwrap().remove(&key);
        let output = match output {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => return refuse(format!("Patch failed: couldn't start the Apparition CLI: {e}")),
            Err(_) => return refuse("Patch failed: it took longer than 90 seconds.".into()),
        };
        let (out, err) = (String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        if !output.status.success() {
            let text = if out.trim().is_empty() { err } else { out };
            let tail: String = text.chars().rev().take(12000).collect::<Vec<_>>().into_iter().rev().collect();
            return refuse(format!("Patch failed: {tail}"));
        }
        match serde_json::from_str::<Value>(out.trim()) {
            Ok(result) => json_reply(200, result),
            Err(_) => refuse(format!("Patch failed: the CLI's answer wasn't JSON: {}", out.trim())),
        }
    }

    /// `POST /api/service/v1/installs`: the device installed a kept build itself, which is
    /// recorded as a fact (nothing here reaches the device).
    pub(super) fn service_installs(&self, req: &Req) -> Response {
        let config = self.config();
        let repos = config.raw.get("repos").cloned().unwrap_or_else(|| json!([]));
        let mut builds: Vec<Value> = self.queue.jobs().iter().filter_map(|j| serde_json::to_value(j).ok()).collect();
        builds.extend(self.state.field("builds").as_array().cloned().unwrap_or_default());
        let store = self.build_store();
        let read = |id: &str| artifacts::read_build(id, &store).map(|k| k.manifest);
        match projects::ledger_entry_from_report(&req.json_body(), &repos, &builds, &read, system_ms()) {
            Ok((device, bundle, entry)) => {
                self.state.record_install(&device, &bundle, entry.clone());
                json_reply(200, json!({ "ok": true, "install": entry }))
            }
            Err(e) => json_reply(e.status, json!({ "ok": false, "error": e.message })),
        }
    }

    /// `POST /api/service/v1/projects/discover`.
    pub(super) fn service_discover(&self, req: &Req) -> Response {
        let repos = self.config().raw.get("repos").cloned().unwrap_or_else(|| json!([]));
        match projects::discover(req.json_body().get("path").unwrap_or(&Value::Null), &repos) {
            Ok(discovery) => json_reply(200, json!({ "discovery": discovery })),
            Err(e) => json_reply(e.status, json!({ "error": e.message })),
        }
    }

    /// `POST /api/service/v1/projects`: add a project to config.json.
    pub(super) fn service_register(&self, req: &Req) -> Response {
        let result = projects::register_project(&self.settings.config_path, &req.json_body());
        self.config.lock().unwrap().0 = None;
        match result {
            Ok(project) => json_reply(200, json!({ "project": project })),
            Err(e) => json_reply(e.status, json!({ "error": e.message })),
        }
    }

    /// `GET /api/resign/options`: the profiles a re-sign can use.
    pub(super) fn resign_options(&self) -> Response {
        json_reply(200, json!({ "profiles": crate::resign::list_profiles().iter().map(crate::resign::public_profile).collect::<Vec<_>>() }))
    }

    /// `POST /api/resign`: either `{buildId, device, profile, dataMode?, dryRun?}` (re-sign that
    /// kept IPA) or `{repo, device, platform?, branch?, profile, ...}` (build that project target
    /// first). Nothing here takes a path from the client.
    pub(super) fn resign(&self, req: &Req) -> Response {
        let fail = |status: u16, message: &str| json_reply(status, json!({ "error": message }));
        let body = req.json_body();
        let is_text = |k: &str| body.get(k).is_some_and(Value::is_string);
        let from_build = is_text("buildId");
        let optional_text = |k: &str| body.get(k).is_none_or(|v| v.is_null() || v.is_string());
        if (!from_build && !is_text("repo")) || !is_text("device") || !is_text("profile") || !optional_text("platform") || !optional_text("branch")
            || body.get("dataMode").is_some_and(|v| !matches!(v.as_str(), Some("keep" | "discard")))
            || body.get("dryRun").is_some_and(|v| !v.is_boolean()) {
            return fail(400, "Choose a kept build or a project, a device and a profile.");
        }
        let (device, text) = (body["device"].as_str().unwrap_or(""), |k: &str| body.get(k).and_then(Value::as_str));
        let config = self.config();
        let rows: Vec<(&Value, &Value)> = config.raw.get("repos").and_then(Value::as_array).into_iter().flatten()
            .flat_map(|rc| rc["platforms"].as_array().into_iter().flatten().map(move |p| (rc, p))).collect();
        let (rc, target, kept, branch);
        if from_build {
            let kept_build = match artifacts::read_build(text("buildId").unwrap_or(""), &self.build_store()) {
                Ok(k) => k,
                Err(_) => return fail(404, "That build is not kept, or its files no longer match its manifest."),
            };
            if kept_build.manifest["artifact"]["kind"].as_str() != Some("ipa") {
                return fail(400, "Only an IPA can be re-signed.");
            }
            let bundle = kept_build.manifest["app"]["bundleId"].as_str().unwrap_or("");
            let platform = kept_build.manifest["platform"].as_str().unwrap_or("").to_lowercase();
            let Some(found) = rows.iter().find(|(_, p)| p["device"].as_str() == Some(device) && p["bundleId"].as_str() == Some(bundle)
                && p["platform"].as_str().unwrap_or("").to_lowercase() == platform) else {
                return fail(400, &format!("No configured project installs {bundle} on {device}, so there is no signing setup for it."));
            };
            (rc, target) = *found;
            kept = Some(kept_build.manifest["buildId"].as_str().unwrap_or("").to_string());
            branch = None;
        } else {
            let name = text("repo").unwrap_or("");
            let Some(found) = rows.iter().find(|(r, p)| r["name"].as_str() == Some(name) && p["device"].as_str() == Some(device)
                && text("platform").filter(|w| !w.is_empty()).is_none_or(|w| p["platform"].as_str() == Some(w))) else {
                return fail(400, &format!("{name} has no target on {device}{}.", text("platform").filter(|w| !w.is_empty()).map(|w| format!(" for {w}")).unwrap_or_default()));
            };
            (rc, target) = *found;
            if target["platform"].as_str().unwrap_or("").eq_ignore_ascii_case("macos") {
                return fail(400, "A Mac app has no device install to re-sign.");
            }
            let wanted = text("branch").filter(|b| !b.is_empty()).unwrap_or("current");
            if wanted != "current" && !rc["branches"].as_array().is_some_and(|b| b.iter().any(|x| x.as_str() == Some(wanted))) {
                return fail(400, &format!("{wanted} is not one of {name}'s branches."));
            }
            kept = None;
            branch = Some(wanted.to_string());
        }
        let Some(profile) = crate::resign::find_profile(text("profile").unwrap_or("")) else {
            return fail(400, "That profile is not in the build-signing folder.");
        };
        let udid = self.state.field("devices")["list"].as_array().into_iter().flatten()
            .find(|d| d["name"].as_str() == Some(device)).and_then(|d| d["udid"].as_str().map(String::from));
        let platform = target["platform"].as_str().unwrap_or("");
        if let Some(problem) = crate::resign::profile_problem(&profile, udid.as_deref(), platform) {
            return fail(400, &problem);
        }
        let jobs = self.queue.enqueue_resign(&self.queue_context(), rc["name"].as_str().unwrap_or(""), platform, device,
            target["bundleId"].as_str().unwrap_or(""), kept.as_deref(), branch.as_deref(), &profile.id,
            text("dataMode").unwrap_or("keep"), body["dryRun"] == Value::Bool(true));
        json_reply(200, json!({ "jobs": jobs }))
    }
}
