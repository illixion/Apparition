//! What the libraries' file, time and random calls come to on Windows, which has no POSIX layer
//! to pass them to.
//!
//! Files go through Rust's standard library rather than the C runtime's `_open` family: paths
//! are UTF-8 here and the C runtime reads them in the system's code page, so a user folder with
//! a letter outside it wouldn't open. A descriptor is an index into a table of open files, and
//! `errno` is a number kept here, in Linux's values (`asm-generic/errno-base.h`), since the
//! libraries were built against those.
//!
//! There are no owners or permission bits to report: every file reads as an ordinary user's,
//! with the mode a new file would have on Linux under the usual `umask`.

use std::ffi::{CStr, c_char, c_int, c_void};
use std::fs::{File, Metadata, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use super::{FileStat, OpenHow};

const ENOENT: c_int = 2;
const EIO: c_int = 5;
const EBADF: c_int = 9;
const EACCES: c_int = 13;
const EEXIST: c_int = 17;
const ENOTDIR: c_int = 20;
const EISDIR: c_int = 21;
const EINVAL: c_int = 22;

const S_IFDIR: u32 = 0o040000;
const S_IFREG: u32 = 0o100000;

/// The user and group every file is said to belong to: an ordinary account, not root.
const OWNER: u32 = 1000;

static ERRNO: AtomicI32 = AtomicI32::new(0);
static UMASK: AtomicU32 = AtomicU32::new(0o022);

/// Open files; descriptor `n` is entry `n - FIRST`. The numbers below are the standard streams',
/// which the libraries aren't given.
static FILES: Mutex<Vec<Option<File>>> = Mutex::new(Vec::new());
const FIRST: c_int = 3;

fn fail<T>(code: c_int, value: T) -> T {
    ERRNO.store(code, Ordering::Relaxed);
    value
}

fn code(error: &std::io::Error) -> c_int {
    match error.kind() {
        ErrorKind::NotFound => ENOENT,
        ErrorKind::PermissionDenied => EACCES,
        ErrorKind::AlreadyExists => EEXIST,
        ErrorKind::NotADirectory => ENOTDIR,
        ErrorKind::IsADirectory => EISDIR,
        ErrorKind::InvalidInput => EINVAL,
        _ => EIO,
    }
}

/// The library's path as Windows takes it. The library joins names with `/`, which Windows
/// reads as a separator except after a `\\?\` prefix, so every one becomes `\`.
unsafe fn path(text: *const c_char) -> Option<PathBuf> {
    if text.is_null() {
        return None;
    }
    let text = unsafe { CStr::from_ptr(text) }.to_str().ok()?;
    Some(PathBuf::from(text.replace('/', "\\")))
}

/// Runs `action` on the open file behind `fd`.
fn with_file<T>(fd: c_int, missing: T, action: impl FnOnce(&File) -> T) -> T {
    let files = FILES.lock().expect("open files");
    match usize::try_from(fd - FIRST).ok().and_then(|index| files.get(index)?.as_ref()) {
        Some(file) => action(file),
        None => fail(EBADF, missing),
    }
}

fn since_1970(time: std::io::Result<SystemTime>) -> (i64, u64) {
    match time.ok().and_then(|time| time.duration_since(UNIX_EPOCH).ok()) {
        Some(elapsed) => (elapsed.as_secs() as i64, u64::from(elapsed.subsec_nanos())),
        None => (0, 0),
    }
}

fn file_stat(metadata: &Metadata) -> FileStat {
    let size = metadata.len() as i64;
    let mode = if metadata.is_dir() {
        S_IFDIR | 0o755
    } else if metadata.permissions().readonly() {
        S_IFREG | 0o444
    } else {
        S_IFREG | 0o644
    };
    let modified = since_1970(metadata.modified());
    FileStat {
        dev: 0,
        ino: 0,
        mode,
        nlink: 1,
        uid: OWNER,
        gid: OWNER,
        rdev: 0,
        size,
        blksize: 4096,
        blocks: (size + 511) / 512,
        // Windows keeps no time of the last status change; the last write stands in for it.
        times: [since_1970(metadata.accessed()), modified, modified],
    }
}

pub(super) unsafe fn fstat(fd: c_int) -> Option<FileStat> {
    with_file(fd, None, |file| match file.metadata() {
        Ok(metadata) => Some(file_stat(&metadata)),
        Err(error) => fail(code(&error), None),
    })
}

pub(super) unsafe fn lstat(text: *const c_char) -> Option<FileStat> {
    let Some(path) = (unsafe { path(text) }) else { return fail(EINVAL, None) };
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => Some(file_stat(&metadata)),
        Err(error) => fail(code(&error), None),
    }
}

pub(super) unsafe fn open(text: *const c_char, how: &OpenHow) -> c_int {
    let Some(path) = (unsafe { path(text) }) else { return fail(EINVAL, -1) };
    let mut options = OpenOptions::new();
    // Appending is a way of writing to Rust, and can't be combined with truncating.
    let append = how.append && how.write && !how.truncate;
    options.read(how.read).write(how.write && !append).append(append).truncate(how.truncate && how.write);
    if how.create && how.exclusive {
        options.create_new(true);
    } else if how.create {
        options.create(true);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) => return fail(code(&error), -1),
    };
    let mut files = FILES.lock().expect("open files");
    let index = match files.iter().position(Option::is_none) {
        Some(free) => free,
        None => {
            files.push(None);
            files.len() - 1
        }
    };
    files[index] = Some(file);
    FIRST + index as c_int
}

pub(super) unsafe fn close(fd: c_int) -> c_int {
    let mut files = FILES.lock().expect("open files");
    match usize::try_from(fd - FIRST).ok().and_then(|index| files.get_mut(index)?.take()) {
        Some(_) => 0,
        None => fail(EBADF, -1),
    }
}

pub(super) unsafe fn read(fd: c_int, to: *mut c_void, count: usize) -> isize {
    let to = unsafe { std::slice::from_raw_parts_mut(to.cast::<u8>(), count) };
    with_file(fd, -1, |mut file| match file.read(to) {
        Ok(got) => got as isize,
        Err(error) => fail(code(&error), -1),
    })
}

pub(super) unsafe fn write(fd: c_int, from: *const c_void, count: usize) -> isize {
    let from = unsafe { std::slice::from_raw_parts(from.cast::<u8>(), count) };
    with_file(fd, -1, |mut file| match file.write(from) {
        Ok(put) => put as isize,
        Err(error) => fail(code(&error), -1),
    })
}

pub(super) unsafe fn ftruncate(fd: c_int, length: i64) -> c_int {
    let Ok(length) = u64::try_from(length) else { return fail(EINVAL, -1) };
    with_file(fd, -1, |file| match file.set_len(length) {
        Ok(()) => 0,
        Err(error) => fail(code(&error), -1),
    })
}

/// There are no permission bits to set; it only has to be a file that exists.
pub(super) unsafe fn chmod(text: *const c_char, mode: u32) -> c_int {
    let _ = mode;
    let Some(path) = (unsafe { path(text) }) else { return fail(EINVAL, -1) };
    match std::fs::metadata(path) {
        Ok(_) => 0,
        Err(error) => fail(code(&error), -1),
    }
}

pub(super) unsafe fn mkdir(text: *const c_char, mode: u32) -> c_int {
    let _ = mode;
    let Some(path) = (unsafe { path(text) }) else { return fail(EINVAL, -1) };
    match std::fs::create_dir(path) {
        Ok(()) => 0,
        Err(error) => fail(code(&error), -1),
    }
}

/// Kept and given back as `umask` does, and otherwise unused.
pub(super) fn umask(mask: u32) -> u32 {
    UMASK.swap(mask & 0o777, Ordering::Relaxed)
}

pub(super) fn errno() -> *mut c_int {
    ERRNO.as_ptr()
}

/// Seconds and microseconds since 1970.
pub(super) fn now() -> (i64, i64) {
    let (seconds, nanoseconds) = since_1970(Ok(SystemTime::now()));
    (seconds, (nanoseconds / 1000) as i64)
}

pub(super) fn random() -> u32 {
    use ring::rand::SecureRandom;
    let mut bytes = [0u8; 4];
    ring::rand::SystemRandom::new().fill(&mut bytes).expect("the system's random numbers");
    u32::from_ne_bytes(bytes)
}

/// The C runtime's name for it.
pub(super) unsafe fn strcasecmp(a: *const c_char, b: *const c_char) -> c_int {
    unsafe extern "C" {
        fn _stricmp(a: *const c_char, b: *const c_char) -> c_int;
    }
    unsafe { _stricmp(a, b) }
}
