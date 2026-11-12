//! The "libc" a loaded Android library sees: what its imports resolve to.
//!
//! Most of Bionic's functions mean the same here and are passed straight through. The ones that
//! don't are written out below, from the Linux kernel's UAPI headers (`asm-generic/stat.h`,
//! `arch/x86/include/uapi/asm/stat.h`, `asm-generic/fcntl.h`) rather than from any other loader:
//!
//! - `struct stat` and `struct timeval` have other layouts on macOS;
//! - `open` takes its mode as a variadic argument, which Apple's arm64 calling convention passes
//!   on the stack and Linux's passes in a register, so the mode is a plain parameter here, and
//!   the flag bits are the Linux numbers;
//! - `mode_t` is 16 bits here and 32 there;
//! - `errno` lives behind `__errno` rather than `__error`.
//!
//! Locks are no-ops, because everything that calls into the library runs on one thread (see
//! `adi.rs`). Anything else the libraries import resolves to a stub that aborts when called, so
//! a code path nobody expected fails loudly instead of running with a wrong answer.

use std::collections::HashMap;
use std::ffi::{CStr, c_char, c_int, c_void};
use std::sync::{Mutex, OnceLock};

use super::elf::{Import, Library};

// Linux values (asm-generic/fcntl.h; the same on arm64 and x86-64 for these).
const LINUX_O_ACCMODE: c_int = 0o3;
const LINUX_O_CREAT: c_int = 0o100;
const LINUX_O_EXCL: c_int = 0o200;
const LINUX_O_TRUNC: c_int = 0o1000;
const LINUX_O_APPEND: c_int = 0o2000;
const LINUX_O_CLOEXEC: c_int = 0o2000000;

/// `struct stat` as Linux lays it out (asm-generic/stat.h, which arm64 uses).
#[cfg(target_arch = "aarch64")]
#[repr(C)]
struct LinuxStat {
    dev: u64,
    ino: u64,
    mode: u32,
    nlink: u32,
    uid: u32,
    gid: u32,
    rdev: u64,
    _pad1: u64,
    size: i64,
    blksize: i32,
    _pad2: i32,
    blocks: i64,
    atime: i64,
    atime_nsec: u64,
    mtime: i64,
    mtime_nsec: u64,
    ctime: i64,
    ctime_nsec: u64,
    _unused: [u32; 2],
}

/// `struct stat` as Linux lays it out (arch/x86/include/uapi/asm/stat.h, x86-64).
#[cfg(target_arch = "x86_64")]
#[repr(C)]
struct LinuxStat {
    dev: u64,
    ino: u64,
    nlink: u64,
    mode: u32,
    uid: u32,
    gid: u32,
    _pad0: u32,
    rdev: u64,
    size: i64,
    blksize: i64,
    blocks: i64,
    atime: i64,
    atime_nsec: u64,
    mtime: i64,
    mtime_nsec: u64,
    ctime: i64,
    ctime_nsec: u64,
    _unused: [i64; 3],
}

#[repr(C)]
struct LinuxTimeval {
    sec: i64,
    usec: i64,
}

fn convert(stat: &libc::stat, out: &mut LinuxStat) {
    // Zero first, so the padding is zero too.
    unsafe { std::ptr::write_bytes(out, 0, 1) };
    out.dev = stat.st_dev as u64;
    out.ino = stat.st_ino;
    out.mode = u32::from(stat.st_mode);
    out.nlink = stat.st_nlink as _;
    out.uid = stat.st_uid;
    out.gid = stat.st_gid;
    out.rdev = stat.st_rdev as u64;
    out.size = stat.st_size;
    out.blksize = stat.st_blksize as _;
    out.blocks = stat.st_blocks;
    out.atime = stat.st_atime;
    out.atime_nsec = stat.st_atime_nsec as u64;
    out.mtime = stat.st_mtime;
    out.mtime_nsec = stat.st_mtime_nsec as u64;
    out.ctime = stat.st_ctime;
    out.ctime_nsec = stat.st_ctime_nsec as u64;
}

unsafe extern "C" fn shim_fstat(fd: c_int, out: *mut LinuxStat) -> c_int {
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    let result = unsafe { libc::fstat(fd, &mut stat) };
    if result == 0 {
        convert(&stat, unsafe { &mut *out });
    }
    result
}

unsafe extern "C" fn shim_lstat(path: *const c_char, out: *mut LinuxStat) -> c_int {
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    let result = unsafe { libc::lstat(path, &mut stat) };
    if result == 0 {
        convert(&stat, unsafe { &mut *out });
    }
    result
}

unsafe extern "C" fn shim_open(path: *const c_char, flags: c_int, mode: u32) -> c_int {
    let mut host = match flags & LINUX_O_ACCMODE {
        1 => libc::O_WRONLY,
        2 => libc::O_RDWR,
        _ => libc::O_RDONLY,
    };
    for (linux, mine) in [
        (LINUX_O_CREAT, libc::O_CREAT),
        (LINUX_O_EXCL, libc::O_EXCL),
        (LINUX_O_TRUNC, libc::O_TRUNC),
        (LINUX_O_APPEND, libc::O_APPEND),
        (LINUX_O_CLOEXEC, libc::O_CLOEXEC),
    ] {
        if flags & linux != 0 {
            host |= mine;
        }
    }
    unsafe { libc::open(path, host, mode as libc::c_uint) }
}

unsafe extern "C" fn shim_gettimeofday(out: *mut LinuxTimeval, _timezone: *mut c_void) -> c_int {
    let mut now = libc::timeval { tv_sec: 0, tv_usec: 0 };
    let result = unsafe { libc::gettimeofday(&mut now, std::ptr::null_mut()) };
    if !out.is_null() {
        unsafe { out.write(LinuxTimeval { sec: now.tv_sec as i64, usec: i64::from(now.tv_usec) }) };
    }
    result
}

unsafe extern "C" fn shim_chmod(path: *const c_char, mode: u32) -> c_int {
    unsafe { libc::chmod(path, mode as libc::mode_t) }
}

unsafe extern "C" fn shim_mkdir(path: *const c_char, mode: u32) -> c_int {
    unsafe { libc::mkdir(path, mode as libc::mode_t) }
}

unsafe extern "C" fn shim_umask(mask: u32) -> u32 {
    u32::from(unsafe { libc::umask(mask as libc::mode_t) })
}

unsafe extern "C" fn shim_errno() -> *mut c_int {
    #[cfg(target_vendor = "apple")]
    unsafe {
        libc::__error()
    }
    #[cfg(not(target_vendor = "apple"))]
    unsafe {
        libc::__errno_location()
    }
}

unsafe extern "C" fn shim_arc4random() -> u32 {
    let mut bytes = [0u8; 4];
    // (The libc crate has no getentropy for musl.)
    #[cfg(target_vendor = "apple")]
    unsafe {
        libc::getentropy(bytes.as_mut_ptr().cast(), bytes.len())
    };
    #[cfg(not(target_vendor = "apple"))]
    unsafe {
        libc::getrandom(bytes.as_mut_ptr().cast(), bytes.len(), 0)
    };
    u32::from_ne_bytes(bytes)
}

unsafe extern "C" fn shim_memcpy_chk(to: *mut c_void, from: *const c_void, length: usize, _capacity: usize) -> *mut c_void {
    unsafe { libc::memcpy(to, from, length) }
}

unsafe extern "C" fn shim_memmove_chk(to: *mut c_void, from: *const c_void, length: usize, _capacity: usize) -> *mut c_void {
    unsafe { libc::memmove(to, from, length) }
}

/// No Android system properties here: every one reads as unset.
unsafe extern "C" fn shim_property_get(_name: *const c_char, value: *mut c_char) -> c_int {
    unsafe { *value = 0 };
    0
}

unsafe extern "C" fn shim_ok() -> c_int {
    0
}

unsafe extern "C" fn shim_ok_finalize(_dso: *mut c_void) {}

/// Bionic's `pthread_once_t` is one `int`, 0 until the routine has run.
unsafe extern "C" fn shim_pthread_once(once: *mut c_int, routine: unsafe extern "C" fn()) -> c_int {
    unsafe {
        if *once == 0 {
            *once = 1;
            routine();
        }
    }
    0
}

unsafe extern "C" fn shim_abort() -> ! {
    std::process::abort()
}

static MISSING: Mutex<Vec<String>> = Mutex::new(Vec::new());

unsafe extern "C" fn shim_unresolved() -> ! {
    let names = MISSING.lock().map(|names| names.join(", ")).unwrap_or_default();
    eprintln!("anisette: a library called something that isn't provided (one of: {names})");
    std::process::abort()
}

// MARK: dlopen and friends

static LOADED: Mutex<Vec<(String, &'static Library)>> = Mutex::new(Vec::new());

/// Makes `library` what `dlopen` returns for a path ending in `name`.
pub(super) fn register(name: &str, library: &'static Library) {
    LOADED.lock().expect("loaded libraries").push((name.to_owned(), library));
}

unsafe extern "C" fn shim_dlopen(path: *const c_char, _flags: c_int) -> *mut c_void {
    if path.is_null() {
        return std::ptr::dangling_mut();
    }
    let path = unsafe { CStr::from_ptr(path) }.to_string_lossy();
    let name = path.rsplit('/').next().unwrap_or(&path);
    let loaded = LOADED.lock().expect("loaded libraries");
    match loaded.iter().find(|(known, _)| known == name) {
        Some((_, library)) => std::ptr::from_ref::<Library>(library).cast_mut().cast(),
        None => std::ptr::null_mut(),
    }
}

unsafe extern "C" fn shim_dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void {
    let name = unsafe { CStr::from_ptr(name) }.to_string_lossy();
    let loaded = LOADED.lock().expect("loaded libraries");
    let found = match loaded.iter().find(|(_, library)| std::ptr::from_ref::<Library>(library).cast::<c_void>() == handle.cast_const()) {
        Some((_, library)) => library.symbol(&name),
        // The program itself (a null or placeholder handle): the libraries, then the shims.
        None => loaded.iter().find_map(|(_, library)| library.symbol(&name)).or_else(|| function(&name)),
    };
    found.map_or(std::ptr::null_mut(), |address| address as *mut c_void)
}

// MARK: Resolution

/// The address of the replacement for the function `name`, if there is one.
fn function(name: &str) -> Option<usize> {
    macro_rules! direct {
        ($($libc:ident),* $(,)?) => { match name { $(stringify!($libc) => Some(libc::$libc as *const () as usize),)* _ => None } };
    }
    let mine: Option<usize> = match name {
        "fstat" => Some(shim_fstat as *const () as usize),
        "lstat" => Some(shim_lstat as *const () as usize),
        "open" => Some(shim_open as *const () as usize),
        "gettimeofday" => Some(shim_gettimeofday as *const () as usize),
        "chmod" => Some(shim_chmod as *const () as usize),
        "mkdir" => Some(shim_mkdir as *const () as usize),
        "umask" => Some(shim_umask as *const () as usize),
        "__errno" => Some(shim_errno as *const () as usize),
        "arc4random" => Some(shim_arc4random as *const () as usize),
        "__memcpy_chk" => Some(shim_memcpy_chk as *const () as usize),
        "__memmove_chk" => Some(shim_memmove_chk as *const () as usize),
        "__system_property_get" => Some(shim_property_get as *const () as usize),
        "__cxa_atexit" | "pthread_rwlock_init" | "pthread_rwlock_destroy" | "pthread_rwlock_rdlock" | "pthread_rwlock_wrlock" | "pthread_rwlock_unlock" => Some(shim_ok as *const () as usize),
        "__cxa_finalize" => Some(shim_ok_finalize as *const () as usize),
        "pthread_once" => Some(shim_pthread_once as *const () as usize),
        "abort" | "__stack_chk_fail" => Some(shim_abort as *const () as usize),
        "dlopen" => Some(shim_dlopen as *const () as usize),
        "dlsym" => Some(shim_dlsym as *const () as usize),
        "dlclose" => Some(shim_ok as *const () as usize),
        _ => None,
    };
    // These take and return the same things as the Linux versions.
    mine.or_else(|| {
        direct!(
            malloc, free, calloc, realloc, memchr, memcmp, memcpy, memmove, memset, strlen, strcmp, strncmp, strncpy,
            strcasecmp, strtol, strtoll, strtoull, strtod, atof, close, read, write, ftruncate,
        )
    })
}

/// Zeroed storage for the data symbols a library imports (CoreFoundation's constants and the
/// like). Reading them gives zeros, which is what the code that never runs would want.
fn object(name: &str) -> usize {
    static OBJECTS: OnceLock<Mutex<HashMap<String, usize>>> = OnceLock::new();
    let mut objects = OBJECTS.get_or_init(Default::default).lock().expect("objects");
    *objects.entry(name.to_owned()).or_insert_with(|| Box::leak(vec![0u8; 4096].into_boxed_slice()).as_ptr() as usize)
}

/// Resolves a library's undefined symbol: our replacement, zeroed storage for data, or a stub
/// that aborts when called.
pub(super) fn resolve(import: &Import) -> Option<usize> {
    if import.object {
        return Some(object(import.name));
    }
    function(import.name).or_else(|| {
        MISSING.lock().expect("missing").push(import.name.to_owned());
        Some(shim_unresolved as *const () as usize)
    })
}
