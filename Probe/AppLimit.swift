import Foundation

/// installd's limit on apps signed by free developer profiles: three on a device at once,
/// Apparition included. An install past it fails with "Application verification failed: This
/// device has reached the maximum number of installed apps using a free developer profile:
/// {( "TEAM.bundle.id", ... )}", which names the apps holding the slots. The Browse and
/// Library tabs show them and offer to remove one, then retry the install.
struct AppLimit: Sendable, Equatable {
    /// What to run again once a slot is free.
    enum Retry: Sendable, Equatable {
        /// An app in the library, by its InstalledApp id (its kept IPA is re-signed).
        case app(id: String)
        case selfRefresh
        case selfUpdate
    }

    /// Bundle IDs of the apps holding the slots, team prefix stripped, in installd's order.
    var blocking: [String]
    /// The app that couldn't install, as the user knows it.
    let appName: String
    let retry: Retry
    /// Why the last removal failed, shown on the card.
    var removalError: String?

    static let marker = "maximum number of installed apps using a free developer profile"

    /// The bundle IDs installd lists in an app-limit error, or nil when `text` isn't one.
    /// Entries are application identifiers ("<TEAMID>.<bundle id>"); the team is dropped.
    static func blockingApps(in text: String) -> [String]? {
        guard let range = text.range(of: marker, options: .caseInsensitive) else { return nil }
        let rest = text[range.upperBound...]
        // installd prints an NSSet: {( "a", "b" )}. Without one, the error names no apps.
        guard let open = rest.range(of: "{("), let close = rest.range(of: ")}", range: open.upperBound..<rest.endIndex) else {
            return []
        }
        let junk = CharacterSet.whitespacesAndNewlines.union(CharacterSet(charactersIn: "\"\\"))
        return rest[open.upperBound..<close.lowerBound]
            .split(separator: ",")
            .map { $0.trimmingCharacters(in: junk) }
            .filter { !$0.isEmpty }
            .map(bundleId(fromApplicationIdentifier:))
    }

    /// "V4H6J97B85.com.example.app" -> "com.example.app". A team ID is ten uppercase letters
    /// and digits; anything else is left as it is.
    static func bundleId(fromApplicationIdentifier identifier: String) -> String {
        guard let dot = identifier.firstIndex(of: ".") else { return identifier }
        let prefix = identifier[..<dot]
        let isTeam = prefix.count == 10 && prefix.allSatisfy { $0.isASCII && ($0.isUppercase || $0.isNumber) }
        return isTeam ? String(identifier[identifier.index(after: dot)...]) : identifier
    }

    #if DEBUG
    /// The error a fourth free-profile install gave on an iPhone, for the debug endpoint.
    static let sampleError = """
        error: install failed: error: Install failed
        Application verification failed: This device has reached the maximum number of installed apps using a free developer profile: {(
        "V4H6J97B85.com.illixion.apparition.V4H6J97B85",
        "U8T3Z4P7JT.com.illixion.apparition.U8T3Z4P7JT",
        "9T5F88VJFL.com.illixion.ppsspp.debug"
        )}
        """
    #endif
}
