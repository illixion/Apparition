//! Keeps a Mac awake while devices use its companion.
//!
//! A sleeping Mac with "Wake for network access" on wakes when a TCP SYN arrives for a port it
//! listens on, which is how Apparition on a device wakes it (it knocks on the relay port at the
//! Mac's LAN address). Woken that way, macOS goes back to sleep about a minute later, in the
//! middle of a refresh. So every relay, pairing and channel connection holds an IOKit
//! "prevent idle system sleep" assertion (what `caffeinate -i` takes), kept for a few minutes
//! after the last one closes, since a refresh opens several in a row. The display may still
//! sleep. Elsewhere this does nothing.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long the Mac stays awake after the last connection closes.
const TAIL: Duration = Duration::from_secs(180);

struct State {
    active: usize,
    last: Option<Instant>,
    #[cfg(target_os = "macos")]
    assertion: Option<u32>,
    watching: bool,
}

static STATE: Mutex<State> = Mutex::new(State {
    active: 0,
    last: None,
    #[cfg(target_os = "macos")]
    assertion: None,
    watching: false,
});

/// Held for as long as a connection is open: the Mac doesn't idle-sleep meanwhile, nor for
/// [`TAIL`] after.
pub struct Busy(());

impl Drop for Busy {
    fn drop(&mut self) {
        let mut state = STATE.lock().unwrap();
        state.active = state.active.saturating_sub(1);
        state.last = Some(Instant::now());
    }
}

/// A connection started.
pub fn busy() -> Busy {
    let mut state = STATE.lock().unwrap();
    state.active += 1;
    state.last = Some(Instant::now());
    hold(&mut state);
    if !state.watching {
        state.watching = true;
        std::thread::Builder::new()
            .name("stay-awake".into())
            .spawn(watch)
            .map_err(|_| state.watching = false)
            .ok();
    }
    Busy(())
}

/// Whether the Mac is being kept awake now.
pub fn holding() -> bool {
    #[cfg(target_os = "macos")]
    {
        STATE.lock().unwrap().assertion.is_some()
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

fn wanted(state: &State) -> bool {
    state.active > 0 || state.last.is_some_and(|last| last.elapsed() < TAIL)
}

/// Releases the assertion once nothing has used the companion for [`TAIL`].
fn watch() {
    loop {
        std::thread::sleep(Duration::from_secs(15));
        let mut state = STATE.lock().unwrap();
        if !wanted(&state) {
            release(&mut state);
        }
    }
}

#[cfg(target_os = "macos")]
mod iokit {
    use std::ffi::{c_char, c_void};

    pub type CFStringRef = *const c_void;

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        pub fn CFStringCreateWithCString(allocator: *const c_void, text: *const c_char, encoding: u32) -> CFStringRef;
        pub fn CFRelease(object: *const c_void);
    }

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        pub fn IOPMAssertionCreateWithName(kind: CFStringRef, level: u32, name: CFStringRef, id: *mut u32) -> i32;
        pub fn IOPMAssertionRelease(id: u32) -> i32;
    }

    pub const UTF8: u32 = 0x0800_0100;
    pub const LEVEL_ON: u32 = 255;
}

#[cfg(target_os = "macos")]
fn hold(state: &mut State) {
    if state.assertion.is_some() {
        return;
    }
    use iokit::*;
    unsafe {
        let kind = CFStringCreateWithCString(std::ptr::null(), c"PreventUserIdleSystemSleep".as_ptr(), UTF8);
        let name = CFStringCreateWithCString(std::ptr::null(), c"Apparition companion serving a device".as_ptr(), UTF8);
        let mut id = 0u32;
        if !kind.is_null() && !name.is_null() && IOPMAssertionCreateWithName(kind, LEVEL_ON, name, &mut id) == 0 {
            state.assertion = Some(id);
        }
        if !kind.is_null() {
            CFRelease(kind);
        }
        if !name.is_null() {
            CFRelease(name);
        }
    }
}

#[cfg(target_os = "macos")]
fn release(state: &mut State) {
    if let Some(id) = state.assertion.take() {
        unsafe { iokit::IOPMAssertionRelease(id) };
    }
}

#[cfg(not(target_os = "macos"))]
fn hold(_: &mut State) {}

#[cfg(not(target_os = "macos"))]
fn release(_: &mut State) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_connection_holds_the_mac_awake_and_its_tail_outlasts_it() {
        let busy = busy();
        assert_eq!(holding(), cfg!(target_os = "macos"));
        drop(busy);
        let state = STATE.lock().unwrap();
        assert!(wanted(&state), "the tail keeps it awake after the connection closes");
        assert_eq!(state.active, 0);
    }
}
