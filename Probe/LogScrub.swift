import Foundation
import os

/// Error text from Apple, installd or the core made safe to log as public. That text is worth
/// keeping in the log (it is how a failure gets diagnosed), but it can carry who the person is:
/// the Apple ID's email (a free account's certificate is named after it), the team ID (also the
/// suffix of every bundle ID Apparition installs) and the team's name, which on a free account
/// is the person's own. Their devices are theirs too (a name such as "Sam's Apple Vision Pro",
/// an identifier), and so are their own servers' addresses and names (an anisette server, a
/// companion). Each becomes a placeholder; the rest of the text stays.
///
/// A log line is what a problem report carries (Settings > Report a Problem), so a kind of value
/// added here gets a seed in ApparitionTests/ReportPrivacyTests.swift.
enum LogScrub {
    private static let names = OSAllocatedUnfairLock(initialState: Set<String>())

    private static let email = try! NSRegularExpression(pattern: #"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}"#)
    /// Ten capitals and digits with at least one of each: a team ID (or a certificate's ID,
    /// written the same way beside its name).
    private static let teamId = try! NSRegularExpression(pattern: #"\b(?=[A-Z0-9]*[0-9])(?=[A-Z0-9]*[A-Z])[A-Z0-9]{10}\b"#)
    private static let url = try! NSRegularExpression(pattern: #"\b[a-z][a-z0-9+.-]*://[^\s)"']+"#, options: .caseInsensitive)
    /// IPv4 addresses, with a port if any, except the ones Apparition itself uses: LocalDevVPN's
    /// and loopback say how it connected, not where the person is.
    private static let address = try! NSRegularExpression(pattern: #"\b(?!10\.7\.0\.1\b)(?!127\.0\.0\.1\b)(?:\d{1,3}\.){3}\d{1,3}(?::\d+)?\b"#)
    /// A device's identifier: 8 hex digits, a dash and 16 more (since 2018), 40 hex digits
    /// before that, or a UUID (a simulator's, CoreDevice's, and a provisioning profile's, which
    /// names the devices it lists).
    private static let deviceId = try! NSRegularExpression(pattern: #"\b(?:[0-9A-Fa-f]{8}-[0-9A-Fa-f]{16}|[0-9A-Fa-f]{40}|[0-9A-Fa-f]{8}(?:-[0-9A-Fa-f]{4}){3}-[0-9A-Fa-f]{12})\b"#)
    /// Sixteen hex digits or more in a row: a certificate's serial number, a key's fingerprint.
    private static let longHex = try! NSRegularExpression(pattern: #"\b[0-9A-Fa-f]{16,}\b"#)
    /// Groups of hex digits between colons: an IPv6 address (a tailnet's, a link-local one with
    /// its `%en0`) or a hardware address. `isAddress` keeps a time of day out of it.
    private static let colons = try! NSRegularExpression(pattern: #"(?<![0-9A-Za-z:.])(?:[0-9A-Fa-f]{0,4}:){2,7}[0-9A-Fa-f]{0,4}(?:%[0-9A-Za-z.]+)?(?![0-9A-Za-z:])"#)
    /// A named host with its port (`studio.local:49160`), which is how the core names a
    /// companion or a server it couldn't reach. Three digits or more, so `Domain:61` stays.
    private static let hostAndPort = try! NSRegularExpression(pattern: #"(?<![A-Za-z0-9.@/_-])(?=[A-Za-z0-9.-]*[A-Za-z])[A-Za-z0-9](?:[A-Za-z0-9.-]*[A-Za-z0-9])?:\d{3,5}\b"#)
    /// A name only a home network or a tailnet gives out.
    private static let privateHost = try! NSRegularExpression(pattern: #"(?<![A-Za-z0-9.@/_-])[A-Za-z0-9](?:[A-Za-z0-9-]*[A-Za-z0-9])?(?:\.[A-Za-z0-9-]+)*\.(?:local|ts\.net|lan|home\.arpa|internal)\b"#, options: .caseInsensitive)

    /// Names a device has before anyone names it, and the addresses Apparition itself uses.
    /// Never taken out: they say what kind of device it is and how it connected, not whose.
    private static let genericNames: Set<String> = [
        "Apple Vision Pro", "iPhone", "iPad", "Apple TV", "Apple Watch", "Mac",
        "localhost", "127.0.0.1", ProbeModel.vpnLoopback, ProbeModel.relayHost,
    ]

    /// Team and certificate names to take out, as they become known (sign-in, imports).
    static func remember(_ newNames: some Sequence<String?>) {
        let kept = newNames.compactMap { $0?.trimmingCharacters(in: .whitespaces) }.filter { $0.count >= 3 }
        guard !kept.isEmpty else { return }
        names.withLock { $0.formUnion(kept) }
    }

    /// What this copy was told about the person's own devices and servers, read when a line is
    /// scrubbed so a change takes effect at once: the paired device's name and identifier, the
    /// companion's name, and the hosts of the companion and the anisette server. The patterns
    /// catch most of these by shape; a name has no shape.
    private static func known() -> [(value: String, placeholder: String)] {
        let defaults = UserDefaults.standard
        let values: [(String?, String)] = [
            (defaults.string(forKey: "pairedName"), "<name>"),
            (defaults.string(forKey: "companionName"), "<name>"),
            (defaults.string(forKey: "pairedUDID"), "<device>"),
            (host(ofAddress: defaults.string(forKey: "relay")), "<host>"),
            (defaults.string(forKey: "anisetteURL").flatMap { URL(string: $0)?.host }, "<host>"),
        ]
        return values.compactMap { value, placeholder in
            guard let value = value?.trimmingCharacters(in: .whitespaces), value.count >= 3,
                  !genericNames.contains(value) else { return nil }
            return (value, placeholder)
        }
    }

    /// `host:port` or `[v6]:port` without its port.
    private static func host(ofAddress address: String?) -> String? {
        guard let address, !address.isEmpty else { return nil }
        if address.hasPrefix("["), let close = address.firstIndex(of: "]") {
            return String(address[address.index(after: address.startIndex)..<close])
        }
        guard let colon = address.lastIndex(of: ":"), address.firstIndex(of: ":") == colon else { return address }
        return String(address[..<colon])
    }

    static func text(_ text: String) -> String {
        var out = text
        let exact = names.withLock { $0.map { (value: $0, placeholder: "<name>") } } + known()
        // Longest first, so a name that contains another goes whole.
        for (value, placeholder) in exact.sorted(by: { $0.value.count > $1.value.count }) {
            out = out.replacingOccurrences(of: value, with: placeholder)
        }
        out = replace(email, in: out, with: "<email>")
        out = replace(url, in: out, with: "<url>")
        out = replace(deviceId, in: out, with: "<device>")
        out = replace(longHex, in: out, with: "<hex>")
        out = replace(address, in: out, with: "<address>")
        out = replace(colons, in: out, with: "<address>", where: isAddress)
        out = replace(hostAndPort, in: out, with: "<address>") { !isPublicService($0) }
        out = replace(privateHost, in: out, with: "<host>")
        return replace(teamId, in: out, with: "<team>")
    }

    /// Hosts every copy talks to: naming one says which service failed, not whose server it is.
    private static let publicServices = ["apple.com", "icloud.com", "github.com", "githubusercontent.com"]

    private static func isPublicService(_ hostAndPort: Substring) -> Bool {
        let host = hostAndPort.prefix { $0 != ":" }.lowercased()
        return publicServices.contains { host == $0 || host.hasSuffix("." + $0) }
    }

    /// Whether colon-separated hex is an address rather than a time (`12:34:56`): it skips a
    /// run of zeros (`::`), or has the six groups of a hardware address or more.
    private static func isAddress(_ candidate: Substring) -> Bool {
        candidate.contains("::") || candidate.filter { $0 == ":" }.count >= 5
    }

    private static func replace(_ pattern: NSRegularExpression, in text: String, with template: String,
                                where accept: ((Substring) -> Bool)? = nil) -> String {
        guard let accept else {
            return pattern.stringByReplacingMatches(in: text, range: NSRange(text.startIndex..., in: text), withTemplate: template)
        }
        var out = text
        // Last match first, so the ranges of the earlier ones still hold.
        for match in pattern.matches(in: text, range: NSRange(text.startIndex..., in: text)).reversed() {
            guard let range = Range(match.range, in: out), accept(out[range]) else { continue }
            out.replaceSubrange(range, with: template)
        }
        return out
    }
}
