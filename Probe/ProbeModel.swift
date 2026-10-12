import DebugTrace
import Darwin
import Foundation
import Network
import Observation

private let log = DebugLogger(subsystem: Bundle.main.bundleIdentifier ?? "Apparition", category: "Probe")

/// Spike 1: can an app pair with, and open an RSD tunnel to, the device it runs on?
@MainActor @Observable
final class ProbeModel {
    struct Advertised: Codable, Identifiable, Sendable {
        var id: String { "\(type)|\(name)" }
        let name: String
        let type: String
        var port: Int?
        var addresses: [String]
        var isSelf: Bool
        var txt: [String: String]
    }

    struct Reachability: Codable, Sendable {
        let host: String
        let port: Int
        let result: String
        let elapsedMs: Int
    }

    struct Attempt: Codable, Sendable {
        let action: String
        let host: String
        let port: Int
        let startedAt: String
        var finishedAt: String?
        var outcome: String?
        var peer: IDevice.PeerDevice?
        var deviceUUID: String?
        var services: [IDevice.Service]?
        var detail: String?
        var events: [String]?
    }

    nonisolated static let manualPairingType = "_remotepairing-manual-pairing._tcp"
    nonisolated static let pairedType = "_remotepairing._tcp"
    nonisolated static let hostname = "Apparition"
    /// LocalDevVPN routes this address back to the device's own services.
    nonisolated static let vpnLoopback = "10.7.0.1"
    /// Pass as `host` to go through the Apparition Companion relay instead.
    nonisolated static let relayHost = "relay"

    /// The Apparition Companion relay ("ip:port"), or nil to use LocalDevVPN.
    var relay: String? = UserDefaults.standard.string(forKey: "relay") {
        didSet { UserDefaults.standard.set(relay, forKey: "relay") }
    }

    /// Where unattended work (installs, refreshes) opens its tunnel: the relay when one is set.
    var defaultTunnelHost: String { relay == nil ? Self.vpnLoopback : Self.relayHost }

    /// This device's own RemotePairing port: always 49152 on visionOS. iPhone and iPad pick
    /// another one each boot, so it's the one Bonjour last reported for this device.
    var defaultTunnelPort: Int {
        #if os(visionOS)
        49152
        #else
        UserDefaults.standard.object(forKey: Self.selfTunnelPortKey) as? Int ?? 49152
        #endif
    }

    nonisolated static let selfTunnelPortKey = "selfTunnelPort"

    func tunnelTarget(host: String, port: Int) -> IDevice.Target {
        if host == Self.relayHost, let relay {
            return IDevice.Target(host: "0.0.0.0", port: UInt16(port), relay: relay)
        }
        return IDevice.Target(host: host, port: UInt16(port), relay: nil)
    }

    private(set) var advertised: [String: Advertised] = [:]
    private(set) var browserStates: [String: String] = [:]
    private(set) var interfaces: [String] = []
    private(set) var reachability: [Reachability] = []
    private(set) var attempts: [Attempt] = []
    private(set) var busy = false
    private(set) var pinRequested = false
    private(set) var hasPairingFile = false
    /// Every user app on this headset at the last listApps, whoever installed it.
    private(set) var deviceApps: [DeviceApp] = []
    private(set) var deviceAppsFetchedAt: String?

    struct DeviceApp: Codable, Sendable, Identifiable, Hashable {
        var id: String { bundleId }
        let bundleId: String
        let name: String
        let version: String?
        /// Signed with get-task-allow, so it can be launched with JIT.
        let debuggable: Bool
        let teamId: String?
        let signer: String?
    }

    private struct StoredApps: Codable {
        let fetchedAt: String
        let apps: [DeviceApp]
    }

    private static var deviceAppsURL: URL { AccountModel.dataDir.appendingPathComponent("device-apps.json") }

    private final class AppsBox: @unchecked Sendable {
        var apps: [DeviceApp]?
    }

    private var browsers: [NWBrowser] = []
    private var resolvers: [String: ServiceResolver] = [:]
    private let pinBox = PinBox()

    private var pairingFileURL: URL {
        FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("apparition_rp_pairing.plist")
    }

    init() {
        let documents = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
        IDevice.startLogging(to: documents.appendingPathComponent("idevice.log"))
        hasPairingFile = FileManager.default.fileExists(atPath: pairingFileURL.path)
        loadDeviceApps()
        pinBox.onRequest = { [weak self] in
            Task { @MainActor in self?.pinRequested = true }
        }
    }

    // MARK: Discovery

    func startBrowsing() {
        browsers.forEach { $0.cancel() }
        browsers = []
        advertised = [:]
        interfaces = Self.interfaceAddresses()
        for type in [Self.manualPairingType, Self.pairedType] {
            let browser = NWBrowser(for: .bonjourWithTXTRecord(type: type, domain: "local."), using: .tcp)
            browser.stateUpdateHandler = { [weak self] state in
                Task { @MainActor in
                    self?.browserStates[type] = "\(state)"
                    log.info("browser \(type, privacy: .public): \(String(describing: state), privacy: .public)")
                }
            }
            browser.browseResultsChangedHandler = { [weak self] results, _ in
                Task { @MainActor in self?.update(type: type, results: results) }
            }
            browser.start(queue: .main)
            browsers.append(browser)
        }
    }

    private func update(type: String, results: Set<NWBrowser.Result>) {
        var seen = Set<String>()
        for result in results {
            guard case let .service(name, _, domain, _) = result.endpoint else { continue }
            var txt: [String: String] = [:]
            if case let .bonjour(record) = result.metadata {
                for (key, entry) in record.dictionary { txt[key] = entry }
            }
            let key = "\(type)|\(name)"
            seen.insert(key)
            if advertised[key] == nil {
                advertised[key] = Advertised(name: name, type: type, port: nil, addresses: [], isSelf: false, txt: txt)
                resolve(key: key, name: name, type: type, domain: domain)
            } else {
                advertised[key]?.txt = txt
            }
        }
        for key in advertised.keys where key.hasPrefix(type + "|") && !seen.contains(key) {
            advertised[key] = nil
            log.info("service gone: \(key, privacy: .private)")
        }
    }

    private func resolve(key: String, name: String, type: String, domain: String) {
        let resolver = ServiceResolver(name: name, type: type + ".", domain: domain) { [weak self] port, addresses in
            Task { @MainActor in
                guard let self else { return }
                let own = Set(Self.interfaceAddresses().map { $0.components(separatedBy: " ").last ?? $0 })
                self.advertised[key]?.port = port
                self.advertised[key]?.addresses = addresses
                self.advertised[key]?.isSelf = addresses.contains { own.contains($0) }
                if type == Self.pairedType, self.advertised[key]?.isSelf == true, port > 0 {
                    UserDefaults.standard.set(port, forKey: Self.selfTunnelPortKey)
                }
                self.resolvers[key] = nil
                log.info("resolved \(type, privacy: .public) port \(port, privacy: .public), \(addresses.count, privacy: .public) addresses")
            }
        }
        resolvers[key] = resolver
        resolver.start()
    }

    /// Hosts worth trying for a service: what it advertised, loopback, and the LocalDevVPN address.
    func candidateHosts(for service: Advertised) -> [String] {
        var hosts = service.addresses
        for extra in ["127.0.0.1", "::1", Self.vpnLoopback] where !hosts.contains(extra) { hosts.append(extra) }
        return hosts
    }

    // MARK: Reachability

    func scan() async {
        reachability = []
        let targets = advertised.values.compactMap { service -> [(String, Int)]? in
            guard let port = service.port else { return nil }
            return candidateHosts(for: service).map { ($0, port) }
        }.flatMap { $0 }
        for (host, port) in targets {
            reachability.append(await Self.probeTCP(host: host, port: port))
        }
    }

    private static func probeTCP(host: String, port: Int) async -> Reachability {
        let started = Date()
        let connection = NWConnection(host: NWEndpoint.Host(host), port: NWEndpoint.Port(rawValue: UInt16(port))!, using: .tcp)
        let result: String = await withCheckedContinuation { continuation in
            let once = OnceFlag()
            @Sendable func finish(_ text: String) {
                guard once.claim() else { return }
                connection.cancel()
                continuation.resume(returning: text)
            }
            connection.stateUpdateHandler = { state in
                switch state {
                case .ready: finish("connected")
                case .failed(let error): finish("failed: \(error)")
                case .waiting(let error): finish("waiting: \(error)")
                default: break
                }
            }
            connection.start(queue: .global())
            DispatchQueue.global().asyncAfter(deadline: .now() + 4) { finish("timeout") }
        }
        return Reachability(host: host, port: port, result: result,
                            elapsedMs: Int(Date().timeIntervalSince(started) * 1000))
    }

    // MARK: Pairing and tunnel

    func submitPin(_ pin: String) {
        pinRequested = false
        pinBox.submit(pin.trimmingCharacters(in: .whitespacesAndNewlines))
    }

    func cancelPin() {
        pinRequested = false
        pinBox.submit(nil)
    }

    func forgetPairing() {
        try? FileManager.default.removeItem(at: pairingFileURL)
        hasPairingFile = false
    }

    func pair(host: String, port: Int) async {
        if host == Self.relayHost, let relay {
            await run(action: "pair", host: host, port: port) { file, box in
                try IDevice.pairViaRelay(relay, port: UInt16(port), hostname: Self.hostname, file: file,
                                         pin: PinBox.callback, context: box)
                return { _ in }
            }
            return
        }
        await run(action: "pair", host: host, port: port) { file, box in
            let peer = try IDevice.pair(host: host, port: UInt16(port), hostname: Self.hostname, file: file,
                                        pin: PinBox.callback, context: box)
            return { attempt in attempt.peer = peer }
        }
    }

    func tunnel(host: String, port: Int) async {
        let target = tunnelTarget(host: host, port: port)
        await run(action: "tunnel", host: host, port: port) { file, box in
            let (uuid, services) = try IDevice.tunnelServices(target: target, hostname: Self.hostname,
                                                              file: file, pin: PinBox.callback, context: box)
            return { attempt in
                attempt.deviceUUID = uuid
                attempt.services = services
            }
        }
    }

    /// Reads the device's UDID and name through the tunnel and saves them for signing.
    @discardableResult
    func identify(host: String, port: Int) async -> String {
        let target = tunnelTarget(host: host, port: port)
        return await run(action: "identify", host: host, port: port) { file, box in
            let peer = try IDevice.identify(target: target, hostname: Self.hostname, file: file,
                                            pin: PinBox.callback, context: box)
            return { attempt in attempt.peer = peer }
        }
    }

    /// Returns the attempt's outcome: "ok", or "error: ..." (or "busy").
    @discardableResult
    /// "ok", "queued" (with `detachAfterMs`: installd holds the update, as for Apparition
    /// itself until it closes), or what failed.
    func install(host: String, port: Int, appPath: String, sourceIPA: String? = nil, asIPA: Bool = true,
                 detachAfterMs: UInt64 = 0) async -> String {
        let target = tunnelTarget(host: host, port: port)
        let outcome = await run(action: "install", host: host, port: port) { file, box in
            let reply = try IDevice.install(target: target, hostname: Self.hostname, file: file,
                                            pin: PinBox.callback, context: box, appPath: appPath,
                                            sourceIPA: sourceIPA, asIPA: asIPA, detachAfterMs: detachAfterMs)
            guard reply["ok"] as? Bool == true else {
                throw IDevice.ProbeError(reply["error"] as? String ?? "install failed")
            }
            let elapsed = reply["elapsedMs"] as? Int ?? 0
            var detail = reply["queued"] as? Bool == true ? "queued after \(elapsed) ms" : "installed in \(elapsed) ms"
            if let bytes = reply["bytes"] as? Int {
                detail += String(format: " (packed %.0f MB in %d ms, %d of %d files reused, upload %d ms)",
                                 Double(bytes) / 1e6, reply["packMs"] as? Int ?? 0, reply["reused"] as? Int ?? 0,
                                 reply["files"] as? Int ?? 0, reply["uploadMs"] as? Int ?? 0)
            }
            log.info("\(detail, privacy: .public)")
            return { attempt in attempt.detail = detail }
        }
        if outcome == "ok", attempts.first?.action == "install", attempts.first?.detail?.hasPrefix("queued") == true {
            return "queued"
        }
        return outcome
    }

    /// Lists the headset's user apps through the tunnel and keeps the list (device-apps.json),
    /// so the Library can show it before the next tunnel opens.
    @discardableResult
    func listApps(host: String, port: Int) async -> String {
        let target = tunnelTarget(host: host, port: port)
        let box = AppsBox()
        let outcome = await run(action: "apps", host: host, port: port) { file, context in
            let reply = try IDevice.listApps(target: target, hostname: Self.hostname, file: file,
                                             pin: PinBox.callback, context: context)
            guard reply["ok"] as? Bool == true, let apps = reply["apps"] else {
                throw IDevice.ProbeError(reply["error"] as? String ?? "app lookup failed")
            }
            box.apps = try JSONDecoder().decode([DeviceApp].self, from: JSONSerialization.data(withJSONObject: apps))
            let detail = "\(box.apps?.count ?? 0) apps, \(box.apps?.filter(\.debuggable).count ?? 0) debuggable"
            return { attempt in attempt.detail = detail }
        }
        if let apps = box.apps {
            deviceApps = apps
            deviceAppsFetchedAt = DebugTime.iso(Date())
            if let data = try? JSONEncoder().encode(StoredApps(fetchedAt: deviceAppsFetchedAt!, apps: apps)) {
                try? data.write(to: Self.deviceAppsURL, options: .atomic)
            }
        }
        return outcome
    }

    private func loadDeviceApps() {
        guard let data = try? Data(contentsOf: Self.deviceAppsURL),
              let stored = try? JSONDecoder().decode(StoredApps.self, from: data) else { return }
        deviceApps = stored.apps
        deviceAppsFetchedAt = stored.fetchedAt
    }

    func launchJIT(host: String, port: Int, bundleId: String, waitSeconds: Int) async {
        let target = tunnelTarget(host: host, port: port)
        await run(action: "jit", host: host, port: port) { file, box in
            let reply = try IDevice.launchJIT(target: target, hostname: Self.hostname, file: file,
                                              pin: PinBox.callback, context: box,
                                              bundleId: bundleId, waitSeconds: UInt32(waitSeconds))
            guard reply["ok"] as? Bool == true else {
                throw IDevice.ProbeError(reply["error"] as? String ?? "JIT launch failed")
            }
            let events = reply["events"] as? [String]
            let detail = "pid \(reply["pid"] ?? "?"), prepared \(reply["preparedBytes"] ?? 0) bytes, detached \(reply["detached"] ?? false)"
            return { attempt in
                attempt.detail = detail
                attempt.events = events
            }
        }
    }

    @discardableResult
    private func run(action: String, host: String, port: Int,
                     _ body: @escaping @Sendable (IDevice.PairingFile, UnsafeMutableRawPointer) throws -> (@Sendable (inout Attempt) -> Void)) async -> String {
        guard !busy else { return "busy" }
        busy = true
        defer { busy = false }
        attempts.insert(Attempt(action: action, host: host, port: port, startedAt: DebugTime.iso(Date())), at: 0)
        log.info("\(action, privacy: .public) start")

        let url = pairingFileURL
        let box = Unmanaged.passUnretained(pinBox).toOpaque()
        let outcome: Result<(@Sendable (inout Attempt) -> Void), Error> = await Task.detached {
            Result {
                let file = try FileManager.default.fileExists(atPath: url.path)
                    ? IDevice.PairingFile.load(Data(contentsOf: url))
                    : IDevice.PairingFile.generate(hostname: Self.hostname)
                // A pair-setup updates the file in place, and a later step can still fail (a
                // tunnel on the wrong port): persist it either way, or the new keys are lost
                // while the device keeps the pairing.
                let apply: @Sendable (inout Attempt) -> Void
                do {
                    apply = try body(file, box)
                } catch {
                    try? file.serialized().write(to: url, options: .completeFileProtection)
                    throw error
                }
                try file.serialized().write(to: url, options: .completeFileProtection)
                return apply
            }
        }.value

        pinRequested = false
        hasPairingFile = FileManager.default.fileExists(atPath: url.path)
        attempts[0].finishedAt = DebugTime.iso(Date())
        switch outcome {
        case .success(let apply):
            apply(&attempts[0])
            attempts[0].outcome = "ok"
            if let peer = attempts[0].peer {
                UserDefaults.standard.set(peer.udid, forKey: "pairedUDID")
                UserDefaults.standard.set(peer.name, forKey: "pairedName")
            }
            log.info("\(action, privacy: .public) ok")
        case .failure(let error):
            attempts[0].outcome = "error: \(error)"
            log.error("\(action, privacy: .public) failed: \(String(describing: error), privacy: .public)")
        }
        return attempts[0].outcome ?? "unknown"
    }

    // MARK: Setup helpers (read by the UI)

    /// Port of the manual-pairing service this device advertises while Settings > General >
    /// Remote Devices is open: what `pair(host: relayHost, port:)` connects to.
    var selfManualPairingPort: Int? {
        advertised.values.first { $0.type == Self.manualPairingType && $0.isSelf && ($0.port ?? 0) > 0 }?.port
    }

    /// The newest attempt of `action` ("pair", "identify", "jit", ...), finished or not.
    func latestAttempt(_ action: String) -> Attempt? {
        attempts.first { $0.action == action }
    }

    // MARK: Helpers

    static func interfaceAddresses() -> [String] {
        var head: UnsafeMutablePointer<ifaddrs>?
        guard getifaddrs(&head) == 0 else { return [] }
        defer { freeifaddrs(head) }
        var out: [String] = []
        var cursor = head
        while let entry = cursor {
            defer { cursor = entry.pointee.ifa_next }
            guard let addr = entry.pointee.ifa_addr else { continue }
            let family = Int32(addr.pointee.sa_family)
            guard family == AF_INET || family == AF_INET6 else { continue }
            if let text = numericHost(addr, length: socklen_t(addr.pointee.sa_len)) {
                out.append("\(String(cString: entry.pointee.ifa_name)) \(text)")
            }
        }
        return out
    }

    nonisolated static func numericHost(_ addr: UnsafePointer<sockaddr>, length: socklen_t) -> String? {
        var buffer = [CChar](repeating: 0, count: Int(NI_MAXHOST))
        guard getnameinfo(addr, length, &buffer, socklen_t(buffer.count), nil, 0, NI_NUMERICHOST) == 0 else { return nil }
        return String(cString: buffer)
    }
}

/// Hands the PIN from the UI (or the debug server) to idevice's blocking C callback.
final class PinBox: @unchecked Sendable {
    var onRequest: (() -> Void)?
    /// Set by callers whose request carries details (2FA); called before waiting.
    var onRequestJSON: ((String) -> Void)?
    private let semaphore = DispatchSemaphore(value: 0)
    private let lock = NSLock()
    /// Owned C string; kept until the next request because idevice reads it after the callback returns.
    private var pin: UnsafeMutablePointer<CChar>?

    func submit(_ value: String?) {
        lock.lock()
        free(pin)
        pin = value.flatMap { strdup($0) }
        lock.unlock()
        semaphore.signal()
    }

    /// Called on idevice's thread; blocks until a PIN arrives or five minutes pass.
    func waitForPin() -> UnsafePointer<CChar>? {
        lock.lock(); free(pin); pin = nil; lock.unlock()
        while semaphore.wait(timeout: .now()) == .success {}  // drop stale submissions
        onRequest?()
        log.info("pin requested")
        guard semaphore.wait(timeout: .now() + 300) == .success else { return nil }
        lock.lock(); defer { lock.unlock() }
        return pin.map { UnsafePointer($0) }
    }

    static let callback: IDevice.PinCallback = { context in
        guard let context else { return nil }
        return Unmanaged<PinBox>.fromOpaque(context).takeUnretainedValue().waitForPin()
    }
}

final class OnceFlag: @unchecked Sendable {
    private let lock = NSLock()
    private var claimed = false
    func claim() -> Bool {
        lock.lock(); defer { lock.unlock() }
        if claimed { return false }
        claimed = true
        return true
    }
}

/// Resolves a Bonjour service to its port and numeric addresses with NetService,
/// which (unlike connecting through NWConnection) doesn't open a connection to it.
final class ServiceResolver: NSObject, NetServiceDelegate, @unchecked Sendable {
    private let service: NetService
    private let completion: (Int, [String]) -> Void

    init(name: String, type: String, domain: String, completion: @escaping (Int, [String]) -> Void) {
        service = NetService(domain: domain, type: type, name: name)
        self.completion = completion
        super.init()
        service.delegate = self
    }

    func start() {
        service.schedule(in: .main, forMode: .common)
        service.resolve(withTimeout: 10)
    }

    func netServiceDidResolveAddress(_ sender: NetService) {
        let addresses = (sender.addresses ?? []).compactMap { data in
            data.withUnsafeBytes { raw -> String? in
                guard let base = raw.bindMemory(to: sockaddr.self).baseAddress else { return nil }
                return ProbeModel.numericHost(base, length: socklen_t(data.count))
            }
        }
        completion(sender.port, Array(Set(addresses)).sorted())
    }

    func netService(_ sender: NetService, didNotResolve errorDict: [String: NSNumber]) {
        completion(-1, [])
    }
}
