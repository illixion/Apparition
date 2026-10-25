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
        var id = UUID()
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
    /// Pass as `host` to go through the companion relay instead.
    nonisolated static let relayHost = "relay"

    /// The companion relay ("ip:port"), or nil to use LocalDevVPN.
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
    /// What stops Apparition reaching this device (no pairing, or one it turned down), if anything.
    private(set) var pairingProblem: PairingRecord.Problem?
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

    private var pairingFileURL: URL { PairingRecord.url }

    init() {
        let documents = AppFolders.documents
        IDevice.startLogging(to: documents.appendingPathComponent("idevice.log"))
        refreshPairing()
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

    /// Deletes the pairing, and the copy kept in the Keychain.
    func forgetPairing() {
        PairingRecord.delete()
        refreshPairing()
    }

    /// Restores the pairing from the Keychain if the file is gone (or keeps the Keychain copy up
    /// to date), and reads what's wrong with it. At launch, when the app comes to the front (a
    /// computer may have handed a new one over) and around every action that needs it.
    func refreshPairing() {
        pairingProblem = PairingRecord.sync()
        hasPairingFile = PairingRecord.exists
    }

    func pair(host: String, port: Int) async {
        if host == Self.relayHost, let relay {
            await run(action: "pair", host: host, port: port, needsPairing: false) { file, box in
                try IDevice.pairViaRelay(relay, port: UInt16(port), hostname: Self.hostname, file: file,
                                         pin: PinBox.callback, context: box)
                return { _ in }
            }
            return
        }
        await run(action: "pair", host: host, port: port, needsPairing: false) { file, box in
            let peer = try IDevice.pair(host: host, port: UInt16(port), hostname: Self.hostname, file: file,
                                        pin: PinBox.callback, context: box)
            return { attempt in attempt.peer = peer }
        }
    }

    func tunnel(host: String, port: Int) async {
        let target = tunnelTarget(host: host, port: port)
        // Diagnostics: the one tunnel that may pair-set-up (asking for the PIN) when there's no
        // pairing yet or the device turns the saved one down.
        await run(action: "tunnel", host: host, port: port, needsPairing: false) { file, box in
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
                                            pin: nil, context: box)
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
                                            pin: nil, context: box, appPath: appPath,
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
                                             pin: nil, context: context)
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

    /// Deletes `bundleId` and its data from the device through the tunnel, and drops it from
    /// the kept app list. Returns "ok" or what failed.
    @discardableResult
    func uninstall(host: String, port: Int, bundleId: String) async -> String {
        let target = tunnelTarget(host: host, port: port)
        let outcome = await run(action: "uninstall", host: host, port: port) { file, box in
            let reply = try IDevice.uninstall(target: target, hostname: Self.hostname, file: file,
                                              pin: nil, context: box, bundleId: bundleId)
            guard reply["ok"] as? Bool == true else {
                throw IDevice.ProbeError(reply["error"] as? String ?? "uninstall failed")
            }
            return { _ in }
        }
        if outcome == "ok" {
            deviceApps.removeAll { $0.bundleId == bundleId }
            if let fetchedAt = deviceAppsFetchedAt,
               let data = try? JSONEncoder().encode(StoredApps(fetchedAt: fetchedAt, apps: deviceApps)) {
                try? data.write(to: Self.deviceAppsURL, options: .atomic)
            }
        }
        return outcome
    }

    /// What copying one app's data to another's left behind.
    struct DataMove: Sendable {
        let files: Int
        let bytes: Int
        /// Files that didn't arrive whole: the copy is incomplete when there are any.
        let failed: [String]

        var complete: Bool { failed.isEmpty }

        var summary: String {
            "\(files) files, \(ByteCountFormatter.string(fromByteCount: Int64(bytes), countStyle: .file))"
        }
    }

    private final class MoveBox: @unchecked Sendable {
        var move: DataMove?
    }

    /// Copies the data of the installed app `from` to the installed app `to` through the tunnel
    /// (apparition_migrate_container). Changes nothing in `from`. Returns "ok" and what was copied,
    /// or what failed.
    func migrateData(host: String, port: Int, from: String, to: String) async -> (outcome: String, move: DataMove?) {
        let target = tunnelTarget(host: host, port: port)
        let box = MoveBox()
        let outcome = await run(action: "migrate", host: host, port: port) { file, context in
            let reply = try IDevice.migrateContainer(target: target, hostname: Self.hostname, file: file,
                                                     pin: nil, context: context, from: from, to: to)
            guard reply["ok"] as? Bool == true else {
                throw IDevice.ProbeError(reply["error"] as? String ?? "data copy failed")
            }
            let move = DataMove(files: reply["files"] as? Int ?? 0, bytes: reply["bytes"] as? Int ?? 0,
                                failed: reply["failed"] as? [String] ?? [])
            box.move = move
            let detail = "copied \(move.summary) in \(reply["ms"] as? Int ?? 0) ms" + (move.complete ? "" : ", \(move.failed.count) failed")
            log.info("\(detail, privacy: .public)")
            return { attempt in attempt.detail = detail }
        }
        return (outcome, box.move)
    }

    private func loadDeviceApps() {
        guard let data = try? Data(contentsOf: Self.deviceAppsURL),
              let stored = try? JSONDecoder().decode(StoredApps.self, from: data) else { return }
        deviceApps = stored.apps
        deviceAppsFetchedAt = stored.fetchedAt
    }

    /// Apps a debugger is serving right now (launchJIT, attachJIT), by bundle ID. A session
    /// lasts as long as the app wants a debugger, which for Wine (Madeira) is the whole time it
    /// runs, so it doesn't hold `busy`: installs and refreshes go on beside it.
    private(set) var jitSessions: Set<String> = []
    private let keepAlive = JITKeepAlive()
    private let notice = JITSessionNotice()
    /// What the user calls an app (LibraryModel.displayName), for the session notification.
    var appName: (String) -> String = { $0 }

    /// Ends the JIT session serving `bundleId` now: the debugger detaches and the app keeps
    /// running without JIT. From the session notification's Stop, or the library.
    func stopJIT(bundleId: String) {
        let reply = AccountModel.call(apparition_jit_stop(bundleId))
        log.info("jit stop asked: \(reply["stopped"] as? Bool == true ? "stopping" : "no session", privacy: .public)")
    }

    func launchJIT(host: String, port: Int, bundleId: String, waitSeconds: Int) async {
        await serveJIT(host: host, port: port, bundleId: bundleId) { target, file, box in
            try IDevice.launchJIT(target: target, hostname: Self.hostname, file: file,
                                  pin: nil, context: box,
                                  bundleId: bundleId, waitSeconds: UInt32(waitSeconds))
        }
    }

    /// A JIT request from another app (an `enable-jit` link) that failed: what the JIT request
    /// window (visionOS) or an alert shows, since nothing else in view would.
    struct JITFailure: Identifiable, Sendable {
        let id = UUID()
        let bundleId: String
        let message: String
        /// Set when the pairing is what's wrong: Setup is where to go.
        let problem: PairingRecord.Problem?
    }

    var jitFailure: JITFailure?

    /// Takes the running process `pid` of `bundleId` (an app that asked for JIT itself, through
    /// an `enable-jit` link) and serves its JIT requests until it exits or detaches. The app is
    /// brought back to the front once attached.
    func attachJIT(host: String, port: Int, bundleId: String, pid: Int) async {
        await serveJIT(host: host, port: port, bundleId: bundleId, reportsFailure: true) { target, file, box in
            try IDevice.attachJIT(target: target, hostname: Self.hostname, file: file,
                                  pin: nil, context: box,
                                  pid: UInt64(max(pid, 0)), bundleId: bundleId, waitSeconds: 0)
        }
    }

    private func serveJIT(host: String, port: Int, bundleId: String, reportsFailure: Bool = false,
                          _ body: @escaping @Sendable (IDevice.Target, IDevice.PairingFile, UnsafeMutableRawPointer) throws -> [String: Any]) async {
        guard !jitSessions.contains(bundleId) else {
            log.info("jit already serving this app")
            return
        }
        let report = { [weak self] (outcome: String) in
            guard reportsFailure, outcome.hasPrefix("error"), let self else { return }
            self.jitFailure = JITFailure(bundleId: bundleId,
                                         message: outcome.hasPrefix("error: ") ? String(outcome.dropFirst(7)) : outcome,
                                         problem: PairingRecord.problem(in: outcome))
        }
        // No pairing: say so now, without showing a session that can't start.
        refreshPairing()
        if pairingProblem == .missing {
            report(await run(action: "jit", host: host, port: port, exclusive: false) { _, _ in { _ in } })
            return
        }
        let target = tunnelTarget(host: host, port: port)
        jitSessions.insert(bundleId)
        await keepAlive.prepare()
        await notice.prepare()
        notice.onStop = { [weak self] in self?.stopJIT(bundleId: $0) }
        keepAlive.update(active: true)
        notice.show(bundleId: bundleId, name: appName(bundleId))
        defer {
            jitSessions.remove(bundleId)
            notice.remove(bundleId: bundleId)
            keepAlive.update(active: !jitSessions.isEmpty)
        }
        let outcome = await run(action: "jit", host: host, port: port, exclusive: false) { file, box in
            let reply = try body(target, file, box)
            guard reply["ok"] as? Bool == true else {
                throw IDevice.ProbeError(reply["error"] as? String ?? "JIT launch failed")
            }
            let events = reply["events"] as? [String]
            let detail = "pid \(reply["pid"] ?? "?"), prepared \(reply["preparedBytes"] ?? 0) bytes, \(reply["ended"] ?? "ended")"
            return { attempt in
                attempt.detail = detail
                attempt.events = events
            }
        }
        report(outcome)
    }

    /// With `exclusive` (everything but a JIT session), refuses to start while another
    /// exclusive action runs, and holds `busy` meanwhile. With `needsPairing` (everything but
    /// pairing and the diagnostic tunnel), fails at once when there's no pairing, rather than
    /// making up a new one the device has never seen.
    @discardableResult
    private func run(action: String, host: String, port: Int, exclusive: Bool = true, needsPairing: Bool = true,
                     _ body: @escaping @Sendable (IDevice.PairingFile, UnsafeMutableRawPointer) throws -> (@Sendable (inout Attempt) -> Void)) async -> String {
        if exclusive {
            guard !busy else { return "busy" }
            busy = true
        }
        defer { if exclusive { busy = false } }
        let attempt = Attempt(action: action, host: host, port: port, startedAt: DebugTime.iso(Date()))
        attempts.insert(attempt, at: 0)
        log.info("\(action, privacy: .public) start")

        // A computer may have handed a pairing over since, or the Keychain copy can bring it back.
        refreshPairing()
        let url = pairingFileURL
        let box = Unmanaged.passUnretained(pinBox).toOpaque()
        let outcome: Result<(@Sendable (inout Attempt) -> Void), Error> = await Task.detached {
            Result {
                if needsPairing && !FileManager.default.fileExists(atPath: url.path) { throw PairingMissing() }
                let file = try FileManager.default.fileExists(atPath: url.path)
                    ? IDevice.PairingFile.load(Data(contentsOf: url))
                    : IDevice.PairingFile.generate(hostname: Self.hostname)
                let before = try? file.serialized()
                // A pair-setup updates the file in place, and a later step can still fail (a
                // tunnel on the wrong port): persist it either way, or the new keys are lost
                // while the device keeps the pairing. Only when it changed: a JIT session
                // runs beside other actions for hours, and mustn't put back an older file.
                let save = {
                    if let after = try? file.serialized(), after != before {
                        try after.write(to: url, options: .completeFileProtection)
                    }
                }
                let apply: @Sendable (inout Attempt) -> Void
                do {
                    apply = try body(file, box)
                } catch {
                    try? save()
                    throw error
                }
                try save()
                return apply
            }
        }.value

        pinRequested = false
        // The device answering to the pairing clears an earlier refusal; a refusal is kept
        // (for this record only) so Setup and Settings can say so.
        switch outcome {
        case .success: PairingRecord.clearRejected()
        case .failure(let error) where error is PairingRejected: PairingRecord.markRejected()
        case .failure: break
        }
        refreshPairing()
        // Other attempts may have started since: find this one again.
        guard let index = attempts.firstIndex(where: { $0.id == attempt.id }) else { return "unknown" }
        attempts[index].finishedAt = DebugTime.iso(Date())
        switch outcome {
        case .success(let apply):
            apply(&attempts[index])
            attempts[index].outcome = "ok"
            if let peer = attempts[index].peer {
                UserDefaults.standard.set(peer.udid, forKey: "pairedUDID")
                UserDefaults.standard.set(peer.name, forKey: "pairedName")
            }
            log.info("\(action, privacy: .public) ok")
        case .failure(let error):
            attempts[index].outcome = "error: \(error)"
            // The app-limit error lists other apps' bundle IDs, which stay out of the log.
            let reason = AppLimit.blockingApps(in: "\(error)") == nil ? String(describing: error) : "free-profile app limit reached"
            log.error("\(action, privacy: .public) failed: \(reason, privacy: .public)")
        }
        return attempts[index].outcome ?? "unknown"
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
