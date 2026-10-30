//! Serving an app's JIT requests as its debugger, through the tunnel's debugserver.
//!
//! The app is launched suspended or attached to while it runs, and then served until it no
//! longer needs a debugger:
//! - legacy `brk #0x69` (x0 = region, x1 = size; UTM's QEMU): prepare in place, then detach;
//! - universal `brk #0xf00d` (x16 = 1 prepare x0/x1, x16 = 0 detach; StikDebug's JIT26). Its
//!   x16 = 2 (a script for StikDebug to run) and unknown commands are skipped.
//!
//! A StikDebug JIT script sent with the request (`script-data`) is run by the app instead,
//! against a `Script` session: then the script, not `serve`, answers the app.
//!
//! Everything else the app does goes on as it would without a debugger, which is what Wine
//! (Madeira) needs: it signals and faults constantly and handles both itself.
//! - debugserver doesn't catch EXC_BAD_ACCESS or EXC_BAD_INSTRUCTION (`QSetIgnoredExceptions`),
//!   and passes every signal but SIGTRAP (`QPassSignals`), so they reach the app's own handlers
//!   without a stop;
//! - a fault that stops here anyway is handed back as the signal it stands for, and an
//!   EXC_RESOURCE (the memory high-water mark) is only noted;
//! - a BRK that isn't one of the above (Swift's `brk #1` for a failed check) is handed back as
//!   SIGTRAP, so the app crashes as it would undebugged instead of stopping here forever;
//! - a fault that keeps stopping at one place, undelivered, kills the app rather than parking
//!   its thread.

use std::{
    collections::HashMap,
    future::Future,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use idevice::{RsdService, debug_proxy::DebugProxyClient, rsd::RsdHandshake, tcp::handle::AdapterHandle};
use rootcause::prelude::*;
use serde_json::{Value, json};

const PAGE: u64 = 16 * 1024;
const PC: u32 = 0x20;
const X16: u32 = 16;
const SIGTRAP: u8 = 5;
/// The same fault this many times in a row, at one place, wasn't handled by the app.
const FAULT_REPEATS: u32 = 8;
/// Event lines kept for the reply; a Wine session stops thousands of times.
const EVENT_BUDGET: usize = 200;

pub enum Target<'a> {
    /// Launch the app suspended (ending a running instance) and attach before it runs.
    Launch(&'a str),
    /// Attach to the running process `pid` (0: `bring_back`'s running process). With
    /// `bring_back`, that app is brought to the front again afterwards: it asked from its own
    /// screen, and opening Apparition left it.
    Attach { pid: u64, bring_back: Option<&'a str> },
}

/// Running sessions' stop signals, by bundle ID (or pid, when only that is known).
static STOPS: Mutex<Option<HashMap<String, Arc<tokio::sync::Notify>>>> = Mutex::new(None);

/// Asks the session serving `key` (a bundle ID, or a pid) to detach now, leaving the app
/// running without a debugger. False when no session serves it.
pub fn stop(key: &str) -> bool {
    let stops = STOPS.lock().unwrap();
    match stops.as_ref().and_then(|stops| stops.get(key)) {
        Some(notify) => {
            notify.notify_one();
            true
        }
        None => false,
    }
}

/// Holds a session's place in `STOPS` until it ends.
struct Registration(Vec<String>);

impl Registration {
    fn new(keys: Vec<String>, notify: &Arc<tokio::sync::Notify>) -> Self {
        let mut stops = STOPS.lock().unwrap();
        let stops = stops.get_or_insert_with(HashMap::new);
        for key in &keys {
            stops.insert(key.clone(), notify.clone());
        }
        Registration(keys)
    }

    fn add(&mut self, key: String, notify: &Arc<tokio::sync::Notify>) {
        STOPS.lock().unwrap().get_or_insert_with(HashMap::new).insert(key.clone(), notify.clone());
        self.0.push(key);
    }
}

/// `work`, unless the session is asked to stop first (None).
async fn unless_stopped<T>(stop: &tokio::sync::Notify, work: impl Future<Output = Result<T, Report>>) -> Option<Result<T, Report>> {
    tokio::select! {
        result = work => Some(result),
        _ = stop.notified() => None,
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        if let Some(stops) = STOPS.lock().unwrap().as_mut() {
            for key in &self.0 {
                stops.remove(key);
            }
        }
    }
}

/// The pid of `bundle_id`'s running process, if it runs.
pub async fn running_pid(adapter: &mut AdapterHandle, handshake: &mut RsdHandshake, bundle_id: &str) -> Result<Option<u64>, Report> {
    let mut apps = idevice::core_device::AppServiceClient::connect_rsd(adapter, handshake)
        .await
        .context("appservice connect failed")?;
    let list = apps.list_apps(false, true, false, false, false).await.context("Can't list the device's apps")?;
    let Some(app) = list.iter().find(|app| app.bundle_identifier == bundle_id) else {
        bail!("{bundle_id} isn't installed");
    };
    let bundle = format!("{}/", app.path.trim_end_matches('/'));
    let processes = apps.list_processes().await.context("Can't list the device's processes")?;
    Ok(processes
        .iter()
        .find(|p| p.executable_url.as_ref().is_some_and(|url| url.relative.contains(&bundle)))
        .map(|p| p.pid as u64))
}

/// Serves `target`'s JIT requests until it detaches, exits or is killed. `wait_seconds` (0: no
/// limit) is how long to wait for its first `brk #0xf00d` request before detaching; once it has
/// asked, it's served until it says it's done, since it may ask again at any time (Wine
/// prepares each DLL it loads). `on_event` sees each step as it happens. Returns
/// `{pid, events, preparedBytes, detached, ended}`.
pub async fn serve(
    adapter: &mut AdapterHandle,
    handshake: &mut RsdHandshake,
    target: Target<'_>,
    wait_seconds: u32,
    on_event: &mut (dyn FnMut(&str) + Send),
) -> Result<Value, Report> {
    let started = Instant::now();
    let mut events = Events { lines: Vec::new(), dropped: 0, started, on_event };

    let (target_pid, bring_back, bundle_id) = match target {
        Target::Launch(bundle_id) => (None, None, Some(bundle_id)),
        Target::Attach { pid, bring_back } => (Some(pid), bring_back, bring_back),
    };
    let stop_requested = Arc::new(tokio::sync::Notify::new());
    // Registered before anything that can wait (the launch, finding the process, attaching), so
    // Stop works while the session is still starting, too.
    let mut registration = Registration::new(bundle_id.map(str::to_string).into_iter().collect(), &stop_requested);
    let setup = unless_stopped(&stop_requested, async {
        let pid = match (target_pid, bundle_id) {
            (None, Some(bundle_id)) => {
                let mut apps = idevice::core_device::AppServiceClient::connect_rsd(adapter, handshake)
                    .await
                    .context("appservice connect failed")?;
                let launched = apps
                    .launch_application(bundle_id, &[], true, true, None, None, None)
                    .await
                    .context("launch failed")?;
                events.log(format!("launched suspended, pid {}", launched.pid));
                launched.pid as u64
            }
            (Some(0), Some(bundle_id)) => running_pid(adapter, handshake, bundle_id)
                .await?
                .ok_or_else(|| report!("{bundle_id} isn't running"))?,
            (Some(pid), _) if pid != 0 => pid,
            _ => bail!("no process to attach to"),
        };

        let proxy = DebugProxyClient::connect_rsd(adapter, handshake).await.context("debugproxy connect failed")?;
        let mut gdb = Gdb { proxy };
        gdb.proxy.send_raw(b"+").await?;
        gdb.proxy.send_raw(&Gdb::frame("QStartNoAckMode")).await?;
        let ack = gdb.proxy.read_response().await?;
        gdb.proxy.set_ack_mode(false);
        events.log(format!("no-ack mode: {}", ack.unwrap_or_default()));
        gdb.request("QThreadSuffixSupported").await?;
        // Before attaching: debugserver refuses it (E35) once it has a process. Best effort, as
        // an older one may not know it; the fault handling below works either way. debugserver
        // has no QPassSignals, so signals still stop here and are passed on below.
        let ignored = gdb.request("QSetIgnoredExceptions:EXC_BAD_ACCESS|EXC_BAD_INSTRUCTION").await?;
        let attach = gdb.request(&format!("vAttach;{pid:x}")).await?;
        if !attach.starts_with('T') && !attach.starts_with('S') {
            bail!("debugserver couldn't attach to pid {pid}: {attach} (is the app development-signed, and still running?)");
        }
        events.log(format!("attached to pid {pid}; faults left to the app: {}", or_none(&ignored)));

        if let Some(bundle_id) = bring_back {
            let reply = async {
                let mut apps = idevice::core_device::AppServiceClient::connect_rsd(adapter, handshake).await?;
                apps.launch_application(bundle_id, &[], false, false, None, None, None).await
            }
            .await;
            events.log(match reply {
                Ok(_) => format!("brought {bundle_id} back to the front"),
                Err(error) => format!("couldn't bring {bundle_id} back to the front: {error}"),
            });
        }
        Ok((pid, gdb))
    })
    .await;
    let Some(setup) = setup else {
        // Nothing attached yet, or debugserver drops the process with the connection.
        events.log("asked to stop before the debugger attached".into());
        return Ok(json!({ "pid": 0, "events": events.lines, "preparedBytes": 0, "detached": true, "ended": "stopped" }));
    };
    let (pid, mut gdb) = setup?;
    registration.add(pid.to_string(), &stop_requested);

    let mut prepared_bytes = 0u64;
    let mut detached = false;
    let mut served = false;
    let mut resume = "c".to_string();
    let mut last_fault: Option<(u64, u64)> = None;
    let mut fault_repeats = 0u32;
    let mut counts = Counts::default();
    let deadline = (wait_seconds > 0).then(|| Duration::from_secs(wait_seconds as u64));
    let ended = loop {
        gdb.proxy.send_raw(&Gdb::frame(&resume)).await?;
        resume = "c".into();
        // An app that never asks (UTM until a VM starts) never stops, so the wait has to end
        // while it runs: interrupt it, then detach.
        let left = deadline.filter(|_| !served).map(|deadline| deadline.saturating_sub(started.elapsed()));
        let waited = tokio::select! {
            stop = async {
                match left {
                    Some(left) => tokio::time::timeout(left, gdb.read()).await.ok(),
                    None => Some(gdb.read().await),
                }
            } => stop.map(Some),
            _ = stop_requested.notified() => Some(None),
        };
        let stop = match waited {
            Some(Some(stop)) => stop?,
            // Timed out, or asked to stop: interrupt the app, then leave it running.
            reason => {
                let ended = if reason.is_none() { "timed out" } else { "stopped" };
                events.log(if reason.is_none() {
                    "timed out waiting for a JIT request; detaching".into()
                } else {
                    "asked to stop; detaching".into()
                });
                gdb.proxy.send_raw(&[0x03]).await?;
                // Bounded: a process that died meanwhile may never answer.
                let _ = tokio::time::timeout(CLOSE_WAIT, gdb.read()).await;
                events.log(match tokio::time::timeout(CLOSE_WAIT, gdb.request("D")).await {
                    Ok(Ok(reply)) => format!("detached: {reply}"),
                    Ok(Err(error)) => format!("detach failed: {error}"),
                    Err(_) => "detach timed out".into(),
                });
                detached = true;
                break ended;
            }
        };
        if stop.starts_with('W') || stop.starts_with('X') {
            events.log(format!("process exited: {stop}"));
            break "exited";
        }
        let Some(thread) = field(&stop, "thread") else {
            events.log(format!("unexpected stop reply: {}", &stop[..stop.len().min(60)]));
            continue;
        };
        let signal = u8::from_str_radix(stop.get(1..3).unwrap_or("00"), 16).unwrap_or(0);
        let metype = field(&stop, "metype").unwrap_or(0);
        let medata: Vec<u64> = fields(&stop, "medata");

        // EXC_SOFTWARE / EXC_SOFT_SIGNAL: a unix signal on its way to the app (pthread_kill,
        // Wine's thread suspends). Only an old debugserver that ignored QPassSignals stops
        // for these; deliver the original signal, never judge it.
        if metype == 5 && medata.first() == Some(&0x10003) {
            let signo = medata.get(1).copied().filter(|s| (1..=31).contains(s)).unwrap_or(0) as u8;
            counts.signals += 1;
            if counts.signals <= 4 {
                events.log(format!("signal {signo} stopped here; passing it on"));
            }
            if signo != 0 {
                resume = continue_with(signo, thread);
            }
            continue;
        }
        // EXC_RESOURCE: the task crossed its memory high-water mark. An advisory for the whole
        // task, not a fault in the thread it names: resume without a signal.
        if metype == 11 {
            counts.resource += 1;
            if counts.resource <= 4 {
                let code = medata.first().copied().unwrap_or(0);
                events.log(format!("EXC_RESOURCE (code {code:#x}, memory limit {} MB?); continuing", code & 0x1fff));
            }
            continue;
        }

        let pc = gdb.register(PC, thread).await?;
        let word = gdb.request(&format!("m{pc:x},4")).await?;
        let instruction = u32::from_str_radix(&word, 16).map(u32::swap_bytes).ok();
        let immediate = instruction.filter(|i| i & 0xffe0_001f == 0xd420_0000).map(|i| (i >> 5) & 0xffff);
        match immediate {
            Some(0x69) => {
                let (address, length) = (gdb.register(0, thread).await?, gdb.register(1, thread).await?);
                let t = Instant::now();
                let pages = gdb.prepare(address, length).await?;
                prepared_bytes += length;
                events.log(format!("brk #0x69: prepared {} MiB at {address:#x} ({pages} pages) in {:.3}s",
                                   length >> 20, t.elapsed().as_secs_f64()));
                gdb.set_register(PC, thread, pc + 4).await?;
                // QEMU makes one such request per VM; it needs no debugger afterwards.
                let reply = gdb.request("D").await?;
                events.log(format!("detached: {reply}"));
                detached = true;
                break "detached";
            }
            Some(0xf00d) => {
                last_fault = None;
                served = true;
                match gdb.register(X16, thread).await? {
                    // Nothing to prepare (StikDebug's script answers this with nothing too).
                    1 if gdb.register(0, thread).await? == 0 && gdb.register(1, thread).await? == 0 => {
                        gdb.set_register(PC, thread, pc + 4).await?;
                        events.log("brk #0xf00d prepare: nothing asked".into());
                    }
                    1 => {
                        let (mut address, length) = (gdb.register(0, thread).await?, gdb.register(1, thread).await?);
                        if address == 0 {
                            let reply = gdb.request(&format!("_M{length:x},rx")).await?;
                            address = u64::from_str_radix(&reply, 16).unwrap_or(0);
                        }
                        let t = Instant::now();
                        let pages = if address != 0 { gdb.prepare(address, length).await? } else { 0 };
                        prepared_bytes += length;
                        gdb.set_register(0, thread, address).await?;
                        gdb.set_register(PC, thread, pc + 4).await?;
                        counts.prepares += 1;
                        // Wine prepares every DLL it loads: the first few say enough.
                        if counts.prepares <= 8 || length >= 16 << 20 {
                            events.log(format!("brk #0xf00d prepare: {} KiB at {address:#x} ({pages} pages) in {:.3}s",
                                               length >> 10, t.elapsed().as_secs_f64()));
                        }
                    }
                    // Experimental, Apparition only: copy x2 bytes from x1 to x0, which may be the
                    // app's own signed __TEXT (debugserver writes through VM_PROT_COPY).
                    // x0 = 0 on success. Used to test whether patched text runs after detach.
                    // Not 2: that's StikDebug's "here is a script", handled below.
                    0x4150 => {
                        let (destination, source, length) =
                            (gdb.register(0, thread).await?, gdb.register(1, thread).await?, gdb.register(2, thread).await?);
                        let result = if length == 0 || length > 4096 {
                            format!("refused length {length}")
                        } else {
                            let bytes = gdb.request(&format!("m{source:x},{length:x}")).await?;
                            gdb.request(&format!("M{destination:x},{length:x}:{bytes}")).await?
                        };
                        gdb.set_register(0, thread, if result == "OK" { 0 } else { 1 }).await?;
                        gdb.set_register(PC, thread, pc + 4).await?;
                        events.log(format!("brk #0xf00d write: {length} bytes to {destination:#x}: {result}"));
                    }
                    0 => {
                        gdb.set_register(PC, thread, pc + 4).await?;
                        let reply = gdb.request("D").await?;
                        events.log(format!("brk #0xf00d detach: {reply}"));
                        detached = true;
                        break "detached";
                    }
                    // 2 hands StikDebug a script of breakpoint handlers to run, and anything
                    // else is someone else's extension: skip it rather than end the session,
                    // as StikDebug does with commands it doesn't know.
                    other => {
                        gdb.set_register(PC, thread, pc + 4).await?;
                        events.log(format!("brk #0xf00d: skipped command {other:#x}{}",
                                           if other == 2 { " (a StikDebug script; Apparition runs none)" } else { "" }));
                    }
                }
            }
            // A trap of the app's own (Swift's runtime checks are brk #1): undebugged it
            // crashes, so let it. Resuming would only run the BRK again.
            Some(other) => {
                events.log(format!("brk #{other:#x} at {pc:#x} isn't a JIT request; passing SIGTRAP (the app crashes)"));
                resume = continue_with(SIGTRAP, thread);
            }
            None => {
                // A fault the app's own handlers didn't take (or a stop we don't know): hand it
                // back as the signal it stands for. Never skip the instruction.
                let code = medata.first().copied().unwrap_or(0);
                let deliver = match metype {
                    1 if code == 1 => 11, // EXC_BAD_ACCESS, KERN_INVALID_ADDRESS: SIGSEGV
                    1 => 10,              // EXC_BAD_ACCESS otherwise: SIGBUS
                    2 => 4,               // EXC_BAD_INSTRUCTION: SIGILL
                    3 => 8,               // EXC_ARITHMETIC: SIGFPE
                    6 => SIGTRAP,         // EXC_BREAKPOINT that isn't a BRK
                    _ => signal,          // not an exception: the signal debugserver reports
                };
                fault_repeats = if last_fault == Some((thread, pc)) { fault_repeats + 1 } else { 1 };
                last_fault = Some((thread, pc));
                if fault_repeats >= FAULT_REPEATS {
                    events.log(format!("fault at {pc:#x} came back {fault_repeats} times undelivered; killing the app rather than leaving its thread stuck"));
                    gdb.proxy.send_raw(&Gdb::frame("k")).await.ok();
                    break "killed";
                }
                counts.faults += 1;
                if counts.faults <= 16 {
                    events.log(format!("stop at {pc:#x} (signal {signal}, exception {metype}, code {code:#x}); passing signal {deliver}"));
                }
                // A stop that isn't a fault and has no signal (our own interrupt) just resumes.
                if deliver != 0 && !(deliver == SIGTRAP && metype == 0) {
                    resume = continue_with(deliver, thread);
                }
            }
        }
    };
    if events.dropped > 0 {
        events.lines.push(format!("({} more lines not kept)", events.dropped));
    }
    Ok(json!({
        "pid": pid,
        "events": events.lines,
        "preparedBytes": prepared_bytes,
        "detached": detached,
        "ended": ended,
        "prepares": counts.prepares,
        "faults": counts.faults,
        "signals": counts.signals,
    }))
}

#[derive(Default)]
struct Counts {
    prepares: u64,
    faults: u64,
    signals: u64,
    resource: u64,
}

struct Events<'a> {
    lines: Vec<String>,
    dropped: usize,
    started: Instant,
    on_event: &'a mut (dyn FnMut(&str) + Send),
}

impl Events<'_> {
    fn log(&mut self, line: String) {
        let line = format!("{:>7.3}s {line}", self.started.elapsed().as_secs_f64());
        tracing::info!("jit: {line}");
        (self.on_event)(&line);
        if self.lines.len() < EVENT_BUDGET {
            self.lines.push(line);
        } else {
            self.dropped += 1;
        }
    }
}

fn or_none(reply: &str) -> &str {
    if reply.is_empty() { "unsupported" } else { reply }
}

/// Resumes the app with `signal` delivered to `thread`, and every other thread running.
fn continue_with(signal: u8, thread: u64) -> String {
    format!("vCont;C{signal:02x}:{thread:x};c")
}

/// The first `key:<hex>;` in a stop reply.
fn field(reply: &str, key: &str) -> Option<u64> {
    fields(reply, key).into_iter().next()
}

/// Every `key:<hex>;` in a stop reply (`medata` repeats).
fn fields(reply: &str, key: &str) -> Vec<u64> {
    // The first pair follows the "T<signal>".
    let pairs = if reply.starts_with('T') { reply.get(3..).unwrap_or("") } else { reply };
    pairs
        .split(';')
        .filter_map(|pair| pair.split_once(':'))
        .filter(|(k, _)| *k == key)
        .filter_map(|(_, value)| u64::from_str_radix(value.trim_start_matches("0x"), 16).ok())
        .collect()
}

/// Minimal gdb-remote client over the tunnel's debugproxy (no-ack mode, pipelined writes).
struct Gdb {
    proxy: DebugProxyClient<Box<dyn idevice::ReadWrite>>,
}

impl Gdb {
    fn frame(payload: &str) -> Vec<u8> {
        let sum = payload.bytes().fold(0u8, |acc, b| acc.wrapping_add(b));
        format!("${payload}#{sum:02x}").into_bytes()
    }

    async fn read(&mut self) -> Result<String, Report> {
        loop {
            let reply = self.proxy.read_response().await?.ok_or_else(|| report!("debugserver sent no packet"))?;
            // Inferior output forwarded by debugserver; not a reply.
            if reply.starts_with('O') && reply != "OK" {
                continue;
            }
            return Ok(reply);
        }
    }

    async fn request(&mut self, payload: &str) -> Result<String, Report> {
        self.proxy.send_raw(&Self::frame(payload)).await?;
        self.read().await
    }

    async fn pipeline(&mut self, payloads: &[String]) -> Result<Vec<String>, Report> {
        let mut replies = Vec::with_capacity(payloads.len());
        // Bounded batches keep the socket buffers from filling while nobody reads.
        for batch in payloads.chunks(512) {
            let bytes: Vec<u8> = batch.iter().flat_map(|p| Self::frame(p)).collect();
            self.proxy.send_raw(&bytes).await?;
            for _ in batch {
                replies.push(self.read().await?);
            }
        }
        Ok(replies)
    }

    async fn register(&mut self, number: u32, thread: u64) -> Result<u64, Report> {
        let value = self.request(&format!("p{number:x};thread:{thread:x};")).await?;
        let bytes: Vec<u8> = (0..value.len().min(16))
            .step_by(2)
            .filter_map(|i| u8::from_str_radix(&value[i..i + 2], 16).ok())
            .collect();
        if bytes.len() != 8 {
            bail!("cannot read register {number}: {value}");
        }
        Ok(u64::from_le_bytes(bytes.try_into().unwrap()))
    }

    async fn set_register(&mut self, number: u32, thread: u64, value: u64) -> Result<(), Report> {
        let hex: String = value.to_le_bytes().iter().map(|b| format!("{b:02x}")).collect();
        let reply = self.request(&format!("P{number:x}={hex};thread:{thread:x};")).await?;
        if reply != "OK" {
            bail!("cannot write register {number}: {reply}");
        }
        Ok(())
    }

    /// Writes one zero word into every 16 KiB page of [address, address + length): TXM only
    /// lets a page execute after the debugger has written to it. Zeros match fresh memory.
    async fn prepare(&mut self, address: u64, length: u64) -> Result<usize, Report> {
        if address % PAGE != 0 || length == 0 || length > (2u64 << 30) {
            bail!("refusing to prepare {length:#x} bytes at {address:#x}");
        }
        let writes: Vec<String> = (0..length.div_ceil(PAGE))
            .map(|page| format!("M{:x},4:00000000", address + page * PAGE))
            .collect();
        let replies = self.pipeline(&writes).await?;
        if let Some(bad) = replies.iter().find(|r| *r != "OK") {
            bail!("memory write refused: {bad}");
        }
        Ok(writes.len())
    }
}

// MARK: Scripts

/// A debugserver connection lent to a StikDebug JIT script (an `enable-jit` link's
/// `script-data`), which the app runs in JavaScriptCore: the script attaches, continues and
/// answers the app's breakpoints itself, through `send` and `prepare`. Like StikDebug, the
/// connection is only put in no-ack mode; everything else is the script's.
///
/// It may attach only to the process it was opened for, and once that process exits or is
/// detached, or the session is stopped, every further call fails, which ends a script that
/// loops on `c`.
pub struct Script {
    gdb: Gdb,
    pid: u64,
    stop_requested: Arc<tokio::sync::Notify>,
    _registration: Registration,
    started: Instant,
    events: Vec<String>,
    prepared_bytes: u64,
    /// Why the session ended, once it has.
    ended: Option<&'static str>,
}

/// How long closing waits for debugserver to answer an interrupt or a detach.
const CLOSE_WAIT: Duration = Duration::from_secs(5);

impl Script {
    /// Connects to debugserver for `bundle_id`'s process: a new one launched suspended with
    /// `launch` (ending a running instance; the script then attaches), else `pid`, or the app's
    /// running process when that is 0. An app that's attached to while it runs is brought
    /// back to the front: it asked from its own screen, and opening Apparition left it.
    pub async fn open(
        adapter: &mut AdapterHandle,
        handshake: &mut RsdHandshake,
        pid: u64,
        bundle_id: Option<&str>,
        launch: bool,
    ) -> Result<Script, Report> {
        let started = Instant::now();
        let mut events = Vec::new();
        let mut log = |line: String| {
            let line = format!("{:>7.3}s {line}", started.elapsed().as_secs_f64());
            tracing::info!("jit script: {line}");
            events.push(line);
        };
        let stop_requested = Arc::new(tokio::sync::Notify::new());
        // Registered before anything that can wait, so Stop works while it starts, too.
        let mut registration = Registration::new(bundle_id.map(str::to_string).into_iter().collect(), &stop_requested);
        let setup = unless_stopped(&stop_requested, async {
            let pid = match (launch, pid, bundle_id) {
                (true, _, Some(bundle_id)) => {
                    let mut apps = idevice::core_device::AppServiceClient::connect_rsd(adapter, handshake)
                        .await
                        .context("appservice connect failed")?;
                    let launched = apps
                        .launch_application(bundle_id, &[], true, true, None, None, None)
                        .await
                        .context("launch failed")?;
                    log(format!("launched suspended, pid {}", launched.pid));
                    launched.pid as u64
                }
                (true, _, None) => bail!("nothing to launch"),
                (false, 0, Some(bundle_id)) => running_pid(adapter, handshake, bundle_id)
                    .await?
                    .ok_or_else(|| report!("{bundle_id} isn't running"))?,
                (false, 0, None) => bail!("no process to attach to"),
                (false, pid, _) => pid,
            };

            let proxy = DebugProxyClient::connect_rsd(adapter, handshake).await.context("debugproxy connect failed")?;
            let mut gdb = Gdb { proxy };
            gdb.proxy.send_raw(b"+").await?;
            gdb.proxy.send_raw(&Gdb::frame("QStartNoAckMode")).await?;
            let ack = gdb.proxy.read_response().await?;
            gdb.proxy.set_ack_mode(false);
            log(format!("no-ack mode: {}", ack.unwrap_or_default()));

            if let (false, Some(bundle_id)) = (launch, bundle_id) {
                let reply = async {
                    let mut apps = idevice::core_device::AppServiceClient::connect_rsd(adapter, handshake).await?;
                    apps.launch_application(bundle_id, &[], false, false, None, None, None).await
                }
                .await;
                log(match reply {
                    Ok(_) => format!("brought {bundle_id} back to the front"),
                    Err(error) => format!("couldn't bring {bundle_id} back to the front: {error}"),
                });
            }
            Ok((pid, gdb))
        })
        .await;
        let (pid, gdb) = setup.ok_or_else(|| report!("Apparition stopped this JIT session"))??;
        registration.add(pid.to_string(), &stop_requested);
        Ok(Script { gdb, pid, stop_requested, _registration: registration, started, events, prepared_bytes: 0, ended: None })
    }

    pub fn pid(&self) -> u64 {
        self.pid
    }

    fn log(&mut self, line: String) {
        let line = format!("{:>7.3}s {line}", self.started.elapsed().as_secs_f64());
        tracing::info!("jit script: {line}");
        if self.events.len() < EVENT_BUDGET {
            self.events.push(line);
        }
    }

    fn check_running(&self) -> Result<(), Report> {
        match self.ended {
            None => Ok(()),
            Some("exited") => bail!("the app exited"),
            Some("detached") => bail!("the debugger has detached"),
            Some(_) => bail!("Apparition stopped this JIT session"),
        }
    }

    /// Sends one gdb-remote packet (without its `$…#xx` framing) and returns the reply,
    /// skipping the app's forwarded output. A resume waits for the app's next stop, however
    /// long, unless the session is stopped meanwhile: the app is then interrupted and
    /// detached, and this fails.
    pub async fn send(&mut self, command: &str) -> Result<String, Report> {
        self.check_running()?;
        if let Some(target) = attach_target(command)
            && target != Some(self.pid)
        {
            bail!("this session may only attach to pid {}", self.pid);
        }
        self.gdb.proxy.send_raw(&Gdb::frame(command)).await?;
        let reply = tokio::select! {
            reply = self.gdb.read() => reply?,
            _ = self.stop_requested.notified() => {
                self.log("asked to stop; detaching".into());
                self.interrupt_and_detach().await;
                self.ended = Some("stopped");
                bail!("Apparition stopped this JIT session");
            }
        };
        if reply.starts_with('W') || reply.starts_with('X') {
            self.log(format!("process exited: {reply}"));
            self.ended = Some("exited");
        } else if command == "D" && reply == "OK" {
            self.log("the script detached".into());
            self.ended = Some("detached");
        } else if command == "k" {
            self.log("the script killed the app".into());
            self.ended = Some("exited");
        }
        Ok(reply)
    }

    /// Makes [address, address + length) executable for the app (StikDebug's
    /// `prepare_memory_region`). Returns the number of pages written.
    pub async fn prepare(&mut self, address: u64, length: u64) -> Result<usize, Report> {
        self.check_running()?;
        let t = Instant::now();
        let pages = self.gdb.prepare(address, length).await?;
        self.prepared_bytes += length;
        self.log(format!("prepared {} KiB at {address:#x} ({pages} pages) in {:.3}s",
                         length >> 10, t.elapsed().as_secs_f64()));
        Ok(pages)
    }

    /// Ends the session once the script has returned, as StikDebug does: a process still
    /// attached is interrupted and detached, and keeps running. Returns
    /// `{pid, events, preparedBytes, detached, ended}`.
    pub async fn close(mut self) -> Value {
        let ended = match self.ended {
            Some(ended) => ended,
            None => {
                self.log("script finished; detaching".into());
                self.interrupt_and_detach().await;
                "detached"
            }
        };
        json!({
            "pid": self.pid,
            "events": self.events,
            "preparedBytes": self.prepared_bytes,
            "detached": ended != "exited",
            "ended": ended,
        })
    }

    /// Best effort, bounded: the app may be stopped already (no reply to the interrupt) or
    /// gone.
    async fn interrupt_and_detach(&mut self) {
        if self.gdb.proxy.send_raw(&[0x03]).await.is_ok() {
            let _ = tokio::time::timeout(CLOSE_WAIT, self.gdb.read()).await;
        }
        let reply = tokio::time::timeout(CLOSE_WAIT, self.gdb.request("D")).await;
        self.log(match reply {
            Ok(Ok(reply)) => format!("detached: {reply}"),
            Ok(Err(error)) => format!("detach failed: {error}"),
            Err(_) => "detach timed out".into(),
        });
    }
}

/// For a packet that attaches debugserver to a process: the pid it names (None when it names
/// none, as `vAttachName` and `vRun` do). None for any other packet.
fn attach_target(command: &str) -> Option<Option<u64>> {
    if let Some(pid) = command.strip_prefix("vAttach;") {
        return Some(u64::from_str_radix(pid, 16).ok());
    }
    ["vAttach", "vRun", "A"].iter().any(|prefix| command.starts_with(prefix)).then_some(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_attach_targets() {
        assert_eq!(attach_target("vAttach;1a2b"), Some(Some(0x1a2b)));
        assert_eq!(attach_target("vAttach;zz"), Some(None));
        assert_eq!(attach_target("vAttachName;4c697665"), Some(None));
        assert_eq!(attach_target("vAttachWait;4c697665"), Some(None));
        assert_eq!(attach_target("vRun;2f62696e"), Some(None));
        assert_eq!(attach_target("A10,0,2f62696e"), Some(None));
        assert_eq!(attach_target("c"), None);
        assert_eq!(attach_target("vCont;c"), None);
        assert_eq!(attach_target("m1000,4"), None);
        assert_eq!(attach_target("P20=0010000000000000;thread:1a;"), None);
    }

    #[test]
    fn reads_stop_reply_fields() {
        let stop = "T05thread:1a2b;threads:1a2b,1a2c;00:0000000000000000;metype:6;mecount:2;medata:1;medata:0;";
        assert_eq!(field(stop, "thread"), Some(0x1a2b));
        assert_eq!(field(stop, "metype"), Some(6));
        assert_eq!(fields(stop, "medata"), vec![1, 0]);
        let soft = "T11thread:7;metype:5;mecount:2;medata:10003;medata:1e;";
        assert_eq!(fields(soft, "medata"), vec![0x10003, 0x1e]);
        assert_eq!(field("S05", "thread"), None);
    }
}
