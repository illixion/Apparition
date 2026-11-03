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
/// UDP never wakes it. A companion found over Tailscale says its LAN addresses in its INFO reply,
/// remembered here for when this device is home. Once awake, the companion keeps the Mac from
/// sleeping while it's used (core/src/stay_awake.rs).
enum CompanionWake {
    private static let key = "companionWakeAddresses"

    /// What the companion reached at `host` said its LAN addresses are.
    static func remember(_ addresses: [String], for host: String) {
        var all = UserDefaults.standard.dictionary(forKey: key) as? [String: [String]] ?? [:]
        guard all[host] != addresses else { return }
        all[host] = addresses
        // A handful of companions at most; drop the oldest-looking extras rather than grow.
        if all.count > 16, let extra = all.keys.first(where: { $0 != host }) { all[extra] = nil }
        UserDefaults.standard.set(all, forKey: key)
    }

    /// `host` and the LAN addresses remembered for it, keeping those on one of this device's own
    /// networks: the only places a knock can wake a Mac.
    static func candidates(host: String) -> [String] {
        let remembered = (UserDefaults.standard.dictionary(forKey: key) as? [String: [String]])?[host] ?? []
        let networks = LocalNetworks.current()
        var seen = Set<String>()
        return ([host] + remembered).filter { seen.insert($0).inserted }.filter { address in
            guard let ip = LocalNetworks.parse(address) else { return false }
            return networks.contains { $0.mask != 0xFFFF_FFFF && ($0.address & $0.mask) == (ip & $0.mask) }
        }
    }

    /// When the companion at `host` doesn't answer on `port`, knocks at its LAN addresses until
    /// one answers or `seconds` pass. True when it answers (it was awake, or woke). Returns at
    /// once, false, when none of its addresses is on this network.
    @discardableResult
    static func wakeIfAsleep(host: String, port: UInt16, seconds: Double = 20) async -> Bool {
        guard let port = NWEndpoint.Port(rawValue: port) else { return false }
        if await connects(host, port, timeout: 1.5) { return true }
        let targets = candidates(host: host)
        guard !targets.isEmpty else { return false }
        log.info("companion didn't answer; knocking at \(targets.count, privacy: .public) LAN address(es) to wake it")
        let started = Date()
        while Date().timeIntervalSince(started) < seconds {
            for target in targets where await connects(target, port, timeout: 2) {
                log.info("companion answered after \(Int(Date().timeIntervalSince(started)), privacy: .public) s")
                return true
            }
            try? await Task.sleep(for: .milliseconds(500))
        }
        log.info("companion didn't wake within \(Int(seconds), privacy: .public) s")
        return false
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
