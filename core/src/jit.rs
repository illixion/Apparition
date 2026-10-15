//! Serving an app's JIT requests as its debugger, through the tunnel's debugserver.
//!
//! The app is launched suspended or attached to while it runs, and then served until it no
//! longer needs a debugger:
//! - legacy `brk #0x69` (x0 = region, x1 = size; UTM's QEMU): prepare in place, then detach;
//! - universal `brk #0xf00d` (x16 = 1 prepare x0/x1, x16 = 0 detach; StikDebug's JIT26). Its
//!   x16 = 2 (a script for StikDebug to run) and unknown commands are skipped.
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
    /// Attach to the running process `pid`. With `bring_back`, that app is brought to the
    /// front again afterwards: it asked from its own screen, and opening Apparition left it.
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

    let (pid, bring_back, bundle_id) = match target {
        Target::Launch(bundle_id) => {
            let mut apps = idevice::core_device::AppServiceClient::connect_rsd(adapter, handshake)
                .await
                .context("appservice connect failed")?;
            let launched = apps
                .launch_application(bundle_id, &[], true, true, None, None, None)
                .await
                .context("launch failed")?;
            events.log(format!("launched suspended, pid {}", launched.pid));
            (launched.pid as u64, None, Some(bundle_id))
        }
        Target::Attach { pid, bring_back } => (pid, bring_back, bring_back),
    };
    let stop_requested = Arc::new(tokio::sync::Notify::new());
    let _registration = Registration::new(
        std::iter::once(pid.to_string()).chain(bundle_id.map(str::to_string)).collect(),
        &stop_requested,
    );

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
                gdb.read().await?;
                gdb.request("D").await.ok();
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

#[cfg(test)]
mod tests {
    use super::*;

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
