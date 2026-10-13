//! `apparition-anisette-helper`: prints this Mac's anisette from AuthKit, for macOS 27, where
//! nothing else gives it to programs that aren't Apple's (see core's `anisette::native`).
//!
//! The build signs it with AuthKit's private client entitlements (`anisette-helper.entitlements`),
//! so macOS only runs it with SIP and AMFI off. Apparition runs it when it's next to its own
//! program; started any other way it does the same and exits.

fn main() {
    #[cfg(target_os = "macos")]
    std::process::exit(apparition_core::anisette::native::helper_main());
    #[cfg(not(target_os = "macos"))]
    {
        eprintln!("only for macOS");
        std::process::exit(1);
    }
}
