//! Crash logs kept beside the traces (`store/lib/crashlogs.js`, the read side).
//!
//! `data/crashlogs/index.json` is shared with `bin/crash-reports`, which moves an investigated
//! crash's files to the Trash, so nothing is cached here: each call reads the index, forgets
//! entries whose `.ips` is gone and answers from what is on disk. Pulling logs off devices is off
//! (it woke sleeping Apple TVs, 2026-10-31), so only older logs and linking remain.

use serde_json::{Value, json};
use std::{fs, path::{Path, PathBuf}};

/// Held across each read-modify-write of the index in this process, so two requests can't lose
/// each other's change. (`bin/crash-reports` writes it too, rarely and from a person's command.)
static INDEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub struct CrashLogs {
    dir: PathBuf,
}

/// A MetricKit diagnostic that a crash log may be the full report of.
#[derive(Debug, PartialEq)]
pub struct Ref {
    pub trace_id: String,
    pub bundle_id: Option<String>,
    pub pid: i64,
    pub at: i64,
    pub tolerance: i64,
}

fn parse_time(text: &str) -> Option<i64> {
    // ISO 8601 as MetricKit writes it ("2026-10-09T08:00:00Z", fractional seconds allowed).
    let t = text.trim();
    let (date, rest) = t.split_once('T')?;
    let mut d = date.split('-').map(|p| p.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let rest = rest.trim_end_matches('Z');
    let (clock, offset_min) = match rest.rfind(['+', '-']).filter(|i| *i > 0) {
        Some(i) => {
            let off = &rest[i + 1..];
            let (oh, om) = off.split_once(':').map_or((off, "0"), |(h, m)| (h, m));
            let sign = if rest.as_bytes()[i] == b'-' { -1 } else { 1 };
            (&rest[..i], sign * (oh.parse::<i64>().ok()? * 60 + om.parse::<i64>().ok()?))
        }
        None => (rest, 0),
    };
    let mut c = clock.split(':');
    let (h, mi) = (c.next()?.parse::<i64>().ok()?, c.next()?.parse::<i64>().ok()?);
    let sec: f64 = c.next().unwrap_or("0").parse().ok()?;
    // days from civil
    let (y2, m2) = if m <= 2 { (y - 1, m + 9) } else { (y, m - 3) };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * m2 + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 86_400 + h * 3_600 + mi * 60 - offset_min * 60) as f64 * 1000.0 + (sec * 1000.0).round()) as i64)
}

/// One ref per diagnostic in each trace's `crashes/*.json`, for either layout (27+:
/// `environment.pid` and `timeRange`; before: `diagnosticMetaData.pid` and `timeStampBegin`).
pub fn crash_refs(trace_dir: &Path, metas: &[Value]) -> Vec<Ref> {
    let mut refs = Vec::new();
    for meta in metas {
        let Some(id) = meta["id"].as_str() else { continue };
        let Ok(names) = fs::read_dir(trace_dir.join(id).join("files").join("crashes")) else { continue };
        let mut names: Vec<_> = names.flatten().map(|e| e.path()).collect();
        names.sort();
        for path in names {
            let Some(report) = fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).and_then(|v| v.get("report").cloned()).filter(|r| !r.is_null()) else { continue };
            // 27+ gives an exact UTC time. Before, a local time with no zone, whose device may not
            // be in the Mac's zone, so only the process id and app really identify the crash.
            let begin = report.pointer("/timeRange/begin").and_then(Value::as_str).filter(|s| !s.is_empty());
            let at = begin.or(report["timeStampBegin"].as_str()).and_then(parse_time).unwrap_or(0);
            let diagnostics = std::iter::once(report.get("crashDiagnostic").cloned())
                .chain(report.get("crashDiagnostics").and_then(Value::as_array).into_iter().flatten().cloned().map(Some))
                .flatten().filter(|d| !d.is_null() && d.as_bool() != Some(false));
            for d in diagnostics {
                let pid = report.pointer("/environment/pid").filter(|v| !v.is_null()).or_else(|| d.pointer("/diagnosticMetaData/pid")).and_then(Value::as_i64);
                let bundle = report.pointer("/environment/bundleIdentifier").filter(|v| !v.is_null())
                    .or_else(|| d.pointer("/diagnosticMetaData/bundleIdentifier").filter(|v| !v.is_null()))
                    .or_else(|| meta.pointer("/app/bundleId")).and_then(Value::as_str).map(String::from);
                if let (Some(pid), true) = (pid, at != 0) {
                    refs.push(Ref { trace_id: id.into(), bundle_id: bundle, pid, at, tolerance: if begin.is_some() { 10 * 60_000 } else { 48 * 3_600_000 } });
                }
            }
        }
    }
    refs
}

impl CrashLogs {
    pub fn new(dir: PathBuf) -> CrashLogs {
        CrashLogs { dir }
    }

    fn index_path(&self) -> PathBuf {
        self.dir.join("index.json")
    }

    fn read_index(&self) -> Value {
        let mut index = json!({ "entries": [], "pull": {} });
        if let Some(Value::Object(file)) = fs::read_to_string(self.index_path()).ok().and_then(|t| serde_json::from_str(&t).ok()) {
            for (k, v) in file {
                index[k] = v;
            }
        }
        index
    }

    fn save(&self, index: &Value) {
        // Unique per write: two saves never share a temporary file.
        let tmp = self.dir.join(format!("index.json.tmp-{}-{}", std::process::id(), crate::auth::new_token()));
        if fs::create_dir_all(&self.dir).is_ok() && fs::write(&tmp, serde_json::to_string_pretty(index).unwrap_or_default()).is_ok() {
            let _ = fs::rename(tmp, self.index_path());
        }
    }

    /// The index with entries whose `.ips` was moved away dropped: removing the files is the
    /// whole cleanup, and their symbolicated text goes with them.
    fn current(&self) -> Value {
        let _held = INDEX.lock().unwrap_or_else(|e| e.into_inner());
        let mut index = self.read_index();
        let entries = index["entries"].as_array().cloned().unwrap_or_default();
        let (kept, gone): (Vec<Value>, Vec<Value>) = entries.into_iter().partition(|e| e["file"].as_str().is_some_and(|f| self.dir.join(f).exists()));
        if !gone.is_empty() {
            for e in &gone {
                if let Some(s) = e["symbolicated"].as_str() {
                    let _ = fs::remove_file(self.dir.join(s));
                }
            }
            index["entries"] = Value::Array(kept);
            self.save(&index);
        }
        index
    }

    /// Newest capture first.
    pub fn list(&self) -> Vec<Value> {
        let mut entries = self.current()["entries"].as_array().cloned().unwrap_or_default();
        entries.sort_by(|a, b| b["capturedAt"].as_f64().partial_cmp(&a["capturedAt"].as_f64()).unwrap_or(std::cmp::Ordering::Equal));
        entries
    }

    pub fn status(&self) -> Value {
        self.current()["pull"].clone()
    }

    pub fn get(&self, id: &str) -> Option<Value> {
        self.list().into_iter().find(|e| e["id"].as_str() == Some(id))
    }

    /// The file under `dir` for `rel`, only when it stays inside.
    pub fn resolve(&self, rel: &str) -> Option<PathBuf> {
        let path = Path::new(rel);
        path.components().all(|c| matches!(c, std::path::Component::Normal(_))).then(|| self.dir.join(path))
    }

    /// Ties each crash log to the MetricKit trace of the same crash: the same app, process id and,
    /// within the ref's tolerance, the same moment. Run again when a trace arrives, since either
    /// side can come first.
    pub fn link(&self, refs: &[Ref]) -> usize {
        let _held = INDEX.lock().unwrap_or_else(|e| e.into_inner());
        let mut index = self.read_index();
        let mut linked = 0;
        if let Some(entries) = index["entries"].as_array_mut() {
            for e in entries.iter_mut() {
                if e["traceId"].as_str().is_some_and(|t| !t.is_empty()) || e["pid"].is_null() {
                    continue;
                }
                let (Some(pid), Some(at)) = (e["pid"].as_i64(), e["capturedAt"].as_f64()) else { continue };
                if let Some(hit) = refs.iter().find(|r| r.bundle_id.as_deref() == e["bundleId"].as_str() && r.pid == pid && ((r.at as f64) - at).abs() < r.tolerance as f64) {
                    e["traceId"] = Value::String(hit.trace_id.clone());
                    linked += 1;
                }
            }
        }
        if linked > 0 {
            self.save(&index);
        }
        linked
    }
}
