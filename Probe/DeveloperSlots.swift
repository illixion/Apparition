import DebugTrace
import Foundation

private let log = DebugLogger(subsystem: Bundle.main.bundleIdentifier ?? "Apparition", category: "Slots")

/// The signed-in account's development certificates and App IDs (core/src/account.rs).
/// A free account has two certificate slots and ten App IDs a week; this is what the user
/// looks at, and frees, when signing fails because they're used up. Nothing here acts on
/// its own: revoking and deleting are always the user's choice.
enum DeveloperSlots {
    struct Certificate: Codable, Sendable, Identifiable {
        var id: String { serialNumber ?? name ?? "?" }
        let name: String?
        let serialNumber: String?
        let machineName: String?
        let status: String?
        let expires: String?
        /// "ios", "mac", ... (Apple's certificatePlatform, or the certificate type's platform).
        let platform: String?
        /// Apple's name for the certificate type, e.g. "Apple Development".
        let typeName: String?
        /// Made under Apparition's machine name, so it's the one this app signs with (if its
        /// key is on this device) or one an earlier install of Apparition left behind.
        let madeByApparition: Bool
        /// Its key is the one this headset keeps: Apparition signs with this one (perhaps
        /// shared with the desktop installer).
        let inUseHere: Bool?
    }

    struct AppID: Codable, Sendable, Identifiable {
        var id: String { appIdId }
        let appIdId: String
        let identifier: String
        let name: String
        let expires: String?
    }

    struct AppIDs: Codable, Sendable {
        let appIds: [AppID]
        let maxQuantity: Int?
        let availableQuantity: Int?
    }

    struct Failure: Error, CustomStringConvertible {
        let description: String
    }

    /// Whether a sign or install error is Apple saying a slot is used up: error 7460 (maximum
    /// number of certificates) or the App ID limit. The UI offers this screen next to it.
    static func isSlotError(_ text: String) -> Bool {
        let lower = text.lowercased()
        if lower.contains("7460") || lower.contains("maximum number of certificates") { return true }
        guard lower.contains("app id") else { return false }
        return ["limit", "maximum", "more than", "9401"].contains { lower.contains($0) }
    }

    static func certificates() async throws -> [Certificate] {
        struct Reply: Decodable { let certificates: [Certificate] }
        return try await decode(Reply.self) { apparition_list_certificates() }.certificates
    }

    /// Apps signed with the certificate stop launching until they're signed again.
    static func revokeCertificate(serialNumber: String) async throws {
        _ = try await decode(Empty.self) { apparition_revoke_certificate(serialNumber) }
        log.notice("revoked a development certificate")
    }

    static func appIDs() async throws -> AppIDs {
        try await decode(AppIDs.self) { apparition_list_app_ids() }
    }

    static func deleteAppID(_ appIdId: String) async throws {
        _ = try await decode(Empty.self) { apparition_delete_app_id(appIdId) }
        log.notice("deleted an App ID")
    }

    private struct Empty: Decodable {}

    private static func decode<T: Decodable>(_ type: T.Type,
                                             _ call: @escaping @Sendable () -> UnsafeMutablePointer<CChar>?) async throws -> T {
        let reply = await Task.detached { AccountModel.call(call()) }.value
        guard reply["ok"] as? Bool == true else {
            throw Failure(description: reply["error"] as? String ?? "failed")
        }
        let data = try JSONSerialization.data(withJSONObject: reply)
        return try JSONDecoder().decode(T.self, from: data)
    }
}

@MainActor
enum DeveloperSlotsEndpoints {
    static func register() {
        DebugSurface.shared.register([
            .query("certificates", "The account's development certificates of every platform (a free account has two slots)",
                   timeout: .seconds(60)) { _ in
                try await DeveloperSlots.certificates()
            },
            .command("revokeCertificate", "Revoke a development certificate by serial number. Apps signed with it stop launching until re-signed",
                     parameters: [.string("serialNumber", "From certificates", required: true)],
                     destructive: true, timeout: .seconds(60)) { args in
                try await DeveloperSlots.revokeCertificate(serialNumber: try args.requireString("serialNumber"))
                return try await DeveloperSlots.certificates()
            },
            .query("appIds", "The account's App IDs with expiry, and how many more it may register",
                   timeout: .seconds(60)) { _ in
                try await DeveloperSlots.appIDs()
            },
            .command("deleteAppId", "Delete an App ID by appIdId (Apple may refuse on a free account)",
                     parameters: [.string("appIdId", "From appIds", required: true)],
                     destructive: true, timeout: .seconds(60)) { args in
                try await DeveloperSlots.deleteAppID(try args.requireString("appIdId"))
                return try await DeveloperSlots.appIDs()
            },
        ])
    }
}
