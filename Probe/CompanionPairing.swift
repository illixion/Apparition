import DebugTrace
import Foundation
import Observation
import RAVECompanion

private let log = DebugLogger(subsystem: Bundle.main.bundleIdentifier ?? "Apparition", category: "Companion")

extension RAVECompanionNamespace {
    /// Apparition's companion (the desktop app or `apparition relay`), which pairs devices on the
    /// relay's port plus one. Only the direct (address) route is used: Find Companion already
    /// knows the address, so there is no knock.
    static let apparition = RAVECompanionNamespace(
        serviceStem: "apparition", labelPrefix: "Apparition", urlScheme: "apparition", defaultPairPort: 49161,
        hostAppName: "Apparition on that computer"
    )
}

/// What a device asks a companion for when pairing.
enum CompanionGrant: String, Codable, Hashable, Sendable {
    case anisette
}

/// Pairs this device with a companion whose anisette server serves paired devices only, and
/// hands back the server's URL with this device's token in its path (`http://ip:6969/t/<token>`),
/// which any v3 client takes as its base URL. Both screens show a six-digit code; the person
/// allows the pairing on the computer if they match.
@MainActor @Observable
final class CompanionPairing {
    enum Phase: Equatable {
        case idle
        case waiting(companion: String)
        /// Compare with the computer's screen.
        case confirming(companion: String, code: String)
        case failed(String)
    }

    private(set) var phase: Phase = .idle
    private var client: RAVEPairingClient<CompanionGrant>?
    private var watching: Task<Void, Never>?

    private let locator = RAVECompanionLocator(
        namespace: .apparition,
        deviceID: RAVECompanionLocator.persistentDeviceID(key: "companionDeviceID"),
        deviceName: { UserDefaults.standard.string(forKey: "pairedName") ?? ThisDevice.name }
    )

    /// Whether `url` is an anisette URL a pairing with this companion gave.
    nonisolated static func isPaired(_ url: String, to companion: CompanionFinder.Companion) -> Bool {
        guard let offered = companion.anisetteURL.flatMap(URL.init(string:)), let current = URL(string: url) else { return false }
        return current.host == offered.host && current.port == offered.port && current.path.hasPrefix("/t/")
    }

    /// The anisette URL for `companion`: as offered when it serves anyone, after a pairing
    /// when it serves paired devices only. nil when the pairing failed or was cancelled.
    func anisetteURL(for companion: CompanionFinder.Companion) async -> String? {
        guard let offered = companion.anisetteURL else { return nil }
        guard let port = companion.anisettePairingPort else { return offered }
        guard let host = URL(string: offered)?.host else { return nil }
        cancel()
        let link = RAVECompanionPairLink(.apparition, macID: "", name: companion.name, port: port, addresses: [host])
        let client = RAVEPairingClient<CompanionGrant>(
            locator: locator, link: link,
            deviceName: UserDefaults.standard.string(forKey: "pairedName") ?? ThisDevice.name,
            requested: [.anisette])
        self.client = client
        phase = .waiting(companion: companion.name)
        client.start()
        log.info("pairing with a companion for its anisette")
        let grant: RAVEPairingGrant? = await withCheckedContinuation { continuation in
            watching = Task { [weak self] in
                while !Task.isCancelled {
                    switch client.state {
                    case .confirming(let code):
                        self?.phase = .confirming(companion: companion.name, code: RAVEPairingExchange.display(code))
                    case .paired(let grant):
                        return continuation.resume(returning: grant)
                    case .failed(let reason):
                        self?.phase = .failed(reason)
                        return continuation.resume(returning: nil)
                    case .idle:
                        return continuation.resume(returning: nil)
                    case .waitingForHost:
                        break
                    }
                    try? await Task.sleep(for: .milliseconds(200))
                }
                continuation.resume(returning: nil)
            }
        }
        self.client = nil
        watching = nil
        guard let grant else {
            log.info("companion pairing ended without a grant")
            return nil
        }
        phase = .idle
        log.info("paired with a companion")
        var components = URLComponents(string: offered)
        components?.path = "/t/\(grant.token)"
        return components?.string
    }

    func cancel() {
        client?.cancel()
        watching?.cancel()
        if case .failed = phase { return }
        phase = .idle
    }

    func dismissFailure() {
        if case .failed = phase { phase = .idle }
    }
}
