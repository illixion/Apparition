//! What the libraries' file, time and random calls come to on macOS and Linux: this system's own
//! C library, which sets its own `errno`.

use std::ffi::{c_char, c_int, c_void};

use super::{FileStat, OpenHow};

fn file_stat(stat: &libc::stat) -> FileStat {
    FileStat {
        dev: stat.st_dev as u64,
        ino: stat.st_ino,
        mode: u32::from(stat.st_mode),
        nlink: stat.st_nlink as u64,
        uid: stat.st_uid,
        gid: stat.st_gid,
        rdev: stat.st_rdev as u64,
        size: stat.st_size,
        blksize: stat.st_blksize as i64,
        blocks: stat.st_blocks,
        times: [
            (stat.st_atime, stat.st_atime_nsec as u64),
            (stat.st_mtime, stat.st_mtime_nsec as u64),
            (stat.st_ctime, stat.st_ctime_nsec as u64),
        ],
    }
}

pub(super) unsafe fn fstat(fd: c_int) -> Option<FileStat> {
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    (unsafe { libc::fstat(fd, &mut stat) } == 0).then(|| file_stat(&stat))
}

pub(super) unsafe fn lstat(path: *const c_char) -> Option<FileStat> {
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    (unsafe { libc::lstat(path, &mut stat) } == 0).then(|| file_stat(&stat))
}

pub(super) unsafe fn open(path: *const c_char, how: &OpenHow) -> c_int {
    let mut flags = match (how.read, how.write) {
        (true, true) => libc::O_RDWR,
        (false, true) => libc::O_WRONLY,
        _ => libc::O_RDONLY,
    };
    for (wanted, flag) in [(how.create, libc::O_CREAT), (how.exclusive, libc::O_EXCL), (how.truncate, libc::O_TRUNC), (how.append, libc::O_APPEND)] {
        if wanted {
            flags |= flag;
        }
    }
    // Nothing here starts another program, so no descriptor is ever inherited.
    unsafe { libc::open(path, flags | libc::O_CLOEXEC, how.mode as libc::c_uint) }
}

pub(super) unsafe fn close(fd: c_int) -> c_int {
    unsafe { libc::close(fd) }
}

pub(super) unsafe fn read(fd: c_int, to: *mut c_void, count: usize) -> isize {
    unsafe { libc::read(fd, to, count) }
}

pub(super) unsafe fn write(fd: c_int, from: *const c_void, count: usize) -> isize {
    unsafe { libc::write(fd, from, count) }
}

pub(super) unsafe fn ftruncate(fd: c_int, length: i64) -> c_int {
    unsafe { libc::ftruncate(fd, length) }
}

pub(super) unsafe fn chmod(path: *const c_char, mode: u32) -> c_int {
    unsafe { libc::chmod(path, mode as libc::mode_t) }
}

pub(super) unsafe fn mkdir(path: *const c_char, mode: u32) -> c_int {
    unsafe { libc::mkdir(path, mode as libc::mode_t) }
}

pub(super) fn umask(mask: u32) -> u32 {
    u32::from(unsafe { libc::umask(mask as libc::mode_t) })
}

pub(super) fn errno() -> *mut c_int {
    #[cfg(target_vendor = "apple")]
    unsafe {
        libc::__error()
    }
    #[cfg(not(target_vendor = "apple"))]
    unsafe {
        libc::__errno_location()
    }
}

/// Seconds and microseconds since 1970.
pub(super) fn now() -> (i64, i64) {
    let mut now = libc::timeval { tv_sec: 0, tv_usec: 0 };
    unsafe { libc::gettimeofday(&mut now, std::ptr::null_mut()) };
    (now.tv_sec as i64, i64::from(now.tv_usec))
}

pub(super) fn random() -> u32 {
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

pub(super) unsafe fn strcasecmp(a: *const c_char, b: *const c_char) -> c_int {
    unsafe { libc::strcasecmp(a, b) }
}
