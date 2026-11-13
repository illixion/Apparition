import DebugTrace
import Foundation
import Security
import Testing

@testable import Apparition

/// What a problem report can carry (Settings > Report a Problem; CONTRIBUTING.md, "What a problem
/// report can carry").
///
/// A report is a debug trace captured as a release build would capture one: the app's own log,
/// the feature marks, and the debug queries marked free of personal data. It goes to a public
/// issue, encrypted to the maintainer. This puts a marker into every place Apparition keeps
/// something of the person's (their Apple ID, team, device, servers, signing material), does
/// what makes Apparition log, and fails if a marker reaches any file of the report. A new
/// endpoint marked `releaseSafe`, a log value marked `.public` or an error text passed on
/// unscrubbed fails here, not in someone's report.
///
/// Hosted by the app, on the simulator: `scripts/test-report-privacy.sh`.
@MainActor
@Suite(.serialized) struct ReportPrivacyTests {
    /// One marker per kind of personal value, shaped like the real thing where a shape is what
    /// gets it recognised.
    enum Seed {
        static let appleID = "seedy.mcseed@appleid-seed.example"
        static let teamID = "5EED7EAM01"
        static let teamName = "Seedy McSeedface"
        static let certificateName = "Apple Development: Seedy McSeedface (5EEDCER701)"
        static let certificateSerial = "5EED00C0FFEE5EED5EED"
        static let deviceName = "Seedy's Apple Vision Pro"
        static let udid = "00008112-5EED5EED5EED5EED"
        static let watchUDID = "5eed5eed5eed5eed5eed5eed5eed5eed5eed5eed"
        static let anisetteHost = "anisette-seed.example"
        static let anisetteURL = "https://anisette-seed.example:6969/v3"
        static let companionName = "Seedy's MacBook Pro"
        static let companionHost = "companion-seed.invalid"
        static let companionRelay = "companion-seed.invalid:49160"
        static let lanName = "seedbook.local"
        static let lanAddress = "192.0.2.77"
        static let tailnetName = "seedbook.tail5eed.ts.net"
        static let tailnetAddress = "fd7a:115c:a1e0::5eed"
        static let hardwareAddress = "5e:ed:5e:ed:5e:ed"
        static let password = "seed-pass-word-abcd"

        /// Every marker, plus the parts of one that would give it away on their own.
        static let all = [
            appleID, "seedy.mcseed", teamID, teamName, "McSeedface", "5EEDCER701", certificateSerial,
            deviceName, "Seedy's", udid, "5EED5EED5EED5EED", watchUDID, anisetteHost, companionName,
            companionHost, lanName, lanAddress, tailnetName, tailnetAddress, hardwareAddress, password,
        ]
    }

    /// Error text in the shapes the core, installd and Apple give it, each carrying markers.
    /// Apparition logs such text as public after `LogScrub.text` (ProbeModel.run, LibraryModel.fail).
    static let errorTexts = [
        "Can't reach \(Seed.companionRelay): Connection refused (os error 61)",
        "\(Seed.lanName):49161 didn't finish the handshake",
        "Can't reach [\(Seed.tailnetAddress)]:49160: No route to host (os error 65)",
        "Can't reach \(Seed.tailnetName):49160",
        "the companion at \(Seed.lanAddress):49160 (\(Seed.hardwareAddress)) closed the connection",
        "Couldn't reach the anisette server at \(Seed.anisetteHost):6969, which \(Seed.anisetteURL) names",
        "error sending request for url (\(Seed.anisetteURL)/provision): dns error",
        "The iPhone forwarded a different device (\(Seed.udid)) instead of the Watch \(Seed.watchUDID)",
        "Device \(Seed.deviceName) (\(Seed.udid)) is not registered to team \(Seed.teamName) (\(Seed.teamID))",
        "No profile for com.example.app.\(Seed.teamID) signed by \(Seed.certificateName), serial \(Seed.certificateSerial)",
        "Apple said: an account for \(Seed.appleID) can't make another certificate (7460)",
        "\(Seed.companionName) didn't answer",
    ]

    // MARK: The report

    @Test(.timeLimit(.minutes(3)))
    func aReportCarriesNoneOfThePersonsValues() async throws {
        let seeded = SeededApp()
        defer { seeded.restore() }
        await seeded.makeItLog()

        let stage = try await DebugTrace.stage(note: nil, surface: seeded.surface, options: .report)
        #expect(stage.privacy == .release)
        #expect(!seeded.settings.pairingSecrets.isEmpty, "no pairing record was made, so none was looked for")
        #expect(stage.file("logs.txt") != nil)
        #expect(stage.file("snapshot.json") != nil)
        for file in stage.files {
            let text = String(decoding: file.data, as: UTF8.self)
            for seed in Seed.all + seeded.settings.pairingSecrets where text.localizedCaseInsensitiveContains(seed) {
                Issue.record("\(file.path) carries \(seed): \(Self.context(of: seed, in: text))")
            }
        }

        // The lines were logged, so their absence above means they were scrubbed, not lost.
        let logs = stage.file("logs.txt")?.text ?? ""
        #expect(logs.contains("report-privacy: Can't reach <host>:49160"))
        #expect(logs.contains("report-privacy: private <private>"))
        #expect(logs.contains("tunnel failed"))
    }

    /// The same state does reach a development snapshot, so the test above would notice an
    /// endpoint that started to pass it on.
    @Test func theSeedsAreWhereTheEndpointsRead() async throws {
        let seeded = SeededApp()
        defer { seeded.restore() }
        #expect(Keychain.load(account: Seed.appleID) == Seed.password, "the password isn't where Apparition remembers one")
        #expect(PairingRecord.exists)
        var replies = ""
        for name in ["accountState", "probeState", "libraryState"] {
            replies += try await seeded.surface.call(name).get().json?.serializedString() ?? ""
        }
        for seed in [Seed.teamID, Seed.teamName, Seed.udid, Seed.deviceName, Seed.anisetteHost,
                     Seed.companionHost, Seed.appleID, Seed.certificateName, Seed.watchUDID] {
            #expect(replies.contains(seed), "\(seed) isn't in the seeded state any more: seed it where it lives now")
        }
    }

    /// What the running app registered, as a release trace would read it.
    @Test func theAppsOwnReleaseSnapshotCarriesNoneOfThem() async throws {
        let settings = SeededSettings(stores: true)
        defer { settings.restore() }
        let snapshot = await DebugSurface.shared.snapshot(privacy: .release)
        let text = snapshot.json.serializedString()
        for seed in Seed.all where text.localizedCaseInsensitiveContains(seed) {
            Issue.record("the release snapshot carries \(seed): \(Self.context(of: seed, in: text))")
        }
        #expect(snapshot.files.isEmpty)
        let withheld = snapshot.json["withheldInRelease"]?.arrayValue?.compactMap(\.stringValue) ?? []
        for name in ["accountState", "probeState", "libraryState", "signing", "nearbySigning", "verboseLog"] {
            #expect(withheld.contains(name), "\(name) returns the person's own values and is now in release traces")
        }
        // DebugTrace puts every registered attachment into a trace, a report included, whatever
        // the privacy mode. Apparition registers none; one that is added needs the same care.
        #expect(DebugSurface.shared.registeredAttachments.isEmpty)
    }

    // MARK: Error text

    @Test(arguments: errorTexts)
    func scrubbedErrorTextCarriesNoneOfThem(text: String) {
        let settings = SeededSettings(stores: false)
        defer { settings.restore() }
        let scrubbed = LogScrub.text(text)
        for seed in Seed.all {
            #expect(!scrubbed.localizedCaseInsensitiveContains(seed), "\(seed) is still in: \(scrubbed)")
        }
    }

    /// What isn't the person's stays, or the log stops being worth reading. And a shape is
    /// enough: an address or identifier Apparition was never told about goes too.
    @Test func scrubbingGoesByShapeAndKeepsTheRest() {
        let kept = [
            "installed in 4210 ms (packed 312 MB in 1840 ms, 12 of 96 files reused, upload 2100 ms)",
            "tunnel failed: Can't reach 10.7.0.1:49152: Connection refused (os error 61)",
            "Error Domain=NSPOSIXErrorDomain Code=61 \"Connection refused\"",
            "sign-in ok at 2026-11-13T00:37:29Z after 12:34:56",
            "ApplicationVerificationFailed: Failed to verify code signature of /var/installd/Library/Caches/UTM.app",
            "the reinstall didn't take: this is still the copy from before",
            "CONNECT gsa.apple.com:443 was refused",
        ]
        for text in kept {
            #expect(LogScrub.text(text) == text)
        }
        #expect(LogScrub.text("Can't reach studio.local:49160: timed out") == "Can't reach <address>: timed out")
        #expect(LogScrub.text("lookup of mini.tail1a2b3.ts.net failed") == "lookup of <host> failed")
        #expect(LogScrub.text("no route to fe80::1c2d:3e4f:5a6b:7c8d%en0") == "no route to <address>")
        #expect(LogScrub.text("device 00008130-001C25D80A12345E isn't in the profile") == "device <device> isn't in the profile")
        #expect(LogScrub.text("simulator 59FADA1D-A2D2-47EA-9AFA-CFE1AD52237F") == "simulator <device>")
        #expect(LogScrub.text("no profile for com.utmapp.UTM.ABCDE12345") == "no profile for com.utmapp.UTM.<team>")
    }

    private static func context(of seed: String, in text: String) -> String {
        guard let range = text.range(of: seed, options: .caseInsensitive) else { return "" }
        let start = text.index(range.lowerBound, offsetBy: -80, limitedBy: text.startIndex) ?? text.startIndex
        let end = text.index(range.upperBound, offsetBy: 80, limitedBy: text.endIndex) ?? text.endIndex
        return "…" + text[start..<end].replacingOccurrences(of: "\n", with: "⏎") + "…"
    }
}

/// The settings and stores Apparition reads the person's own things from, holding the markers.
/// `restore` puts back what was there: the test host is the real app, and its settings outlive
/// the test.
@MainActor
private final class SeededSettings {
    typealias Seed = ReportPrivacyTests.Seed

    /// Long runs of the pairing record made for this test: it holds the key a device trusts.
    private(set) var pairingSecrets: [String] = []

    private var savedDefaults: [String: Any?] = [:]
    private var madePairing = false
    private var savedPassword = false

    private static let defaults: [String: Any] = [
        "pairedUDID": Seed.udid,
        "pairedName": Seed.deviceName,
        "anisetteURL": Seed.anisetteURL,
        "appleID": Seed.appleID,
        "relay": Seed.companionRelay,
        "companionName": Seed.companionName,
        "connectionMode": ProbeModel.ConnectionMode.companion.rawValue,
    ]

    /// `stores`: also the remembered password in the Keychain and a pairing record in Documents.
    init(stores: Bool) {
        let store = UserDefaults.standard
        for (key, value) in Self.defaults {
            savedDefaults[key] = store.object(forKey: key)
            store.set(value, forKey: key)
        }
        // Names with no shape to recognise them by, as sign-in and a certificate import leave them.
        LogScrub.remember([Seed.teamName, Seed.certificateName])
        guard stores else { return }

        // The remembered password, where sign-in keeps it. Added as its own item rather than
        // with Keychain.save, which clears whatever password the host already remembers.
        savedPassword = SecItemAdd(Self.passwordItem.merging([kSecValueData as String: Data(Seed.password.utf8)]) { $1 } as CFDictionary, nil) == errSecSuccess
        // A pairing record, where pairing leaves it. The same: never replace a real one.
        if !PairingRecord.exists, let record = try? IDevice.PairingFile.generate(hostname: ProbeModel.hostname).serialized(),
           (try? record.write(to: PairingRecord.url, options: .atomic)) != nil {
            madePairing = true
            let text = String(decoding: record, as: UTF8.self)
            if let runs = try? NSRegularExpression(pattern: "[A-Za-z0-9+/=]{24,}") {
                pairingSecrets = runs.matches(in: text, range: NSRange(text.startIndex..., in: text))
                    .compactMap { Range($0.range, in: text).map { String(text[$0]) } }
            }
        }
    }

    func restore() {
        let store = UserDefaults.standard
        for (key, value) in savedDefaults {
            if let value { store.set(value, forKey: key) } else { store.removeObject(forKey: key) }
        }
        if savedPassword { SecItemDelete(Self.passwordItem as CFDictionary) }
        if madePairing { PairingRecord.delete() }
    }

    /// The seeded account's item in the service `Keychain` keeps passwords under.
    private static let passwordItem: [String: Any] = [
        kSecClass as String: kSecClassGenericPassword,
        kSecAttrService as String: "com.illixion.apparition.appleid",
        kSecAttrAccount as String: Seed.appleID,
    ]
}

/// Apparition's models holding the markers, registered on a surface of their own the way the
/// app registers its own (ApparitionApp.init).
@MainActor
private struct SeededApp {
    typealias Seed = ReportPrivacyTests.Seed

    let settings = SeededSettings(stores: true)
    let surface = DebugSurface()
    let probe: ProbeModel
    let account: AccountModel
    let library: LibraryModel

    init() {
        // After the settings: the models read them as they are made.
        probe = ProbeModel()
        account = AccountModel()
        account.seedForTests(
            email: Seed.appleID, team: .init(id: Seed.teamID, name: Seed.teamName, kind: "free"),
            lastError: "Apple said no to \(Seed.appleID), team \(Seed.teamName) (\(Seed.teamID))")
        let signing = SigningStore()
        library = LibraryModel(account: account, probe: probe, signing: signing)
        library.seedForTests(installed: [
            .init(id: "com.example.seeded", source: "https://source-seed.example/apps.json", name: "Seeded App",
                  version: "1.0", ipaFile: "Seeded.ipa", stripExtensions: false,
                  installedBundleId: "com.example.seeded.\(Seed.teamID)",
                  lastError: ReportPrivacyTests.errorTexts.joined(separator: "; "),
                  signedWith: Seed.certificateName, assignedTeam: Seed.teamID, watchUDIDs: [Seed.watchUDID]),
        ], status: "Install Seeded App: error: \(ReportPrivacyTests.errorTexts[0])")

        ProbeEndpoints.register(probe, on: surface)
        VerboseLogEndpoints.register(on: surface)
        AccountEndpoints.register(account, on: surface)
        SigningEndpoints.register(signing, on: surface)
        NearbyEndpoints.register(signing, account: account, on: surface)
        LibraryEndpoints.register(library, account: account, on: surface)
        CompanionEndpoints.register(CompanionFinder(), probe: probe, on: surface)
        DeveloperSlotsEndpoints.register(on: surface)
        ExperimentEndpoints.register(probe: probe, account: account, on: surface)
        DebugScreens.register(on: surface)
        VaultEndpoints.register(on: surface)
        IntentEndpoints.register(probe: probe, account: account, library: library, on: surface)
        // Release-safe: its state names no computer, address or secret, so it is in every report.
        DebugAccessEndpoints.register(DebugAccessModel(), on: surface)
    }

    /// Does what makes Apparition write log lines about the person's own things.
    func makeItLog() async {
        // The app's own path: a tunnel through a companion that can't be found fails in the
        // core, and ProbeModel logs the core's words. Nothing leaves this Mac: the name is under
        // .invalid, which never resolves.
        await probe.tunnel(host: ProbeModel.relayHost, port: 49152)

        // The shapes the app's other failure paths log the same way, which a test can't reach
        // without a device or Apple: public after LogScrub, or private.
        let log = DebugLogger(subsystem: Bundle.main.bundleIdentifier ?? "Apparition", category: "ReportPrivacyTests")
        for text in ReportPrivacyTests.errorTexts {
            log.error("report-privacy: \(LogScrub.text(text), privacy: .public)")
        }
        log.error("report-privacy: private \(Seed.appleID, privacy: .private) \(Seed.deviceName)")
        log.error("report-privacy: hashed \(Seed.udid, privacy: .private(mask: .hash))")
        log.error("report-privacy: sensitive \(Seed.password, privacy: .sensitive)")
    }

    func restore() { settings.restore() }
}
