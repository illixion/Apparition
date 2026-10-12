import Foundation

/// Official releases of Apparition, from apparition.illixion.com. The core fetches and checks
/// them (core/src/release.rs): only a release whose SHA256SUMS is signed by a pinned release
/// key, and an IPA matching it, ever comes back. The app is then re-signed like a refresh.
enum SelfUpdate {
    struct Latest: Codable, Sendable, Equatable {
        let version: String
        let build: Int?
        let date: String?
        let notes: String?
        let signer: String
        /// Size in bytes of this platform's app, nil when the release has none for it.
        let appSize: Int?
    }

    struct Downloaded: Sendable {
        let version: String
        let appURL: URL
    }

    struct Failure: Error, CustomStringConvertible {
        let description: String
    }

    #if os(visionOS)
    static let platform = "visionos"
    #elseif os(tvOS)
    static let platform = "tvos"
    #else
    static let platform = "ios"
    #endif

    /// The version this copy says it is.
    static var currentVersion: String {
        Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String ?? "0"
    }

    static func isNewer(_ candidate: String, than current: String) -> Bool {
        candidate.compare(current, options: .numeric) == .orderedDescending
    }

    // MARK: Test overrides

    /// A development build can be pointed at another site, with other keys, to try an update
    /// without signing one with the release keys. An official build (APPARITION_RELEASE, set by
    /// scripts/package-release.sh) always uses the official site and keys.
    static let siteKey = "releaseSiteOverride"
    static let signersKey = "releaseSignersOverride"
    #if !APPARITION_RELEASE
    static var site: String? { UserDefaults.standard.string(forKey: siteKey) }
    static var signers: String? { UserDefaults.standard.string(forKey: signersKey) }
    #else
    static var site: String? { nil }
    static var signers: String? { nil }
    #endif

    // MARK: Calls

    static func latest() async throws -> Latest {
        let (site, signers) = (Self.site, Self.signers)
        let reply = await Task.detached {
            AccountModel.call(apparition_release_latest(site, signers, platform))
        }.value
        guard reply["ok"] as? Bool == true, let version = reply["version"] as? String,
              let signer = reply["signer"] as? String
        else { throw Failure(description: reply["error"] as? String ?? "no reply") }
        return Latest(version: version, build: reply["build"] as? Int, date: reply["date"] as? String,
                      notes: reply["notes"] as? String, signer: signer,
                      appSize: (reply["asset"] as? [String: Any])?["size"] as? Int)
    }

    /// Downloads, verifies and unpacks the latest release's app, then checks that the bundle
    /// is the release it claims to be.
    static func download() async throws -> Downloaded {
        let (site, signers) = (Self.site, Self.signers)
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("self-update")
        let reply = await Task.detached {
            AccountModel.call(apparition_release_download_app(site, signers, platform, dir.path))
        }.value
        guard reply["ok"] as? Bool == true, let version = reply["version"] as? String,
              let appPath = reply["appPath"] as? String
        else { throw Failure(description: reply["error"] as? String ?? "no reply") }
        let app = URL(fileURLWithPath: appPath)
        let info = NSDictionary(contentsOf: app.appendingPathComponent("Info.plist")) as? [String: Any]
        guard info?["CFBundleShortVersionString"] as? String == version else {
            throw Failure(description: "the downloaded app says it's version \(info?["CFBundleShortVersionString"] as? String ?? "?"), not \(version)")
        }
        return Downloaded(version: version, appURL: app)
    }

    static func cleanUp() {
        try? FileManager.default.removeItem(at: FileManager.default.temporaryDirectory.appendingPathComponent("self-update"))
    }
}
