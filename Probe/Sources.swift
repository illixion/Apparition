import Foundation

/// App sources Apparition can install from:
/// - AltStore/SideStore source JSON (`apps[].versions[]`, or the older app-level `version`/`downloadURL`);
/// - `github:owner/repo`, the newest release with a matching `.ipa` asset (see fetchGitHub).
///   Many visionOS builds (UTM's `UTM-visionOS.ipa`) are only published that way.
enum Sources {
    struct App: Codable, Sendable, Identifiable {
        /// The source's bundle identifier, or `github:owner/repo` until the IPA is downloaded.
        let id: String
        let name: String
        let developer: String?
        let version: String
        let date: String?
        let downloadURL: String
        let size: Int?
        let minOSVersion: String?
        let iconURL: String?
        /// An AltStore PAL listing: the download is an EU App Marketplace package, not an IPA,
        /// so it can't be sideloaded. Missing in sources saved before this was recorded.
        var marketplace: Bool? = nil
    }

    struct Source: Codable, Sendable, Identifiable {
        /// The URL as entered: an https URL or `github:owner/repo`.
        let id: String
        var assetPattern: String?
        var prereleases: Bool
        var name: String?
        var apps: [App] = []
        var fetchedAt: String?
        var error: String?
    }

    static func fetch(_ source: Source) async throws -> (name: String, apps: [App]) {
        if source.id.hasPrefix("github:") {
            return try await fetchGitHub(repo: String(source.id.dropFirst("github:".count)),
                                         pattern: source.assetPattern ?? "", prereleases: source.prereleases)
        }
        guard let url = URL(string: source.id), url.scheme == "https" || url.scheme == "http" else {
            throw IDevice.ProbeError("not a source URL: \(source.id)")
        }
        return try parseAltStore(try await get(url))
    }

    static func parseAltStore(_ data: Data) throws -> (name: String, apps: [App]) {
        guard let root = try JSONSerialization.jsonObject(with: data) as? [String: Any],
              let apps = root["apps"] as? [[String: Any]] else {
            throw IDevice.ProbeError("not an AltStore source (no apps array)")
        }
        let parsed = apps.compactMap { app -> App? in
            guard let bundle = app["bundleIdentifier"] as? String, let name = app["name"] as? String else { return nil }
            // Sources list versions newest first; older sources put one version on the app itself.
            let latest = (app["versions"] as? [[String: Any]])?.first ?? app
            guard let version = latest["version"] as? String,
                  let download = latest["downloadURL"] as? String else { return nil }
            // PAL apps carry a marketplaceID; the odd one that also ships an IPA stays installable.
            let marketplace = app["marketplaceID"] != nil
                && !(URL(string: download)?.path.lowercased().hasSuffix(".ipa") ?? false)
            return App(id: bundle, name: name, developer: app["developerName"] as? String, version: version,
                       date: (latest["date"] ?? latest["versionDate"]) as? String, downloadURL: download,
                       size: latest["size"] as? Int, minOSVersion: latest["minOSVersion"] as? String,
                       iconURL: app["iconURL"] as? String, marketplace: marketplace)
        }
        return (root["name"] as? String ?? "Source", parsed)
    }

    static func fetchGitHub(repo: String, pattern: String, prereleases: Bool) async throws -> (name: String, apps: [App]) {
        let parts = repo.split(separator: "/")
        guard parts.count == 2, let url = URL(string: "https://api.github.com/repos/\(repo)/releases?per_page=20") else {
            throw IDevice.ProbeError("expected github:owner/repo, got github:\(repo)")
        }
        guard let releases = try JSONSerialization.jsonObject(with: try await get(url)) as? [[String: Any]] else {
            throw IDevice.ProbeError("unexpected GitHub reply for \(repo)")
        }
        for release in releases {
            if release["draft"] as? Bool == true { continue }
            if release["prerelease"] as? Bool == true && !prereleases { continue }
            let assets = release["assets"] as? [[String: Any]] ?? []
            // A pattern ending in .ipa names the asset exactly; otherwise the shortest matching
            // name wins, so "visionOS" picks UTM-visionOS.ipa over UTM-Remote-visionOS.ipa.
            let wanted = pattern.lowercased()
            let matches = assets.filter { asset in
                let name = (asset["name"] as? String ?? "").lowercased()
                guard name.hasSuffix(".ipa") else { return false }
                return wanted.hasSuffix(".ipa") ? name == wanted : (wanted.isEmpty || name.contains(wanted))
            }
            guard let asset = matches.min(by: { ($0["name"] as? String ?? "").count < ($1["name"] as? String ?? "").count }),
                  let download = asset["browser_download_url"] as? String else { continue }
            let tag = release["tag_name"] as? String ?? "?"
            let version = tag.hasPrefix("v") ? String(tag.dropFirst()) : tag
            let app = App(id: "github:\(repo)", name: String(parts[1]), developer: String(parts[0]), version: version,
                          date: release["published_at"] as? String, downloadURL: download,
                          size: asset["size"] as? Int, minOSVersion: nil, iconURL: nil)
            return (repo, [app])
        }
        throw IDevice.ProbeError("no release of \(repo) has an .ipa matching \"\(pattern)\"")
    }

    private static func get(_ url: URL) async throws -> Data {
        var request = URLRequest(url: url)
        request.setValue("Apparition", forHTTPHeaderField: "User-Agent")
        let (data, response) = try await URLSession.shared.data(for: request)
        let status = (response as? HTTPURLResponse)?.statusCode ?? 0
        guard status == 200 else { throw IDevice.ProbeError("HTTP \(status) from \(url.host() ?? "?")") }
        return data
    }
}
