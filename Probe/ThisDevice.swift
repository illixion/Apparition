import SwiftUI

/// What to call the device Apparition runs on, for the text it shows: the same app is the
/// headset's on visionOS and the iPhone's or iPad's on iOS.
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
    #else
    static var noun: String { isPad ? "iPad" : "iPhone" }
    static var title: String { noun }
    static var name: String { noun }
    static var os: String { isPad ? "iPadOS" : "iOS" }
    static let home = "Home Screen"
    static let utmAssetPattern = "UTM.ipa"

    private static var isPad: Bool { UIDevice.current.userInterfaceIdiom == .pad }
    #endif
}
