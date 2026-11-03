import DebugTrace
import Foundation
import Network

private let log = DebugLogger(subsystem: Bundle.main.bundleIdentifier ?? "Apparition", category: "Companion")

/// Wakes a sleeping Mac companion on this network, the way Screen Sharing (and ssh-exec) do.
///
/// A Mac with "Wake for network access" on keeps its network interface answering while it
/// sleeps, and wakes when a TCP SYN arrives for a port it listens on: tested 2026-11-03 on a
/// MacBook Pro asleep (ping TTL 32), which answered the companion's relay port 8.6 s after the
/// first knock. So knocking is connecting, again and again, until the Mac answers. Nothing else
/// is sent and no entitlement is needed. It works only at the Mac's LAN address: Tailscale's
/// UDP never wakes it, which is why `CompanionRoutes` prefers a LAN address whenever this device
/// is on that network. Once awake, the companion keeps the Mac from sleeping while it's used
/// (core/src/stay_awake.rs).
enum CompanionWake {
    /// When the companion at `host` doesn't answer on `port`, knocks there and at `others`
    /// (its other LAN addresses) until one answers or `seconds` pass. Returns the address that
    /// answered, or nil. Only addresses on this device's own networks are knocked; with none, it
    /// returns at once.
    @discardableResult
    static func wakeIfAsleep(host: String, port: UInt16, others: [String] = [], seconds: Double = 20) async -> String? {
        guard let port = NWEndpoint.Port(rawValue: port) else { return nil }
        if await connects(host, port, timeout: 1.5) { return host }
        let targets = CompanionRoutes.onThisNetwork([host] + others)
        guard !targets.isEmpty else { return nil }
        log.info("companion didn't answer; knocking at \(targets.count, privacy: .public) LAN address(es) to wake it")
        let started = Date()
        while Date().timeIntervalSince(started) < seconds {
            for target in targets where await connects(target, port, timeout: 2) {
                log.info("companion answered after \(Int(Date().timeIntervalSince(started)), privacy: .public) s")
                return target
            }
            try? await Task.sleep(for: .milliseconds(500))
        }
        log.info("companion didn't wake within \(Int(seconds), privacy: .public) s")
        return nil
    }

    /// Whether a TCP connection to `host`:`port` opens within `timeout` seconds. Its SYN is the knock.
    static func connects(_ host: String, _ port: NWEndpoint.Port, timeout: Double) async -> Bool {
        let connection = NWConnection(host: NWEndpoint.Host(host), port: port, using: .tcp)
        defer { connection.cancel() }
        return await withCheckedContinuation { (continuation: CheckedContinuation<Bool, Never>) in
            let gate = Gate()
            connection.stateUpdateHandler = { state in
                switch state {
                case .ready: if gate.claim() { continuation.resume(returning: true) }
                // Refused, or no route: awake without a listener, or not there at all.
                case .failed, .waiting, .cancelled: if gate.claim() { continuation.resume(returning: false) }
                default: break
                }
            }
            connection.start(queue: DispatchQueue(label: "companion-wake"))
            DispatchQueue.global().asyncAfter(deadline: .now() + timeout) {
                if gate.claim() { continuation.resume(returning: false) }
            }
        }
    }

    private final class Gate: @unchecked Sendable {
        private let lock = NSLock()
        private var done = false
        func claim() -> Bool {
            lock.lock()
            defer { lock.unlock() }
            if done { return false }
            done = true
            return true
        }
    }
}

/// Where a companion can be reached, from its own INFO reply: its LAN addresses (wakeable, home)
/// and its Tailscale ones (anywhere), and a stable id that tells it apart from another computer
/// at the same address on another network. Kept per address it answered at, so whichever one a
/// device saved leads to the others.
///
/// [`best`] picks the address to use now: a LAN address on this device's network if the companion
/// answers there, else a Tailscale one, else a knock to wake it at home. So the same saved
/// companion works at home (and wakes) and away (over Tailscale) with nothing to switch.
enum CompanionRoutes {
    struct Record: Codable, Equatable {
        var id: String?
        var relayPort: UInt16
        var lan: [String]
        var tailscale: [String]
    }

    private static let key = "companionRoutes"

    private static func all() -> [String: Record] {
        guard let data = UserDefaults.standard.data(forKey: key) else { return [:] }
        return (try? JSONDecoder().decode([String: Record].self, from: data)) ?? [:]
    }

    static func remember(_ record: Record, for host: String) {
        var all = all()
        guard all[host] != record else { return }
        all[host] = record
        // The same record under each of its addresses, so any saved one finds the rest.
        for address in record.lan + record.tailscale where all[address]?.id == record.id || all[address] == nil {
            all[address] = record
        }
        if all.count > 64 { all = all.filter { $0.value.id == record.id } }
        if let data = try? JSONEncoder().encode(all) { UserDefaults.standard.set(data, forKey: key) }
    }

    static func record(for host: String) -> Record? { all()[host] }

    /// The addresses among `addresses` on one of this device's own networks.
    static func onThisNetwork(_ addresses: [String]) -> [String] {
        let networks = LocalNetworks.current()
        var seen = Set<String>()
        return addresses.filter { seen.insert($0).inserted }.filter { address in
            guard let ip = LocalNetworks.parse(address) else { return false }
            return networks.contains { $0.mask != 0xFFFF_FFFF && ($0.address & $0.mask) == (ip & $0.mask) }
        }
    }

    /// Where to reach the companion last used at `host` now, with `port` the port to use there
    /// (the relay's, or its anisette channel's). nil when it can't be reached anywhere. Without a
    /// record (a companion older than this), only `host` is tried, with a knock.
    static func best(host: String, port: UInt16) async -> String? {
        guard let record = record(for: host) else {
            return await CompanionWake.wakeIfAsleep(host: host, port: port)
        }
        let lan = onThisNetwork(record.lan)
        // Best first: LAN here, then Tailscale, then whatever was saved.
        var order = lan + record.tailscale + [host]
        var seen = Set<String>()
        order = order.filter { seen.insert($0).inserted }
        // One quick round, all at once: the companion awake, wherever this device is.
        let answered = await withTaskGroup(of: (String, Bool).self) { group in
            for candidate in order {
                group.addTask { (candidate, await same(candidate, record: record)) }
            }
            var answered = Set<String>()
            for await (candidate, ok) in group where ok { answered.insert(candidate) }
            return answered
        }
        if let first = order.first(where: answered.contains) {
            if first != host { log.info("companion is now best reached at another of its addresses") }
            return first
        }
        // Nothing answered: asleep at home, perhaps. A knock at its LAN addresses wakes it.
        guard !lan.isEmpty, let woke = await CompanionWake.wakeIfAsleep(host: lan[0], port: record.relayPort, others: Array(lan.dropFirst())),
              await same(woke, record: record) else { return nil }
        return woke
    }

    /// Whether the companion at `host` answers on its relay port as the one `record` describes.
    private static func same(_ host: String, record: Record) async -> Bool {
        guard let found = await CompanionFinder.probe(relay: "\(host):\(record.relayPort)", seconds: 2) else { return false }
        return record.id == nil || found.companionID == record.id
    }
}
