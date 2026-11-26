import DebugTrace
import Foundation

private let log = DebugLogger(subsystem: Bundle.main.bundleIdentifier ?? "Apparition", category: "DiskImage")

/// The personalized Developer Disk Image, which debugserver needs mounted on iOS 17 and later.
/// A restart unmounts it, and until now only Xcode or another app (StikDebug) put it back, so
/// JIT failed after every restart of a device that no computer had prepared since. Before a JIT
/// session Apparition checks over the same tunnel and mounts it when nothing is: at most once per
/// restart, using the bundle handed over by the Mac.
enum DeveloperDiskImage {
    /// Installed by the Mac or its paired computer. Older downloads into Application
    /// Support are intentionally ignored.
    static var bundle: URL {
        AppFolders.documents.appendingPathComponent("Apparition-iOS-DDI-v1.apddi")
    }

    static let needsImage = "This device needs the Developer Disk Image. On your paired computer, run “apparition pairing hand-off” again. Install Xcode from the Mac App Store and open it to install device support if the Mac has no image. On Windows or Linux, pair with a Mac running Apparition and allow developer access."

    /// What `ensure` did: a line for the JIT session's events, and whether it mounted the image
    /// just now, which the tunnel it used can't show yet (see `IDevice.withDeveloperTunnel`).
    struct Outcome {
        let line: String
        var mountedNow = false
        var needsFiles = false
    }

    /// Mounts the Mac's image if needed. Other platforms use their own developer image.
    static func ensure(adapter: OpaquePointer?, handshake: OpaquePointer?) -> Outcome? {
        #if os(iOS)
        let reply = AccountModel.call(apparition_ensure_ddi(adapter, handshake, bundle.path))
        if reply["needsFiles"] as? Bool == true {
            return Outcome(line: "disk image: " + needsImage, needsFiles: true)
        }
        guard reply["ok"] as? Bool == true else {
            let error = reply["error"] as? String ?? "mounting failed"
            log.error("disk image mount failed: \(LogScrub.text(error), privacy: .public)")
            return Outcome(line: "disk image: \(error)")
        }
        switch reply["mounted"] as? String {
        case "already": return Outcome(line: "disk image: already mounted")
        case "now":
            log.info("disk image mounted")
            return Outcome(line: "disk image: mounted", mountedNow: true)
        default: return Outcome(line: "disk image: not mounted")
        }
        #else
        // Apple TV and Vision Pro use their own platform's image, which this doesn't fetch.
        return nil
        #endif
    }

    /// Adds the mount's line to a JIT reply's events, and to its error when JIT failed, since a
    /// missing image is then the likely reason.
    static func annotate(_ reply: [String: Any], with line: String?) -> [String: Any] {
        guard let line else { return reply }
        var reply = reply
        reply["events"] = [line] + (reply["events"] as? [String] ?? [])
        if reply["ok"] as? Bool != true, line != "disk image: already mounted", line != "disk image: mounted" {
            reply["error"] = "\(reply["error"] as? String ?? "Developer service unavailable") (\(line))"
        }
        return reply
    }
}
