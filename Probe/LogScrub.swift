import Foundation
import os

/// Error text from Apple, installd or the core made safe to log as public. That text is worth
/// keeping in the log (it is how a failure gets diagnosed), but it can carry who the person is:
/// the Apple ID's email (a free account's certificate is named after it), the team ID (also the
/// suffix of every bundle ID Apparition installs) and the team's name, which on a free account
/// is the person's own. Each becomes a placeholder; the rest of the text stays.
enum LogScrub {
    private static let names = OSAllocatedUnfairLock(initialState: Set<String>())

    private static let email = try! NSRegularExpression(pattern: #"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}"#)
    /// Ten capitals and digits with at least one of each: a team ID (or a certificate's ID,
    /// written the same way beside its name).
    private static let teamId = try! NSRegularExpression(pattern: #"\b(?=[A-Z0-9]*[0-9])(?=[A-Z0-9]*[A-Z])[A-Z0-9]{10}\b"#)

    /// Team and certificate names to take out, as they become known (sign-in, imports).
    static func remember(_ newNames: some Sequence<String?>) {
        let kept = newNames.compactMap { $0?.trimmingCharacters(in: .whitespaces) }.filter { $0.count >= 3 }
        guard !kept.isEmpty else { return }
        names.withLock { $0.formUnion(kept) }
    }

    static func text(_ text: String) -> String {
        // Longest first, so a name that contains another goes whole.
        var out = text
        for name in names.withLock({ $0 }).sorted(by: { $0.count > $1.count }) {
            out = out.replacingOccurrences(of: name, with: "<name>")
        }
        out = replace(email, in: out, with: "<email>")
        return replace(teamId, in: out, with: "<team>")
    }

    private static func replace(_ pattern: NSRegularExpression, in text: String, with template: String) -> String {
        pattern.stringByReplacingMatches(in: text, range: NSRange(text.startIndex..., in: text), withTemplate: template)
    }
}
