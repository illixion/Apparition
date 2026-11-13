//! The "libc" a loaded Android library sees: what its imports resolve to.
//!
//! Most of Bionic's functions mean the same here and are passed on to this system's own (`unix`,
//! or `windows` where there is no POSIX layer to pass them to). The ones that don't are written
//! out below, from the Linux kernel's UAPI headers (`asm-generic/stat.h`,
//! `arch/x86/include/uapi/asm/stat.h`, `asm-generic/fcntl.h`) rather than from any other loader:
//!
//! - `struct stat` and `struct timeval` have other layouts on macOS and Windows;
//! - `open` takes its mode as a variadic argument, which Apple's arm64 calling convention passes
//!   on the stack and Linux's passes in a register, so the mode is a plain parameter here, and
//!   the flag bits are the Linux numbers;
//! - `mode_t` is 16 bits on macOS and 32 on Linux;
//! - `errno` lives behind `__errno` rather than `__error`;
//! - `long` is 64 bits to the libraries and 32 on Windows.
//!
//! The libraries call these, and are called, with the calling convention they were built for:
//! System V's on x86-64, which isn't Windows' own there (`guest!`, `guest_fn_type!`). On ARM64 the
//! two systems agree for the functions used here.
//!
//! Locks are no-ops, because everything that calls into the library runs on one thread (see
//! `adi.rs`). Anything else the libraries import resolves to a stub that aborts when called, so
//! a code path nobody expected fails loudly instead of running with a wrong answer.

use std::collections::HashMap;
use std::ffi::{CStr, c_char, c_int, c_void};
use std::sync::{Mutex, OnceLock};

use super::elf::{Import, Library};

#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix as host;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as host;

/// Functions the libraries call, with the calling convention the libraries use.
macro_rules! guest {
    ($($(#[$meta:meta])* unsafe fn $name:ident($($arg:ident: $ty:ty),* $(,)?) $(-> $ret:ty)? $body:block)*) => {$(
        #[cfg(target_arch = "x86_64")]
        $(#[$meta])*
        unsafe extern "sysv64" fn $name($($arg: $ty),*) $(-> $ret)? $body
        #[cfg(not(target_arch = "x86_64"))]
        $(#[$meta])*
        unsafe extern "C" fn $name($($arg: $ty),*) $(-> $ret)? $body
    )*};
}

/// The type of a pointer to a function in a library, or to one it is given to call back.
macro_rules! guest_fn_type {
    ($($(#[$meta:meta])* $vis:vis type $name:ident = fn($($arg:ident: $ty:ty),* $(,)?) $(-> $ret:ty)?;)*) => {$(
        #[cfg(target_arch = "x86_64")]
        $(#[$meta])*
        $vis type $name = unsafe extern "sysv64" fn($($arg: $ty),*) $(-> $ret)?;
        #[cfg(not(target_arch = "x86_64"))]
        $(#[$meta])*
        $vis type $name = unsafe extern "C" fn($($arg: $ty),*) $(-> $ret)?;
    )*};
}
pub(super) use guest_fn_type;

// Linux values (asm-generic/fcntl.h; the same on arm64 and x86-64 for these).
const LINUX_O_ACCMODE: c_int = 0o3;
const LINUX_O_CREAT: c_int = 0o100;
const LINUX_O_EXCL: c_int = 0o200;
const LINUX_O_TRUNC: c_int = 0o1000;
const LINUX_O_APPEND: c_int = 0o2000;

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

/// What this system says of a file, for `convert` to lay out as Linux does. The mode is in
/// Linux's bits, which are POSIX's.
struct FileStat {
    dev: u64,
    ino: u64,
    mode: u32,
    nlink: u64,
    uid: u32,
    gid: u32,
    rdev: u64,
    size: i64,
    blksize: i64,
    blocks: i64,
    /// Seconds and nanoseconds since 1970: access, modification, status change.
    times: [(i64, u64); 3],
}

/// How the library wants a file opened: `open`'s Linux flags, taken apart.
struct OpenHow {
    read: bool,
    write: bool,
    create: bool,
    exclusive: bool,
    truncate: bool,
    append: bool,
    /// The permission bits of a file this creates, where the system has them.
    #[cfg_attr(windows, allow(dead_code))]
    mode: u32,
}

fn convert(stat: &FileStat, out: &mut LinuxStat) {
    // Zero first, so the padding is zero too.
    unsafe { std::ptr::write_bytes(out, 0, 1) };
    out.dev = stat.dev;
    out.ino = stat.ino;
    out.mode = stat.mode;
    out.nlink = stat.nlink as _;
    out.uid = stat.uid;
    out.gid = stat.gid;
    out.rdev = stat.rdev;
    out.size = stat.size;
    out.blksize = stat.blksize as _;
    out.blocks = stat.blocks;
    [(out.atime, out.atime_nsec), (out.mtime, out.mtime_nsec), (out.ctime, out.ctime_nsec)] = stat.times;
}

guest! {
    unsafe fn shim_fstat(fd: c_int, out: *mut LinuxStat) -> c_int {
        match unsafe { host::fstat(fd) } {
            Some(stat) => {
                convert(&stat, unsafe { &mut *out });
                0
            }
            None => -1,
        }
    }

    unsafe fn shim_lstat(path: *const c_char, out: *mut LinuxStat) -> c_int {
        match unsafe { host::lstat(path) } {
            Some(stat) => {
                convert(&stat, unsafe { &mut *out });
                0
            }
            None => -1,
        }
    }

    unsafe fn shim_open(path: *const c_char, flags: c_int, mode: u32) -> c_int {
        let access = flags & LINUX_O_ACCMODE;
        let how = OpenHow {
            read: access != 1,
            write: access != 0,
            create: flags & LINUX_O_CREAT != 0,
            exclusive: flags & LINUX_O_EXCL != 0,
            truncate: flags & LINUX_O_TRUNC != 0,
            append: flags & LINUX_O_APPEND != 0,
            mode,
        };
        unsafe { host::open(path, &how) }
    }

    unsafe fn shim_close(fd: c_int) -> c_int {
        unsafe { host::close(fd) }
    }

    unsafe fn shim_read(fd: c_int, to: *mut c_void, count: usize) -> isize {
        unsafe { host::read(fd, to, count) }
    }

    unsafe fn shim_write(fd: c_int, from: *const c_void, count: usize) -> isize {
        unsafe { host::write(fd, from, count) }
    }

    unsafe fn shim_ftruncate(fd: c_int, length: i64) -> c_int {
        unsafe { host::ftruncate(fd, length) }
    }

    unsafe fn shim_gettimeofday(out: *mut LinuxTimeval, timezone: *mut c_void) -> c_int {
        let _ = timezone;
        let (sec, usec) = host::now();
        if !out.is_null() {
            unsafe { out.write(LinuxTimeval { sec, usec }) };
        }
        0
    }

    unsafe fn shim_chmod(path: *const c_char, mode: u32) -> c_int {
        unsafe { host::chmod(path, mode) }
    }

    unsafe fn shim_mkdir(path: *const c_char, mode: u32) -> c_int {
        unsafe { host::mkdir(path, mode) }
    }

    unsafe fn shim_umask(mask: u32) -> u32 {
        host::umask(mask)
    }

    unsafe fn shim_errno() -> *mut c_int {
        host::errno()
    }

    unsafe fn shim_arc4random() -> u32 {
        host::random()
    }

    unsafe fn shim_memcpy_chk(to: *mut c_void, from: *const c_void, length: usize, capacity: usize) -> *mut c_void {
        let _ = capacity;
        unsafe { libc::memcpy(to, from, length) }
    }

    unsafe fn shim_memmove_chk(to: *mut c_void, from: *const c_void, length: usize, capacity: usize) -> *mut c_void {
        let _ = capacity;
        unsafe { libc::memmove(to, from, length) }
    }

    /// No Android system properties here: every one reads as unset.
    unsafe fn shim_property_get(name: *const c_char, value: *mut c_char) -> c_int {
        let _ = name;
        unsafe { *value = 0 };
        0
    }

    unsafe fn shim_ok() -> c_int {
        0
    }

    unsafe fn shim_ok_finalize(dso: *mut c_void) {
        let _ = dso;
    }

    /// Bionic's `pthread_once_t` is one `int`, 0 until the routine has run.
    unsafe fn shim_pthread_once(once: *mut c_int, routine: Routine) -> c_int {
        unsafe {
            if *once == 0 {
                *once = 1;
                routine();
            }
        }
        0
    }

    unsafe fn shim_abort() -> ! {
        std::process::abort()
    }

    unsafe fn shim_unresolved() -> ! {
        let names = MISSING.lock().map(|names| names.join(", ")).unwrap_or_default();
        eprintln!("anisette: a library called something that isn't provided (one of: {names})");
        std::process::abort()
    }
}

guest_fn_type! {
    type Routine = fn();
}

static MISSING: Mutex<Vec<String>> = Mutex::new(Vec::new());

// MARK: The C library

// These take and return the same things as the Linux versions, on every system this runs on;
// only how they are called differs (see `guest!`). `strtol` gives a Linux `long`, 64 bits.
guest! {
    unsafe fn shim_malloc(size: usize) -> *mut c_void {
        unsafe { libc::malloc(size) }
    }

    unsafe fn shim_free(pointer: *mut c_void) {
        unsafe { libc::free(pointer) }
    }

    unsafe fn shim_calloc(count: usize, size: usize) -> *mut c_void {
        unsafe { libc::calloc(count, size) }
    }

    unsafe fn shim_realloc(pointer: *mut c_void, size: usize) -> *mut c_void {
        unsafe { libc::realloc(pointer, size) }
    }

    unsafe fn shim_memchr(bytes: *const c_void, byte: c_int, length: usize) -> *mut c_void {
        unsafe { libc::memchr(bytes, byte, length) }
    }

    unsafe fn shim_memcmp(a: *const c_void, b: *const c_void, length: usize) -> c_int {
        unsafe { libc::memcmp(a, b, length) }
    }

    unsafe fn shim_memcpy(to: *mut c_void, from: *const c_void, length: usize) -> *mut c_void {
        unsafe { libc::memcpy(to, from, length) }
    }

    unsafe fn shim_memmove(to: *mut c_void, from: *const c_void, length: usize) -> *mut c_void {
        unsafe { libc::memmove(to, from, length) }
    }

    unsafe fn shim_memset(to: *mut c_void, byte: c_int, length: usize) -> *mut c_void {
        unsafe { libc::memset(to, byte, length) }
    }

    unsafe fn shim_strlen(text: *const c_char) -> usize {
        unsafe { libc::strlen(text) }
    }

    unsafe fn shim_strcmp(a: *const c_char, b: *const c_char) -> c_int {
        unsafe { libc::strcmp(a, b) }
    }

    unsafe fn shim_strncmp(a: *const c_char, b: *const c_char, length: usize) -> c_int {
        unsafe { libc::strncmp(a, b, length) }
    }

    unsafe fn shim_strncpy(to: *mut c_char, from: *const c_char, length: usize) -> *mut c_char {
        unsafe { libc::strncpy(to, from, length) }
    }

    unsafe fn shim_strcasecmp(a: *const c_char, b: *const c_char) -> c_int {
        unsafe { host::strcasecmp(a, b) }
    }

    unsafe fn shim_strtol(text: *const c_char, end: *mut *mut c_char, base: c_int) -> i64 {
        unsafe { libc::strtoll(text, end, base) }
    }

    unsafe fn shim_strtoll(text: *const c_char, end: *mut *mut c_char, base: c_int) -> i64 {
        unsafe { libc::strtoll(text, end, base) }
    }

    unsafe fn shim_strtoull(text: *const c_char, end: *mut *mut c_char, base: c_int) -> u64 {
        unsafe { libc::strtoull(text, end, base) }
    }

    unsafe fn shim_strtod(text: *const c_char, end: *mut *mut c_char) -> f64 {
        unsafe { libc::strtod(text, end) }
    }

    unsafe fn shim_atof(text: *const c_char) -> f64 {
        unsafe { libc::atof(text) }
    }
}

// MARK: dlopen and friends

static LOADED: Mutex<Vec<(String, &'static Library)>> = Mutex::new(Vec::new());

/// Makes `library` what `dlopen` returns for a path ending in `name`.
pub(super) fn register(name: &str, library: &'static Library) {
    LOADED.lock().expect("loaded libraries").push((name.to_owned(), library));
}

guest! {
    unsafe fn shim_dlopen(path: *const c_char, flags: c_int) -> *mut c_void {
        let _ = flags;
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

    unsafe fn shim_dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void {
        let name = unsafe { CStr::from_ptr(name) }.to_string_lossy();
        let loaded = LOADED.lock().expect("loaded libraries");
        let found = match loaded.iter().find(|(_, library)| std::ptr::from_ref::<Library>(library).cast::<c_void>() == handle.cast_const()) {
            Some((_, library)) => library.symbol(&name),
            // The program itself (a null or placeholder handle): the libraries, then the shims.
            None => loaded.iter().find_map(|(_, library)| library.symbol(&name)).or_else(|| function(&name)),
        };
        found.map_or(std::ptr::null_mut(), |address| address as *mut c_void)
    }
}

// MARK: Resolution

/// The address of the replacement for the function `name`, if there is one.
fn function(name: &str) -> Option<usize> {
    match name {
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
        "malloc" => Some(shim_malloc as *const () as usize),
        "free" => Some(shim_free as *const () as usize),
        "calloc" => Some(shim_calloc as *const () as usize),
        "realloc" => Some(shim_realloc as *const () as usize),
        "memchr" => Some(shim_memchr as *const () as usize),
        "memcmp" => Some(shim_memcmp as *const () as usize),
        "memcpy" => Some(shim_memcpy as *const () as usize),
        "memmove" => Some(shim_memmove as *const () as usize),
        "memset" => Some(shim_memset as *const () as usize),
        "strlen" => Some(shim_strlen as *const () as usize),
        "strcmp" => Some(shim_strcmp as *const () as usize),
        "strncmp" => Some(shim_strncmp as *const () as usize),
        "strncpy" => Some(shim_strncpy as *const () as usize),
        "strcasecmp" => Some(shim_strcasecmp as *const () as usize),
        "strtol" => Some(shim_strtol as *const () as usize),
        "strtoll" => Some(shim_strtoll as *const () as usize),
        "strtoull" => Some(shim_strtoull as *const () as usize),
        "strtod" => Some(shim_strtod as *const () as usize),
        "atof" => Some(shim_atof as *const () as usize),
        "close" => Some(shim_close as *const () as usize),
        "read" => Some(shim_read as *const () as usize),
        "write" => Some(shim_write as *const () as usize),
        "ftruncate" => Some(shim_ftruncate as *const () as usize),
        _ => None,
    }
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

#[cfg(test)]
mod tests {
    use std::ffi::CString;

    use super::*;

    const O_WRONLY: c_int = 1;
    const EEXIST: c_int = 17;

    /// The file calls as a library makes them, with Linux's flags and layouts, in a folder whose
    /// name isn't ASCII: what the library does with its `adi.pb`.
    #[test]
    fn files_are_made_written_and_read_back_through_the_shims() {
        let folder = std::env::temp_dir().join(format!("apparition-shim-\u{e5}\u{43a}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        // The library joins names with a slash, whatever the system's separator.
        let name = |last: &str| CString::new(format!("{}/{last}", folder.to_str().unwrap())).unwrap();
        let folder_name = CString::new(folder.to_str().unwrap()).unwrap();
        let file = name("adi.pb");
        unsafe {
            assert_eq!(shim_mkdir(folder_name.as_ptr(), 0o700), 0);
            assert_eq!(shim_mkdir(folder_name.as_ptr(), 0o700), -1);
            assert_eq!(*shim_errno(), EEXIST);

            let mut stat: LinuxStat = std::mem::zeroed();
            assert_eq!(shim_lstat(folder_name.as_ptr(), &mut stat), 0);
            assert_eq!(stat.mode & 0o170000, 0o040000, "a folder");
            assert_eq!(shim_lstat(file.as_ptr(), &mut stat), -1, "not there yet");
            assert_eq!(shim_open(file.as_ptr(), 0, 0), -1);

            let fd = shim_open(file.as_ptr(), O_WRONLY | LINUX_O_CREAT | LINUX_O_TRUNC, 0o600);
            assert!(fd > 2, "{fd}");
            assert_eq!(shim_write(fd, b"provisioned".as_ptr().cast(), 11), 11);
            assert_eq!(shim_ftruncate(fd, 4), 0);
            assert_eq!(shim_fstat(fd, &mut stat), 0);
            assert_eq!((stat.size, stat.mode & 0o170000), (4, 0o100000));
            assert!(stat.mtime > 1_700_000_000, "{}", stat.mtime);
            assert_eq!(shim_close(fd), 0);
            assert_eq!(shim_close(fd), -1, "closed already");
            assert_eq!(shim_open(file.as_ptr(), O_WRONLY | LINUX_O_CREAT | LINUX_O_EXCL, 0o600), -1);
            assert_eq!(*shim_errno(), EEXIST);
            assert_eq!(shim_chmod(file.as_ptr(), 0o644), 0);

            let fd = shim_open(file.as_ptr(), 0, 0);
            let mut bytes = [0u8; 16];
            assert_eq!(shim_read(fd, bytes.as_mut_ptr().cast(), bytes.len()), 4);
            assert_eq!(&bytes[..4], b"prov");
            assert_eq!(shim_read(fd, bytes.as_mut_ptr().cast(), bytes.len()), 0, "the end");
            assert_eq!(shim_close(fd), 0);
        }
        assert_eq!(std::fs::read(folder.join("adi.pb")).unwrap(), b"prov");
        std::fs::remove_dir_all(&folder).unwrap();
    }

    /// A Linux `long` is 64 bits, which this system's own `strtol` may not give; and the time is
    /// today's, in Linux's `struct timeval`.
    #[test]
    fn long_is_sixty_four_bits_and_the_clock_is_set() {
        let text = CString::new("123456789012").unwrap();
        assert_eq!(unsafe { shim_strtol(text.as_ptr(), std::ptr::null_mut(), 10) }, 123_456_789_012);
        let mut now = LinuxTimeval { sec: 0, usec: 0 };
        unsafe { shim_gettimeofday(&mut now, std::ptr::null_mut()) };
        assert!(now.sec > 1_700_000_000 && (0..1_000_000).contains(&now.usec));
    }
}
