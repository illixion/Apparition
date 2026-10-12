import CryptoKit
import Foundation

/// Apparition's own installed copy: who signed it and when its profile runs out, read from
/// the bundle's embedded.mobileprovision. Self-refresh re-signs this copy under the same
/// bundle ID and team, so installd treats it as an update and keeps the app's data.
struct SelfApp: Sendable {
    let bundleId: String
    let teamId: String?
    let teamName: String?
    let profileName: String?
    let expires: Date?
    /// SHA-1 fingerprints (uppercase hex, as SigningStore ids) of the certificates the profile allows.
    let certificateIds: [String]

    /// Identifies this exact install: the bundle's path (installd moves an update to a new
    /// container) and the main executable's hash (re-signing rewrites its signature). A
    /// re-sign with the same profile keeps the expiry, so the expiry can't tell.
    static func installStamp() -> String {
        let executable = Bundle.main.executableURL.flatMap { try? Data(contentsOf: $0, options: .mappedIfSafe) }
        let hash = executable.map { SHA256.hash(data: $0).prefix(8).map { String(format: "%02x", $0) }.joined() } ?? "-"
        return "\(Bundle.main.bundleURL.deletingLastPathComponent().lastPathComponent)/\(hash)"
    }

    /// Nil without an embedded profile (the simulator, or an App Store or TestFlight build).
    static func current() -> SelfApp? {
        guard let bundleId = Bundle.main.bundleIdentifier,
              let profile = AccountModel.embeddedProfile(Bundle.main.bundleURL.appendingPathComponent("embedded.mobileprovision"))
        else { return nil }
        let certificates = profile["DeveloperCertificates"] as? [Data] ?? []
        return SelfApp(
            bundleId: bundleId,
            teamId: (profile["TeamIdentifier"] as? [String])?.first,
            teamName: profile["TeamName"] as? String,
            profileName: profile["Name"] as? String,
            expires: profile["ExpirationDate"] as? Date,
            certificateIds: certificates.map { Insecure.SHA1.hash(data: $0).map { String(format: "%02X", $0) }.joined() })
    }

    /// The bundle ID to hand the Apple ID signer, which appends the team ID itself: the
    /// installed ID without that suffix, so signing gives back the installed ID.
    func appleIDBaseBundleId(team: String) -> String? {
        let suffix = ".\(team)"
        guard bundleId.hasSuffix(suffix), bundleId.count > suffix.count else { return nil }
        return String(bundleId.dropLast(suffix.count))
    }

    /// Copies the installed bundle (or `source`, an update of it) somewhere writable for
    /// signing, optionally with another CFBundleIdentifier in its Info.plist. Returns the copy's path.
    static func copyBundle(from source: URL = Bundle.main.bundleURL, bundleId: String?) throws -> URL {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("self-refresh")
        try? FileManager.default.removeItem(at: directory)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        let copy = directory.appendingPathComponent(Bundle.main.bundleURL.lastPathComponent)
        try FileManager.default.copyItem(at: source, to: copy)
        if let bundleId {
            let plist = copy.appendingPathComponent("Info.plist")
            var format = PropertyListSerialization.PropertyListFormat.binary
            guard var info = try PropertyListSerialization.propertyList(
                from: Data(contentsOf: plist), options: [], format: &format) as? [String: Any]
            else { throw IDevice.ProbeError("can't read Apparition's Info.plist") }
            info["CFBundleIdentifier"] = bundleId
            try PropertyListSerialization.data(fromPropertyList: info, format: format, options: 0).write(to: plist)
        }
        return copy
    }
}
