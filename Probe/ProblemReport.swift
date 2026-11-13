import DebugTrace
import DebugTraceUI
import Foundation

/// Settings > Report a Problem: where a report goes and who can open it
/// (docs/design/PUBLIC_DEBUGGING_PLAN.md, section 1).
enum ProblemReport {
    /// The maintainer's public key for reports (X25519, base64; key id 8332337272b7d9b4), made
    /// with DebugTrace's `debugtrace-decrypt keygen`. A `.trace` file sealed to it opens only
    /// with the private half, which never leaves the maintainer's Mac. To replace it, make a
    /// new pair and change this line: reports sealed to the old key still open with the old one.
    static let maintainerKey = "PDKM2MoQUg4oqdq13K0WZwxPwog1xgNPR0HFspQE8RY="

    static let issues = "https://github.com/illixion/Apparition/issues/new"

    static var configuration: DebugTraceReportConfiguration {
        DebugTraceReportConfiguration(
            recipient: try? DebugTraceRecipient(publicKey: maintainerKey),
            issueURL: issueURL(),
            destinationName: "Apparition's issue tracker on GitHub")
    }

    /// The bug form with what the app already knows filled in. The field ids are the ones in
    /// .github/ISSUE_TEMPLATE/bug_report.yml (`where`, `version`, `os`).
    static func issueURL(version: String = SettingsView.version,
                         system: String = "\(ThisDevice.name), \(ThisDevice.os) \(osVersion)",
                         place: String = formPlace) -> URL? {
        var components = URLComponents(string: issues)
        components?.queryItems = [
            URLQueryItem(name: "template", value: "bug_report.yml"),
            URLQueryItem(name: "where", value: place),
            URLQueryItem(name: "version", value: version),
            URLQueryItem(name: "os", value: system),
        ]
        return components?.url
    }

    /// The form's "Where does it happen?" option for this device, word for word.
    static var formPlace: String {
        #if os(visionOS)
        "Apparition on Apple Vision Pro"
        #elseif os(tvOS)
        "Apparition on Apple TV"
        #else
        "Apparition on iPhone or iPad"
        #endif
    }

    private static var osVersion: String {
        let version = ProcessInfo.processInfo.operatingSystemVersion
        return version.patchVersion == 0
            ? "\(version.majorVersion).\(version.minorVersion)"
            : "\(version.majorVersion).\(version.minorVersion).\(version.patchVersion)"
    }
}
