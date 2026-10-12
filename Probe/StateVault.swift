import DebugTrace
import Foundation
import Security

private let log = DebugLogger(subsystem: Bundle.main.bundleIdentifier ?? "Apparition", category: "Vault")

/// Keeps a copy of Apparition's irreplaceable files in the Keychain, for tvOS.
///
/// tvOS gives an app only Library/Caches to write to (AppFolders), and the system may clear
/// Caches whenever storage runs low. Most of what lives there can be fetched or rebuilt (kept
/// IPAs, the device's app list, logs), but not the pairing record, imported certificates and
/// their keys, profiles, the Apple ID's certificate key (storage.json) or the library. Those are
/// small, so each is mirrored as one Keychain item (account = its path under AppFolders), and the
/// Keychain is never cleared by the system.
///
/// A marker file next to them tells a purge apart from a deletion: while it exists the files are
/// what's current, and the Keychain follows them (removed files are removed there too). When it's
/// gone, Caches was cleared (or this is the first launch), so whatever is missing comes back from
/// the Keychain first. Files already there, such as a pairing record Apparition Installer just
/// handed over, are never overwritten.
enum StateVault {
    /// Whether the vault runs on this platform. Elsewhere Documents and Application Support persist.
    static var isActive: Bool {
        #if os(tvOS)
        true
        #else
        false
        #endif
    }

    private static let service = "com.illixion.apparition.vault"
    /// Larger files aren't irreplaceable state (an IPA, a log), and don't belong in the Keychain.
    private static let maxSize = 512 * 1024
    private static let markerName = ".vault"

    /// The folder AppFolders.documents and .support sit in, which a purge empties.
    private static var root: URL { AppFolders.support.deletingLastPathComponent() }
    private static var marker: URL { root.appendingPathComponent(markerName) }

    struct Report: Encodable {
        var restored: [String] = []
        var saved: [String] = []
        var removed: [String] = []
        var failed: [String] = []
        var purged = false
    }

    /// Paths under `root` worth keeping: everything in the data directory except what Apparition
    /// rebuilds, plus the pairing record in Documents.
    private static func trackedFiles() -> [String] {
        let fm = FileManager.default
        var paths: [String] = []
        let pairing = AppFolders.documents.appendingPathComponent("apparition_rp_pairing.plist")
        if fm.fileExists(atPath: pairing.path) { paths.append(relative(pairing)) }
        let data = AccountModel.dataDir
        guard let walker = fm.enumerator(at: data, includingPropertiesForKeys: [.isRegularFileKey, .fileSizeKey]) else {
            return paths
        }
        for case let url as URL in walker {
            let values = try? url.resourceValues(forKeys: [.isRegularFileKey, .fileSizeKey])
            guard values?.isRegularFile == true, (values?.fileSize ?? 0) <= maxSize else { continue }
            if url.lastPathComponent == "device-apps.json" || url.pathExtension == "log" { continue }
            paths.append(relative(url))
        }
        return paths.sorted()
    }

    private static func relative(_ url: URL) -> String {
        let base = root.standardizedFileURL.path
        let path = url.standardizedFileURL.path
        return path.hasPrefix(base + "/") ? String(path.dropFirst(base.count + 1)) : path
    }

    /// Restores what a purge removed, then mirrors the files into the Keychain. Run at launch,
    /// before anything reads these files, and whenever the app leaves the foreground.
    @discardableResult
    static func sync() -> Report {
        var report = Report()
        guard isActive else { return report }
        let fm = FileManager.default
        let stored = storedItems()
        let tracked = trackedFiles()
        // A missing marker means a purge; so does a marker with none of the files left (should
        // the system ever clear files one by one), as long as the Keychain has some.
        let purged = !fm.fileExists(atPath: marker.path) || (tracked.isEmpty && !stored.isEmpty)
        if purged {
            report.purged = !stored.isEmpty
            for (path, data) in stored where !fm.fileExists(atPath: root.appendingPathComponent(path).path) {
                let url = root.appendingPathComponent(path)
                do {
                    try fm.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
                    try data.write(to: url, options: .atomic)
                    report.restored.append(path)
                } catch {
                    report.failed.append("\(path): \(error.localizedDescription)")
                }
            }
            try? fm.createDirectory(at: root, withIntermediateDirectories: true)
            fm.createFile(atPath: marker.path, contents: Data())
        }
        let current = trackedFiles()
        for path in current {
            guard let data = try? Data(contentsOf: root.appendingPathComponent(path)) else { continue }
            if stored[path] == data { continue }
            let status = put(path, data)
            if status == errSecSuccess { report.saved.append(path) } else { report.failed.append("\(path): OSStatus \(status)") }
        }
        // Remove what was deleted on purpose, unless a restore just failed (the file would be
        // missing because of that, not because it was removed).
        if report.failed.isEmpty {
            for path in Set(stored.keys).subtracting(current) {
                delete(path)
                report.removed.append(path)
            }
        }
        if !report.restored.isEmpty || !report.saved.isEmpty || !report.removed.isEmpty || !report.failed.isEmpty {
            log.info("vault: restored \(report.restored.count, privacy: .public), saved \(report.saved.count, privacy: .public), removed \(report.removed.count, privacy: .public), failed \(report.failed.count, privacy: .public): \(report.failed)")
        }
        return report
    }

    struct Status: Encodable {
        let active: Bool
        let marker: Bool
        let files: [String]
        let stored: [String: Int]
        let missing: [String]
    }

    /// What's in the Keychain (path and size) next to what's on disk.
    static func status() -> Status {
        let stored = storedItems()
        let files = trackedFiles()
        return Status(active: isActive, marker: FileManager.default.fileExists(atPath: marker.path), files: files,
                      stored: stored.mapValues(\.count), missing: Set(stored.keys).subtracting(files).sorted())
    }

    /// Deletes everything under `root`, as a storage purge would. Development only.
    static func simulatePurge() throws -> [String] {
        // Elsewhere `root` is the whole Library folder.
        guard isActive else { return [] }
        let fm = FileManager.default
        let entries = (try? fm.contentsOfDirectory(atPath: root.path)) ?? []
        for entry in entries { try fm.removeItem(at: root.appendingPathComponent(entry)) }
        return entries
    }

    // MARK: Keychain

    private static func storedItems() -> [String: Data] {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecMatchLimit as String: kSecMatchLimitAll,
            kSecReturnAttributes as String: true,
            kSecReturnData as String: true,
        ]
        var result: CFTypeRef?
        let status = SecItemCopyMatching(query as CFDictionary, &result)
        if status != errSecSuccess && status != errSecItemNotFound {
            log.error("vault: keychain read failed: \(status, privacy: .public)")
        }
        guard let items = result as? [[String: Any]] else { return [:] }
        var stored: [String: Data] = [:]
        for item in items {
            if let path = item[kSecAttrAccount as String] as? String, let data = item[kSecValueData as String] as? Data {
                stored[path] = data
            }
        }
        return stored
    }

    private static func put(_ path: String, _ data: Data) -> OSStatus {
        let match: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: path,
        ]
        let status = SecItemUpdate(match as CFDictionary, [kSecValueData as String: data] as CFDictionary)
        guard status == errSecItemNotFound else { return status }
        var item = match
        item[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
        item[kSecValueData as String] = data
        return SecItemAdd(item as CFDictionary, nil)
    }

    private static func delete(_ path: String) {
        SecItemDelete([kSecClass as String: kSecClassGenericPassword,
                       kSecAttrService as String: service,
                       kSecAttrAccount as String: path] as CFDictionary)
    }
}
