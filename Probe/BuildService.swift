import Foundation
import Observation
import Security

struct BuildServicePasswords {
    var load: (String) -> String?
    var save: (String, String) -> Bool
    var delete: (String?) -> Void

    static let keychain: Self = {
        let service = "com.illixion.apparition.build-service"
        func query(_ origin: String?) -> [String: Any] {
            var values: [String: Any] = [kSecClass as String: kSecClassGenericPassword,
                                        kSecAttrService as String: service]
            if let origin { values[kSecAttrAccount as String] = origin }
            return values
        }
        return Self(load: { origin in
            var values = query(origin)
            values[kSecReturnData as String] = true
            var result: CFTypeRef?
            guard SecItemCopyMatching(values as CFDictionary, &result) == errSecSuccess,
                  let data = result as? Data else { return nil }
            return String(data: data, encoding: .utf8)
        }, save: { origin, password in
            let attributes: [String: Any] = [kSecValueData as String: Data(password.utf8),
                kSecAttrAccessible as String: kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly]
            let status = SecItemUpdate(query(origin) as CFDictionary, attributes as CFDictionary)
            if status == errSecSuccess { return true }
            guard status == errSecItemNotFound else { return false }
            return SecItemAdd(query(origin).merging(attributes) { _, new in new } as CFDictionary, nil) == errSecSuccess
        }, delete: { origin in _ = SecItemDelete(query(origin) as CFDictionary) })
    }()
}

/// A build-service session is independent of pairing, Apple ID and live debug access.
/// Keep its bearer in memory; optionally remember the password in this device's
/// Keychain, separately for each service origin. Never follow credential redirects.
private final class BuildServiceRedirects: NSObject, URLSessionTaskDelegate, @unchecked Sendable {
    func urlSession(_ session: URLSession, task: URLSessionTask,
                    willPerformHTTPRedirection response: HTTPURLResponse, newRequest request: URLRequest,
                    completionHandler: @escaping (URLRequest?) -> Void) { completionHandler(nil) }
}

@MainActor @Observable
final class BuildService {
    struct Target: Decodable, Hashable {
        let platform: String
        let device: String
        let bundleId: String?
    }
    struct Project: Decodable, Identifiable, Hashable {
        /// The checkout's branch and commit; `nil` fields when git couldn't be read.
        struct Git: Decodable, Hashable { let branch: String?; let commit: String?; let dirty: Bool? }
        var id: String { name }
        let name: String
        let branches: [String]?
        let platforms: [Target]
        let git: Git?
    }
    struct Job: Decodable, Identifiable {
        struct Resign: Decodable { let dryRun: Bool?; let result: String?; let after: Int? }
        let id: Int
        let repo: String
        let platform: String
        let device: String?
        let status: String
        let buildId: String?
        let noDeploy: Bool?
        let forResign: Bool?
        let resign: Resign?
        let commit: String?
        let builtBranch: String?
        let error: String?
        /// How the service was asked to get the build onto the device (`mac`, `device`, `none`).
        let installVia: String?
        /// A build the Mac made and kept but could not install itself (the tunnel failed): it can
        /// still be installed from the device.
        let keptBuildId: String?
        var cancellable: Bool { status == "queued" || status == "running" }
        var active: Bool { cancellable }
        /// What the job is, in words, from the flags the service sets.
        var kind: String {
            if let resign { return resign.dryRun == true ? "Re-sign check" : "Re-sign" }
            if forResign == true { return "Build for re-sign" }
            switch installVia {
            case "device": return "Build for the device"
            case "mac": return "Build & install from the Mac"
            case "none": return "Build only"
            default: return noDeploy == true ? "Build only" : "Build & install"
            }
        }
    }
    /// How a build reaches the device (the service's `install`). The Xcode tunnel is `mac`.
    enum InstallMode: String, Sendable {
        case auto, mac, device, none
    }
    /// A device the Mac can install to (the service leaves simulators out).
    struct Device: Decodable, Identifiable {
        var id: String { name }
        let name: String
        let model: String?
        let state: String?
        let platform: String?
        var reachable: Bool { state == "connected" }
    }
    struct Devices: Decodable { let list: [Device] }
    /// The service's ledger of what it installed where.
    struct Install: Decodable {
        let repo: String?
        let branch: String?
        let commit: String?
        let buildId: String?
        let buildNo: String?
        let confirmed: Bool?
        let resigned: String?
        let at: Double?
        /// `apparition` when the device installed it itself and said so; nil for the Mac's own installs.
        let via: String?
    }
    struct State: Decodable {
        let host: String?
        let repos: [Project]
        let builds: [Job]
        let devices: Devices?
        let installs: [String: [String: Install]]?
        /// Devices (lowercased names) whose Apparition is open now; `install: auto` goes to them.
        let appsOpen: [String]?

        private enum CodingKeys: String, CodingKey { case host, repos, builds, devices, installs, appsOpen }
        init(from decoder: Decoder) throws {
            let c = try decoder.container(keyedBy: CodingKeys.self)
            host = try c.decodeIfPresent(String.self, forKey: .host)
            repos = try c.decode([Project].self, forKey: .repos)
            builds = try c.decode([Job].self, forKey: .builds)
            // Extras for display: an entry the client doesn't understand must not cost the projects.
            devices = try? c.decodeIfPresent(Devices.self, forKey: .devices)
            installs = try? c.decodeIfPresent([String: [String: Install]].self, forKey: .installs)
            appsOpen = try? c.decodeIfPresent([String].self, forKey: .appsOpen)
        }
    }
    struct Artifact: Decodable, Identifiable {
        struct App: Decodable { let name: String?; let bundleId: String; let originalBundleId: String?; let build: String?; let version: String? }
        var id: String { buildId }
        let buildId: String
        let platform: String
        let project: String?
        let createdAt: String?
        let app: App
    }
    private struct Artifacts: Decodable { let builds: [Artifact] }
    /// A provisioning profile in the Mac's build-signing folder, as the service describes it:
    /// no path and no device list, only what choosing one needs.
    struct ResignProfile: Decodable, Identifiable, Hashable {
        let id: String
        let name: String
        let appId: String
        let expired: Bool
        let debuggable: Bool
        var label: String { "\(name) · \(appId)" + (debuggable ? "" : " · not debuggable") }
    }
    private struct ResignOptions: Decodable { let profiles: [ResignProfile] }
    struct Trace: Decodable, Identifiable {
        struct App: Decodable { let name: String?; let bundleId: String; let version: String?; let build: String? }
        struct Build: Decodable { let buildId: String?; let repo: String?; let platform: String? }
        struct File: Decodable, Identifiable { let path: String; let bytes: Int; var id: String { path } }
        let id: String
        let receivedAt: Double?
        let createdAt: String?
        let app: App
        let build: Build?
        let files: [File]
    }
    struct CrashLog: Decodable, Identifiable {
        let id: String
        let bundleId: String
        let appName: String?
        let version: String?
        let build: String?
        let exception: String?
        let device: String?
        let capturedAt: Double?
        let symbolicated: String?
        let traceId: String?
    }
    private struct Traces: Decodable { let traces: [Trace] }
    private struct CrashLogs: Decodable { let crashLogs: [CrashLog] }
    private struct Connection: Decodable {
        let `protocol`: String
        let version: Int
        let token: String
        let capabilities: [String]?
    }
    private struct Advertisement: Decodable { let capabilities: [String]? }
    private struct BuildReply: Decodable { let jobs: [Job] }
    private struct Log: Decodable { let chunk: String }
    private struct OK: Decodable { let ok: Bool }
    private struct Reason: Decodable { let error: String }
    enum Failure: LocalizedError {
        case address, response, rejected(Int), refused(String), expired
        var errorDescription: String? {
            switch self {
            case .address: "Use an HTTPS service address, or HTTP on localhost. Do not include credentials, a path or query."
            case .response: "The service returned an unsupported or oversized response."
            case .rejected(let status):
                switch status {
                case 401: "Connect to the build service again."
                // A proxy in front of the service answers these while the service itself is down or restarting.
                case 502, 503, 504: "The build service isn't answering right now (HTTP \(status))."
                default: "The service refused the request (HTTP \(status))."
                }
            case .refused(let reason): reason
            case .expired: "The connection changed; try again."
            }
        }
    }

    var address: String
    private(set) var connected = false
    private(set) var busy = false
    /// What the connected service says it can do (`resign` among them); empty when disconnected.
    private(set) var capabilities: Set<String> = []
    private(set) var state: State?
    private(set) var artifacts: [Artifact] = []
    private(set) var traces: [Trace] = []
    private(set) var crashLogs: [CrashLog] = []
    private(set) var detail = ""
    private(set) var reportDetail = ""
    private(set) var reportDetailOwnerBundleId: String?
    private(set) var reportDetailID: String?
    var message = ""
    var rememberPassword: Bool {
        didSet {
            defaults.set(rememberPassword, forKey: "rememberBuildServicePassword")
            if !rememberPassword { passwords.delete(nil) }
        }
    }
    @ObservationIgnored private var base: URL?
    @ObservationIgnored private var token: String?
    @ObservationIgnored private var generation = UUID()
    @ObservationIgnored private let session: URLSession
    @ObservationIgnored private let defaults: UserDefaults
    @ObservationIgnored private let passwords: BuildServicePasswords
    @ObservationIgnored private var restoreAttempted = false
    /// A poll failed and none has succeeded since: the service is down or restarting, so what it
    /// advertised (`capabilities`) may be out of date once it answers again.
    @ObservationIgnored private var unreachable = false
    /// The sentence a failed poll left in `message`, so a later success clears that and nothing else.
    @ObservationIgnored private var pollMessage: String?
    @ObservationIgnored private var capabilitiesFetchedAt = Date.distantPast

    init(defaults: UserDefaults = .standard, passwords: BuildServicePasswords = .keychain) {
        self.defaults = defaults
        self.passwords = passwords
        address = defaults.string(forKey: "buildServiceURL") ?? ""
        rememberPassword = defaults.object(forKey: "rememberBuildServicePassword") as? Bool ?? true
        let configuration = URLSessionConfiguration.ephemeral
        configuration.httpShouldSetCookies = false
        configuration.httpCookieStorage = nil
        configuration.urlCache = nil
        configuration.timeoutIntervalForRequest = 90
        configuration.timeoutIntervalForResource = 120
        session = URLSession(configuration: configuration, delegate: BuildServiceRedirects(), delegateQueue: nil)
    }

    static func serviceURL(_ value: String) throws -> URL {
        guard var components = URLComponents(string: value.trimmingCharacters(in: .whitespacesAndNewlines)),
              let host = components.host, !host.isEmpty, components.user == nil, components.password == nil,
              components.query == nil, components.fragment == nil,
              components.path.isEmpty || components.path == "/",
              components.scheme == "https" || (components.scheme == "http" && ["localhost", "127.0.0.1", "[::1]", "::1"].contains(host)),
              components.url != nil else { throw Failure.address }
        components.scheme = components.scheme?.lowercased()
        components.host = host.lowercased()
        if (components.scheme == "https" && components.port == 443) || (components.scheme == "http" && components.port == 80) { components.port = nil }
        components.path = "/"
        guard let url = components.url else { throw Failure.address }
        return url
    }

    /// Authorizes a request, and names this device on it so the service knows Apparition is open
    /// here (a build asked for `install: auto` then goes to the device, not through the Xcode
    /// tunnel). A closed Apparition sends nothing, and the service stops counting it.
    private func stamp(_ request: inout URLRequest, token: String) {
        request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        if let name = announcedDevice, let encoded = name.addingPercentEncoding(withAllowedCharacters: .alphanumerics.union(CharacterSet(charactersIn: "-._~"))) {
            request.setValue(encoded, forHTTPHeaderField: "X-Apparition-Device")
        }
    }

    /// The name Xcode shows for this device: the paired name Apparition keeps.
    var announcedDevice: String? {
        guard let name = defaults.string(forKey: "pairedName")?.trimmingCharacters(in: .whitespacesAndNewlines), !name.isEmpty else { return nil }
        return name
    }

    private func request<T: Decodable>(_ path: String, at base: URL, token: String? = nil,
                                       body: [String: Any]? = nil) async throws -> T {
        var request = URLRequest(url: base.appendingPathComponent(path))
        request.setValue("application/json", forHTTPHeaderField: "Accept")
        if let token { stamp(&request, token: token) }
        if let body {
            request.httpMethod = "POST"
            request.setValue("application/json", forHTTPHeaderField: "Content-Type")
            request.httpBody = try JSONSerialization.data(withJSONObject: body)
        }
        let (bytes, response) = try await session.bytes(for: request)
        guard let response = response as? HTTPURLResponse else { throw Failure.response }
        guard response.statusCode == 200 else {
            // The service explains a refused request in `{error}`; keep that sentence, bounded.
            if (400..<500).contains(response.statusCode), response.statusCode != 401, response.expectedContentLength <= 4096 {
                var data = Data()
                for try await byte in bytes { guard data.count < 4096 else { break }; data.append(byte) }
                if let reason = try? JSONDecoder().decode(Reason.self, from: data), !reason.error.isEmpty, reason.error.count <= 500 {
                    throw Failure.refused(reason.error)
                }
            }
            throw Failure.rejected(response.statusCode)
        }
        let limit = 8 * 1024 * 1024
        guard response.expectedContentLength <= limit else { throw Failure.response }
        var data = Data()
        for try await byte in bytes {
            guard data.count < limit else { throw Failure.response }
            data.append(byte)
        }
        do { return try JSONDecoder().decode(T.self, from: data) }
        catch { throw Failure.response }
    }

    private func authenticated<T: Decodable>(_ path: String, body: [String: Any]? = nil) async throws -> T {
        guard let base, let token, connected else { throw Failure.expired }
        let expected = generation
        let reply: T
        do { reply = try await request("api/service/v1/" + path, at: base, token: token, body: body) }
        catch Failure.rejected(401) {
            guard expected == generation else { throw Failure.expired }
            guard rememberPassword, let password = passwords.load(base.absoluteString) else {
                connected = false; self.token = nil
                throw Failure.rejected(401)
            }
            let refreshed: String
            let advertised: Set<String>
            do { (refreshed, advertised) = try await openSession(at: base, password: password) }
            catch Failure.rejected(401) {
                guard expected == generation else { throw Failure.expired }
                passwords.delete(base.absoluteString); connected = false; self.token = nil
                throw Failure.rejected(401)
            }
            guard expected == generation else { throw Failure.expired }
            self.token = refreshed
            // A new session means the service restarted, perhaps as a newer version.
            capabilities = advertised; capabilitiesFetchedAt = Date()
            reply = try await request("api/service/v1/" + path, at: base, token: refreshed, body: body)
        }
        guard expected == generation else { throw Failure.expired }
        return reply
    }

    private func openSession(at url: URL, password: String) async throws -> (token: String, capabilities: Set<String>) {
        let reply: Connection = try await request("api/service/connect", at: url, body: ["password": password])
        guard reply.protocol == "apparition-service", reply.version == 1,
              reply.token.count == 43, reply.token.allSatisfy({ $0.isASCII && ($0.isLetter || $0.isNumber || $0 == "_" || $0 == "-") }) else { throw Failure.response }
        return (reply.token, Set(reply.capabilities ?? []))
    }

    func restoreConnection() async {
        guard !connected, !busy, !restoreAttempted, rememberPassword else { return }
        restoreAttempted = true
        await connectWithRememberedPassword()
    }

    var hasRememberedPassword: Bool {
        guard rememberPassword, let url = try? Self.serviceURL(address) else { return false }
        return passwords.load(url.absoluteString) != nil
    }

    func connectWithRememberedPassword() async {
        guard rememberPassword else { return }
        guard let url = try? Self.serviceURL(address), let password = passwords.load(url.absoluteString) else { return }
        await connect(password: password, restoring: true)
    }

    func connect(password: String, restoring: Bool = false) async {
        guard !busy else { return }
        busy = true
        defer { busy = false }
        let expected = generation
        let destination = try? Self.serviceURL(address)
        do {
            guard let url = destination else { throw Failure.address }
            let opened = try await openSession(at: url, password: password)
            guard expected == generation else { throw Failure.expired }
            base = url; token = opened.token; capabilities = opened.capabilities; capabilitiesFetchedAt = Date()
            connected = true; restoreAttempted = true; unreachable = false; pollMessage = nil
            defaults.set(url.absoluteString, forKey: "buildServiceURL")
            let saved = !rememberPassword || passwords.save(url.absoluteString, password)
            message = saved ? "Connected." : "Connected, but the password could not be saved in this device's Keychain."
            await refresh()
        } catch Failure.rejected(401) {
            guard expected == generation else { return }
            if restoring, let url = destination { passwords.delete(url.absoluteString) }
            message = Failure.rejected(401).localizedDescription
        } catch { if expected == generation { message = error.localizedDescription } }
    }

    func disconnect() async {
        let previous = base, credential = token
        if let origin = previous ?? (try? Self.serviceURL(address)) { passwords.delete(origin.absoluteString) }
        restoreAttempted = true
        generation = UUID(); connected = false; token = nil; base = nil; capabilities = []
        state = nil; artifacts = []; traces = []; crashLogs = []; detail = ""; reportDetail = ""; reportDetailOwnerBundleId = nil; reportDetailID = nil; message = "Disconnected."
        if let previous, let credential {
            let _: OK? = try? await request("api/service/v1/disconnect", at: previous, token: credential, body: [:])
        }
    }

    /// Only the projects, devices and jobs: cheap enough to poll while a build runs.
    func refreshState() async {
        guard connected else { return }
        do {
            state = try await authenticated("state")
            if pollSucceeded() { await refreshCapabilities() }
        } catch { pollFailed(error) }
    }

    /// What the service can do, asked again: it may have been restarted as a newer version since the
    /// session opened (a restart that keeps sessions never shows up as a 401). Without `force`, at
    /// most once a minute.
    func refreshCapabilities(force: Bool = true) async {
        guard connected, force || Date().timeIntervalSince(capabilitiesFetchedAt) > 60 else { return }
        guard let reply: Advertisement = try? await authenticated("capabilities") else { return }
        capabilities = Set(reply.capabilities ?? [])
        capabilitiesFetchedAt = Date()
    }

    private func pollFailed(_ error: Error) {
        unreachable = true
        pollMessage = error.localizedDescription
        message = error.localizedDescription
    }

    /// Notes that the service answered. A message an earlier failed poll left is not true any more.
    /// Returns whether the service had been unreachable.
    @discardableResult
    private func pollSucceeded() -> Bool {
        let recovered = unreachable
        unreachable = false
        if let pollMessage, message == pollMessage { message = "" }
        pollMessage = nil
        return recovered
    }

    /// The first request after a connection is being restored, before there is anything to show.
    var isRestoring: Bool { !connected && (busy || (rememberPassword && !restoreAttempted && hasRememberedPassword)) }

    func refresh() async {
        guard connected else { return }
        do {
            let next: State = try await authenticated("state")
            state = next
            pollSucceeded()
            await refreshCapabilities(force: false)
            let builds: Artifacts = try await authenticated("artifacts")
            artifacts = builds.builds
            let submitted: Traces = try await authenticated("traces")
            traces = submitted.traces
            let crashes: CrashLogs = try await authenticated("crashlogs")
            crashLogs = crashes.crashLogs
        } catch { pollFailed(error) }
    }

    /// Downloads one immutable Apparition Store artifact with the service bearer. The caller hands this
    /// IPA to LibraryModel, which keeps it and runs the usual signing and install review.
    func download(_ artifact: Artifact) async throws -> URL {
        guard let base, let token, connected else { throw Failure.expired }
        guard let id = artifact.buildId.addingPercentEncoding(withAllowedCharacters: .alphanumerics.union(CharacterSet(charactersIn: "-"))),
              var components = URLComponents(url: base, resolvingAgainstBaseURL: false) else { throw Failure.response }
        let prefix = components.percentEncodedPath.hasSuffix("/") ? components.percentEncodedPath : components.percentEncodedPath + "/"
        components.percentEncodedPath = prefix + "api/service/v1/artifacts/\(id)/download"
        guard let url = components.url else { throw Failure.response }
        var request = URLRequest(url: url)
        request.setValue("application/octet-stream", forHTTPHeaderField: "Accept")
        stamp(&request, token: token)
        let (temporary, response) = try await session.download(for: request)
        guard let response = response as? HTTPURLResponse else { throw Failure.response }
        guard response.statusCode == 200 else { throw Failure.rejected(response.statusCode) }
        let values = try temporary.resourceValues(forKeys: [.fileSizeKey])
        guard let size = values.fileSize, size > 0, size <= 2_000_000_000 else { throw Failure.response }
        let destination = FileManager.default.temporaryDirectory.appendingPathComponent("store-\(id).ipa")
        try? FileManager.default.removeItem(at: destination)
        try FileManager.default.moveItem(at: temporary, to: destination)
        return destination
    }

    func matchingProject(for bundleId: String, deviceFamily: [Int]?, preferredDevice: String?) -> (project: Project, target: Target)? {
        let matches = (state?.repos ?? []).flatMap { project in
            project.platforms.filter { $0.bundleId == bundleId }.map { (project, $0) }
        }
        if let preferredDevice,
           let local = matches.first(where: { $0.1.device.caseInsensitiveCompare(preferredDevice) == .orderedSame }) {
            return local
        }
        let preferred = ThisDevice.preferredBuildPlatforms(deviceFamily: deviceFamily)
        for platform in preferred {
            if let match = matches.first(where: { $0.1.platform.lowercased() == platform }) { return match }
        }
        return matches.first
    }

    func artifact(for bundleId: String, installedBuild: String?) -> Artifact? {
        let candidates = artifacts.filter { $0.app.bundleId == bundleId || $0.app.originalBundleId == bundleId }
        if let installedBuild, let exact = candidates.first(where: { $0.app.build == installedBuild }) { return exact }
        return candidates.first
    }

    func latestArtifact(project: Project, target: Target, bundleId: String) -> Artifact? {
        artifacts.filter {
            ($0.app.bundleId == bundleId || $0.app.originalBundleId == bundleId) &&
            ($0.project == nil || $0.project == project.name) &&
            $0.platform.caseInsensitiveCompare(target.platform) == .orderedSame
        }.sorted { ($0.createdAt ?? "") > ($1.createdAt ?? "") }.first
    }

    func traces(for bundleId: String, installedBuild: String?) -> [Trace] {
        let candidates = traces.filter { $0.app.bundleId == bundleId }
        guard let installedBuild else { return candidates }
        return candidates.filter { $0.app.build == installedBuild }
    }

    func crashLogs(for bundleId: String, installedBuild: String?) -> [CrashLog] {
        let candidates = crashLogs.filter { $0.bundleId == bundleId }
        guard let installedBuild else { return candidates }
        return candidates.filter { $0.build == installedBuild }
    }

    func readCrash(_ crash: CrashLog) async {
        let id = "crash:\(crash.id)"
        guard reportDetailID != id else { closeReport(); return }
        reportDetailID = id; reportDetailOwnerBundleId = crash.bundleId
        guard crash.symbolicated != nil else { reportDetail = "No symbolicated report is available."; return }
        reportDetail = "Loading report…"
        do {
            let text = try await authenticatedText("crashlogs/\(crash.id)/symbolicated")
            if reportDetailID == id { reportDetail = text }
        } catch { if reportDetailID == id { reportDetail = error.localizedDescription } }
    }

    func readTraceFile(_ trace: Trace, file: Trace.File) async {
        let id = "trace:\(trace.id):\(file.path)"
        guard reportDetailID != id else { closeReport(); return }
        reportDetailID = id; reportDetailOwnerBundleId = trace.app.bundleId
        reportDetail = "Loading report…"
        let encoded = file.path.split(separator: "/", omittingEmptySubsequences: false)
            .map { String($0).addingPercentEncoding(withAllowedCharacters: .alphanumerics.union(CharacterSet(charactersIn: "-._~"))) ?? "" }
            .joined(separator: "/")
        do {
            let text = try await authenticatedText("traces/\(trace.id)/files/\(encoded)")
            if reportDetailID == id { reportDetail = text }
        } catch { if reportDetailID == id { reportDetail = error.localizedDescription } }
    }

    func closeReport() {
        reportDetailID = nil; reportDetailOwnerBundleId = nil; reportDetail = ""
    }

    private func authenticatedText(_ path: String) async throws -> String {
        guard let base, let token, connected else { throw Failure.expired }
        let expected = generation
        let text = try await requestText("api/service/v1/" + path, at: base, token: token)
        guard expected == generation else { throw Failure.expired }
        return text
    }

    private func requestText(_ path: String, at base: URL, token: String) async throws -> String {
        guard var components = URLComponents(url: base, resolvingAgainstBaseURL: false) else { throw Failure.address }
        let prefix = components.percentEncodedPath.hasSuffix("/") ? components.percentEncodedPath : components.percentEncodedPath + "/"
        components.percentEncodedPath = prefix + path
        guard let url = components.url else { throw Failure.address }
        var request = URLRequest(url: url)
        request.setValue("text/plain", forHTTPHeaderField: "Accept")
        stamp(&request, token: token)
        let (bytes, response) = try await session.bytes(for: request)
        guard let response = response as? HTTPURLResponse, response.statusCode == 200 else { throw Failure.response }
        let limit = 8 * 1024 * 1024
        guard response.expectedContentLength <= limit else { throw Failure.response }
        var data = Data()
        for try await byte in bytes {
            guard data.count < limit else { throw Failure.response }
            data.append(byte)
        }
        guard let text = String(data: data, encoding: .utf8) else { throw Failure.response }
        return text
    }

    /// What the service installed for this target, if anything.
    func install(for target: Target) -> Install? {
        guard let bundleId = target.bundleId else { return nil }
        return state?.installs?[target.device]?[bundleId]
    }

    func device(named name: String) -> Device? { state?.devices?.list.first { $0.name == name } }

    /// Kept builds of a project's apps, newest first.
    func artifacts(for project: Project) -> [Artifact] {
        let ids = Set(project.platforms.compactMap(\.bundleId))
        return artifacts.filter { ids.contains($0.app.bundleId) || $0.app.originalBundleId.map(ids.contains) == true }
            .sorted { ($0.createdAt ?? "") > ($1.createdAt ?? "") }
    }

    func jobs(for project: Project) -> [Job] { (state?.builds ?? []).filter { $0.repo == project.name } }

    func reportCount(for project: Project) -> (crashes: Int, traces: Int) {
        let ids = Set(project.platforms.compactMap(\.bundleId))
        return (crashLogs.filter { ids.contains($0.bundleId) }.count, traces.filter { ids.contains($0.app.bundleId) }.count)
    }

    /// The project a configured bundle ID belongs to.
    func project(named name: String) -> Project? { state?.repos.first { $0.name == name } }

    /// Asks the Mac to build a target. `install` says how the build reaches the device: `.device`
    /// builds and keeps it for Apparition on the device to install (no Mac-to-device route needed),
    /// `.mac` installs through the Xcode tunnel, `.none` only builds. Returns the queued job.
    @discardableResult
    func build(project: Project, target: Target, branch: String, install: InstallMode, launch: Bool, debug: Bool, patch: Bool, injectBackend: String = "dynamic") async -> Job? {
        guard !busy else { return nil }
        busy = true
        defer { busy = false }
        do {
            // `deploy` is what a service without install modes understands; it reads `install` when it can.
            let reply: BuildReply = try await authenticated("build", body: ["repo": project.name, "device": target.device,
                "targetPlatform": target.platform, "branch": branch, "install": install.rawValue, "deploy": install != .none,
                "run": install == .mac && launch,
                "mcp": debug, "inject": patch, "injectBackend": injectBackend, "buildDebug": false])
            message = reply.jobs.first.map { "Queued build #\($0.id)." } ?? "Build queued."
            await refresh()
            return reply.jobs.first
        } catch { message = error.localizedDescription; return nil }
    }

    /// Polls until the job has finished (or the connection is lost, or the task is cancelled), and
    /// returns it as the service last described it.
    func finished(_ job: Job, every interval: Duration = .seconds(2), upTo limit: Duration = .seconds(3600)) async -> Job? {
        let deadline = ContinuousClock.now + limit
        while ContinuousClock.now < deadline, !Task.isCancelled, connected {
            await refreshState()
            if let current = state?.builds.first(where: { $0.id == job.id }), !current.active {
                await refresh()
                return current
            }
            try? await Task.sleep(for: interval)
        }
        return nil
    }

    /// Whether the connected service can hand builds to the device's own Apparition and takes its
    /// word that it installed one (`install: device`, `POST /installs`).
    var installsOnDevice: Bool { connected && capabilities.contains("install-modes") && capabilities.contains("report-install") }

    private struct InstallReport: Decodable { let ok: Bool }

    /// Tells the service this device installed a kept build, so its ledger (and every other client)
    /// shows it. Best effort: an install that happened is not undone by the service not hearing of it.
    @discardableResult
    func reportInstall(target: Target, buildId: String) async -> Bool {
        guard installsOnDevice, let bundleId = target.bundleId else { return false }
        do {
            let reply: InstallReport = try await authenticated("installs", body: ["device": target.device,
                "bundleId": bundleId, "buildId": buildId, "platform": target.platform])
            await refreshState()
            return reply.ok
        } catch { message = error.localizedDescription; return false }
    }

    /// The kept build with this identity, from the last listing.
    func artifact(buildId: String) -> Artifact? { artifacts.first { $0.buildId == buildId } }

    // MARK: Re-sign

    var canResign: Bool { connected && capabilities.contains("resign") }
    /// The service can build a target first when it kept no build to re-sign.
    var canResignByBuilding: Bool { canResign && capabilities.contains("resign-build") }

    /// What a re-sign signs: a kept IPA, or a fresh build of a project target.
    enum ResignSource {
        case kept(Artifact)
        case build(Project, Target)
    }

    /// How to re-sign the app this target installs, if the service can: the exact build on the
    /// device when the Mac kept it, otherwise a fresh build of the target.
    func resignSource(for project: Project, target: Target) -> ResignSource? {
        guard canResign, target.platform.caseInsensitiveCompare("macOS") != .orderedSame else { return nil }
        if let id = install(for: target)?.buildId, let kept = artifacts.first(where: { $0.buildId == id }) { return .kept(kept) }
        return canResignByBuilding ? .build(project, target) : nil
    }

    /// The devices a project installs this build's app on, so only those are offered.
    func devices(for artifact: Artifact) -> [String] {
        var names: [String] = []
        for target in (state?.repos ?? []).flatMap(\.platforms)
        where target.bundleId == artifact.app.bundleId && target.platform.caseInsensitiveCompare(artifact.platform) == .orderedSame
            && !names.contains(target.device) { names.append(target.device) }
        return names
    }

    /// The Mac's unexpired profiles. Empty on failure, with `message` saying why.
    func resignProfiles() async -> [ResignProfile] {
        do { let reply: ResignOptions = try await authenticated("resign/options"); return reply.profiles.filter { !$0.expired } }
        catch { message = error.localizedDescription; return [] }
    }

    /// Asks the Mac to re-sign an app on a device. The Mac signs the IPA (the one it kept, or one
    /// it builds first) with the chosen profile and installs it over the app; if the device refuses
    /// an update to another App ID it backs the data up, reinstalls and restores it (lib/resign.js
    /// in Apparition Store). `dryRun` only checks the data can be reached. Returns the re-sign job, which
    /// is the last one when a build is queued before it.
    func resign(_ source: ResignSource, device: String, profile: ResignProfile, keepData: Bool, dryRun: Bool) async -> Job? {
        guard !busy else { return nil }
        busy = true
        defer { busy = false }
        var body: [String: Any] = ["device": device, "profile": profile.id, "dataMode": keepData ? "keep" : "discard", "dryRun": dryRun]
        switch source {
        case .kept(let artifact): body["buildId"] = artifact.buildId
        case .build(let project, let target): body["repo"] = project.name; body["platform"] = target.platform
        }
        do {
            let reply: BuildReply = try await authenticated("resign", body: body)
            let job = reply.jobs.last
            if reply.jobs.count > 1, let first = reply.jobs.first, let job {
                message = "Queued build #\(first.id), then re-sign #\(job.id)."
            } else {
                message = job.map { "Queued \(dryRun ? "check" : "re-sign") #\($0.id)." } ?? "Queued."
            }
            await refresh()
            return job
        } catch { message = error.localizedDescription; return nil }
    }

    func log(_ job: Job) async {
        do { let reply: Log = try await authenticated("builds/\(job.id)/log"); detail = reply.chunk }
        catch { message = error.localizedDescription }
    }

    func cancel(_ job: Job) async {
        do { let _: OK = try await authenticated("builds/\(job.id)/cancel", body: [:]); await refresh() }
        catch { message = error.localizedDescription }
    }
}
