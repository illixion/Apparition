//! Apple's Authentication Device Identity (ADI) library, run in this process.
//!
//! `libstoreservicescore.so` (with `libCoreADI.so` behind it) is what Apple Music for Android
//! uses to make the machine-bound values that go with an Apple ID sign-in: provisioning the
//! device once, then a fresh one-time password and machine ID for every request.
//!
//! The libraries' entry points have obfuscated names. Their names and signatures are taken from
//! the SideStore project's `omnisette` crate (MPL-2.0), which documents them; everything else
//! here is written for this crate. Every call goes through one dedicated thread, so the
//! libraries never run concurrently (their locks are no-ops, see `shim.rs`).

use std::ffi::{CString, c_char};
use std::path::Path;
use std::sync::mpsc;

use rootcause::prelude::*;

use super::elf::Library;
use super::shim;

/// The directory-services ID ADI uses for "no Apple ID": the device, not a user.
const DS_ID: i64 = -2;

type LoadWithPath = unsafe extern "C" fn(path: *const c_char) -> i32;
type SetIdentifier = unsafe extern "C" fn(id: *const u8, length: u32) -> i32;
type SetPath = unsafe extern "C" fn(path: *const c_char) -> i32;
type Destroy = unsafe extern "C" fn(session: u32) -> i32;
type EndProvisioning = unsafe extern "C" fn(session: u32, ptm: *const u8, ptm_length: u32, tk: *const u8, tk_length: u32) -> i32;
type StartProvisioning = unsafe extern "C" fn(ds_id: i64, spim: *const u8, spim_length: u32, cpim: *mut *const u8, cpim_length: *mut u32, session: *mut u32) -> i32;
type LoginCode = unsafe extern "C" fn(ds_id: i64) -> i32;
type Dispose = unsafe extern "C" fn(pointer: *const u8) -> i32;
type RequestOtp = unsafe extern "C" fn(ds_id: i64, mid: *mut *const u8, mid_length: *mut u32, otp: *mut *const u8, otp_length: *mut u32) -> i32;

/// The library's entry points. Only used on the ADI thread.
pub(crate) struct Exports {
    set_identifier: SetIdentifier,
    set_path: SetPath,
    destroy: Destroy,
    end_provisioning: EndProvisioning,
    start_provisioning: StartProvisioning,
    login_code: LoginCode,
    dispose: Dispose,
    request_otp: RequestOtp,
}

fn check(call: &str, code: i32) -> Result<(), Report> {
    if code == 0 { Ok(()) } else { bail!("{call} failed with ADI error {code}") }
}

/// Copies out a buffer the library allocated, then gives it back.
unsafe fn take(dispose: Dispose, pointer: *const u8, length: u32) -> Vec<u8> {
    if pointer.is_null() {
        return Vec::new();
    }
    let copy = unsafe { std::slice::from_raw_parts(pointer, length as usize) }.to_vec();
    unsafe { dispose(pointer) };
    copy
}

/// What `request_otp` returns: the values that become `X-Apple-I-MD-M` and `X-Apple-I-MD`.
pub(crate) struct Otp {
    pub machine_id: Vec<u8>,
    pub one_time_password: Vec<u8>,
}

impl Exports {
    /// Points the library at this device's identity: its identifier and the folder holding its
    /// provisioning state (`adi.pb`).
    pub(crate) fn select(&self, identifier: &str, folder: &Path) -> Result<(), Report> {
        let path = CString::new(folder.to_str().ok_or_else(|| report!("the provisioning folder isn't UTF-8"))?)?;
        unsafe {
            check("setting the identifier", (self.set_identifier)(identifier.as_ptr(), identifier.len() as u32))?;
            check("setting the provisioning folder", (self.set_path)(path.as_ptr()))
        }
    }

    /// 0 when the selected identity is provisioned, else ADI's reason.
    pub(crate) fn login_code(&self) -> i32 {
        unsafe { (self.login_code)(DS_ID) }
    }

    /// First half of provisioning: Apple's `spim` in, the `cpim` to send back and a session out.
    pub(crate) fn start_provisioning(&self, spim: &[u8]) -> Result<(Vec<u8>, u32), Report> {
        // Wider than the library's 32-bit outputs, so either width reads back correctly.
        let (mut cpim, mut length, mut session) = (std::ptr::null(), 0u64, 0u32);
        unsafe {
            check(
                "starting provisioning",
                (self.start_provisioning)(DS_ID, spim.as_ptr(), spim.len() as u32, &mut cpim, (&mut length as *mut u64).cast(), &mut session),
            )?;
            Ok((take(self.dispose, cpim, length as u32), session))
        }
    }

    /// Second half: Apple's answer to the `cpim`. The folder then holds `adi.pb`.
    pub(crate) fn end_provisioning(&self, session: u32, ptm: &[u8], tk: &[u8]) -> Result<(), Report> {
        unsafe { check("finishing provisioning", (self.end_provisioning)(session, ptm.as_ptr(), ptm.len() as u32, tk.as_ptr(), tk.len() as u32)) }
    }

    /// Gives up on a provisioning session that won't be finished.
    pub(crate) fn abandon(&self, session: u32) {
        unsafe { (self.destroy)(session) };
    }

    pub(crate) fn request_otp(&self) -> Result<Otp, Report> {
        let (mut mid, mut mid_length, mut otp, mut otp_length) = (std::ptr::null(), 0u64, std::ptr::null(), 0u64);
        unsafe {
            check(
                "requesting a one-time password",
                (self.request_otp)(DS_ID, &mut mid, (&mut mid_length as *mut u64).cast(), &mut otp, (&mut otp_length as *mut u64).cast()),
            )?;
            Ok(Otp { machine_id: take(self.dispose, mid, mid_length as u32), one_time_password: take(self.dispose, otp, otp_length as u32) })
        }
    }
}

type Job = Box<dyn FnOnce(&Exports) + Send>;

/// The libraries, loaded on a thread of their own.
pub(crate) struct Adi {
    jobs: mpsc::Sender<Job>,
}

fn find<T: Copy>(library: &Library, name: &str) -> Result<T, Report> {
    assert_eq!(size_of::<T>(), size_of::<usize>());
    let address = library.symbol(name).ok_or_else(|| report!("libstoreservicescore has no export {name}"))?;
    Ok(unsafe { std::mem::transmute_copy(&address) })
}

fn setup(services: &[u8], core: &[u8], folder: &Path) -> Result<Exports, Report> {
    // Loaded images stay for the life of the process, as `dlopen`ed libraries would.
    let services: &'static Library = Box::leak(Box::new(Library::load(services, &shim::resolve).context("loading libstoreservicescore")?));
    let core: &'static Library = Box::leak(Box::new(Library::load(core, &shim::resolve).context("loading libCoreADI")?));
    shim::register("libstoreservicescore.so", services);
    shim::register("libCoreADI.so", core);

    // This one makes the library `dlopen` libCoreADI.so from the folder, which `shim_dlopen`
    // answers with the image above.
    let load: LoadWithPath = find(services, "kq56gsgHG6")?;
    let path = CString::new(folder.to_str().ok_or_else(|| report!("the folder isn't UTF-8"))?)?;
    check("loading libCoreADI", unsafe { load(path.as_ptr()) })?;

    Ok(Exports {
        set_identifier: find(services, "Sph98paBcz")?,
        set_path: find(services, "nf92ngaK92")?,
        destroy: find(services, "fy34trz2st")?,
        end_provisioning: find(services, "uv5t6nhkui")?,
        start_provisioning: find(services, "rsegvyrt87")?,
        login_code: find(services, "aslgmuibau")?,
        dispose: find(services, "jk24uiwqrg")?,
        request_otp: find(services, "qi864985u0")?,
    })
}

impl Adi {
    /// Loads the two libraries (the files' contents) on a new thread.
    pub(crate) fn start(services: Vec<u8>, core: Vec<u8>, folder: &Path) -> Result<Adi, Report> {
        let (jobs, queue) = mpsc::channel::<Job>();
        let (ready, started) = mpsc::channel();
        let folder = folder.to_owned();
        std::thread::Builder::new().name("anisette-adi".into()).stack_size(8 << 20).spawn(move || match setup(&services, &core, &folder) {
            Ok(exports) => {
                let _ = ready.send(Ok(()));
                for job in queue {
                    job(&exports);
                }
            }
            Err(error) => {
                let _ = ready.send(Err(error));
            }
        })?;
        started.recv().map_err(|_| report!("the ADI thread stopped while loading"))??;
        Ok(Adi { jobs })
    }

    /// Runs `job` on the ADI thread and waits for it.
    pub(crate) fn run<R: Send + 'static>(&self, job: impl FnOnce(&Exports) -> R + Send + 'static) -> Result<R, Report> {
        let (reply, answer) = mpsc::channel();
        self.jobs.send(Box::new(move |exports| drop(reply.send(job(exports))))).map_err(|_| report!("the ADI thread has stopped"))?;
        answer.recv().map_err(|_| report!("the ADI thread stopped during a call"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs a folder with the two arm64/x86-64 libraries: `APPARITION_ADI_LIBS=<dir>`.
    #[test]
    fn loads_the_libraries_and_answers() {
        let Some(libs) = std::env::var_os("APPARITION_ADI_LIBS") else { return };
        let libs = Path::new(&libs);
        let folder = std::env::temp_dir().join(format!("apparition-adi-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        let adi = Adi::start(std::fs::read(libs.join("libstoreservicescore.so")).unwrap(), std::fs::read(libs.join("libCoreADI.so")).unwrap(), &folder).unwrap();
        let folder2 = folder.clone();
        let provisioned = adi
            .run(move |adi| {
                adi.select("0123456789ABCDEF", &folder2).unwrap();
                adi.login_code() == 0
            })
            .unwrap();
        assert!(!provisioned, "a new folder has nothing provisioned");
        std::fs::remove_dir_all(&folder).ok();
    }
}
