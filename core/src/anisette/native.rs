//! This Mac's own anisette, from macOS itself rather than Apple's Android libraries.
//!
//! Since 2026-10-12 Apple checks that the anisette matches the device the sign-in claims to be
//! (see claimed_platform). The Android libraries' anisette signs in claiming Android, which gets
//! no 2FA step and so needs the code Apple pushes checked separately; a Mac's own signs in
//! claiming a Mac, through the usual 2FA, so on a Mac it's preferred. A Mac can give its anisette
//! to other programs in two ways:
//!
//! - AOSKit's `+[AOSUtilities retrieveOTPHeadersForDSID:]`, in this process. It needs no
//!   entitlement up to macOS 26. On macOS 27 the daemon behind it (`adid`) refuses programs that
//!   aren't Apple's, and the call returns nothing.
//! - AuthKit's `-[AKAppleIDSession appleIDHeadersForRequest:]`, which needs AuthKit's private
//!   client entitlements. Only a program signed with them gets the headers, and macOS runs such a
//!   program only with SIP and AMFI off (`amfi_get_out_of_my_way=1`). That program is
//!   `apparition-anisette-helper` ([`helper_main`]), next to this one.
//!
//! Only the headers come from the Mac. Provisioning still goes through the Android libraries
//! (Apple accepts it), so a client's stored identity works with any server, Mac or not.
//!
//! The names and the behaviour of these private APIs were found on this machine with the
//! Objective-C runtime (class and method lists) and by trying them; nothing was copied.

use std::ffi::{CStr, CString, c_char, c_void};
use std::path::PathBuf;
use std::sync::OnceLock;

use rootcause::prelude::*;

type Id = *mut c_void;
type Sel = *mut c_void;

#[link(name = "objc")]
unsafe extern "C" {
    fn objc_getClass(name: *const c_char) -> Id;
    fn sel_registerName(name: *const c_char) -> Sel;
    fn objc_msgSend();
    fn objc_autoreleasePoolPush() -> *mut c_void;
    fn objc_autoreleasePoolPop(pool: *mut c_void);
}

#[link(name = "Foundation", kind = "framework")]
unsafe extern "C" {}

unsafe extern "C" {
    fn dlopen(path: *const c_char, mode: i32) -> *mut c_void;
}

const RTLD_NOW: i32 = 2;

fn class(name: &str) -> Id {
    let name = CString::new(name).unwrap();
    unsafe { objc_getClass(name.as_ptr()) }
}

fn sel(name: &str) -> Sel {
    let name = CString::new(name).unwrap();
    unsafe { sel_registerName(name.as_ptr()) }
}

/// `[receiver selector]`, for selectors that return an object.
unsafe fn send0(receiver: Id, selector: &str) -> Id {
    let send: unsafe extern "C" fn(Id, Sel) -> Id = unsafe { std::mem::transmute(objc_msgSend as unsafe extern "C" fn()) };
    unsafe { send(receiver, sel(selector)) }
}

/// `[receiver selector:argument]`, for selectors that take and return an object.
unsafe fn send1(receiver: Id, selector: &str, argument: Id) -> Id {
    let send: unsafe extern "C" fn(Id, Sel, Id) -> Id = unsafe { std::mem::transmute(objc_msgSend as unsafe extern "C" fn()) };
    unsafe { send(receiver, sel(selector), argument) }
}

unsafe fn ns_string(text: &str) -> Id {
    let text = CString::new(text).unwrap();
    let make: unsafe extern "C" fn(Id, Sel, *const c_char) -> Id = unsafe { std::mem::transmute(objc_msgSend as unsafe extern "C" fn()) };
    unsafe { make(class("NSString"), sel("stringWithUTF8String:"), text.as_ptr()) }
}

/// The string `dictionary[key]`, if there is one.
unsafe fn string_for(dictionary: Id, key: &str) -> Option<String> {
    if dictionary.is_null() {
        return None;
    }
    let value = unsafe { send1(dictionary, "objectForKey:", ns_string(key)) };
    if value.is_null() {
        return None;
    }
    let utf8: unsafe extern "C" fn(Id, Sel) -> *const c_char = unsafe { std::mem::transmute(objc_msgSend as unsafe extern "C" fn()) };
    let text = unsafe { utf8(value, sel("UTF8String")) };
    if text.is_null() {
        return None;
    }
    let text = unsafe { CStr::from_ptr(text) }.to_string_lossy().into_owned();
    (!text.is_empty()).then_some(text)
}

fn load(framework: &str) -> bool {
    let path = CString::new(format!("/System/Library/PrivateFrameworks/{framework}.framework/{framework}")).unwrap();
    !unsafe { dlopen(path.as_ptr(), RTLD_NOW) }.is_null()
}

/// This Mac's anisette: the machine id (`X-Apple-I-MD-M`) and one-time password
/// (`X-Apple-I-MD`), both base64 as Apple sends them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Headers {
    pub machine_id: String,
    pub one_time_password: String,
}

impl Headers {
    fn from_json(text: &str) -> Option<Headers> {
        let value: serde_json::Value = serde_json::from_str(text.trim()).ok()?;
        Some(Headers {
            machine_id: value.get("X-Apple-I-MD-M")?.as_str()?.to_string(),
            one_time_password: value.get("X-Apple-I-MD")?.as_str()?.to_string(),
        })
    }

    fn to_json(&self) -> String {
        serde_json::json!({ "X-Apple-I-MD-M": self.machine_id, "X-Apple-I-MD": self.one_time_password }).to_string()
    }
}

/// AOSKit, in this process: works without entitlements up to macOS 26.
fn from_aoskit() -> Option<Headers> {
    if !load("AOSKit") {
        return None;
    }
    unsafe {
        let pool = objc_autoreleasePoolPush();
        let utilities = class("AOSUtilities");
        let headers = if utilities.is_null() {
            None
        } else {
            // "-2" is the DSID for a device that isn't signed in to an Apple ID.
            let headers = send1(utilities, "retrieveOTPHeadersForDSID:", ns_string("-2"));
            match (string_for(headers, "X-Apple-MD-M"), string_for(headers, "X-Apple-MD")) {
                (Some(machine_id), Some(one_time_password)) => Some(Headers { machine_id, one_time_password }),
                _ => None,
            }
        };
        objc_autoreleasePoolPop(pool);
        headers
    }
}

/// AuthKit, in this process: only gives the anisette to a program signed with AuthKit's private
/// client entitlements, which is what [`helper_main`] runs as.
fn from_authkit() -> Option<Headers> {
    if !load("AuthKit") {
        return None;
    }
    unsafe {
        let pool = objc_autoreleasePoolPush();
        let session_class = class("AKAppleIDSession");
        let headers = if session_class.is_null() {
            None
        } else {
            let session = send1(send0(session_class, "alloc"), "initWithIdentifier:", ns_string("com.apple.gs.xcode.auth"));
            let url = send1(class("NSURL"), "URLWithString:", ns_string("https://gsa.apple.com/grandslam/GsService2"));
            let request = send1(class("NSURLRequest"), "requestWithURL:", url);
            let headers = send1(session, "appleIDHeadersForRequest:", request);
            let found = match (string_for(headers, "X-Apple-I-MD-M"), string_for(headers, "X-Apple-I-MD")) {
                (Some(machine_id), Some(one_time_password)) => Some(Headers { machine_id, one_time_password }),
                _ => None,
            };
            let release: unsafe extern "C" fn(Id, Sel) = std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
            release(session, sel("release"));
            found
        };
        objc_autoreleasePoolPop(pool);
        headers
    }
}

/// The entitled helper's file name; it sits next to the program that uses it.
pub const HELPER_NAME: &str = "apparition-anisette-helper";

fn helper_path() -> Option<PathBuf> {
    let path = std::env::current_exe().ok()?.parent()?.join(HELPER_NAME);
    path.is_file().then_some(path)
}

/// The entitled helper, as a separate process. macOS kills it on start unless SIP and AMFI are
/// off, which is the only case it can help in (macOS 27).
fn from_helper() -> Option<Headers> {
    let output = std::process::Command::new(helper_path()?).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Headers::from_json(std::str::from_utf8(&output.stdout).ok()?)
}

/// Runs `f` with this process's stderr sent to /dev/null: AOSKit logs a warning there when
/// macOS 27 refuses it, which is expected while finding the source.
fn quietly<T>(f: impl FnOnce() -> T) -> T {
    let null = std::fs::OpenOptions::new().write(true).open("/dev/null");
    let saved = unsafe { libc::dup(libc::STDERR_FILENO) };
    if let (Ok(null), true) = (&null, saved >= 0) {
        use std::os::fd::AsRawFd;
        unsafe { libc::dup2(null.as_raw_fd(), libc::STDERR_FILENO) };
        let result = f();
        unsafe {
            libc::dup2(saved, libc::STDERR_FILENO);
            libc::close(saved);
        }
        return result;
    }
    if saved >= 0 {
        unsafe { libc::close(saved) };
    }
    f()
}

/// Where this Mac's anisette comes from, once it's known that it can be had.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    AOSKit,
    Helper,
}

/// Which way of getting this Mac's anisette works, found once per process. `None` when neither
/// does: macOS 27 with SIP or AMFI on, or no helper.
pub fn source() -> Option<Source> {
    static SOURCE: OnceLock<Option<Source>> = OnceLock::new();
    *SOURCE.get_or_init(|| {
        if quietly(from_aoskit).is_some() {
            Some(Source::AOSKit)
        } else if from_helper().is_some() {
            Some(Source::Helper)
        } else {
            None
        }
    })
}

/// Fresh headers from [`source`]. Blocks briefly (an XPC round trip, or the helper's start).
pub fn headers() -> Result<Headers, Report> {
    let headers = match source() {
        Some(Source::AOSKit) => from_aoskit(),
        Some(Source::Helper) => from_helper(),
        None => bail!("this Mac doesn't give its anisette to other programs"),
    };
    headers.ok_or_else(|| report!("this Mac stopped giving its anisette"))
}

/// The helper program: prints AuthKit's anisette as JSON and exits 0, or exits 1 if AuthKit
/// gave none. Signed with AuthKit's private entitlements by the build.
pub fn helper_main() -> i32 {
    match from_authkit() {
        Some(headers) => {
            println!("{}", headers.to_json());
            0
        }
        None => {
            eprintln!("AuthKit gave no anisette (this program needs SIP and AMFI off)");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helper_json_round_trips() {
        let headers = Headers { machine_id: "bWFjaGluZQ==".into(), one_time_password: "b3Rw".into() };
        assert_eq!(Headers::from_json(&headers.to_json()), Some(headers));
        assert_eq!(Headers::from_json("{\"X-Apple-I-MD\": \"b3Rw\"}"), None);
        assert_eq!(Headers::from_json("not json"), None);
    }

    /// Prints which source works on this Mac, never the values. `APPARITION_NATIVE=1`.
    #[test]
    fn reports_this_macs_source() {
        if std::env::var_os("APPARITION_NATIVE").is_none() {
            return;
        }
        let source = source();
        eprintln!("native anisette source: {source:?}");
        if source.is_some() {
            let headers = headers().unwrap();
            assert!(headers.machine_id.len() > 16 && headers.one_time_password.len() > 16);
        }
    }
}
