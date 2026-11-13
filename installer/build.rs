// On Windows with MSVC, link the Visual C++ runtime into the programs and keep only the
// Universal CRT, which is part of Windows 10 and 11, as a DLL. Rust's default needs
// VCRUNTIME140.dll, which comes with the Visual C++ Redistributable, not with Windows: a clean or
// stripped-down PC can lack it, and then the command line, and the Windows service that runs it,
// don't start at all. The desktop app gets the same from tauri-build; this is its recipe
// (tauri-build's static_vcruntime.rs, from github.com/ChrisDenton/static_vcruntime), for debug
// builds too. mingw builds (x86_64-pc-windows-gnu) link Windows' own CRT already.
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        return;
    }
    override_msvcrt_lib();
    for library in ["libvcruntimed.lib", "vcruntime.lib", "vcruntimed.lib", "libcmtd.lib", "msvcrt.lib", "msvcrtd.lib", "libucrt.lib", "libucrtd.lib"] {
        println!("cargo:rustc-link-arg=/NODEFAULTLIB:{library}");
    }
    for library in ["libcmt.lib", "libvcruntime.lib", "ucrt.lib"] {
        println!("cargo:rustc-link-arg=/DEFAULTLIB:{library}");
    }
}

/// Rust names msvcrt.lib itself on x86 and x86_64, which /NODEFAULTLIB doesn't undo: an empty
/// msvcrt.lib found first takes its place (on ARM64 the flags above are enough).
fn override_msvcrt_lib() {
    let machine: &[u8] = match std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("x86_64") => &[0x64, 0x86],
        Ok("x86") => &[0x4C, 0x01],
        _ => return,
    };
    let bytes: &[u8] = &[
        1, 0, 94, 3, 96, 98, 60, 0, 0, 0, 1, 0, 0, 0, 0, 0, 132, 1, 46, 100, 114, 101, 99, 116, 118, 101, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 60, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 10, 16, 0, 46, 100, 114, 101, 99, 116, 118, 101, 0, 0, 0, 0, 1, 0,
        0, 0, 3, 0, 4, 0, 0, 0,
    ];
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR");
    let mut library = machine.to_vec();
    library.extend_from_slice(bytes);
    std::fs::write(std::path::Path::new(&out_dir).join("msvcrt.lib"), library).expect("write the empty msvcrt.lib");
    println!("cargo:rustc-link-search=native={out_dir}");
}
