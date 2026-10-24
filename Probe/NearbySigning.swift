import DebugTrace
import Foundation
import Network
import Observation

private let log = DebugLogger(subsystem: Bundle.main.bundleIdentifier ?? "Apparition", category: "Nearby")

/// Handing the Apple ID's certificate key to Apparition on a nearby device or computer, or getting
/// it from one, over the local network, checked by a six-digit code both screens show
/// (core/src/nearby.rs). It replaces AirDrop where there is none (tvOS, a computer that isn't a
/// Mac) and saves typing five words where there is. The install that needs the key listens and
/// advertises `_apparition-key._tcp`; the one that has it browses and connects. The core runs the
/// exchange; this moves its messages, each framed by its length (4 bytes, little-endian).
enum NearbySigning {
    nonisolated static let serviceType = "_apparition-key._tcp"
    nonisolated static let maxMessage = 16 * 1024

    /// What this install calls itself to the other device: the name the sealed files carry too.
    static var ownName: String {
        "Apparition on \(UserDefaults.standard.string(forKey: "pairedName") ?? ThisDevice.name)"
    }

    struct Failure: Error, CustomStringConvertible {
        let description: String
    }

    /// This device's IPv4 address on Wi-Fi or Ethernet, for typing into a computer that can't browse.
    static func localAddress() -> String? {
        var interfaces: UnsafeMutablePointer<ifaddrs>?
        guard getifaddrs(&interfaces) == 0, let first = interfaces else { return nil }
        defer { freeifaddrs(interfaces) }
        var found: String?
        for pointer in sequence(first: first, next: { $0.pointee.ifa_next }) {
            let interface = pointer.pointee
            guard let address = interface.ifa_addr, address.pointee.sa_family == UInt8(AF_INET),
                  String(cString: interface.ifa_name).hasPrefix("en"),
                  interface.ifa_flags & UInt32(IFF_UP) != 0, interface.ifa_flags & UInt32(IFF_LOOPBACK) == 0 else { continue }
            var host = [CChar](repeating: 0, count: Int(NI_MAXHOST))
            guard getnameinfo(address, socklen_t(address.pointee.sa_len), &host, socklen_t(host.count), nil, 0, NI_NUMERICHOST) == 0 else { continue }
            let text = String(cString: host)
            if !text.hasPrefix("169.254.") {
                found = text
                break
            }
        }
        return found
    }
}

/// What the core said after a message or the person's answer.
struct NearbyStep: Sendable {
    let send: [String]
    /// waiting, compare, confirmed, done or cancelled.
    let state: String
    let code: String?
    let peer: String?
    let appleId: String?
    let from: String?
    let reason: String?

    init(_ reply: [String: Any]) throws {
        guard reply["ok"] as? Bool == true else {
            throw NearbySigning.Failure(description: reply["error"] as? String ?? "failed")
        }
        send = reply["send"] as? [String] ?? []
        state = reply["state"] as? String ?? "waiting"
        code = reply["code"] as? String
        peer = reply["peer"] as? String
        let result = reply["result"] as? [String: Any]
        appleId = result?["appleId"] as? String
        from = result?["from"] as? String
        reason = reply["reason"] as? String
    }
}

/// One exchange in the core, by its handle. Forgotten (with any key it holds) when released.
final class NearbyExchange: @unchecked Sendable {
    let handle: UInt64

    private init(handle: UInt64) {
        self.handle = handle
    }

    deinit {
        apparition_nearby_end(handle)
    }

    private static func begin(_ reply: [String: Any]) throws -> (NearbyExchange, [String]) {
        guard reply["ok"] as? Bool == true, let handle = (reply["handle"] as? NSNumber)?.uint64Value else {
            throw NearbySigning.Failure(description: reply["error"] as? String ?? "failed")
        }
        return (NearbyExchange(handle: handle), reply["send"] as? [String] ?? [])
    }

    /// For handing over the key this install keeps for `email`: the exchange and its first message.
    static func sender(email: String, teamId: String?, name: String) async throws -> (NearbyExchange, [String]) {
        let dataDir = AccountModel.dataDir.path
        return try begin(await Task.detached { AccountModel.call(apparition_nearby_send_begin(dataDir, email, teamId, name)) }.value)
    }

    static func receiver(name: String) throws -> NearbyExchange {
        try begin(AccountModel.call(apparition_nearby_receive_begin(AccountModel.dataDir.path, name))).0
    }

    func handle(_ message: String) async throws -> NearbyStep {
        let handle = handle
        return try NearbyStep(await Task.detached { AccountModel.call(apparition_nearby_handle(handle, message)) }.value)
    }

    /// The receiver imports the key here, once both the key and its person's answer are in.
    func confirm(_ matches: Bool) async throws -> NearbyStep {
        let handle = handle
        return try NearbyStep(await Task.detached { AccountModel.call(apparition_nearby_confirm(handle, matches)) }.value)
    }
}

/// Length-prefixed messages on one connection. One task reads; sends queue in order.
final class NearbyLink: @unchecked Sendable {
    let connection: NWConnection
    private var buffer = Data()
    private let queue = DispatchQueue(label: "nearby-signing")

    init(_ connection: NWConnection) {
        self.connection = connection
    }

    /// Waits for the connection to be ready, for `seconds` at most.
    func start(seconds: Double = 10) async throws {
        let connection = connection
        let deadline = Task {
            try await Task.sleep(for: .seconds(seconds))
            connection.cancel()
        }
        defer { deadline.cancel() }
        try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Void, Error>) in
            let once = NearbyOnce()
            connection.stateUpdateHandler = { state in
                switch state {
                case .ready: once.run { continuation.resume() }
                case .failed(let error), .waiting(let error): once.run { continuation.resume(throwing: error) }
                case .cancelled: once.run { continuation.resume(throwing: NearbySigning.Failure(description: "didn't answer")) }
                default: break
                }
            }
            connection.start(queue: queue)
        }
    }

    func send(_ messages: [String]) {
        for message in messages {
            let body = Data(message.utf8)
            var length = UInt32(body.count).littleEndian
            var frame = Data(bytes: &length, count: 4)
            frame.append(body)
            connection.send(content: frame, completion: .idempotent)
        }
    }

    /// The next message's JSON.
    func receive() async throws -> String {
        while true {
            if buffer.count >= 4 {
                let length = buffer.prefix(4).enumerated().reduce(0) { $0 | Int($1.element) << (8 * $1.offset) }
                guard length <= NearbySigning.maxMessage else { throw NearbySigning.Failure(description: "The other device sent too much") }
                if buffer.count >= 4 + length {
                    let body = buffer.subdata(in: buffer.startIndex + 4 ..< buffer.startIndex + 4 + length)
                    buffer.removeFirst(4 + length)
                    return String(decoding: body, as: UTF8.self)
                }
            }
            let connection = connection
            let chunk: Data = try await withCheckedThrowingContinuation { continuation in
                connection.receive(minimumIncompleteLength: 1, maximumLength: 65536) { data, _, complete, error in
                    if let error {
                        continuation.resume(throwing: error)
                    } else if let data, !data.isEmpty {
                        continuation.resume(returning: data)
                    } else if complete {
                        continuation.resume(throwing: NearbySigning.Failure(description: "The other device closed the connection"))
                    } else {
                        continuation.resume(returning: Data())
                    }
                }
            }
            buffer.append(chunk)
        }
    }

    func cancel() {
        connection.cancel()
    }

    /// Closes once what's queued has gone out (a last cancel or done message), then cancels;
    /// after five seconds at most, should the other side stop reading.
    func close() {
        let connection = connection
        connection.send(content: nil, contentContext: .finalMessage, isComplete: true, completion: .contentProcessed { _ in
            connection.cancel()
        })
        queue.asyncAfter(deadline: .now() + 5) { connection.cancel() }
    }
}

/// Resumes a continuation once, whichever state callback comes first.
private final class NearbyOnce: @unchecked Sendable {
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

/// Where a hand-over is, on either side.
enum NearbyPhase: Equatable, Sendable {
    case idle
    /// Receiver: listening. Sender: looking for receivers.
    case waiting
    case connecting(peer: String)
    /// Show the code and ask whether the other device shows the same.
    case compare(code: String, peer: String)
    /// This person confirmed; the other device's hasn't yet.
    case confirmed(code: String, peer: String)
    /// Receiver: the key is imported (`appleId`, `from`). Sender: `peer` took it.
    case done(peer: String, appleId: String?)
    case failed(String)

    var summary: String {
        switch self {
        case .idle: "idle"
        case .waiting: "waiting"
        case .connecting: "connecting"
        case .compare: "compare"
        case .confirmed: "confirmed"
        case .done: "done"
        case .failed: "failed"
        }
    }
}

/// The side that needs the key: listens, advertised over Bonjour, until a key is used or it's stopped.
@MainActor @Observable
final class NearbyReceiver {
    private(set) var phase: NearbyPhase = .idle
    /// Where it listens, for a computer that can't browse (`apparition signing send --to`).
    private(set) var port: UInt16?
    private(set) var address: String?
    private(set) var name = NearbySigning.ownName
    /// After a key is imported (the core ended the session).
    @ObservationIgnored var onImported: (() -> Void)?

    @ObservationIgnored private var listener: NWListener?
    @ObservationIgnored private var current: (link: NearbyLink, exchange: NearbyExchange)?

    var listening: Bool { listener != nil }

    func start() {
        guard listener == nil else { return }
        name = NearbySigning.ownName
        do {
            let listener = try NWListener(using: .tcp)
            listener.service = NWListener.Service(name: name, type: NearbySigning.serviceType, domain: nil,
                                                  txtRecord: NWTXTRecord(["v": "1"]))
            listener.stateUpdateHandler = { [weak self] state in
                Task { @MainActor in self?.listenerChanged(state, port: listener.port?.rawValue) }
            }
            listener.newConnectionHandler = { [weak self] connection in
                Task { @MainActor in self?.accept(connection) }
            }
            listener.start(queue: DispatchQueue(label: "nearby-receiver"))
            self.listener = listener
            address = NearbySigning.localAddress()
            phase = .waiting
            log.info("waiting for a nearby certificate key")
        } catch {
            phase = .failed("Couldn't listen on the network: \(error)")
        }
    }

    func stop() {
        listener?.cancel()
        listener = nil
        current?.link.close()
        current = nil
        port = nil
        if case .done = phase {} else { phase = .idle }
    }

    /// The person's answer to "does the other device show this code?".
    func confirm(_ matches: Bool) async {
        guard let current, case .compare = phase else { return }
        do {
            let step = try await current.exchange.confirm(matches)
            current.link.send(step.send)
            apply(step)
        } catch {
            end("\(error)")
        }
    }

    private func listenerChanged(_ state: NWListener.State, port: UInt16?) {
        switch state {
        case .ready: self.port = port
        case .failed(let error):
            phase = .failed("Stopped listening: \(error)")
            listener = nil
        default: break
        }
    }

    private func accept(_ connection: NWConnection) {
        // One at a time: a second sender waits its turn by trying again.
        guard current == nil, listener != nil else {
            connection.cancel()
            return
        }
        Task { await run(NearbyLink(connection)) }
    }

    private func run(_ link: NearbyLink) async {
        let exchange: NearbyExchange
        do {
            try await link.start()
            exchange = try NearbyExchange.receiver(name: name)
        } catch {
            link.cancel()
            return
        }
        current = (link, exchange)
        var compared = false
        do {
            while current?.exchange === exchange {
                let step = try await exchange.handle(try await link.receive())
                link.send(step.send)
                if step.state == "compare" { compared = true }
                apply(step)
            }
        } catch {
            // Before a code, it was nothing the person saw: keep waiting for a real sender.
            if current?.exchange === exchange {
                if compared { end("\(error)") } else { current = nil; link.cancel(); phase = .waiting }
            }
        }
    }

    private func apply(_ step: NearbyStep) {
        let peer = step.peer ?? "the other device"
        switch step.state {
        case "compare": phase = .compare(code: step.code ?? "", peer: peer)
        case "confirmed": phase = .confirmed(code: step.code ?? "", peer: peer)
        case "done":
            log.notice("imported a certificate key from a nearby device")
            phase = .done(peer: step.from ?? peer, appleId: step.appleId)
            current?.link.close()
            current = nil
            listener?.cancel()
            listener = nil
            onImported?()
        case "cancelled": end(step.reason ?? "Cancelled")
        default:
            switch phase {
            case .waiting, .failed: phase = .connecting(peer: peer)
            default: break
            }
        }
    }

    /// The exchange is over without a key; keeps listening for another try.
    private func end(_ reason: String) {
        current?.link.close()
        current = nil
        phase = .failed(reason)
        log.info("a nearby hand-over ended without a key")
    }
}

/// The side that has the key: finds receivers and hands it to the one the person picks.
@MainActor @Observable
final class NearbySender {
    struct Peer: Identifiable, Equatable, Sendable {
        var id: String { name }
        let name: String
        let endpoint: NWEndpoint
    }

    private(set) var phase: NearbyPhase = .idle
    private(set) var peers: [Peer] = []

    @ObservationIgnored private var browser: NWBrowser?
    /// The connection being set up or exchanged on; nil once reset, so a late error is dropped.
    @ObservationIgnored private var active: NearbyLink?
    @ObservationIgnored private var current: (link: NearbyLink, exchange: NearbyExchange)?

    func startBrowsing() {
        guard browser == nil else { return }
        let browser = NWBrowser(for: .bonjour(type: NearbySigning.serviceType, domain: nil), using: .tcp)
        let own = NearbySigning.ownName
        browser.browseResultsChangedHandler = { [weak self] results, _ in
            let peers = results.compactMap { result -> Peer? in
                guard case let .service(name, _, _, _) = result.endpoint, name != own else { return nil }
                return Peer(name: name, endpoint: result.endpoint)
            }
            Task { @MainActor in self?.peers = peers.sorted { $0.name < $1.name } }
        }
        browser.start(queue: DispatchQueue(label: "nearby-browser"))
        self.browser = browser
    }

    func stopBrowsing() {
        browser?.cancel()
        browser = nil
        peers = []
    }

    /// Back to picking a receiver.
    func reset() {
        active?.close()
        active = nil
        current = nil
        phase = .idle
    }

    /// Over IPv4: the command line and the desktop app listen on IPv4 only, as their relay does.
    private static var parameters: NWParameters {
        let parameters = NWParameters.tcp
        (parameters.defaultProtocolStack.internetProtocol as? NWProtocolIP.Options)?.version = .v4
        return parameters
    }

    func send(to peer: Peer, email: String, teamId: String?) {
        start(NWConnection(to: peer.endpoint, using: Self.parameters), peer: peer.name, email: email, teamId: teamId)
    }

    /// To an address the receiver shows ("192.168.1.20:52144"), for one Bonjour can't reach.
    func send(toAddress address: String, email: String, teamId: String?) {
        let text = address.trimmingCharacters(in: .whitespaces)
        guard let colon = text.lastIndex(of: ":"), let port = NWEndpoint.Port(String(text[text.index(after: colon)...])) else {
            phase = .failed("Type the address and port the other device shows, like 192.168.1.20:52144")
            return
        }
        let host = String(text[..<colon]).trimmingCharacters(in: CharacterSet(charactersIn: "[]"))
        start(NWConnection(host: NWEndpoint.Host(host), port: port, using: Self.parameters), peer: text, email: email, teamId: teamId)
    }

    func confirm(_ matches: Bool) async {
        guard let current, case .compare = phase else { return }
        do {
            let step = try await current.exchange.confirm(matches)
            current.link.send(step.send)
            apply(step)
        } catch {
            fail("\(error)")
        }
    }

    private func start(_ connection: NWConnection, peer: String, email: String, teamId: String?) {
        guard active == nil else { return }
        phase = .connecting(peer: peer)
        let link = NearbyLink(connection)
        active = link
        Task {
            do {
                let (exchange, first) = try await NearbyExchange.sender(email: email, teamId: teamId, name: NearbySigning.ownName)
                try await link.start()
                guard active === link else { return }
                current = (link, exchange)
                link.send(first)
                while active === link {
                    let step = try await exchange.handle(try await link.receive())
                    guard active === link else { return }
                    link.send(step.send)
                    apply(step)
                }
            } catch {
                if active === link { fail("\(peer): \(error)") }
            }
        }
    }

    private func apply(_ step: NearbyStep) {
        let peer = step.peer ?? "the other device"
        switch step.state {
        case "compare": phase = .compare(code: step.code ?? "", peer: peer)
        case "confirmed": phase = .confirmed(code: step.code ?? "", peer: peer)
        case "done":
            log.notice("handed the certificate key to a nearby device")
            phase = .done(peer: peer, appleId: nil)
            active?.close()
            active = nil
            current = nil
        case "cancelled": fail(step.reason ?? "Cancelled")
        default: break
        }
    }

    private func fail(_ reason: String) {
        active?.close()
        active = nil
        current = nil
        phase = .failed(reason)
        log.info("a nearby hand-over failed")
    }
}

@MainActor
enum NearbyEndpoints {
    struct Summary: Encodable {
        let receiver: Side
        let sender: Side
        let peers: [String]
        let name: String
        let port: UInt16?
        let address: String?
    }

    struct Side: Encodable {
        let phase: String
        let code: String?
        let peer: String?
        let detail: String?
    }

    private static func side(_ phase: NearbyPhase) -> Side {
        switch phase {
        case .compare(let code, let peer), .confirmed(let code, let peer): Side(phase: phase.summary, code: code, peer: peer, detail: nil)
        case .connecting(let peer): Side(phase: phase.summary, code: nil, peer: peer, detail: nil)
        case .done(let peer, let appleId): Side(phase: phase.summary, code: nil, peer: peer, detail: appleId)
        case .failed(let reason): Side(phase: phase.summary, code: nil, peer: nil, detail: reason)
        case .idle, .waiting: Side(phase: phase.summary, code: nil, peer: nil, detail: nil)
        }
    }

    static func register(_ signing: SigningStore, account: AccountModel) {
        let receiver = signing.nearbyReceiver, sender = signing.nearbySender
        func summary() -> Summary {
            Summary(receiver: side(receiver.phase), sender: side(sender.phase), peers: sender.peers.map(\.name),
                    name: receiver.name, port: receiver.port, address: receiver.address)
        }
        DebugSurface.shared.register([
            .query("nearbySigning", "Where nearby certificate-key hand-overs are, both sides (codes included), and receivers found") { _ in
                summary()
            },
            .command("nearbyReceive", "Wait for a nearby device's certificate key (as Get Signing from Another Device does)") { _ in
                receiver.start()
                return summary()
            },
            .command("nearbyBrowse", "Look for nearby Apparitions waiting for a key (as Share Signing does)") { _ in
                sender.startBrowsing()
                try await Task.sleep(for: .seconds(3))
                return summary()
            },
            .command("nearbySend", "Hand the signed-in Apple ID's key to a receiver found by nearbyBrowse, or at an address",
                     parameters: [.string("to", "A name from nearbySigning's peers, or ip:port", required: true)]) { args in
                let to = try args.requireString("to")
                guard let email = account.email ?? account.lastAppleID else { throw NearbySigning.Failure(description: "no Apple ID") }
                if let peer = sender.peers.first(where: { $0.name == to }) {
                    sender.send(to: peer, email: email, teamId: account.team?.id)
                } else {
                    sender.send(toAddress: to, email: email, teamId: account.team?.id)
                }
                try await Task.sleep(for: .seconds(2))
                return summary()
            },
            .command("nearbyConfirm", "Answer the code comparison on this device, as the person would",
                     parameters: [.string("side", "Which side answers", required: true, choices: ["receiver", "sender"]),
                                  .boolean("matches", "Whether the codes match", default: true)]) { args in
                let matches = args.bool("matches") ?? true
                if try args.requireString("side") == "sender" { await sender.confirm(matches) } else { await receiver.confirm(matches) }
                try await Task.sleep(for: .seconds(1))
                return summary()
            },
            .command("nearbyStop", "Stop waiting and browsing") { _ in
                receiver.stop()
                sender.stopBrowsing()
                sender.reset()
                return summary()
            },
        ])
    }
}
