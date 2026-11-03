import Foundation
import Network
import RAVECompanion
import DebugTrace

private let log = DebugLogger(subsystem: Bundle.main.bundleIdentifier ?? "Apparition", category: "Companion")

private final class ResumeOnce: @unchecked Sendable {
    private let lock = NSLock()
    private var resumed = false
    func claim() -> Bool {
        lock.lock(); defer { lock.unlock() }
        guard !resumed else { return false }
        resumed = true
        return true
    }
}

/// A loopback-only adapter so isideload's existing anisette-v3 client can use the encrypted
/// companion capability without learning about Network.framework or storing its PSK token.
@MainActor
final class CompanionAnisetteProxy {
    private var listener: NWListener?

    func start(for address: String) async throws -> String {
        stop()
        guard let components = URLComponents(string: address),
              components.scheme == "apparition-anisette",
              let host = components.host, let portValue = components.port,
              let port = UInt16(exactly: portValue),
              let deviceID = components.path.split(separator: "/").first.map(String.init),
              let token = CompanionCredentialStore.load(host: host, port: port, deviceID: deviceID),
              let remotePort = NWEndpoint.Port(rawValue: port) else {
            throw ProxyError.invalidAddress
        }
        let credential = RAVECompanionCredential(token: token, deviceID: deviceID)
        let channel = RAVECompanionChannel(identityBase: "Apparition/Anisette", salt: "Apparition-Anisette-v1")
        let parameters = NWParameters.tcp
        parameters.requiredLocalEndpoint = .hostPort(host: .ipv4(.loopback), port: .any)
        let listener = try NWListener(using: parameters)
        self.listener = listener
        listener.newConnectionHandler = { [weak self] local in
            guard let self else { local.cancel(); return }
            Task { await self.forward(local, host: host, port: remotePort, credential: credential, channel: channel) }
        }
        let localPort: UInt16 = try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<UInt16, Error>) in
            let gate = ResumeOnce()
            listener.stateUpdateHandler = { state in
                switch state {
                case .ready:
                    guard gate.claim() else { return }
                    if let value = listener.port?.rawValue { continuation.resume(returning: value) }
                    else { continuation.resume(throwing: ProxyError.listenerFailed) }
                case .failed(let error):
                    guard gate.claim() else { return }
                    continuation.resume(throwing: error)
                default: break
                }
            }
            listener.start(queue: DispatchQueue(label: "apparition-anisette-loopback"))
        }
        return "http://127.0.0.1:\(localPort)"
    }

    func stop() {
        listener?.cancel()
        listener = nil
    }

    private func forward(_ local: NWConnection, host: String, port: NWEndpoint.Port,
                         credential: RAVECompanionCredential, channel: RAVECompanionChannel) async {
        let remote = NWConnection(host: NWEndpoint.Host(host), port: port,
                                  using: channel.tcpParameters(credentials: [credential], connectionTimeout: 12))
        do {
            try await Self.ready(local)
            try await Self.ready(remote)
            guard let identify = RAVECompanionIdentify.make(
                .apparition, credential: credential,
                exporterLabel: "EXPORTER-Apparition-Anisette-Device-v1", connection: remote
            ) else { throw ProxyError.identifyFailed }
            let body = identify.encoded()
            var frame = UInt32(body.count).littleEndian
            let header = withUnsafeBytes(of: &frame) { Data($0) }
            try await Self.send(header + body, on: remote)
            async let upstream: Void = Self.pipe(from: local, to: remote)
            async let downstream: Void = Self.pipe(from: remote, to: local)
            _ = try await (upstream, downstream)
        } catch {
            log.error("secure anisette channel failed: \(LogScrub.text(String(describing: error)), privacy: .public)")
        }
        local.cancel()
        remote.cancel()
    }

    private static func ready(_ connection: NWConnection) async throws {
        try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Void, Error>) in
            let gate = ResumeOnce()
            connection.stateUpdateHandler = { state in
                switch state {
                case .ready:
                    guard gate.claim() else { return }
                    continuation.resume()
                case .failed(let error):
                    guard gate.claim() else { return }
                    continuation.resume(throwing: error)
                case .cancelled:
                    guard gate.claim() else { return }
                    continuation.resume(throwing: ProxyError.cancelled)
                default: break
                }
            }
            connection.start(queue: DispatchQueue(label: "apparition-anisette-channel"))
        }
    }

    private static func send(_ data: Data, on connection: NWConnection) async throws {
        try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Void, Error>) in
            connection.send(content: data, completion: .contentProcessed { error in
                if let error { continuation.resume(throwing: error) }
                else { continuation.resume() }
            })
        }
    }

    private static func pipe(from source: NWConnection, to destination: NWConnection) async throws {
        while true {
            let (data, complete) = try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<(Data, Bool), Error>) in
                source.receive(minimumIncompleteLength: 1, maximumLength: 64 * 1024) { data, _, complete, error in
                    if let error { continuation.resume(throwing: error) }
                    else { continuation.resume(returning: (data ?? Data(), complete)) }
                }
            }
            if !data.isEmpty { try await send(data, on: destination) }
            if complete { return }
        }
    }

    private enum ProxyError: Error {
        case invalidAddress, listenerFailed, identifyFailed, cancelled
    }
}
