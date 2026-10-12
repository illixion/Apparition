import DebugTrace
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
                companions.append(try await Self.info(endpoint, name: name))
            } catch {
                log.error("companion \(name, privacy: .private) didn't answer: \(String(describing: error), privacy: .public)")
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

    private nonisolated static func browse(seconds: Double) async -> [(String, NWEndpoint)] {
        let browser = NWBrowser(for: .bonjour(type: serviceType, domain: nil), using: .tcp)
        let queue = DispatchQueue(label: "companion-browse")
        browser.start(queue: queue)
        try? await Task.sleep(for: .seconds(seconds))
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
    private nonisolated static func info(_ endpoint: NWEndpoint, name: String) async throws -> Companion {
        let parameters = NWParameters.tcp
        (parameters.defaultProtocolStack.internetProtocol as? NWProtocolIP.Options)?.version = .v4
        let connection = NWConnection(to: endpoint, using: parameters)
        // One deadline for the whole exchange: cancelling the connection ends whichever wait
        // is pending (a connection can sit in .preparing indefinitely).
        let deadline = Task {
            try await Task.sleep(for: .seconds(5))
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
        // An address's description carries its interface ("172.20.48.198%en0"); the relay
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
        let anisette = (json["anisette"] as? Int).map { "http://\(address):\($0)" }
        return Companion(name: name, relay: "\(address):\(port)", anisetteURL: anisette)
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
        ])
    }
}
