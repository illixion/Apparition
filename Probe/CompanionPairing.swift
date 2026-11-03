import DebugTrace
import Foundation
import Observation
import RAVECompanion
import Security

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
    case appleRelay
}

/// Pairs this device with a companion and stores its capability token in the device Keychain.
/// The address contains no secret; all service traffic uses the encrypted channel.
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
        if current.scheme == "apparition-anisette" {
            return current.host == offered.host && current.port == Int(companion.anisetteChannelPort ?? 0)
                && URLComponents(url: current, resolvingAgainstBaseURL: false)?.queryItems?.contains(URLQueryItem(name: "relay", value: "1")) == true
        }
        return false
    }

    /// The anisette URL for `companion`: as offered when it serves anyone, after a pairing
    /// when it serves paired devices only. nil when the pairing failed or was cancelled.
    func anisetteURL(for companion: CompanionFinder.Companion) async -> String? {
        guard let offered = companion.anisetteURL else { return nil }
        guard let port = companion.anisettePairingPort else {
            phase = .failed("Update Apparition on that computer to pair over the encrypted channel.")
            return nil
        }
        guard let host = URL(string: offered)?.host else { return nil }
        guard let channelPort = companion.anisetteChannelPort else {
            phase = .failed("This companion doesn't offer the encrypted anisette channel. Update Apparition on that computer, then try again.")
            return nil
        }
        cancel()
        let link = RAVECompanionPairLink(.apparition, macID: "", name: companion.name, port: port, addresses: [host])
        let client = RAVEPairingClient<CompanionGrant>(
            locator: locator, link: link,
            deviceName: UserDefaults.standard.string(forKey: "pairedName") ?? ThisDevice.name,
            requested: [.anisette, .appleRelay])
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
        guard grant.grants?.contains("appleRelay") == true else {
            phase = .failed("Update Apparition on that computer, then pair again to enable Apple connection relay access.")
            return nil
        }
        phase = .idle
        log.info("paired with a companion")
        let account = CompanionCredentialStore.account(host: host, port: channelPort, deviceID: locator.deviceID)
        guard CompanionCredentialStore.save(grant.token, account: account) else {
            phase = .failed("This device couldn't save the companion credential in Keychain.")
            return nil
        }
        var secure = URLComponents()
        secure.scheme = "apparition-anisette"
        secure.host = host
        secure.port = Int(channelPort)
        secure.path = "/\(locator.deviceID)"
        secure.queryItems = [URLQueryItem(name: "relay", value: "1")]
        return secure.string
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

enum CompanionCredentialStore {
    private static let service = "com.illixion.apparition.companion"

    static func account(host: String, port: UInt16, deviceID: String) -> String {
        "\(host.lowercased()):\(port):\(deviceID)"
    }

    static func save(_ token: String, account: String) -> Bool {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
        ]
        let data = Data(token.utf8)
        let update = SecItemUpdate(query as CFDictionary, [kSecValueData as String: data] as CFDictionary)
        guard update == errSecItemNotFound else { return update == errSecSuccess }
        var item = query
        item[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
        item[kSecValueData as String] = data
        return SecItemAdd(item as CFDictionary, nil) == errSecSuccess
    }

    static func load(host: String, port: UInt16, deviceID: String) -> String? {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account(host: host, port: port, deviceID: deviceID),
            kSecReturnData as String: true,
        ]
        var result: CFTypeRef?
        guard SecItemCopyMatching(query as CFDictionary, &result) == errSecSuccess,
              let data = result as? Data else { return nil }
        return String(data: data, encoding: .utf8)
    }

    static func move(host oldHost: String, to newHost: String, port: UInt16, deviceID: String) {
        guard let token = load(host: oldHost, port: port, deviceID: deviceID),
              save(token, account: account(host: newHost, port: port, deviceID: deviceID)) else { return }
        SecItemDelete([kSecClass as String: kSecClassGenericPassword,
                       kSecAttrService as String: service,
                       kSecAttrAccount as String: account(host: oldHost, port: port, deviceID: deviceID)] as CFDictionary)
    }

}
