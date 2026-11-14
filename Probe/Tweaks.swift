import DebugTrace
import Foundation

private let log = DebugLogger(subsystem: Bundle.main.bundleIdentifier ?? "Apparition", category: "Tweaks")

/// Tweaks kept for each app in the library (core/src/tweaks.rs injects them): the files live in
/// Apparition's container, one folder per app, and go into the app's kept IPA each time it's
/// signed, so refreshes and updates keep them.
enum AppTweaks {
    /// What putting tweaks into an app did, as apparition_inject_tweaks says.
    struct Check: Sendable, Equatable {
        var loads: [String] = []
        var substrate = false
        var warnings: [String] = []
        /// Why they can't go into this app (an encrypted app, code for another platform…).
        var error: String?
    }

    /// The folder holding the tweaks of the library app `id` (a bundle ID or `github:owner/repo`).
    nonisolated static func folder(for id: String) -> URL {
        let safe = id.replacingOccurrences(of: "/", with: "_").replacingOccurrences(of: ":", with: "_")
        return AccountModel.appsDir.appendingPathComponent("Tweaks", isDirectory: true).appendingPathComponent(safe, isDirectory: true)
    }

    /// Copies the file (or .framework/.bundle folder) the person picked into `folder`, under a
    /// name no other tweak there has. Returns that name.
    nonisolated static func keep(_ picked: URL, in folder: URL) throws -> String {
        let scoped = picked.startAccessingSecurityScopedResource()
        defer { if scoped { picked.stopAccessingSecurityScopedResource() } }
        try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
        var name = picked.lastPathComponent
        let stem = picked.deletingPathExtension().lastPathComponent
        let suffix = picked.pathExtension.isEmpty ? "" : ".\(picked.pathExtension)"
        var number = 2
        while FileManager.default.fileExists(atPath: folder.appendingPathComponent(name).path) {
            name = "\(stem) \(number)\(suffix)"
            number += 1
        }
        try FileManager.default.copyItem(at: picked, to: folder.appendingPathComponent(name))
        return name
    }

    /// Deletes what `folder` holds that isn't in `kept`, and the folder when nothing is kept.
    nonisolated static func prune(_ folder: URL, keeping kept: [String]) {
        guard let names = try? FileManager.default.contentsOfDirectory(atPath: folder.path) else { return }
        for name in names where !kept.contains(name) {
            try? FileManager.default.removeItem(at: folder.appendingPathComponent(name))
        }
        if kept.isEmpty { try? FileManager.default.removeItem(at: folder) }
    }

    /// Puts `names` (in `folder`) into the app in the IPA at `ipa`, writing the result to `out`.
    nonisolated static func inject(ipa: String, names: [String], folder: URL, out: String) -> (check: Check, path: String?) {
        let paths = names.map { folder.appendingPathComponent($0).path }
        if let missing = paths.first(where: { !FileManager.default.fileExists(atPath: $0) }) {
            let name = URL(fileURLWithPath: missing).lastPathComponent
            return (Check(error: "The tweak \(name) kept for this app is missing. Review Install Options to add it again."), nil)
        }
        let json = String(data: (try? JSONSerialization.data(withJSONObject: paths)) ?? Data("[]".utf8), encoding: .utf8) ?? "[]"
        let scratch = FileManager.default.temporaryDirectory.appendingPathComponent("tweak-work-\(UUID().uuidString)", isDirectory: true)
        defer { try? FileManager.default.removeItem(at: scratch) }
        try? FileManager.default.createDirectory(at: scratch, withIntermediateDirectories: true)
        let reply = AccountModel.call(apparition_inject_tweaks(ipa, json, out, scratch.path))
        guard reply["ok"] as? Bool == true else {
            return (Check(error: reply["error"] as? String ?? "The tweaks couldn't be added"), nil)
        }
        let check = Check(loads: reply["loads"] as? [String] ?? [], substrate: reply["substrate"] as? Bool == true,
                          warnings: reply["warnings"] as? [String] ?? [])
        log.info("injected \(check.loads.count, privacy: .public) tweaks, hook library \(check.substrate, privacy: .public), \(check.warnings.count, privacy: .public) warnings")
        return (check, out)
    }

    /// Tries `names` against the app in `ipa` without keeping the result: what the review shows
    /// as each tweak is added.
    nonisolated static func check(ipa: String, names: [String], folder: URL) -> Check {
        let out = FileManager.default.temporaryDirectory.appendingPathComponent("tweak-check-\(UUID().uuidString).ipa").path
        defer { try? FileManager.default.removeItem(atPath: out) }
        return inject(ipa: ipa, names: names, folder: folder, out: out).check
    }
}
