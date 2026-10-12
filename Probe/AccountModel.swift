import DebugTrace
import Foundation
import Observation

private let log = DebugLogger(subsystem: Bundle.main.bundleIdentifier ?? "Apparition", category: "Account")

/// Spike 2: Apple ID sign-in, device registration and on-device signing through apparition-core.
/// The password goes from the text field straight into apparition_login and is not kept.
@MainActor @Observable
final class AccountModel {
    struct Team: Codable, Sendable {
        let id: String
        let name: String?
        let kind: String?
    }

    struct SignResult: Codable, Sendable {
        let file: String
        let startedAt: String
        var finishedAt: String?
        var outcome: String?
        var signedPath: String?
        var registeredDevices: [[String: String]]?
        var bundles: [SignedBundle]?
        var strippedExtensions: [String]?
    }

    /// What ended up in a signed bundle, read back from disk.
    struct SignedBundle: Codable, Sendable {
        let path: String
        let bundleId: String?
        let hasSignature: Bool
        let profileName: String?
        let profileTeam: String?
        let profileExpires: String?
        let profilePlatforms: [String]?
        let profileIncludesDevice: Bool?
        let applicationIdentifier: String?
        let getTaskAllow: Bool?
        let entitlementKeys: [String]?
    }

    struct TwoFactorRequest: Sendable {
        /// A trusted phone number Apple can text the code to.
        struct PhoneNumber: Sendable, Identifiable {
            let id: Int
            /// As Apple shows it, mostly masked ("+1 (•••) •••-••12").
            let number: String
        }

        let sms: Bool
        let unknown: Bool
        let lastError: String?
        var numbers: [PhoneNumber] = []
        var selectedNumberId: Int?
    }

    /// Keep the password in this device's Keychain so refreshes can sign in without anyone
    /// present. Off by default; turning it off deletes the stored password.
    var rememberPassword: Bool = UserDefaults.standard.bool(forKey: "rememberPassword") {
        didSet {
            UserDefaults.standard.set(rememberPassword, forKey: "rememberPassword")
            if !rememberPassword { Keychain.delete() }
        }
    }

    /// An anisette-v3 server URL. Apparition ships none: the user enters one, or self-hosts
    /// selfhost/anisette/compose.yaml.
    var anisetteURL: String = UserDefaults.standard.string(forKey: "anisetteURL") ?? "" {
        didSet { UserDefaults.standard.set(anisetteURL, forKey: "anisetteURL") }
    }
    private(set) var busy = false
    private(set) var email: String?
    private(set) var team: Team?
    private(set) var teams: [Team] = []
    private(set) var lastError: String?
    private(set) var twoFactor: TwoFactorRequest?
    private(set) var downloads: [String] = []
    private(set) var signs: [SignResult] = []
    private(set) var downloadProgress: String?

    private let twoFactorBox = PinBox()

    nonisolated static var dataDir: URL {
        FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("Apparition", isDirectory: true)
    }

    nonisolated static var appsDir: URL {
        FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("apps", isDirectory: true)
    }

    init() {
        try? FileManager.default.createDirectory(at: Self.dataDir, withIntermediateDirectories: true)
        try? FileManager.default.createDirectory(at: Self.appsDir, withIntermediateDirectories: true)
        let documents = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
        apparition_init_logging(documents.appendingPathComponent("apparition-core.log").path)
        refreshDownloads()
    }

    // MARK: Sign-in

    func signIn(email: String, password: String) async {
        guard !busy else { return }
        guard !anisetteURL.isEmpty else {
            lastError = "Enter an anisette server URL first (or self-host one: selfhost/anisette in the Apparition repo)."
            return
        }
        busy = true
        lastError = nil
        defer { busy = false; twoFactor = nil }
        log.info("sign-in start")

        let anisette = anisetteURL
        let dataDir = Self.dataDir.path
        let box = Unmanaged.passUnretained(twoFactorBox).toOpaque()
        twoFactorBox.onRequestJSON = { [weak self] json in
            let request = (try? JSONSerialization.jsonObject(with: Data(json.utf8))) as? [String: Any]
            Task { @MainActor in
                let numbers = (request?["numbers"] as? [[String: Any]] ?? []).compactMap { entry in
                    (entry["id"] as? Int).map { TwoFactorRequest.PhoneNumber(id: $0, number: entry["number"] as? String ?? "") }
                }
                self?.twoFactor = TwoFactorRequest(sms: request?["sms"] as? Bool ?? false,
                                                   unknown: request?["unknown"] as? Bool ?? false,
                                                   lastError: request?["lastError"] as? String,
                                                   numbers: numbers,
                                                   selectedNumberId: request?["selectedNumberId"] as? Int)
            }
        }
        let reply = await Task.detached {
            Self.call(apparition_login(email, password, anisette, dataDir, AccountModel.twoFactorCallback, box))
        }.value

        if reply["ok"] as? Bool == true {
            UserDefaults.standard.set(email, forKey: "appleID")
            if rememberPassword { Keychain.save(password, account: email) }
            self.email = reply["email"] as? String
            team = Self.team(reply["team"])
            teams = (reply["teams"] as? [Any] ?? []).compactMap(Self.team)
            log.info("sign-in ok, \(self.teams.count, privacy: .public) teams")
        } else {
            lastError = reply["error"] as? String ?? "unknown error"
            log.error("sign-in failed: \(self.lastError ?? "", privacy: .private)")
        }
    }

    /// Signs in with the Keychain password, for refreshes with nobody present. A 2FA prompt
    /// can't be answered here, so it aborts; Apple skips 2FA for a device it already trusts.
    func signInSilently() async -> Bool {
        if team != nil { return true }
        guard !busy, rememberPassword, !anisetteURL.isEmpty, let email = UserDefaults.standard.string(forKey: "appleID"),
              let password = Keychain.load(account: email) else { return false }
        busy = true
        defer { busy = false }
        log.info("silent sign-in start")
        let anisette = anisetteURL
        let dataDir = Self.dataDir.path
        let reply = await Task.detached {
            Self.call(apparition_login(email, password, anisette, dataDir, AccountModel.twoFactorCallback, nil))
        }.value
        guard reply["ok"] as? Bool == true else {
            lastError = reply["error"] as? String ?? "unknown error"
            log.error("silent sign-in failed: \(self.lastError ?? "", privacy: .private)")
            return false
        }
        self.email = reply["email"] as? String
        team = Self.team(reply["team"])
        teams = (reply["teams"] as? [Any] ?? []).compactMap(Self.team)
        log.info("silent sign-in ok")
        return true
    }

    /// `code:123456`, `devices`, `sms:<id>`, `resend` or `abort`.
    func answerTwoFactor(_ answer: String) {
        twoFactor = nil
        twoFactorBox.submit(answer)
    }

    nonisolated static let twoFactorCallback: apparition_two_factor_fn = { context, params in
        guard let context else { return nil }
        let box = Unmanaged<PinBox>.fromOpaque(context).takeUnretainedValue()
        box.onRequestJSON?(params.map { String(cString: $0) } ?? "{}")
        return box.waitForPin()
    }

    // MARK: Apps

    func refreshDownloads() {
        downloads = ((try? FileManager.default.contentsOfDirectory(atPath: Self.appsDir.path)) ?? [])
            .filter { $0.hasSuffix(".ipa") }.sorted()
    }

    func download(_ urlString: String) async {
        guard let url = URL(string: urlString), !busy else { return }
        busy = true
        lastError = nil
        defer { busy = false; downloadProgress = nil }
        downloadProgress = "downloading \(url.lastPathComponent)"
        do {
            let (temporary, response) = try await URLSession.shared.download(from: url)
            guard (response as? HTTPURLResponse)?.statusCode == 200 else {
                throw IDevice.ProbeError("HTTP \((response as? HTTPURLResponse)?.statusCode ?? 0)")
            }
            let destination = Self.appsDir.appendingPathComponent(url.lastPathComponent)
            try? FileManager.default.removeItem(at: destination)
            try FileManager.default.moveItem(at: temporary, to: destination)
            log.info("downloaded \(url.lastPathComponent, privacy: .public)")
        } catch {
            lastError = "download failed: \(error)"
        }
        refreshDownloads()
    }

    func sign(file: String, udid: String, deviceName: String, stripExtensions: Bool = false) async {
        guard !busy else { return }
        busy = true
        lastError = nil
        defer { busy = false }
        signs.insert(SignResult(file: file, startedAt: DebugTime.iso(Date())), at: 0)
        log.info("sign start: \(file, privacy: .public)")

        let path = Self.appsDir.appendingPathComponent(file).path
        let reply = await Self.signFile(path, udid: udid, deviceName: deviceName, stripExtensions: stripExtensions)
        signs[0].finishedAt = DebugTime.iso(Date())
        if reply["ok"] as? Bool == true, let signed = reply["signedPath"] as? String {
            signs[0].outcome = "ok"
            signs[0].signedPath = signed
            signs[0].registeredDevices = (reply["devices"] as? [[String: Any]])?.map { device in
                device.compactMapValues { $0 as? String }
            }
            signs[0].bundles = Self.inspect(URL(fileURLWithPath: signed), udid: udid)
            signs[0].strippedExtensions = reply["strippedExtensions"] as? [String]
            log.info("sign ok")
        } else {
            signs[0].outcome = "error: \(reply["error"] as? String ?? "unknown")"
            log.error("sign failed: \(self.signs[0].outcome ?? "", privacy: .public)")
        }
    }

    /// Registers the device if needed and signs the IPA at `path`; `{ok, signedPath, ...}`.
    nonisolated static func signFile(_ path: String, udid: String, deviceName: String,
                                     stripExtensions: Bool) async -> [String: Any] {
        await Task.detached { call(apparition_sign(path, udid, deviceName, stripExtensions)) }.value
    }

    /// Forgets the session and, with it, the remembered password, so nothing signs in again
    /// until the user does. Nothing is sent to Apple.
    func signOut() {
        guard !busy else { return }
        _ = Self.call(apparition_logout())
        email = nil
        team = nil
        teams = []
        lastError = nil
        Keychain.delete()
        UserDefaults.standard.removeObject(forKey: "appleID")
        log.info("signed out")
    }

    /// The Apple ID last signed in with (not a secret; the field starts from it).
    var lastAppleID: String? { UserDefaults.standard.string(forKey: "appleID") }

    func refreshSession() {
        let reply = Self.call(apparition_session())
        if reply["signedIn"] as? Bool == true {
            email = reply["email"] as? String
            team = Self.team(reply["team"])
        } else {
            // The core ended the session (an imported certificate key): sign in again when next needed.
            team = nil
        }
    }

    // MARK: Helpers

    nonisolated static func call(_ pointer: UnsafeMutablePointer<CChar>?) -> [String: Any] {
        guard let pointer else { return ["ok": false, "error": "no reply"] }
        defer { apparition_string_free(pointer) }
        let data = Data(String(cString: pointer).utf8)
        return (try? JSONSerialization.jsonObject(with: data)) as? [String: Any] ?? ["ok": false, "error": "bad reply"]
    }

    private static func team(_ value: Any?) -> Team? {
        guard let value = value as? [String: Any], let id = value["id"] as? String else { return nil }
        return Team(id: id, name: value["name"] as? String, kind: value["kind"] as? String)
    }

    /// Reads back the main app and every nested bundle: identifier, signature, embedded profile.
    nonisolated static func inspect(_ app: URL, udid: String) -> [SignedBundle] {
        var bundles = [app]
        for folder in ["PlugIns", "Extensions", "Frameworks"] {
            let directory = app.appendingPathComponent(folder)
            let entries = (try? FileManager.default.contentsOfDirectory(at: directory, includingPropertiesForKeys: nil)) ?? []
            bundles += entries.filter { ["appex", "framework", "app"].contains($0.pathExtension) }
        }
        return bundles.map { bundle in
            let info = NSDictionary(contentsOf: bundle.appendingPathComponent("Info.plist")) as? [String: Any]
            let profile = embeddedProfile(bundle.appendingPathComponent("embedded.mobileprovision"))
            let entitlements = profile?["Entitlements"] as? [String: Any]
            let devices = profile?["ProvisionedDevices"] as? [String]
            return SignedBundle(
                path: bundle.path.replacingOccurrences(of: app.deletingLastPathComponent().path + "/", with: ""),
                bundleId: info?["CFBundleIdentifier"] as? String,
                hasSignature: FileManager.default.fileExists(atPath: bundle.appendingPathComponent("_CodeSignature").path),
                profileName: profile?["Name"] as? String,
                profileTeam: (profile?["TeamIdentifier"] as? [String])?.first,
                profileExpires: (profile?["ExpirationDate"] as? Date).map(DebugTime.iso),
                profilePlatforms: profile?["Platform"] as? [String],
                profileIncludesDevice: devices.map { $0.contains(udid) },
                applicationIdentifier: entitlements?["application-identifier"] as? String,
                getTaskAllow: entitlements?["get-task-allow"] as? Bool,
                entitlementKeys: entitlements.map { $0.keys.sorted() })
        }
    }

    /// The plist inside a CMS-signed .mobileprovision, without verifying the signature.
    nonisolated static func embeddedProfile(_ url: URL) -> [String: Any]? {
        guard let data = try? Data(contentsOf: url),
              let start = data.range(of: Data("<?xml".utf8)),
              let end = data.range(of: Data("</plist>".utf8), in: start.lowerBound..<data.endIndex)
        else { return nil }
        let plist = data.subdata(in: start.lowerBound..<end.upperBound)
        return (try? PropertyListSerialization.propertyList(from: plist, format: nil)) as? [String: Any]
    }
}

/// The Apple ID password, stored only on this device and only when the user opts in.
enum Keychain {
    private static let service = "com.illixion.apparition.appleid"

    static func save(_ password: String, account: String) {
        delete()
        let item: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
            kSecAttrAccessible as String: kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly,
            kSecValueData as String: Data(password.utf8),
        ]
        let status = SecItemAdd(item as CFDictionary, nil)
        if status != errSecSuccess { log.error("keychain save failed: \(status, privacy: .public)") }
    }

    static func load(account: String) -> String? {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
            kSecReturnData as String: true,
        ]
        var result: CFTypeRef?
        guard SecItemCopyMatching(query as CFDictionary, &result) == errSecSuccess,
              let data = result as? Data else { return nil }
        return String(data: data, encoding: .utf8)
    }

    static func delete() {
        SecItemDelete([kSecClass as String: kSecClassGenericPassword,
                       kSecAttrService as String: service] as CFDictionary)
    }

    static var hasPassword: Bool {
        guard let account = UserDefaults.standard.string(forKey: "appleID") else { return false }
        return load(account: account) != nil
    }
}
