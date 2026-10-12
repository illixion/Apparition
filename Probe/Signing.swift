import DebugTrace
import Foundation
import Observation

private let log = DebugLogger(subsystem: Bundle.main.bundleIdentifier ?? "Apparition", category: "Signing")

/// The user's own signing certificates and provisioning profiles (core/src/identity.rs), for
/// signing without an Apple ID: a paid developer account's .p12 and year-long profiles that
/// list this headset. Which one signs an app is the app's own choice, else the default here.
@MainActor @Observable
final class SigningStore {
    struct Identity: Codable, Sendable, Identifiable, Hashable {
        /// The certificate's SHA-1 fingerprint.
        let id: String
        let name: String
        let teamId: String?
        let teamName: String?
        /// development, distribution or other.
        let kind: String
        let serialNumber: String?
        let expires: String?
        let expired: Bool
    }

    struct Profile: Codable, Sendable, Identifiable, Hashable {
        var id: String { uuid }
        let uuid: String
        let name: String
        let teamId: String
        let teamName: String?
        /// `com.example.app`, `com.example.*` or `*`.
        let appIdPattern: String
        /// development, adHoc, enterprise or appStore.
        let kind: String
        /// get-task-allow: apps signed with it can be launched with JIT.
        let debuggable: Bool
        let platforms: [String]
        /// Lists visionOS (xrOS) among its platforms; a tvOS-only profile can't sign for the headset.
        let forVision: Bool?
        let deviceCount: Int
        let includesDevice: Bool?
        let expires: String?
        let expired: Bool
        /// The imported certificates this profile allows to sign.
        let identityIds: [String]
    }

    struct Failure: Error, CustomStringConvertible {
        let description: String
    }

    /// The signer value meaning "the signed-in Apple ID" (isideload makes the certificate
    /// and a 7-day profile).
    nonisolated static let appleID = "appleID"
    private nonisolated static let defaultSignerKey = "signWith"

    private(set) var identities: [Identity] = []
    private(set) var profiles: [Profile] = []

    /// What signs an app that hasn't chosen: `appleID` or an imported certificate's id.
    var defaultSigner: String = UserDefaults.standard.string(forKey: SigningStore.defaultSignerKey) ?? SigningStore.appleID {
        didSet { UserDefaults.standard.set(defaultSigner, forKey: Self.defaultSignerKey) }
    }

    /// Whether apps sign with an imported certificate by default, so setup needs no Apple ID.
    nonisolated static var signsWithOwnCertificate: Bool {
        (UserDefaults.standard.string(forKey: defaultSignerKey) ?? appleID) != appleID
    }

    private nonisolated static var dataDir: String { AccountModel.dataDir.path }
    private nonisolated static var udid: String? { UserDefaults.standard.string(forKey: "pairedUDID") }

    init() {
        load()
    }

    func load() {
        let reply = AccountModel.call(apparition_signing_list(Self.dataDir, Self.udid))
        struct List: Decodable { let identities: [Identity]; let profiles: [Profile] }
        guard let list = try? Self.decode(List.self, reply) else { return }
        identities = list.identities
        profiles = list.profiles
        if defaultSigner != Self.appleID, identity(defaultSigner) == nil {
            defaultSigner = Self.appleID
        }
    }

    func identity(_ id: String?) -> Identity? {
        identities.first { $0.id == id }
    }

    /// The imported profiles this certificate can sign with on this headset.
    func usableProfiles(for identity: Identity) -> [Profile] {
        profiles.filter {
            $0.identityIds.contains(identity.id) && !$0.expired && $0.kind != "appStore"
                && $0.forVision != false && $0.includesDevice == true
        }
    }

    /// The signer for an app: its own choice while that certificate is still imported, else the default.
    func signer(for app: LibraryModel.InstalledApp) -> String {
        if let chosen = app.signWith, chosen == Self.appleID || identity(chosen) != nil { return chosen }
        return defaultSigner
    }

    func signerName(_ signer: String) -> String {
        signer == Self.appleID ? "Apple ID" : identity(signer)?.name ?? "Removed certificate"
    }

    // MARK: Import and delete

    /// Copies the picked file out of its security scope and imports it. The password opens
    /// the .p12 in the core and is not kept anywhere.
    func importCertificate(from url: URL, password: String) async throws -> Identity {
        let path = try Self.copyIn(url)
        defer { try? FileManager.default.removeItem(atPath: path) }
        let dataDir = Self.dataDir
        let reply = await Task.detached { AccountModel.call(apparition_identity_import(dataDir, path, password)) }.value
        struct Imported: Decodable { let identity: Identity }
        let identity = try Self.decode(Imported.self, reply).identity
        log.notice("imported a signing certificate (\(identity.kind, privacy: .public))")
        load()
        return identity
    }

    func importProfile(from url: URL) async throws -> Profile {
        let path = try Self.copyIn(url)
        defer { try? FileManager.default.removeItem(atPath: path) }
        let (dataDir, udid) = (Self.dataDir, Self.udid)
        let reply = await Task.detached { AccountModel.call(apparition_profile_import(dataDir, path, udid)) }.value
        struct Imported: Decodable { let profile: Profile }
        let profile = try Self.decode(Imported.self, reply).profile
        log.notice("imported a \(profile.kind, privacy: .public) profile")
        load()
        return profile
    }

    func deleteIdentity(_ identity: Identity) throws {
        _ = try Self.decode(Empty.self, AccountModel.call(apparition_identity_delete(Self.dataDir, identity.id)))
        log.notice("deleted a signing certificate")
        load()
    }

    func deleteProfile(_ profile: Profile) throws {
        _ = try Self.decode(Empty.self, AccountModel.call(apparition_profile_delete(Self.dataDir, profile.uuid)))
        log.notice("deleted a profile")
        load()
    }

    // MARK: Files and sharing

    /// A certificate waiting for its password, from the Files inbox, a share or the picker.
    struct IncomingCertificate: Identifiable, Equatable {
        let id = UUID()
        let url: URL
    }

    /// What arrived and how it went, shown by IncomingSigningSheet until dismissed.
    struct IncomingResult: Identifiable {
        let id = UUID()
        let file: String
        let text: String
        let ok: Bool
        var identityId: String? = nil
        var profileUUID: String? = nil
    }

    private(set) var pendingCertificates: [IncomingCertificate] = []
    /// After a shared Apple ID certificate key is imported: the core ended the session.
    var onAppleIDKeyImported: (() -> Void)?
    private(set) var incomingResults: [IncomingResult] = []

    /// Documents/Signing, which Files shows as Apparition > Signing. Anything put there is
    /// imported the next time Apparition is in front, and removed once imported.
    nonisolated static var inboxDir: URL {
        FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("Signing", isDirectory: true)
    }

    private enum Kind { case certificate, profile, signingFile }

    private static func kind(of url: URL) -> Kind? {
        switch url.pathExtension.lowercased() {
        case "p12", "pfx": .certificate
        case "mobileprovision", "provisionprofile": .profile
        case "apparitionsigning": .signingFile
        default: nil
        }
    }

    /// Imports what's in the Files inbox: profiles at once, certificates once their
    /// password is typed.
    func scanInbox() async {
        try? FileManager.default.createDirectory(at: Self.inboxDir, withIntermediateDirectories: true)
        let files = (try? FileManager.default.contentsOfDirectory(at: Self.inboxDir, includingPropertiesForKeys: nil)) ?? []
        for file in files.sorted(by: { $0.lastPathComponent < $1.lastPathComponent }) {
            await receive(file)
        }
    }

    /// A file opened in or shared to Apparition, picked in Signing, or found in the inbox.
    func receive(_ url: URL) async {
        switch Self.kind(of: url) {
        case .profile:
            do {
                let profile = try await importProfile(from: url)
                removeIfOurs(url)
                var text = "Imported \(profile.name) (\(profile.appIdPattern))"
                if profile.forVision == false { text += ", but it isn't for visionOS" }
                else if profile.includesDevice == false { text += ", but it doesn't list this headset" }
                report(url, text, ok: true, profileUUID: profile.uuid)
            } catch {
                report(url, "\(error)", ok: false)
            }
        case .signingFile:
            do {
                let path = try Self.copyIn(url)
                defer { try? FileManager.default.removeItem(atPath: path) }
                let dataDir = Self.dataDir
                let reply = await Task.detached { AccountModel.call(apparition_import_signing_file(dataDir, path)) }.value
                struct Imported: Decodable { let appleId: String; let from: String; let replaced: Bool }
                let imported = try Self.decode(Imported.self, reply)
                removeIfOurs(url)
                onAppleIDKeyImported?()
                var text = "Apps signed with \(imported.appleId) now use the same certificate as \(imported.from)"
                if imported.replaced {
                    text += ". This headset's own certificate for it stays on the account until you revoke it in Certificates & App IDs"
                }
                report(url, text, ok: true)
            } catch {
                report(url, "\(error)", ok: false)
            }
        case .certificate:
            if !pendingCertificates.contains(where: { $0.url.standardizedFileURL == url.standardizedFileURL }) {
                pendingCertificates.append(IncomingCertificate(url: url))
            }
        case nil:
            if !Self.isInbox(url) { report(url, "Not a certificate (.p12), provisioning profile or Apparition signing file", ok: false) }
        }
    }

    /// Imports a waiting certificate with its password. A wrong password throws and leaves it
    /// waiting, so it can be typed again.
    func importPending(_ incoming: IncomingCertificate, password: String) async throws {
        let identity = try await importCertificate(from: incoming.url, password: password)
        pendingCertificates.removeAll { $0.id == incoming.id }
        removeIfOurs(incoming.url)
        let usable = usableProfiles(for: identity).count
        report(incoming.url, "Imported \(identity.name)" + (usable == 0 ? "" : ", with \(usable) profile\(usable == 1 ? "" : "s") for this headset"),
               ok: true, identityId: identity.id)
    }

    /// Skips a waiting certificate. One in the inbox stays there until imported or deleted in Files.
    func skipPending(_ incoming: IncomingCertificate) {
        pendingCertificates.removeAll { $0.id == incoming.id }
    }

    func clearResults() {
        incomingResults = []
    }

    /// The sheet was dismissed: skip what still waits for a password (an inbox file comes back
    /// at the next scan) and forget the results.
    func dismissIncoming() {
        pendingCertificates = []
        incomingResults = []
    }

    private func report(_ url: URL, _ text: String, ok: Bool, identityId: String? = nil, profileUUID: String? = nil) {
        incomingResults.append(IncomingResult(file: url.lastPathComponent, text: text, ok: ok,
                                              identityId: identityId, profileUUID: profileUUID))
        if ok { log.info("incoming signing file imported") } else { log.error("incoming signing file failed: \(text, privacy: .private)") }
    }

    private static func isInbox(_ url: URL) -> Bool {
        url.standardizedFileURL.path.hasPrefix(inboxDir.standardizedFileURL.path)
    }

    /// Removes a file once imported, if it's a copy in Apparition's own Documents (the inbox,
    /// or Documents/Inbox, where a share lands). A file opened in place elsewhere is left alone.
    private func removeIfOurs(_ url: URL) {
        let documents = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0].standardizedFileURL.path
        guard url.standardizedFileURL.path.hasPrefix(documents + "/") else { return }
        try? FileManager.default.removeItem(at: url)
    }

    /// What an imported half still lacks, for the import sheet: a certificate without a
    /// profile for this headset, or a profile none of the certificates can sign with. Either
    /// half may come first; nil once the set is complete (or the item was removed).
    func missingHalf(of result: IncomingResult) -> String? {
        if let id = result.identityId, let identity = identity(id) {
            return usableProfiles(for: identity).isEmpty
                ? "Waiting for a provisioning profile for this certificate that lists this headset. Share or open the .mobileprovision next."
                : nil
        }
        if let uuid = result.profileUUID, let profile = profiles.first(where: { $0.uuid == uuid }) {
            if profile.forVision == false || profile.includesDevice == false || profile.expired { return nil }  // already reported
            return profile.identityIds.contains(where: { identity($0) != nil })
                ? nil
                : "Waiting for the certificate this profile was made for. Share or open its .p12 next."
        }
        return nil
    }

    /// Removes an imported half that's no longer wanted (an incomplete set, a wrong file).
    func removeImported(_ result: IncomingResult) {
        if let id = result.identityId, let identity = identity(id) { try? deleteIdentity(identity) }
        if let uuid = result.profileUUID, let profile = profiles.first(where: { $0.uuid == uuid }) { try? deleteProfile(profile) }
        incomingResults.removeAll { $0.id == result.id }
    }

    // MARK: Signing

    /// Signs the IPA at `path` with an imported certificate; `{ok, signedPath, bundleId, ...}`.
    nonisolated static func signFile(_ path: String, identityId: String, udid: String,
                                     stripExtensions: Bool) async -> [String: Any] {
        let dataDir = Self.dataDir
        return await Task.detached {
            AccountModel.call(apparition_sign_offline(dataDir, path, identityId, nil, udid, stripExtensions))
        }.value
    }

    // MARK: Helpers

    private struct Empty: Decodable {}

    private static func decode<T: Decodable>(_ type: T.Type, _ reply: [String: Any]) throws -> T {
        guard reply["ok"] as? Bool == true else {
            throw Failure(description: reply["error"] as? String ?? "failed")
        }
        return try JSONDecoder().decode(T.self, from: JSONSerialization.data(withJSONObject: reply))
    }

    /// A picked file (Files, iCloud Drive) is only readable inside its security scope.
    private static func copyIn(_ url: URL) throws -> String {
        let scoped = url.startAccessingSecurityScopedResource()
        defer { if scoped { url.stopAccessingSecurityScopedResource() } }
        let copy = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString + "-" + url.lastPathComponent)
        try FileManager.default.copyItem(at: url, to: copy)
        return copy.path
    }
}

@MainActor
enum SigningEndpoints {
    static func register(_ store: SigningStore) {
        DebugSurface.shared.register([
            .query("signing", "Imported certificates and profiles (no keys), and the default signer") { _ in
                store.load()
                return SigningSummary(defaultSigner: store.defaultSigner, identities: store.identities, profiles: store.profiles)
            },
            .command("setDefaultSigner", "Sign apps with \"appleID\" or an imported certificate's id by default",
                     parameters: [.string("signer", "appleID or an identity id from signing", required: true)]) { args in
                let signer = try args.requireString("signer")
                guard signer == SigningStore.appleID || store.identity(signer) != nil else {
                    throw SigningStore.Failure(description: "no imported certificate \(signer)")
                }
                store.defaultSigner = signer
                return SigningSummary(defaultSigner: store.defaultSigner, identities: store.identities, profiles: store.profiles)
            },
.command("scanSigningInbox", "Import what's in Documents/Signing now (a .p12 then asks for its password on the headset)") { _ in
                await store.scanInbox()
                return SigningSummary(defaultSigner: store.defaultSigner, identities: store.identities, profiles: store.profiles)
            },
                        .command("importProfile", "Import a .mobileprovision already in Apparition's Documents folder",
                     parameters: [.string("file", "File name inside Documents", required: true)]) { args in
                let name = try args.requireString("file")
                guard !name.contains("/") else { throw SigningStore.Failure(description: "a file name, not a path") }
                let documents = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
                return try await store.importProfile(from: documents.appendingPathComponent(name))
            },
        ])
    }

    struct SigningSummary: Encodable {
        let defaultSigner: String
        let identities: [SigningStore.Identity]
        let profiles: [SigningStore.Profile]
    }
}
