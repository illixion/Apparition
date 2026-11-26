import DebugTrace
import Foundation

private let log = DebugLogger(subsystem: Bundle.main.bundleIdentifier ?? "Apparition", category: "DiskImage")

/// The personalized Developer Disk Image, which debugserver needs mounted on iOS 17 and later.
/// A restart unmounts it, and until now only Xcode or another app (StikDebug) put it back, so
/// JIT failed after every restart of a device that no computer had prepared since. Before a JIT
/// session Apparition checks over the same tunnel and mounts it when nothing is: at most once per
/// restart. The image comes from Xcode on the Mac (handed over at install time), or from a
/// download the person allowed; nothing is fetched without that.
enum DeveloperDiskImage {
    /// Handed over by the Mac or its paired computer, from Xcode's own copy.
    static var bundle: URL {
        AppFolders.documents.appendingPathComponent("Apparition-iOS-DDI-v1.apddi")
    }

    /// Where the download the person allowed is kept: Xcode's three files, as a public mirror serves them.
    static var downloadDirectory: URL {
        FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("DeveloperDiskImage", isDirectory: true)
    }
    static let source = URL(string: "https://raw.githubusercontent.com/doronz88/DeveloperDiskImage/main/PersonalizedImages/Xcode_iOS_DDI_Personalized/")!
    static let files = ["Image.dmg", "Image.dmg.trustcache", "BuildManifest.plist"]
    static let sourceHost = "GitHub (raw.githubusercontent.com)"
    static let downloadSize = "about 18 MB"

    enum Availability { case handedOver, downloaded, none }

    static var availability: Availability {
        let fm = FileManager.default
        if fm.fileExists(atPath: bundle.path) { return .handedOver }
        if files.allSatisfy({ fm.fileExists(atPath: downloadDirectory.appendingPathComponent($0).path) }) { return .downloaded }
        return .none
    }

    /// Deletes the download, e.g. a damaged one. The image Xcode handed over is not touched.
    static func removeDownload() {
        try? FileManager.default.removeItem(at: downloadDirectory)
    }

    /// What the mount is given: the Mac's bundle first, else the download.
    private static var path: String? {
        switch availability {
        case .handedOver: bundle.path
        case .downloaded: downloadDirectory.path
        case .none: nil
        }
    }

    /// In every failure that a download would fix, so the alert can offer it.
    static let marker = "Developer Disk Image needed"

    static let needsImage = "\(marker). JIT needs Apple's Developer Disk Image, which normally comes from Xcode on your Mac: reinstall Apparition from a Mac that has Xcode (open Xcode once first). Without Xcode, Apparition can download the same image (\(downloadSize)) from \(sourceHost) if you allow it, in Settings › Developer Disk Image. Apple's servers still personalize it for this device."

    /// Downloads Xcode's image from the public mirror into `downloadDirectory`, replacing any earlier
    /// copy. Only ever started by the person asking for it. Files go in a scratch folder first, so a
    /// failed download leaves what was there.
    static func download() async throws {
        let fm = FileManager.default
        let staging = downloadDirectory.deletingLastPathComponent().appendingPathComponent("DeveloperDiskImage.partial", isDirectory: true)
        try? fm.removeItem(at: staging)
        try fm.createDirectory(at: staging, withIntermediateDirectories: true)
        defer { try? fm.removeItem(at: staging) }
        for name in files {
            let url = source.appendingPathComponent(name)
            // SafeSession: the image is code the device runs, so it gets the same refusal of
            // interception CAs as the app's other downloads.
            let location: URL, response: URLResponse
            do {
                (location, response) = try await SafeSession.download(from: url)
            } catch {
                throw IDevice.ProbeError(explain(error))
            }
            guard (response as? HTTPURLResponse)?.statusCode == 200 else {
                throw IDevice.ProbeError("downloading \(name) failed (HTTP \((response as? HTTPURLResponse)?.statusCode ?? 0))")
            }
            try fm.moveItem(at: location, to: staging.appendingPathComponent(name))
        }
        // Checked against its own manifest before it replaces a good copy.
        let check = AccountModel.call(apparition_ddi_check(staging.path))
        guard check["ok"] as? Bool == true else {
            throw IDevice.ProbeError("the download from \(sourceHost) is damaged (\(check["error"] as? String ?? "it doesn't match its manifest"))")
        }
        try? fm.removeItem(at: downloadDirectory)
        try fm.moveItem(at: staging, to: downloadDirectory)
        log.info("disk image downloaded")
    }

    /// A person's words for a failed download, not the URLError dump (which names the URL and a
    /// dozen internal keys). Anything that isn't a plain network failure keeps its own description.
    private static func explain(_ error: Error) -> String {
        guard let error = error as? URLError else { return String(describing: error) }
        switch error.code {
        case .notConnectedToInternet, .dataNotAllowed, .internationalRoamingOff:
            return "this device is offline. Connect to the internet and try again."
        case .timedOut:
            return "\(sourceHost) took too long to answer. Try again on a better connection."
        case .cannotFindHost, .cannotConnectToHost, .dnsLookupFailed, .networkConnectionLost:
            return "couldn't reach \(sourceHost). Check the connection and try again."
        default:
            return "\(error.localizedDescription)"
        }
    }

    /// What `ensure` did: a line for the JIT session's events, and whether it mounted the image
    /// just now, which the tunnel it used can't show yet (see `IDevice.withDeveloperTunnel`).
    struct Outcome {
        let line: String
        var mountedNow = false
        var needsFiles = false
    }

    /// Mounts the image (handed over, or downloaded with permission) if nothing is mounted. Other platforms use their own developer image.
    static func ensure(adapter: OpaquePointer?, handshake: OpaquePointer?) -> Outcome? {
        #if os(iOS)
        let reply = AccountModel.call(apparition_ensure_ddi(adapter, handshake, path))
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
