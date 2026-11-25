import DebugTrace
import Foundation

private let log = DebugLogger(subsystem: Bundle.main.bundleIdentifier ?? "Apparition", category: "DiskImage")

/// The personalized Developer Disk Image, which debugserver needs mounted on iOS 17 and later.
/// A restart unmounts it, and until now only Xcode or another app (StikDebug) put it back, so
/// JIT failed after every restart of a device that no computer had prepared since. Before a JIT
/// session Apparition checks over the same tunnel and mounts it when nothing is: at most once per
/// restart, with the files downloaded once and kept.
enum DeveloperDiskImage {
    /// Xcode's personalized iOS image, the copy StikDebug and pymobiledevice3 fetch. The device
    /// mounts it only after Apple's TSS server has signed it for this device, so a changed file
    /// is refused rather than mounted.
    static let source = URL(string: "https://raw.githubusercontent.com/doronz88/DeveloperDiskImage/main/PersonalizedImages/Xcode_iOS_DDI_Personalized/")!
    static let files = ["Image.dmg", "Image.dmg.trustcache", "BuildManifest.plist"]

    static var directory: URL {
        FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("DeveloperDiskImage", isDirectory: true)
    }

    static var hasFiles: Bool {
        files.allSatisfy { FileManager.default.fileExists(atPath: directory.appendingPathComponent($0).path) }
    }

    /// Downloads whichever files are missing. Blocks, as the tunnel's callers already do off the
    /// main thread.
    static func download() throws {
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        for name in files {
            let destination = directory.appendingPathComponent(name)
            if FileManager.default.fileExists(atPath: destination.path) { continue }
            let url = source.appendingPathComponent(name)
            let done = DispatchSemaphore(value: 0)
            var failure: Error?
            // SafeSession: the image is code the device runs, so it gets the same refusal of
            // interception CAs as the app's other downloads.
            let task = SafeSession.shared.downloadTask(with: url) { location, response, error in
                defer { done.signal() }
                if let error { failure = NationalCAs.explain(error, url: url); return }
                guard let location, (response as? HTTPURLResponse)?.statusCode == 200 else {
                    failure = IDevice.ProbeError("downloading \(name) failed (HTTP \((response as? HTTPURLResponse)?.statusCode ?? 0))")
                    return
                }
                do { try FileManager.default.moveItem(at: location, to: destination) } catch { failure = error }
            }
            task.resume()
            done.wait()
            if let failure { throw failure }
        }
    }

    /// What `ensure` did: a line for the JIT session's events, and whether it mounted the image
    /// just now, which the tunnel it used can't show yet (see `IDevice.withJITTunnel`).
    struct Outcome {
        let line: String
        var mountedNow = false
    }

    /// Mounts the image over an open tunnel unless one is mounted, downloading its files the first
    /// time. Nil where there's nothing to do. Never throws: JIT can still work when something else
    /// mounted the image.
    static func ensure(adapter: OpaquePointer?, handshake: OpaquePointer?) -> Outcome? {
        #if os(iOS)
        var reply = AccountModel.call(apparition_ensure_ddi(adapter, handshake, hasFiles ? directory.path : nil))
        if reply["ok"] as? Bool == true, reply["needsFiles"] as? Bool == true {
            do {
                try download()
            } catch {
                log.error("disk image download failed: \(LogScrub.text(String(describing: error)), privacy: .public)")
                return Outcome(line: "disk image: couldn't download it: \(error)")
            }
            reply = AccountModel.call(apparition_ensure_ddi(adapter, handshake, directory.path))
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
            reply["error"] = "\(reply["error"] as? String ?? "JIT failed") (\(line))"
        }
        return reply
    }
}
