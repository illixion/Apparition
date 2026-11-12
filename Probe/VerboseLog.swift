import DebugTrace
import Foundation

/// The verbose record of a sign-in: what Apparition sends to Apple's servers and what they answer,
/// written by the core (core/src/verbose.rs) to `apparition-verbose.log` in Documents, which the
/// Files app shows. It holds structure and sizes, never a password, token, key or anisette value;
/// Apple's own error text can still name a team or an app ID, so it is read before it is shared.
///
/// Off unless switched on (Settings > Advanced, or the `setVerboseLog` debug endpoint). It is a file
/// of its own, not part of `apparition-core.log`, so nothing that reports that log carries it.
enum VerboseLog {
    static let key = "verboseLog"
    static let fileName = "apparition-verbose.log"
    /// The longest piece the debug endpoint returns in one reply.
    static let longestTail = 512 * 1024

    static var url: URL { AppFolders.documents.appendingPathComponent(fileName) }
    private static var olderURL: URL { AppFolders.documents.appendingPathComponent(fileName + ".1") }

    static var isOn: Bool { UserDefaults.standard.bool(forKey: key) }

    /// Called once the core's logging is set up, so a switch left on carries across launches.
    static func applyAtLaunch() {
        apparition_set_verbose(isOn)
    }

    static func set(_ on: Bool) {
        UserDefaults.standard.set(on, forKey: key)
        apparition_set_verbose(on)
    }

    static var bytes: Int {
        (try? url.resourceValues(forKeys: [.fileSizeKey]).fileSize) ?? 0
    }

    /// The file's last `maxBytes`, from the start of a line, and whether that cut off the start.
    static func tail(maxBytes: Int) -> (text: String, cut: Bool) {
        guard let handle = try? FileHandle(forReadingFrom: url) else { return ("", false) }
        defer { try? handle.close() }
        let size = (try? handle.seekToEnd()) ?? 0
        let start = size > UInt64(maxBytes) ? size - UInt64(maxBytes) : 0
        guard (try? handle.seek(toOffset: start)) != nil, let data = try? handle.readToEnd() else { return ("", false) }
        var text = String(decoding: data, as: UTF8.self)
        if start > 0, let newline = text.firstIndex(of: "\n") {
            text = String(text[text.index(after: newline)...])
        }
        return (text, start > 0)
    }

    static func clear() {
        try? FileManager.default.removeItem(at: url)
        try? FileManager.default.removeItem(at: olderURL)
    }
}

/// What an agent can do with it through the debug server, so the file doesn't have to be fetched
/// off the device with Xcode's tools.
@MainActor
enum VerboseLogEndpoints {
    struct State: Encodable {
        let enabled: Bool
        /// The file's name inside Apparition's Documents folder.
        let file: String
        let bytes: Int
        /// True when `text` starts partway into the file because it is longer than `maxKB`.
        let cutAtStart: Bool
        let text: String
    }

    static func state(maxKB: Int) -> State {
        let (text, cut) = VerboseLog.tail(maxBytes: min(max(maxKB, 1) * 1024, VerboseLog.longestTail))
        return State(enabled: VerboseLog.isOn, file: VerboseLog.fileName, bytes: VerboseLog.bytes, cutAtStart: cut, text: text)
    }

    static func register() {
        let size: DebugParameter = .integer("maxKB", "How much of the end of the record to return, in KB", default: 64, range: 1...512)
        DebugSurface.shared.register([
            .query("verboseLog", "The verbose record of sign-ins: each request Apparition sends to Apple's servers and the answer, as structure and sizes (no passwords, tokens or anisette values). Off until setVerboseLog turns it on; returns the end of the file",
                   parameters: [size]) { args in
                state(maxKB: args.int("maxKB") ?? 64)
            },
            .command("setVerboseLog", "Turn the verbose record of sign-ins on or off, and/or empty it. Then trigger a sign-in on the device (the password is typed there, never here) and read it with verboseLog",
                     parameters: [
                        .boolean("enabled", "On or off; left out, it stays as it is"),
                        .boolean("clear", "Empty the record first", default: false),
                        size,
                     ]) { args in
                if args.bool("clear") == true { VerboseLog.clear() }
                if let enabled = args.bool("enabled") { VerboseLog.set(enabled) }
                return state(maxKB: args.int("maxKB") ?? 64)
            },
        ])
    }
}
