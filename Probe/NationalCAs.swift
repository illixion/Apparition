import Foundation
import Security
import DebugTrace

private let log = DebugLogger(subsystem: Bundle.main.bundleIdentifier ?? "Apparition", category: "Network")

/// HTTPS that refuses governments' interception certificate authorities, even where this device
/// trusts them: the same refusal as the core's (core/vendor/isideload/src/tls.rs), for what the app
/// fetches itself. That is code: app sources, GitHub releases, IPAs from a link and builds from the
/// Store, which this device then signs with the person's Apple ID and installs. A source's own
/// SHA-256 can't help there, as it comes over the same connection.
///
/// Russia and Kazakhstan ask people to install national root CAs (on iOS, a profile and Settings >
/// General > About > Certificate Trust Settings), and from then on the device trusts whatever they
/// sign. So after the system has built and evaluated a chain, it is refused when any certificate in
/// it, root included, is one of theirs (by its key or name) or was issued by one, or, for Apple's and
/// GitHub's hosts, was issued by any CA in either country. The certificates are the core's own
/// files, bundled as `national/` (project.yml).
enum NationalCAs {
    struct Authority {
        let label: String
        let subject: Data
        let key: Data
    }

    static let authorities: [Authority] = {
        guard let folder = Bundle.main.url(forResource: "national", withExtension: nil),
              let files = try? FileManager.default.contentsOfDirectory(at: folder, includingPropertiesForKeys: nil) else {
            log.error("the national CA certificates aren't in the app")
            return []
        }
        return files.filter { $0.pathExtension == "der" }.sorted { $0.lastPathComponent < $1.lastPathComponent }.compactMap { file in
            guard let data = try? Data(contentsOf: file), let certificate = SecCertificateCreateWithData(nil, data as CFData),
                  let subject = SecCertificateCopyNormalizedSubjectSequence(certificate) as Data?, let key = publicKey(certificate) else {
                log.error("a national CA certificate didn't load: \(file.lastPathComponent, privacy: .public)")
                return nil
            }
            let country = file.lastPathComponent.hasPrefix("ru-") ? "Russia's" : "Kazakhstan's"
            let name = SecCertificateCopySubjectSummary(certificate) as String? ?? "unnamed"
            return Authority(label: "\(country) national certificate authority (\(name))", subject: subject, key: key)
        }
    }()

    /// Hosts that never get a certificate from a CA in Russia or Kazakhstan.
    static func isAppleOrGitHub(_ host: String) -> Bool {
        let host = host.lowercased().trimmingCharacters(in: CharacterSet(charactersIn: "."))
        return ["apple.com", "mzstatic.com", "icloud.com", "icloud-content.com", "cdn-apple.com", "apple-cloudkit.com", "aaplimg.com",
                "github.com", "githubusercontent.com", "github.io"]
            .contains { host == $0 || host.hasSuffix("." + $0) }
    }

    static func publicKey(_ certificate: SecCertificate) -> Data? {
        guard let key = SecCertificateCopyKey(certificate) else { return nil }
        return SecKeyCopyExternalRepresentation(key, nil) as Data?
    }

    /// Why the chain `certificates` (the server's first) is refused for `host`, or nil.
    static func refusal(_ certificates: [SecCertificate], host: String) -> String? {
        let strict = isAppleOrGitHub(host)
        for certificate in certificates {
            let subject = SecCertificateCopyNormalizedSubjectSequence(certificate) as Data?
            let issuer = SecCertificateCopyNormalizedIssuerSequence(certificate) as Data?
            let key = publicKey(certificate)
            // The certificate itself first, so the message names the most specific CA.
            if let found = authorities.first(where: { $0.key == key || $0.subject == subject })
                ?? authorities.first(where: { $0.subject == issuer }) {
                return found.label
            }
            if strict, let issuer, let country = countries(in: issuer).first(where: { ["RU", "KZ"].contains($0) }) {
                return "a certificate authority in \(country == "RU" ? "Russia" : "Kazakhstan"), which never certifies \(host)"
            }
        }
        return nil
    }

    /// The C= values in a DER name: each `06 03 55 04 06` (the countryName OID), then its string.
    static func countries(in name: Data) -> [String] {
        let bytes = [UInt8](name)
        var found: [String] = []
        var index = 0
        while index + 7 <= bytes.count {
            if bytes[index..<index + 5] == [0x06, 0x03, 0x55, 0x04, 0x06][...] {
                let length = Int(bytes[index + 6])
                if length < 0x80, index + 7 + length <= bytes.count {
                    found.append(String(decoding: bytes[(index + 7)..<(index + 7 + length)], as: UTF8.self).uppercased())
                }
                index += 5
            } else {
                index += 1
            }
        }
        return found
    }

    /// The chain the system built for `trust` (it evaluates it first), anchor included.
    static func chain(_ trust: SecTrust) -> [SecCertificate] {
        _ = SecTrustEvaluateWithError(trust, nil)
        return (SecTrustCopyCertificateChain(trust) as? [SecCertificate]) ?? []
    }

    /// What the app says when a download was refused (the URLError is only "cancelled").
    struct Refused: Error, CustomStringConvertible {
        let host: String
        let why: String
        var description: String {
            "\(host) is certified by \(why), which can read and change what comes through it, so Apparition won't use it, even though this device trusts it. Use another network (or a VPN), or turn that certificate off in Settings > General > About > Certificate Trust Settings."
        }
    }

    private static let lock = NSLock()
    nonisolated(unsafe) private static var refused: [String: String] = [:]

    /// Answers a server-trust challenge: refused (and remembered for `explain`), or left to the system.
    static func handle(_ challenge: URLAuthenticationChallenge) -> (URLSession.AuthChallengeDisposition, URLCredential?) {
        guard challenge.protectionSpace.authenticationMethod == NSURLAuthenticationMethodServerTrust,
              let trust = challenge.protectionSpace.serverTrust else { return (.performDefaultHandling, nil) }
        let host = challenge.protectionSpace.host
        if let why = refusal(chain(trust), host: host) {
            log.error("refused \(host, privacy: .private(mask: .hash)): \(why, privacy: .public)")
            lock.withLock { refused[host.lowercased()] = why }
            return (.cancelAuthenticationChallenge, nil)
        }
        return (.performDefaultHandling, nil)
    }

    /// Runs `request`, turning a refusal's "cancelled" into what happened.
    static func explained<T>(_ url: URL?, _ request: () async throws -> T) async throws -> T {
        do { return try await request() } catch { throw explain(error, url: url) }
    }

    /// `error` as a refusal, when a refusal is why the request to `url` failed.
    static func explain(_ error: Error, url: URL?) -> Error {
        guard (error as? URLError)?.code == .cancelled, let host = url?.host()?.lowercased(),
              let why = lock.withLock({ refused[host] }) else { return error }
        return Refused(host: host, why: why)
    }
}

/// The session the app's own downloads use, with the refusal.
final class SafeSession: NSObject, URLSessionDelegate, @unchecked Sendable {
    static let shared: URLSession = URLSession(configuration: .default, delegate: SafeSession(), delegateQueue: nil)

    func urlSession(_ session: URLSession, didReceive challenge: URLAuthenticationChallenge,
                    completionHandler: @escaping @Sendable (URLSession.AuthChallengeDisposition, URLCredential?) -> Void) {
        let (disposition, credential) = NationalCAs.handle(challenge)
        completionHandler(disposition, credential)
    }

    static func data(for request: URLRequest) async throws -> (Data, URLResponse) {
        do { return try await shared.data(for: request) } catch { throw NationalCAs.explain(error, url: request.url) }
    }

    static func download(from url: URL) async throws -> (URL, URLResponse) {
        do { return try await shared.download(from: url) } catch { throw NationalCAs.explain(error, url: url) }
    }
}
