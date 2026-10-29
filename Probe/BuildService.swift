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
    }
    struct Project: Decodable, Identifiable {
        var id: String { name }
        let name: String
        let branches: [String]?
        let platforms: [Target]
    }
    struct Job: Decodable, Identifiable {
        let id: Int
        let repo: String
        let platform: String
        let device: String?
        let status: String
        let buildId: String?
        var cancellable: Bool { status == "queued" || status == "running" }
    }
    struct State: Decodable {
        let host: String?
        let repos: [Project]
        let builds: [Job]
    }
    struct Artifact: Decodable, Identifiable {
        struct App: Decodable { let name: String?; let bundleId: String; let build: String? }
        var id: String { buildId }
        let buildId: String
        let platform: String
        let app: App
    }
    private struct Artifacts: Decodable { let builds: [Artifact] }
    private struct Connection: Decodable {
        let `protocol`: String
        let version: Int
        let token: String
    }
    private struct BuildReply: Decodable { let jobs: [Job] }
    private struct Log: Decodable { let chunk: String }
    private struct OK: Decodable { let ok: Bool }
    enum Failure: LocalizedError {
        case address, response, rejected(Int), expired
        var errorDescription: String? {
            switch self {
            case .address: "Use an HTTPS service address, or HTTP on localhost. Do not include credentials, a path or query."
            case .response: "The service returned an unsupported or oversized response."
            case .rejected(let status): status == 401 ? "Connect to the build service again." : "The service refused the request (HTTP \(status))."
            case .expired: "The connection changed; try again."
            }
        }
    }

    var address: String
    private(set) var connected = false
    private(set) var busy = false
    private(set) var state: State?
    private(set) var artifacts: [Artifact] = []
    private(set) var detail = ""
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

    private func request<T: Decodable>(_ path: String, at base: URL, token: String? = nil,
                                       body: [String: Any]? = nil) async throws -> T {
        var request = URLRequest(url: base.appendingPathComponent(path))
        request.setValue("application/json", forHTTPHeaderField: "Accept")
        if let token { request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization") }
        if let body {
            request.httpMethod = "POST"
            request.setValue("application/json", forHTTPHeaderField: "Content-Type")
            request.httpBody = try JSONSerialization.data(withJSONObject: body)
        }
        let (bytes, response) = try await session.bytes(for: request)
        guard let response = response as? HTTPURLResponse else { throw Failure.response }
        guard response.statusCode == 200 else { throw Failure.rejected(response.statusCode) }
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
            do { refreshed = try await openSession(at: base, password: password) }
            catch Failure.rejected(401) {
                guard expected == generation else { throw Failure.expired }
                passwords.delete(base.absoluteString); connected = false; self.token = nil
                throw Failure.rejected(401)
            }
            guard expected == generation else { throw Failure.expired }
            self.token = refreshed
            reply = try await request("api/service/v1/" + path, at: base, token: refreshed, body: body)
        }
        guard expected == generation else { throw Failure.expired }
        return reply
    }

    private func openSession(at url: URL, password: String) async throws -> String {
        let reply: Connection = try await request("api/service/connect", at: url, body: ["password": password])
        guard reply.protocol == "apparition-service", reply.version == 1,
              reply.token.count == 43, reply.token.allSatisfy({ $0.isASCII && ($0.isLetter || $0.isNumber || $0 == "_" || $0 == "-") }) else { throw Failure.response }
        return reply.token
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
        do {
            let url = try Self.serviceURL(address)
            let credential = try await openSession(at: url, password: password)
            guard expected == generation else { throw Failure.expired }
            base = url; token = credential; connected = true; restoreAttempted = true
            defaults.set(url.absoluteString, forKey: "buildServiceURL")
            let saved = !rememberPassword || passwords.save(url.absoluteString, password)
            message = saved ? "Connected." : "Connected, but the password could not be saved in this device's Keychain."
            await refresh()
        } catch Failure.rejected(401) {
            if restoring, let url = try? Self.serviceURL(address) { passwords.delete(url.absoluteString) }
            message = Failure.rejected(401).localizedDescription
        } catch { message = error.localizedDescription }
    }

    func disconnect() async {
        let previous = base, credential = token
        if let origin = previous ?? (try? Self.serviceURL(address)) { passwords.delete(origin.absoluteString) }
        restoreAttempted = true
        generation = UUID(); connected = false; token = nil; base = nil
        state = nil; artifacts = []; detail = ""; message = "Disconnected."
        if let previous, let credential {
            let _: OK? = try? await request("api/service/v1/disconnect", at: previous, token: credential, body: [:])
        }
    }

    func refresh() async {
        guard connected else { return }
        do {
            let next: State = try await authenticated("state")
            state = next
            let builds: Artifacts = try await authenticated("artifacts")
            artifacts = builds.builds
        } catch { message = error.localizedDescription }
    }

    func build(project: Project, target: Target, branch: String, deploy: Bool, launch: Bool, debug: Bool, patch: Bool) async {
        guard !busy else { return }
        busy = true
        defer { busy = false }
        do {
            let reply: BuildReply = try await authenticated("build", body: ["repo": project.name, "device": target.device,
                "targetPlatform": target.platform, "branch": branch, "deploy": deploy, "run": deploy && launch,
                "mcp": debug, "inject": patch, "buildDebug": false])
            message = reply.jobs.first.map { "Queued build #\($0.id)." } ?? "Build queued."
            await refresh()
        } catch { message = error.localizedDescription }
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
