import DebugTrace
import dnssd
import Foundation
import Network
import Observation

private let log = DebugLogger(subsystem: Bundle.main.bundleIdentifier ?? "Apparition", category: "Companion")

/// Finds companions (Apparition on a Mac or PC, or `apparition relay`) on the local network (Bonjour) and asks one what it offers.
/// Over Tailscale there is no Bonjour, so the relay is typed in and `check` asks it directly.
@MainActor @Observable
final class CompanionFinder {
    struct Companion: Encodable, Identifiable, Equatable {
        var id: String { relay }
        let name: String
        /// "ip:port", what the relay setting takes.
        let relay: String
        /// http://ip:port of the anisette server it runs, if one answers.
        let anisetteURL: String?
        /// The port to pair on first, when its anisette server serves paired devices only.
        var anisettePairingPort: UInt16? = nil
        /// TLS-PSK channel for anisette, advertised after pairing is enabled.
        var anisetteChannelPort: UInt16? = nil
        /// The companion's stable id, the same at each of its addresses (CompanionRoutes).
        var companionID: String? = nil
    }

    nonisolated static let serviceType = "_apparition._tcp"

    private(set) var found: [Companion] = []
    private(set) var searching = false
    private(set) var lastError: String?

    /// Browses for `seconds` and asks every companion found for its info.
    @discardableResult
    func search(seconds: Double = 3) async -> [Companion] {
        searching = true
        defer { searching = false }
        let services = await Self.browse(seconds: seconds)
        var companions: [Companion] = []
        for (name, endpoint) in services {
            do {
                companions.append(try await Self.reach(name: name, endpoint: endpoint))
            } catch {
                log.error("companion \(name, privacy: .private) didn't answer: \(LogScrub.text(String(describing: error)), privacy: .public)")
            }
        }
        found = companions
        lastError = companions.isEmpty ? "No companion found on this network (on Tailscale, type its address instead)." : nil
        return companions
    }

    /// Asks the companion at `relay` ("ip:port") what it offers.
    func check(relay: String) async -> Companion? {
        guard let colon = relay.lastIndex(of: ":"), let port = NWEndpoint.Port(String(relay[relay.index(after: colon)...])) else {
            lastError = "expected ip:port, got \(relay)"
            return nil
        }
        let host = String(relay[..<colon]).trimmingCharacters(in: CharacterSet(charactersIn: "[]"))
        do {
            let companion = try await Self.info(.hostPort(host: NWEndpoint.Host(host), port: port), name: host)
            lastError = nil
            if !found.contains(companion) { found.append(companion) }
            return companion
        } catch {
            lastError = "\(relay) didn't answer: \(error)"
            return nil
        }
    }

    /// Finds the companion advertised as `name` again, for when its saved address stopped
    /// answering (a new DHCP lease, another network). Stops browsing as soon as it shows up.
    func locate(name: String, seconds: Double = 4) async -> Companion? {
        guard let endpoint = await Self.browse(seconds: seconds, until: name).first(where: { $0.0 == name })?.1 else {
            log.info("companion \(name, privacy: .private) not on this network")
            return nil
        }
        do {
            let companion = try await Self.reach(name: name, endpoint: endpoint)
            if let index = found.firstIndex(where: { $0.name == name }) { found[index] = companion } else { found.append(companion) }
            return companion
        } catch {
            log.error("companion \(name, privacy: .private) didn't answer: \(LogScrub.text(String(describing: error)), privacy: .public)")
            return nil
        }
    }

    /// Whether the companion at `relay` ("ip:port") answers within `seconds`.
    nonisolated static func answers(relay: String, seconds: Double = 2) async -> Bool {
        await probe(relay: relay, seconds: seconds) != nil
    }

    /// What the companion at `relay` ("ip:port") says, if it answers within `seconds`.
    nonisolated static func probe(relay: String, seconds: Double = 2) async -> Companion? {
        guard let colon = relay.lastIndex(of: ":"), let port = NWEndpoint.Port(String(relay[relay.index(after: colon)...])) else { return nil }
        let host = String(relay[..<colon]).trimmingCharacters(in: CharacterSet(charactersIn: "[]"))
        return try? await info(.hostPort(host: NWEndpoint.Host(host), port: port), name: host, seconds: seconds)
    }

    /// Reaches a found service at the best of its addresses. Bonjour can return several (a
    /// computer on two networks, or a self-assigned 169.254 one while its DHCP lease is
    /// missing), and connecting to the service itself takes whichever the system picks, which
    /// is then saved. So: resolve every IPv4 address, try ones on this device's own subnets
    /// first and link-local ones last, and keep the first that answers. Falls back to the
    /// service endpoint when resolving gives nothing.
    private nonisolated static func reach(name: String, endpoint: NWEndpoint) async throws -> Companion {
        if case let .service(service, type, domain, _) = endpoint,
           let resolved = await Resolver.resolve(name: service, type: type, domain: domain) {
            var lastError: Error?
            for address in rank(resolved.addresses) {
                do {
                    return try await info(.hostPort(host: NWEndpoint.Host(address), port: NWEndpoint.Port(rawValue: resolved.port)!),
                                          name: name, seconds: 2)
                } catch {
                    lastError = error
                }
            }
            if let lastError, resolved.addresses.count > 0 {
                log.info("no resolved address of \(name, privacy: .private) answered (\(LogScrub.text(String(describing: lastError)), privacy: .public)), trying the service")
            }
        }
        return try await info(endpoint, name: name)
    }

    /// Same subnet as one of this device's interfaces first, then routed ones, then link-local
    /// (169.254/16), which only makes sense when this device is link-local on that link too.
    nonisolated static func rank(_ addresses: [String]) -> [String] {
        let own = LocalNetworks.current()
        func score(_ address: String) -> Int {
            guard let ip = LocalNetworks.parse(address) else { return 3 }
            let linkLocal = ip & 0xFFFF_0000 == 0xA9FE_0000
            let onLink = own.contains { ip & $0.mask == $0.address & $0.mask }
            switch (onLink, linkLocal) {
            case (true, false): return 0
            case (false, false): return 1
            case (true, true): return 2
            case (false, true): return 4
            }
        }
        var seen = Set<String>()
        return addresses.filter { seen.insert($0).inserted && score($0) < 4 }.sorted { score($0) < score($1) }
    }

    private nonisolated static func browse(seconds: Double, until name: String? = nil) async -> [(String, NWEndpoint)] {
        let browser = NWBrowser(for: .bonjour(type: serviceType, domain: nil), using: .tcp)
        let queue = DispatchQueue(label: "companion-browse")
        browser.start(queue: queue)
        let deadline = Date().addingTimeInterval(seconds)
        repeat {
            try? await Task.sleep(for: .milliseconds(name == nil ? Int(seconds * 1000) : 250))
            if let name, browser.browseResults.contains(where: {
                if case let .service(found, _, _, _) = $0.endpoint { found == name } else { false }
            }) { break }
        } while Date() < deadline
        let results = browser.browseResults
        browser.cancel()
        return results.compactMap { result in
            guard case let .service(name, _, _, _) = result.endpoint else { return nil }
            return (name, result.endpoint)
        }
    }

    /// Connects over IPv4 (the relay setting and the Rust side want a plain ip:port), sends
    /// `APPARITION-INFO 1`, and reads back `INFO {json}`. The address comes from the connection
    /// itself, so it is the one this device actually reached.
    private nonisolated static func info(_ endpoint: NWEndpoint, name: String, seconds: Double = 5) async throws -> Companion {
        let parameters = NWParameters.tcp
        (parameters.defaultProtocolStack.internetProtocol as? NWProtocolIP.Options)?.version = .v4
        let connection = NWConnection(to: endpoint, using: parameters)
        // One deadline for the whole exchange: cancelling the connection ends whichever wait
        // is pending (a connection can sit in .preparing indefinitely).
        let deadline = Task {
            try await Task.sleep(for: .seconds(seconds))
            connection.cancel()
        }
        defer {
            deadline.cancel()
            connection.cancel()
        }
        try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Void, Error>) in
            let once = Once()
            connection.stateUpdateHandler = { state in
                switch state {
                case .ready: once.run { continuation.resume() }
                case .failed(let error), .waiting(let error): once.run { continuation.resume(throwing: error) }
                case .cancelled: once.run { continuation.resume(throwing: IDevice.ProbeError("timed out")) }
                default: break
                }
            }
            connection.start(queue: DispatchQueue(label: "companion-info"))
        }
        guard case let .hostPort(host, port) = connection.currentPath?.remoteEndpoint else {
            throw IDevice.ProbeError("no address for \(name)")
        }
        // An address's description carries its interface ("192.0.2.10%en0"); the relay
        // setting wants the bare address.
        func bare(_ address: Any) -> String {
            String("\(address)".split(separator: "%", maxSplits: 1).first ?? "")
        }
        let address: String
        switch host {
        case .ipv4(let v4): address = bare(v4)
        case .ipv6(let v6): address = v6.asIPv4.map(bare) ?? "[\(bare(v6))]"
        case .name(let text, _): address = text
        @unknown default: address = bare(host)
        }

        connection.send(content: Data("APPARITION-INFO 1\n".utf8), completion: .idempotent)
        let reply: Data = try await withCheckedThrowingContinuation { continuation in
            connection.receive(minimumIncompleteLength: 1, maximumLength: 4096) { data, _, _, error in
                if let error { continuation.resume(throwing: error) } else { continuation.resume(returning: data ?? Data()) }
            }
        }
        let line = String(decoding: reply, as: UTF8.self).trimmingCharacters(in: .whitespacesAndNewlines)
        guard line.hasPrefix("INFO "),
              let json = try JSONSerialization.jsonObject(with: Data(line.dropFirst(5).utf8)) as? [String: Any] else {
            throw IDevice.ProbeError("not an Apparition companion (or an old one): \(line.prefix(40))")
        }
        // Every way to reach it, for picking the one that works wherever this device is.
        let companionID = json["id"] as? String
        CompanionRoutes.remember(.init(id: companionID, relayPort: port.rawValue,
                                       lan: json["addresses"] as? [String] ?? [],
                                       tailscale: json["tailscale"] as? [String] ?? []), for: address)
        let anisette = (json["anisette"] as? Int).map { "http://\(address):\($0)" }
        let pairing = (json["anisettePairing"] as? Int).flatMap(UInt16.init(exactly:))
        let channel = (json["anisetteChannel"] as? Int).flatMap(UInt16.init(exactly:))
        return Companion(name: name, relay: "\(address):\(port)", anisetteURL: anisette,
                         anisettePairingPort: pairing, anisetteChannelPort: channel, companionID: companionID)
    }
}

/// This device's IPv4 networks (address and netmask, host byte order), from getifaddrs.
enum LocalNetworks {
    struct Network { let address: UInt32; let mask: UInt32 }

    static func current() -> [Network] {
        var head: UnsafeMutablePointer<ifaddrs>?
        guard getifaddrs(&head) == 0, let first = head else { return [] }
        defer { freeifaddrs(head) }
        var networks: [Network] = []
        for entry in sequence(first: first, next: { $0.pointee.ifa_next }) {
            guard let addr = entry.pointee.ifa_addr, addr.pointee.sa_family == UInt8(AF_INET),
                  let mask = entry.pointee.ifa_netmask,
                  entry.pointee.ifa_flags & UInt32(IFF_UP) != 0, entry.pointee.ifa_flags & UInt32(IFF_LOOPBACK) == 0 else { continue }
            func v4(_ pointer: UnsafeMutablePointer<sockaddr>) -> UInt32 {
                pointer.withMemoryRebound(to: sockaddr_in.self, capacity: 1) { UInt32(bigEndian: $0.pointee.sin_addr.s_addr) }
            }
            networks.append(Network(address: v4(addr), mask: v4(mask)))
        }
        return networks
    }

    static func parse(_ text: String) -> UInt32? {
        var address = in_addr()
        guard inet_pton(AF_INET, text, &address) == 1 else { return nil }
        return UInt32(bigEndian: address.s_addr)
    }
}

/// Resolves a Bonjour service to its port and every IPv4 address of its host, with dnssd
/// (DNSServiceResolve, then DNSServiceGetAddrInfo), which NWConnection keeps to itself.
private final class Resolver: @unchecked Sendable {
    struct Result { let port: UInt16; let addresses: [String] }

    private let queue = DispatchQueue(label: "companion-resolve")
    private var resolveRef: DNSServiceRef?
    private var addressRef: DNSServiceRef?
    private var port: UInt16 = 0
    private var addresses: [String] = []
    private var continuation: CheckedContinuation<Result?, Never>?

    static func resolve(name: String, type: String, domain: String, seconds: Double = 2) async -> Result? {
        let resolver = Resolver()
        return await withCheckedContinuation { continuation in
            resolver.queue.async {
                resolver.continuation = continuation
                resolver.start(name: name, type: type, domain: domain.isEmpty ? "local." : domain)
                // Addresses arrive one by one: collect for a moment after the first, up to the deadline.
                resolver.queue.asyncAfter(deadline: .now() + seconds) { resolver.finish() }
            }
        }
    }

    private func start(name: String, type: String, domain: String) {
        let context = Unmanaged.passRetained(self).toOpaque()
        let status = DNSServiceResolve(&resolveRef, 0, 0, name, type, domain, { _, _, _, error, _, host, port, _, _, context in
            guard let context else { return }
            let resolver = Unmanaged<Resolver>.fromOpaque(context).takeUnretainedValue()
            guard error == kDNSServiceErr_NoError, let host else { return resolver.finish() }
            resolver.resolved(host: String(cString: host), port: UInt16(bigEndian: port))
        }, context)
        guard status == kDNSServiceErr_NoError, let resolveRef else {
            Unmanaged<Resolver>.fromOpaque(context).release()
            return finish()
        }
        DNSServiceSetDispatchQueue(resolveRef, queue)
    }

    private func resolved(host: String, port: UInt16) {
        guard addressRef == nil else { return }
        self.port = port
        let context = Unmanaged.passUnretained(self).toOpaque()
        let status = DNSServiceGetAddrInfo(&addressRef, 0, 0, DNSServiceProtocol(kDNSServiceProtocol_IPv4), host, { _, _, _, error, _, address, _, context in
            guard let context, error == kDNSServiceErr_NoError, let address, address.pointee.sa_family == UInt8(AF_INET) else { return }
            let resolver = Unmanaged<Resolver>.fromOpaque(context).takeUnretainedValue()
            var text = [CChar](repeating: 0, count: Int(INET_ADDRSTRLEN))
            address.withMemoryRebound(to: sockaddr_in.self, capacity: 1) { pointer in
                var v4 = pointer.pointee.sin_addr
                _ = inet_ntop(AF_INET, &v4, &text, socklen_t(INET_ADDRSTRLEN))
            }
            let found = String(cString: text)
            if resolver.addresses.isEmpty {
                resolver.queue.asyncAfter(deadline: .now() + 0.5) { resolver.finish() }
            }
            resolver.addresses.append(found)
        }, context)
        if status == kDNSServiceErr_NoError, let addressRef {
            DNSServiceSetDispatchQueue(addressRef, queue)
        } else {
            finish()
        }
    }

    private func finish() {
        guard let continuation else { return }
        self.continuation = nil
        if let addressRef { DNSServiceRefDeallocate(addressRef) }
        if let resolveRef {
            DNSServiceRefDeallocate(resolveRef)
            Unmanaged.passUnretained(self).release()
        }
        addressRef = nil
        resolveRef = nil
        continuation.resume(returning: port == 0 ? nil : Result(port: port, addresses: addresses))
    }
}

/// Resumes a continuation once, whichever state callback comes first.
private final class Once: @unchecked Sendable {
    private let lock = NSLock()
    private var done = false
    func run(_ body: () -> Void) {
        lock.lock()
        defer { lock.unlock() }
        guard !done else { return }
        done = true
        body()
    }
}

@MainActor
enum CompanionEndpoints {
    static func register(_ finder: CompanionFinder, probe: ProbeModel) {
        DebugSurface.shared.register([
            .command("findCompanions", "Browse the local network for Apparition companions and ask each what it offers (relay address, anisette server)",
                     parameters: [.number("seconds", "How long to browse", default: 3)], timeout: .seconds(30)) { args in
                await finder.search(seconds: args.double("seconds") ?? 3)
            },
            .command("checkCompanion", "Ask the companion at relay (default: the relay set now) what it offers",
                     parameters: [.string("relay", "ip:port; default the current relay")]) { args in
                guard let relay = args.string("relay") ?? probe.relay else { throw IDevice.ProbeError("no relay set") }
                guard let companion = await finder.check(relay: relay) else {
                    throw IDevice.ProbeError(finder.lastError ?? "no answer")
                }
                return companion
            },
            .command("followCompanion", "Pick where to reach the companion now, as an action does (its LAN address here, knocking to wake it, else its Tailscale one), and switch to it. Returns the relay before and after",
                     timeout: .seconds(60)) { _ in
                let before = probe.relay
                probe.companionMayHaveMoved()
                await probe.prepareRoute(host: ProbeModel.relayHost)
                return ["before": before, "after": probe.relay]
            },
        ])
    }
}
