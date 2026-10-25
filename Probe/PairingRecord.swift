import CryptoKit
import DebugTrace
import Foundation
import Security

private let log = DebugLogger(subsystem: Bundle.main.bundleIdentifier ?? "Apparition", category: "Pairing")

/// Apparition's RemotePairing record with the device it runs on
/// (Documents/apparition_rp_pairing.plist), and a copy of it in the Keychain.
///
/// On iPhone and iPad the record only comes from a computer: `apparition pair --usb` makes it
/// and installing Apparition (or `apparition pairing hand-off`) puts it in Documents. Deleting
/// Apparition, or reinstalling it some other way, empties Documents and nothing brings it back.
/// The Keychain keeps an app's items across that (for a copy signed by the same team), so a
/// copy there restores the file at launch. The copy is this-device-only and never syncs to
/// iCloud. tvOS has StateVault, which already keeps the record in the Keychain with the rest.
///
/// The record holds the private key the device trusts: it is never logged, nor is anything
/// read from it.
enum PairingRecord {
    /// What stops Apparition reaching its device.
    enum Problem: String, Codable, Sendable {
        /// No record: never paired, or the file went with a reinstall.
        case missing
        /// The device turned the record down at pair-verify: it no longer trusts it.
        case rejected
    }

    static var url: URL { AppFolders.documents.appendingPathComponent("apparition_rp_pairing.plist") }

    static var exists: Bool { FileManager.default.fileExists(atPath: url.path) }

    private static let service = "com.illixion.apparition.pairing"
    private static let account = "apparition_rp_pairing.plist"
    /// The fingerprint of the record the device last turned down.
    private static let rejectedKey = "rejectedPairing"

    /// Whether the Keychain copy is kept here (StateVault does it on tvOS).
    private static var keepsCopy: Bool { !StateVault.isActive }

    /// Brings the file back from the Keychain when it's missing, or puts the file's current
    /// contents in the Keychain. Run at launch and whenever the app comes to the front (a
    /// computer may have handed a new record over meanwhile). Returns the problem, if any.
    @discardableResult
    static func sync() -> Problem? {
        // An existing file that can't be read (data protection, while locked) isn't missing:
        // it's left alone.
        if keepsCopy {
            if exists {
                if let data = try? Data(contentsOf: url), storedCopy() != data {
                    let status = store(data)
                    if status == errSecSuccess {
                        log.info("pairing copied to the keychain")
                    } else {
                        log.error("pairing keychain copy failed: \(status, privacy: .public)")
                    }
                }
            } else if let data = storedCopy() {
                do {
                    try data.write(to: url, options: [.atomic, .completeFileProtectionUntilFirstUserAuthentication])
                    log.info("pairing restored from the keychain")
                } catch {
                    log.error("pairing restore failed: \((error as NSError).domain, privacy: .public) \((error as NSError).code, privacy: .public)")
                }
            }
        }
        return problem()
    }

    /// What's wrong with the pairing on disk now, or nil when there's a record the device
    /// hasn't turned down.
    static func problem() -> Problem? {
        guard exists else { return .missing }
        guard let rejected = UserDefaults.standard.string(forKey: rejectedKey),
              let data = try? Data(contentsOf: url) else { return nil }
        return fingerprint(data) == rejected ? .rejected : nil
    }

    /// Remembers that the device turned down the record on disk now. A different record (a new
    /// hand-over, pairing again) clears it, as does any connection that gets through.
    static func markRejected() {
        guard let data = try? Data(contentsOf: url) else { return }
        UserDefaults.standard.set(fingerprint(data), forKey: rejectedKey)
    }

    static func clearRejected() {
        UserDefaults.standard.removeObject(forKey: rejectedKey)
    }

    /// Deletes the file and the Keychain copy.
    static func delete() {
        try? FileManager.default.removeItem(at: url)
        SecItemDelete([kSecClass as String: kSecClassGenericPassword,
                       kSecAttrService as String: service] as CFDictionary)
        clearRejected()
        // tvOS: drop the vault's copy now rather than when the app next leaves the front.
        StateVault.sync()
    }

    /// A short digest that tells records apart without keeping anything of their keys.
    private static func fingerprint(_ data: Data) -> String {
        SHA256.hash(data: data).prefix(8).map { String(format: "%02x", $0) }.joined()
    }

    private static func storedCopy() -> Data? {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
            kSecReturnData as String: true,
        ]
        var result: CFTypeRef?
        let status = SecItemCopyMatching(query as CFDictionary, &result)
        if status != errSecSuccess && status != errSecItemNotFound {
            log.error("pairing keychain read failed: \(status, privacy: .public)")
        }
        return result as? Data
    }

    private static func store(_ data: Data) -> OSStatus {
        let match: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
        ]
        let status = SecItemUpdate(match as CFDictionary, [kSecValueData as String: data] as CFDictionary)
        guard status == errSecItemNotFound else { return status }
        var item = match
        // Readable after the first unlock since boot, so a background refresh can restore it;
        // never leaves this device (no iCloud Keychain, not in backups to another device).
        item[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
        item[kSecAttrSynchronizable as String] = false
        item[kSecValueData as String] = data
        return SecItemAdd(item as CFDictionary, nil)
    }

    // MARK: What to tell people

    /// How to get a pairing on this platform, as a sentence or two.
    static var howToPair: String {
        #if os(visionOS)
        "Pair in Setup > Pair This Headset, with Settings > General > Remote Devices open beside Apparition."
        #elseif os(tvOS)
        "Open Settings > Remotes and Devices > Remote App and Devices on this Apple TV, run apparition pair on a computer on the same network (the Apple TV shows a code twice), then apparition pairing hand-off."
        #else
        "On a computer that this \(ThisDevice.noun) trusts, run apparition pair --usb, then apparition pairing hand-off (or install Apparition again from the Apparition desktop app). Wi-Fi is enough: a cable is only needed while the \(ThisDevice.noun) doesn't trust the computer yet."
        #endif
    }

    static func message(_ problem: Problem) -> String {
        switch problem {
        case .missing:
            "Apparition isn't paired with this \(ThisDevice.noun), so it can't reach it to install apps or enable JIT. \(howToPair)"
        case .rejected:
            "This \(ThisDevice.noun) no longer trusts Apparition's pairing, so Apparition can't reach it. Pair again: \(howToPair)"
        }
    }

    /// The problem an action's outcome ("error: …") reports, if it was one of these.
    static func problem(in outcome: String) -> Problem? {
        [Problem.missing, .rejected].first { outcome == "error: \(message($0))" }
    }
}

/// The device turned down Apparition's pairing at pair-verify (apparition_tunnel_create's
/// "pairingRejected").
struct PairingRejected: Error, CustomStringConvertible {
    var description: String { PairingRecord.message(.rejected) }
}

/// An action that needs the pairing ran without one.
struct PairingMissing: Error, CustomStringConvertible {
    var description: String { PairingRecord.message(.missing) }
}
