import SwiftUI

/// What to call the device Apparition runs on, for the text it shows: the same app is the
/// headset's on visionOS, the iPhone's or iPad's on iOS, and the Apple TV's on tvOS.
enum ThisDevice {
    #if os(visionOS)
    /// For "this headset" mid-sentence.
    static let noun = "headset"
    /// For titles: "Pair This Headset".
    static let title = "Headset"
    /// The default name before pairing reports the real one.
    static let name = "Apple Vision Pro"
    static let os = "visionOS"
    static let home = "Home View"
    /// The text the name of UTM's release asset for this platform contains.
    static let utmAssetPattern = "visionOS"
    static func appCompatibilityLabel(deviceFamily: [Int]?) -> String {
        if deviceFamily?.contains(7) == true { return "visionOS app" }
        if deviceFamily?.contains(2) == true { return "iPad app · compatibility mode" }
        if deviceFamily?.contains(1) == true { return "iPhone app · compatibility mode" }
        return "Compatible app"
    }
    static func preferredBuildPlatforms(deviceFamily: [Int]?) -> [String] {
        if deviceFamily?.contains(7) == true { return ["visionos", "xros"] }
        if deviceFamily?.contains(2) == true { return ["ipad", "ios"] }
        if deviceFamily?.contains(1) == true { return ["ios", "iphone"] }
        return []
    }
    #elseif os(tvOS)
    static let noun = "Apple TV"
    static let title = "Apple TV"
    static let name = "Apple TV"
    static let os = "tvOS"
    static let home = "Home Screen"
    static let utmAssetPattern = "tvOS"
    static func appCompatibilityLabel(deviceFamily: [Int]?) -> String { "tvOS app" }
    static func preferredBuildPlatforms(deviceFamily: [Int]?) -> [String] { ["tvos"] }
    #else
    static var noun: String { isPad ? "iPad" : "iPhone" }
    static var title: String { noun }
    static var name: String { noun }
    static var os: String { isPad ? "iPadOS" : "iOS" }
    static let home = "Home Screen"
    static let utmAssetPattern = "UTM.ipa"

    static func appCompatibilityLabel(deviceFamily: [Int]?) -> String {
        if isPad {
            if deviceFamily?.contains(2) == true { return "iPad app" }
            if deviceFamily == [1] || (deviceFamily?.contains(1) == true && deviceFamily?.contains(2) != true) {
                return "iPhone app · compatibility mode"
            }
            return "iPad-compatible app"
        }
        return "iPhone app"
    }

    static func preferredBuildPlatforms(deviceFamily: [Int]?) -> [String] {
        if isPad {
            if deviceFamily == [1] || (deviceFamily?.contains(1) == true && deviceFamily?.contains(2) != true) {
                return ["ios", "iphone"]
            }
            if deviceFamily?.contains(2) == true { return ["ipad", "ios"] }
        }
        return ["ios", "iphone"]
    }

    private static var isPad: Bool { UIDevice.current.userInterfaceIdiom == .pad }
    #endif
}

/// Where Apparition keeps its files. tvOS lets an app open neither Documents nor Application
/// Support, only Caches, so there both live under Library/Caches/Apparition (which the system
/// may clear when storage runs low). Apparition Installer writes its handovers to the same
/// place (api::Tunnel::put_documents).
enum AppFolders {
    /// What Files shows as Apparition's folder: the Signing inbox, kept IPAs, the pairing file.
    static var documents: URL {
        #if os(tvOS)
        caches.appendingPathComponent("Documents", isDirectory: true)
        #else
        FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
        #endif
    }

    /// Private state (accounts, signing identities, the library).
    static var support: URL {
        #if os(tvOS)
        caches.appendingPathComponent("Support", isDirectory: true)
        #else
        FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
        #endif
    }

    #if os(tvOS)
    private static var caches: URL {
        let url = FileManager.default.urls(for: .cachesDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("Apparition", isDirectory: true)
        try? FileManager.default.createDirectory(at: url.appendingPathComponent("Documents"), withIntermediateDirectories: true)
        return url
    }
    #endif
}
