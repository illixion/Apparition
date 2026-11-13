import Foundation
import Security
import Testing

@testable import Apparition

/// The app's own downloads refuse national interception CAs (Probe/NationalCAs.swift), as the
/// core does (core/vendor/isideload/src/tls.rs); these are the core's tests' cases and fixtures.
struct NationalCATests {
    /// The core's fixtures, read from the checkout (the simulator reads the Mac's files).
    static let fixtures = URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent()
        .appendingPathComponent("core/vendor/isideload/src/tls")

    static func certificate(_ name: String) throws -> SecCertificate {
        let data = try Data(contentsOf: fixtures.appendingPathComponent(name))
        return try #require(SecCertificateCreateWithData(nil, data as CFData), "\(name) isn't a certificate")
    }

    @Test func everyNationalCertificateIsInTheApp() throws {
        let files = try FileManager.default.contentsOfDirectory(atPath: Self.fixtures.appendingPathComponent("national").path).filter { $0.hasSuffix(".der") }
        #expect(NationalCAs.authorities.count == files.count)
        #expect(NationalCAs.authorities.count == 12)
    }

    @Test func theNationalCAsThemselvesAreRefusedAnywhere() throws {
        for file in try FileManager.default.contentsOfDirectory(atPath: Self.fixtures.appendingPathComponent("national").path) where file.hasSuffix(".der") {
            let why = NationalCAs.refusal([try Self.certificate("national/\(file)")], host: "example.com")
            #expect(why != nil, "\(file)")
        }
    }

    /// A server certificate issued under Russia's Sub CA's name, alone and with a CA of that name
    /// but another key.
    @Test func whatANationalCAIssuedIsRefusedByItsIssuersName() throws {
        let leaf = try Self.certificate("test/leaf-from-ru-lookalike.der")
        #expect(NationalCAs.refusal([leaf], host: "example.com")?.contains("Russian Trusted Sub CA") == true)
        #expect(NationalCAs.refusal([leaf, try Self.certificate("test/lookalike-ru-sub-ca.der")], host: "example.com") != nil)
    }

    @Test func aCAInRussiaIsRefusedForAppleAndGitHubOnly() throws {
        let chain = [try Self.certificate("test/leaf-from-other-ru-ca.der"), try Self.certificate("test/other-ru-ca.der")]
        #expect(NationalCAs.refusal(chain, host: "gsa.apple.com")?.contains("Russia") == true)
        #expect(NationalCAs.refusal(chain, host: "objects.githubusercontent.com") != nil)
        #expect(NationalCAs.refusal(chain, host: "example.ru") == nil)
    }

    @Test func applesOwnChainsPass() throws {
        let gsa = try (1...3).map { try Self.certificate("test/gsa.apple.com-\($0).der") }
        let cdn = try (1...3).map { try Self.certificate("test/apps.mzstatic.com-\($0).der") }
        #expect(NationalCAs.refusal(gsa, host: "gsa.apple.com") == nil)
        #expect(NationalCAs.refusal(cdn, host: "apps.mzstatic.com") == nil)
    }

    @Test func hosts() {
        for host in ["gsa.apple.com", "github.com", "objects.githubusercontent.com", "API.GITHUB.COM", "apps.mzstatic.com."] {
            #expect(NationalCAs.isAppleOrGitHub(host), "\(host)")
        }
        for host in ["notgithub.com", "github.com.example", "example.ru"] {
            #expect(!NationalCAs.isAppleOrGitHub(host), "\(host)")
        }
    }

    /// A real connection through the app's session to GitHub and Apple works. Needs the internet.
    @Test func connects() async throws {
        for url in ["https://api.github.com/", "https://apps.mzstatic.com/"] {
            var request = URLRequest(url: URL(string: url)!)
            request.setValue("Apparition", forHTTPHeaderField: "User-Agent")
            let (_, response) = try await SafeSession.data(for: request)
            #expect((response as? HTTPURLResponse) != nil, "\(url)")
        }
    }
}
