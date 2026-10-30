import BackgroundTasks
import DebugTrace
import Foundation
import Observation

private let log = DebugLogger(subsystem: Bundle.main.bundleIdentifier ?? "Apparition", category: "Library")

/// Spike 5: sources, the apps installed from them, and keeping those apps signed.
///
/// Installing downloads the IPA into Documents/apps (kept, so a refresh never needs the
/// network for it), signs it for this device and installs it through the LocalDevVPN tunnel.
/// A refresh re-signs the kept IPA and reinstalls it over the old copy, which keeps its data.
@MainActor @Observable
final class LibraryModel {
    struct InstalledApp: Codable, Sendable, Identifiable {
        /// The source app's id: its bundle identifier, or `github:owner/repo`.
        let id: String
        let source: String
        var name: String
        var version: String
        var ipaFile: String
        var stripExtensions: Bool
        /// After signing, e.g. com.utmapp.UTM.<team>: what launchJIT needs.
        var installedBundleId: String?
        var signedAt: String?
        var expiresAt: String?
        /// The last failure, kept until a refresh succeeds; `lastErrorAt` says when it happened
        /// (nil in records saved before it was kept).
        var lastError: String? { didSet { lastErrorAt = lastError == nil ? nil : DebugTime.iso(Date()) } }
        var lastErrorAt: String?
        /// What signs it: `SigningStore.appleID`, an imported certificate's id, or nil for the default.
        var signWith: String?
        /// What signed the installed copy, for the card: "Apple ID" or the certificate's name.
        var signedWith: String?
        /// A copy of this app under another bundle ID (signed by another team) whose data the next
        /// install takes over, chosen when installing. Retained until every file arrives and the old copy is removed.
        var takeOver: String?
        /// Persisted signing plan; nil selections preserve the behavior of older installs.
        var selectedExtensions: [String]?
        var assignedBundleId: String?
        var assignedTeam: String?
        var sourceBundleId: String?
        var discardDestinationData: Bool?
        var needsInstallReview: Bool?
        var reviewedBundleId: String?
    }

    /// Apparition's own refreshes: it re-signs and reinstalls itself (`refreshSelf`).
    struct SelfRecord: Codable, Sendable {
        var signedAt: String?
        var signedWith: String?
        var lastError: String? { didSet { lastErrorAt = lastError == nil ? nil : DebugTime.iso(Date()) } }
        var lastErrorAt: String?
        /// Set when the update is handed to installd. installd holds an update of the running
        /// app until it closes (as for App Store updates), so the next launch finishes the record.
        var pending: PendingSelfRefresh?
    }

    struct PendingSelfRefresh: Codable, Sendable {
        let startedAt: String
        let runStartedAt: String?
        let previousExpires: String?
        /// `SelfApp.installStamp()` before installing: a different one at launch means the update took.
        let previousStamp: String?
        let signedWith: String
        /// The release being installed, when this is an update rather than a refresh.
        var version: String?
    }

    /// The latest official release, as last checked (`checkForUpdate`). Not kept across launches.
    struct UpdateCheck: Sendable {
        var latest: SelfUpdate.Latest?
        var checkedAt: Date?
        var error: String?
        var checking = false
    }

    struct RefreshRun: Codable, Sendable {
        let startedAt: String
        let trigger: String
        var finishedAt: String?
        var results: [String: String] = [:]
        /// When the system ended a background run's time before it finished.
        var expiredAt: String?
    }

    /// Short runs (about 30 s), offered often.
    nonisolated static let refreshTaskId = "com.illixion.apparition.refresh"
    /// Longer runs (minutes), typically while idle. A refresh that uploads a large app needs these.
    nonisolated static let processingTaskId = "com.illixion.apparition.refresh-processing"
    /// Test hook: the next background run refreshes everything, due or not.
    nonisolated static let forceNextBackgroundKey = "forceNextBackgroundRefresh"
    /// Free-account profiles last 7 days; refresh once fewer than this many are left.
    nonisolated static let refreshMargin: TimeInterval = 3 * 24 * 3600

    private(set) var sources: [Sources.Source] = []
    private(set) var installed: [InstalledApp] = []
    private var assignments = InstallAssignments()
    private(set) var busy = false {
        didSet {
            if !busy {
                removedWhileBusy.removeAll()
                stopping = false
            }
        }
    }
    /// Stop was pressed: what's under way ends at its next wait, and the run takes on nothing more.
    private(set) var stopping = false
    /// A result for work that Stop ended: not an error, and the app's last error stays as it was.
    nonisolated static let stoppedResult = "stopped"
    /// The IPA download under way, for Stop to cancel.
    private var downloading: Task<(URL, URLResponse), Error>?
    /// Apps taken out of the library while a refresh or install was running, so that run doesn't
    /// put the record back when it finishes (`upsert`).
    private var removedWhileBusy: Set<String> = []
    /// Why this refresh run's sign-in failed, once it has: the rest of the run fails with it
    /// instead of trying again for each app (an anisette server that times out would cost the
    /// wait once per app, with no way to stop the run).
    private var runSignInFailure: String?
    private var inRefreshRun = false
    private(set) var status: String?
    private(set) var refreshRuns: [RefreshRun] = []
    private(set) var selfRecord = SelfRecord()
    private(set) var updateCheck = UpdateCheck()
    /// Set when the last install hit installd's free-profile app limit; cleared by the next
    /// install that gets past it, fails otherwise, or is dismissed.
    private(set) var appLimit: AppLimit?
    /// Set when signing stopped rather than make a second certificate, because Apparition on
    /// another device has the Apple ID's (core's check_certificate). The Library and Browse offer
    /// to get its key or make one anyway, then run the install again.
    private(set) var certificateChoice: CertificateChoice?
    /// The last install error of each source app, by bundle ID, shown under that app's row so
    /// it's in view where Install was tapped.
    private(set) var installErrors: [String: String] = [:]
    /// An app from a file (shared, opened in Apparition or picked), on its way to Install.
    private(set) var incoming: IncomingFile?

    /// Whether `status` is an install error already shown under its app's row.
    var statusIsInline: Bool {
        guard let status else { return false }
        return installErrors.values.contains { status.hasSuffix($0) }
    }
    /// This copy of Apparition, read once: its profile only changes with a reinstall, which relaunches.
    let selfApp = SelfApp.current()
    /// The result key for Apparition itself in a refresh run.
    nonisolated static let selfResultKey = "apparition"

    private let account: AccountModel
    private let probe: ProbeModel
    let signing: SigningStore

    private static var stateURL: URL { AccountModel.dataDir.appendingPathComponent("library.json") }

    private struct Stored: Codable {
        var sources: [Sources.Source]
        var installed: [InstalledApp]
        var refreshRuns: [RefreshRun]?
        var selfRecord: SelfRecord?
        var assignments: InstallAssignments?
    }

    init(account: AccountModel, probe: ProbeModel, signing: SigningStore) {
        self.account = account
        self.probe = probe
        self.signing = signing
        if let data = try? Data(contentsOf: Self.stateURL),
           let stored = try? JSONDecoder().decode(Stored.self, from: data) {
            sources = stored.sources
            installed = stored.installed
            assignments = stored.assignments ?? InstallAssignments()
            refreshRuns = stored.refreshRuns ?? []
            selfRecord = stored.selfRecord ?? SelfRecord()
        }
        finishPendingSelfRefresh()
        dropAnisetteErrors(changed: false)
        account.anisetteChanged = { [weak self] in self?.dropAnisetteErrors(changed: true) }
        probe.appName = { [weak self] bundleId in self?.displayName(of: bundleId) ?? bundleId }
    }

    /// The address an anisette error names ("…the anisette server at host:port, which…").
    nonisolated static func anisetteHost(in error: String) -> String? {
        guard let marker = error.range(of: "anisette server at ") else { return nil }
        let host = error[marker.upperBound...].prefix { !$0.isWhitespace && $0 != "," }
        return host.isEmpty ? nil : String(host)
    }

    /// A saved failure that blamed an anisette server stops being true once the address changes,
    /// and it would otherwise stay on the card until the next refresh succeeds. With `changed`,
    /// every such error goes; at launch, only one that names another address than the one set now.
    private func dropAnisetteErrors(changed: Bool) {
        let current = AccountModel.AnisetteProblem(url: account.anisetteURL, unreachable: false).host
        func stale(_ error: String?) -> Bool {
            guard let error, error.localizedCaseInsensitiveContains("anisette") else { return false }
            if changed { return true }
            return Self.anisetteHost(in: error).map { $0 != current } ?? false
        }
        var dropped = false
        for index in installed.indices where stale(installed[index].lastError) {
            installed[index].lastError = nil
            dropped = true
        }
        if stale(selfRecord.lastError) {
            selfRecord.lastError = nil
            dropped = true
        }
        if dropped { save() }
    }

    private func writeState(installed: [InstalledApp]) throws {
        let stored = Stored(sources: sources, installed: installed, refreshRuns: Array(refreshRuns.prefix(20)),
                            selfRecord: selfRecord, assignments: assignments)
        try JSONEncoder().encode(stored).write(to: Self.stateURL, options: .atomic)
    }

    private func save() {
        do {
            try writeState(installed: installed)
            StateVault.sync()
        } catch {
            log.error("could not save library: \(String(describing: error), privacy: .private)")
        }
    }

    // MARK: Sources

    /// A source from an `apparition://source?url=…` link, waiting for Browse to show it in Add
    /// Source. Nothing is added until the person there taps Add.
    var linkedSource: String?

    /// The source an `apparition://source?url=<source>` link names (an https URL or
    /// `github:owner/repo`), or nil for any other URL.
    nonisolated static func sourceLink(_ url: URL) -> String? {
        guard url.scheme?.lowercased() == "apparition", url.host()?.lowercased() == "source",
              let value = URLComponents(url: url, resolvingAgainstBaseURL: false)?
                  .queryItems?.first(where: { $0.name == "url" })?.value?
                  .trimmingCharacters(in: .whitespacesAndNewlines),
              !value.isEmpty
        else { return nil }
        return value
    }

    /// An app asking for JIT for its running process: `apparition://enable-jit?bundle-id=…&pid=…`,
    /// or the same link under StikDebug's schemes (`stikdebug`, `stikjit`), which apps such as
    /// Madeira open. Without a pid, the app's running process is used.
    struct JITRequest: Identifiable, Equatable {
        var id: String { "\(bundleId)|\(pid)|\(script?.utf8.count ?? 0)" }
        let bundleId: String
        /// 0: the link named no process, so the app is launched anew under the debugger, as
        /// StikDebug does (LiveContainer then starts the app it was asked to run).
        let pid: Int
        /// A StikDebug JIT script (JavaScript) to be the app's debugger, from `script-data`.
        var script: String? = nil
    }

    /// An enable-jit link from an app Apparition didn't install, waiting to be allowed.
    var pendingJITRequest: JITRequest?

    nonisolated static func jitRequest(_ url: URL) -> JITRequest? {
        guard let scheme = url.scheme?.lowercased(), ["apparition", "stikdebug", "stikjit"].contains(scheme),
              url.host()?.lowercased() == "enable-jit" else { return nil }
        let items = URLComponents(url: url, resolvingAgainstBaseURL: false)?.queryItems ?? []
        func value(_ names: String...) -> String? {
            items.first { item in names.contains { $0.caseInsensitiveCompare(item.name) == .orderedSame } }?
                .value?.trimmingCharacters(in: .whitespacesAndNewlines)
        }
        guard let bundleId = value("bundle-id", "bundleId", "bundleID", "bundle_id"), !bundleId.isEmpty else { return nil }
        let pid = value("pid").flatMap(Int.init) ?? 0
        let script = value("script-data", "scriptData", "script_data").flatMap(jitScript)
        return JITRequest(bundleId: bundleId, pid: max(pid, 0), script: script)
    }

    /// A `script-data` value: base64 or base64url of UTF-8 JavaScript, as StikDebug reads it.
    nonisolated static func jitScript(_ value: String) -> String? {
        var base64 = value.replacingOccurrences(of: "-", with: "+").replacingOccurrences(of: "_", with: "/")
        base64 += String(repeating: "=", count: (4 - base64.count % 4) % 4)
        guard let data = Data(base64Encoded: base64), let script = String(data: data, encoding: .utf8),
              !script.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { return nil }
        return script
    }

    /// An app Apparition installed asks without a question; any other app's request waits for
    /// the person to allow it.
    func isManaged(_ bundleId: String) -> Bool {
        installed.contains { $0.installedBundleId == bundleId }
    }

    func addSource(_ id: String, assetPattern: String?, prereleases: Bool) async {
        let id = id.trimmingCharacters(in: .whitespacesAndNewlines)
        // Adding a source again takes the new options (a GitHub asset pattern, prereleases).
        if let index = sources.firstIndex(where: { $0.id == id }) {
            sources[index].assetPattern = assetPattern
            sources[index].prereleases = prereleases
        } else {
            sources.append(Sources.Source(id: id, assetPattern: assetPattern, prereleases: prereleases))
        }
        await refreshSource(id)
    }

    func removeSource(_ id: String) {
        sources.removeAll { $0.id == id }
        save()
    }

    func refreshSources() async {
        for source in sources { await refreshSource(source.id) }
    }

    private func refreshSource(_ id: String) async {
        guard let index = sources.firstIndex(where: { $0.id == id }) else { return }
        do {
            let (name, apps) = try await Sources.fetch(sources[index])
            sources[index].name = name
            sources[index].apps = apps
            sources[index].error = nil
        } catch {
            sources[index].error = "\(error)"
        }
        sources[index].fetchedAt = DebugTime.iso(Date())
        save()
    }

    // MARK: Install and refresh

    /// Downloads (if this version isn't kept yet), signs and installs `appId` from `sourceId`.
    /// `signer` and `takeOver` come from the install-time choice (`requestInstall`): what signs it
    /// from now on, and a copy under another bundle ID whose data it takes over.
    func install(sourceId: String, appId: String, stripExtensions: Bool, signer: String? = nil, takeOver: String? = nil) async {
        guard let app = sources.first(where: { $0.id == sourceId })?.apps.first(where: { $0.id == appId }) else {
            status = "no app \(appId) in \(sourceId)"
            return
        }
        guard app.marketplace != true else {
            status = "error: \(app.name) is an EU App Marketplace (AltStore PAL) listing, not an IPA, so it can't be sideloaded"
            return
        }
        installErrors[app.id] = nil
        defer {
            let failed = "install \(app.name): error: "
            if appLimit == nil, certificateChoice == nil, let status, status.hasPrefix(failed) {
                installErrors[app.id] = String(status.dropFirst(failed.count))
            }
        }
        await exclusive("install \(app.name)") {
            let file = try await self.download(app)
            let previousFile = self.installed.first { $0.id == app.id }?.ipaFile
            var record = self.installed.first { $0.id == app.id }
                ?? InstalledApp(id: app.id, source: sourceId, name: app.name, version: app.version,
                                ipaFile: file, stripExtensions: stripExtensions)
            record.version = app.version
            record.ipaFile = file
            record.stripExtensions = stripExtensions
            if let signer { record.signWith = signer }
            if let takeOver { record.takeOver = takeOver }
            guard try await self.reviewInstall(&record) else { return "cancelled" }
            if let previousFile, previousFile != file {
                try? FileManager.default.removeItem(at: AccountModel.appsDir.appendingPathComponent(previousFile))
            }
            let outcome = await self.signAndInstall(&record)
            self.upsert(record)
            return outcome
        }
    }

    // MARK: Choosing the signer at install time

    /// An install waiting for the person to choose what signs it.
    struct SignerChoice: Identifiable, Equatable {
        let id = UUID()
        let sourceId: String
        let appId: String
        let appName: String
        /// The app's own bundle ID, when the source lists one (a GitHub release doesn't).
        let bundleId: String?
        let stripExtensions: Bool
    }

    private(set) var signerChoice: SignerChoice?
    var installReview: InstallReview?
    private var reviewContinuation: CheckedContinuation<InstallDecision?, Never>?

    func finishInstallReview(_ decision: InstallDecision?) {
        installReview = nil
        let continuation = reviewContinuation
        reviewContinuation = nil
        continuation?.resume(returning: decision)
    }


    /// Installs an app from a source. A new app, with more than one way to sign it (the Apple ID
    /// and an imported certificate, say), asks first what signs it: an installed app can't be
    /// replaced by a copy signed by another team, so it is chosen before the first install. An app
    /// already in the library keeps its signer (its menu changes it), and one way to sign it needs no asking.
    func requestInstall(sourceId: String, appId: String, stripExtensions: Bool) {
        guard let app = sources.first(where: { $0.id == sourceId })?.apps.first(where: { $0.id == appId }) else {
            status = "no app \(appId) in \(sourceId)"
            return
        }
        let choices = signing.signerOptions(account: account).filter { $0.unusable == nil }
        guard !installed.contains(where: { $0.id == appId }), choices.count > 1, !busy else {
            Task { await install(sourceId: sourceId, appId: appId, stripExtensions: stripExtensions) }
            return
        }
        signerChoice = SignerChoice(sourceId: sourceId, appId: appId, appName: app.name,
                                    bundleId: app.id.hasPrefix("github:") ? nil : app.id, stripExtensions: stripExtensions)
    }

    func cancelSignerChoice() {
        signerChoice = nil
    }

    /// Runs the install `requestInstall` held back, with what the person chose.
    func confirmSignerChoice(signer: String, takeOver: String?) async {
        guard let choice = signerChoice else { return }
        signerChoice = nil
        await install(sourceId: choice.sourceId, appId: choice.appId, stripExtensions: choice.stripExtensions,
                      signer: signer, takeOver: takeOver)
    }

    /// Copies of the app whose own bundle ID is `bundleId` that are on the device under another
    /// bundle ID, as the last app listing has them: the same app signed by another team (an Apple ID
    /// adds its team ID to the bundle ID). Only development-signed ones, since only their data can be read.
    func otherCopies(of bundleId: String) -> [ProbeModel.DeviceApp] {
        probe.deviceApps.filter { app in
            guard app.debuggable, app.bundleId != Bundle.main.bundleIdentifier else { return false }
            if app.bundleId == bundleId { return true }
            guard app.bundleId.hasPrefix(bundleId + ".") else { return false }
            let suffix = app.bundleId.dropFirst(bundleId.count + 1)
            return suffix.count == 10 && suffix.allSatisfy { $0.isASCII && ($0.isUppercase || $0.isNumber) }
        }
    }

    /// The app's team on the device if `bundleId` is installed there under a team other than `team`:
    /// installd refuses to replace such a copy. Reads the device's apps again before saying so.
    func conflictingTeam(bundleId: String, team: String) async -> String? {
        func clash() -> String? {
            probe.deviceApps.first { $0.bundleId == bundleId && $0.teamId != nil && $0.teamId != team }?.teamId
        }
        guard clash() != nil else { return nil }
        await refreshDeviceApps()
        return clash()
    }

    /// The ways to sign an app now (`SigningStore.signerOptions`).
    var signerOptions: [SigningStore.SignerOption] { signing.signerOptions(account: account) }

    /// The app on the device with this bundle ID, as the last app listing has it.
    func installedOnDevice(_ bundleId: String) -> ProbeModel.DeviceApp? {
        probe.deviceApps.first { $0.bundleId == bundleId }
    }

    /// Copies one installed app's data to another's, nothing more (the debug endpoint's use).
    func migrateData(from: String, to: String) async -> (outcome: String, move: ProbeModel.DataMove?) {
        await probe.migrateData(host: probe.defaultTunnelHost, port: probe.defaultTunnelPort, from: from, to: to)
    }

    /// Reads the device's apps through the tunnel again.
    func refreshDeviceApps() async {
        await probe.listApps(host: probe.defaultTunnelHost, port: probe.defaultTunnelPort)
    }

    // MARK: Apps from a file

    /// What the library lists as the source of an app that came from a file.
    nonisolated static let fileSource = "a file"

    /// An .ipa or .deb the person gave Apparition, from reading it to asking whether to install it.
    enum IncomingFile: Equatable {
        case reading(String)
        case ready(IncomingApp)
        case failed(file: String, message: String)
    }

    struct IncomingApp: Equatable {
        let bundleId: String
        let name: String
        let version: String
        /// The kept copy, in the apps folder, as an IPA whatever it arrived as.
        let ipaFile: String
        let fileName: String
    }

    nonisolated static func isAppFile(_ url: URL) -> Bool {
        ["ipa", "deb"].contains(url.pathExtension.lowercased())
    }

    /// Keeps a copy of an .ipa, or the app a .deb package holds (repacked as an IPA), and reads what
    /// app it is. Nothing is signed or installed until `installIncoming`, which the person confirms.
    func receiveAppFile(_ url: URL) async {
        let fileName = url.lastPathComponent
        guard !busy else {
            incoming = .failed(file: fileName, message: "Apparition is busy with another install. Open the file again when it finishes.")
            return
        }
        discardIncoming()
        incoming = .reading(fileName)
        let scoped = url.startAccessingSecurityScopedResource()
        defer { if scoped { url.stopAccessingSecurityScopedResource() } }
        let folder = AccountModel.appsDir
        let part = folder.appendingPathComponent("incoming-\(UUID().uuidString).part")
        let kept = folder.appendingPathComponent("incoming-\(UUID().uuidString).kept")
        defer {
            try? FileManager.default.removeItem(at: part)
            try? FileManager.default.removeItem(at: kept)
        }
        do {
            // Off the main actor: an app is hundreds of megabytes to copy, and a package is repacked.
            try await Task.detached {
                try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
                try FileManager.default.copyItem(at: url, to: part)
                try await Self.keepAsIPA(part, to: kept, subject: fileName)
            }.value
            let info = await Task.detached { AccountModel.call(apparition_app_info(kept.path)) }.value
            guard info["ok"] as? Bool == true, let bundleId = info["bundleId"] as? String else {
                throw IDevice.ProbeError("\(fileName): \(info["error"] as? String ?? "no app in it")")
            }
            let name = info["name"] as? String ?? bundleId
            let version = info["version"] as? String ?? ""
            let safe = bundleId.replacingOccurrences(of: "/", with: "_").replacingOccurrences(of: ":", with: "_")
            let file = "file_\(safe)-\(version).ipa"
            let destination = folder.appendingPathComponent(file)
            try? FileManager.default.removeItem(at: destination)
            try FileManager.default.moveItem(at: kept, to: destination)
            incoming = .ready(IncomingApp(bundleId: bundleId, name: name, version: version, ipaFile: file, fileName: fileName))
            log.info("received an app file")
        } catch {
            incoming = .failed(file: fileName, message: "\(error)")
            log.error("could not read an app file: \(String(describing: error))")
        }
        account.refreshDownloads()
    }

    /// Signs and installs the app `receiveAppFile` read, and keeps it refreshed like any other.
    func installIncoming(signer: String? = nil, takeOver: String? = nil) async {
        guard case .ready(let app) = incoming else { return }
        incoming = nil
        installErrors[app.bundleId] = nil
        defer {
            let failed = "install \(app.name): error: "
            if appLimit == nil, certificateChoice == nil, let status, status.hasPrefix(failed) {
                installErrors[app.bundleId] = String(status.dropFirst(failed.count))
            }
        }
        await exclusive("install \(app.name)") {
            // The same app from a source keeps its source; a file just replaces the kept copy.
            let existing = self.installed.first { $0.id == app.bundleId }
            var record = existing ?? InstalledApp(id: app.bundleId, source: Self.fileSource, name: app.name, version: app.version,
                                                  ipaFile: app.ipaFile, stripExtensions: true)
            record.version = app.version
            record.ipaFile = app.ipaFile
            if let signer { record.signWith = signer }
            if let takeOver { record.takeOver = takeOver }
            guard try await self.reviewInstall(&record) else { return "cancelled" }
            if let old = existing?.ipaFile, old != app.ipaFile {
                try? FileManager.default.removeItem(at: AccountModel.appsDir.appendingPathComponent(old))
            }
            let outcome = await self.signAndInstall(&record)
            self.upsert(record)
            self.account.refreshDownloads()
            return outcome
        }
    }

    /// Drops the app `receiveAppFile` read, and its kept copy unless the library uses it.
    func discardIncoming() {
        if case .ready(let app) = incoming, !installed.contains(where: { $0.ipaFile == app.ipaFile }) {
            try? FileManager.default.removeItem(at: AccountModel.appsDir.appendingPathComponent(app.ipaFile))
            account.refreshDownloads()
        }
        incoming = nil
    }

    /// Re-signs and reinstalls every kept app that expires within `refreshMargin` (all of them
    /// with `force`). Called by the background task and by hand.
    @discardableResult
    func refresh(force: Bool, trigger: String) async -> RefreshRun {
        var run = RefreshRun(startedAt: DebugTime.iso(Date()), trigger: trigger)
        refreshRuns.insert(run, at: 0)
        save()
        let due = installed.filter { force || Self.isDue($0) }
        // Apparition itself only when due, even with force: each update waits for the app to close.
        let selfDue = selfIsDue
        if due.isEmpty && !selfDue {
            run.results["-"] = "nothing due"
        } else if busy {
            run.results["-"] = "busy"
        } else {
            busy = true
            inRefreshRun = true
            defer { inRefreshRun = false; runSignInFailure = nil }
            for var record in due {
                guard !stopping else {
                    run.results[record.id] = Self.stoppedResult
                    continue
                }
                status = "refreshing \(record.name)"
                run.results[record.id] = await signAndInstall(&record)
                upsert(record)
            }
            if selfDue && !stopping {
                // Last: its update waits in installd until Apparition closes.
                status = "refreshing Apparition"
                run.results[Self.selfResultKey] = await signAndInstallSelf(runStartedAt: run.startedAt)
            }
            busy = false
            status = nil
        }
        run.finishedAt = DebugTime.iso(Date())
        if let index = refreshRuns.firstIndex(where: { $0.startedAt == run.startedAt && $0.trigger == trigger }) {
            run.expiredAt = refreshRuns[index].expiredAt
            refreshRuns[index] = run
        } else {
            refreshRuns.insert(run, at: 0)
        }
        save()
        scheduleRefresh()
        log.info("refresh (\(trigger, privacy: .public)): \(run.results.count, privacy: .public) results")
        return run
    }

    nonisolated static func isDue(_ app: InstalledApp, now: Date = Date()) -> Bool {
        guard let expires = expiryDate(app) else { return true }
        return expires.timeIntervalSince(now) < refreshMargin
    }

    /// When the app's signing profile runs out, if it was signed here.
    nonisolated static func expiryDate(_ app: InstalledApp) -> Date? {
        app.expiresAt.flatMap { try? Date($0, strategy: Date.ISO8601FormatStyle(includingFractionalSeconds: true)) }
    }

    /// Re-signs and reinstalls one app now, due or not (the Library's per-app Refresh).
    func refresh(appId: String) async {
        guard let found = installed.first(where: { $0.id == appId }) else { return }
        var record = found
        var run = RefreshRun(startedAt: DebugTime.iso(Date()), trigger: "app")
        await exclusive("refresh \(record.name)") {
            let outcome = await self.signAndInstall(&record)
            self.upsert(record)
            run.results[record.id] = outcome
            return outcome
        }
        guard !run.results.isEmpty else { return }  // busy: nothing ran
        run.finishedAt = DebugTime.iso(Date())
        refreshRuns.insert(run, at: 0)
        save()
    }

    /// When Apparition opened, for telling a device app listing taken since from an older one.
    nonisolated static let launchedAt = Date()

    /// Whether the device's apps, listed since Apparition opened, no longer include this app: it
    /// was deleted from the Home Screen. An older listing, or none, says nothing, and neither does
    /// one taken before the app was last installed (the listing doesn't have it yet).
    func isMissingFromDevice(_ app: InstalledApp) -> Bool {
        guard let bundleId = app.installedBundleId,
              let listed = Date.iso(probe.deviceAppsFetchedAt), listed >= Self.launchedAt else { return false }
        if let signed = Date.iso(app.signedAt), listed < signed { return false }
        return !probe.deviceApps.contains { $0.bundleId == bundleId }
    }

    /// Forgets an installed app: Apparition stops refreshing it and deletes its kept IPA. The
    /// app itself stays on the device until its profile expires (or the user deletes it).
    /// Works while a refresh runs (a stuck one would otherwise keep a deleted app in the library);
    /// that run then leaves the record out.
    func removeInstalled(_ id: String) throws {
        let removed = installed.filter { $0.id == id }
        let remaining = installed.filter { $0.id != id }
        // Persist first, so a failed write cannot make the app appear removed until relaunch.
        try writeState(installed: remaining)
        if busy { removedWhileBusy.insert(id) }
        installed = remaining
        StateVault.sync()
        for file in Set(removed.map(\.ipaFile)) where !remaining.contains(where: { $0.ipaFile == file }) {
            do {
                try FileManager.default.removeItem(at: AccountModel.appsDir.appendingPathComponent(file))
            } catch let error as CocoaError where error.code == .fileNoSuchFile {
                continue
            } catch {
                log.error("could not delete a removed app's kept copy: \(String(describing: error), privacy: .private)")
            }
        }
        account.refreshDownloads()
        log.info("removed an app from the library")
    }

    // MARK: The free-profile app limit

    func dismissAppLimit() {
        appLimit = nil
    }

    /// Deletes `bundleId` from the device to free a slot under the app limit, then retries the
    /// install that hit it. A library app removed this way is forgotten too, or its next
    /// refresh would take the slot back. Never Apparition itself.
    func removeForAppLimit(_ bundleId: String) async {
        guard let limit = appLimit, bundleId != Bundle.main.bundleIdentifier, !busy else { return }
        appLimit?.removalError = nil
        var outcome = "busy"
        await exclusive("remove \(displayName(of: bundleId))") {
            outcome = await self.probe.uninstall(host: self.probe.defaultTunnelHost, port: self.probe.defaultTunnelPort,
                                                 bundleId: bundleId)
            return outcome
        }
        guard outcome == "ok" else {
            appLimit?.removalError = outcome.hasPrefix("error: ") ? String(outcome.dropFirst(7)) : outcome
            log.error("removing an app for the app limit failed")
            return
        }
        assignments.remove(target: bundleId)
        save()
        log.info("removed \(bundleId, privacy: .private(mask: .hash)) for the app limit")
        if let managed = installed.first(where: { $0.installedBundleId == bundleId }) {
            do {
                try removeInstalled(managed.id)
            } catch {
                appLimit?.removalError = "The app was removed from the device, but not from the library: \(error.localizedDescription)"
                log.error("could not remove an app from the library: \(String(describing: error), privacy: .private)")
                return
            }
        }
        appLimit = nil
        await retry(limit.retry)
    }

    /// Runs the install that hit the limit again (after the user deleted an app themselves).
    func retryAfterAppLimit() async {
        guard let limit = appLimit, !busy else { return }
        await retry(limit.retry)
    }

    // MARK: Another Apparition's certificate

    struct CertificateChoice: Sendable, Equatable {
        /// The app that wasn't signed, as the user knows it.
        let appName: String
        let retry: AppLimit.Retry
    }

    nonisolated static let sharedCertificateMessage = "Apparition on another device already has this Apple ID's certificate: get its key from there (Settings > Get Signing from Another Device), or make a new certificate here"

    func dismissCertificateChoice() {
        certificateChoice = nil
    }

    /// Runs the signing that stopped again, once the other device's key is here.
    func retryAfterCertificateChoice() async {
        guard let choice = certificateChoice, !busy else { return }
        certificateChoice = nil
        await retry(choice.retry)
    }

    /// Lets the core make a certificate of this install's own, then runs the signing again.
    func makeNewCertificate() async {
        guard let choice = certificateChoice, !busy, let email = account.email ?? account.lastAppleID else { return }
        _ = await Task.detached { AccountModel.call(apparition_allow_new_certificate(email)) }.value
        log.notice("making a new certificate, as chosen")
        certificateChoice = nil
        await retry(choice.retry)
    }

    private func retry(_ retry: AppLimit.Retry) async {
        switch retry {
        case .app(let id): await refresh(appId: id)
        case .selfRefresh: await refreshSelf()
        case .selfUpdate: await updateSelf()
        }
    }

    #if DEBUG
    /// Shows the app-limit card for `error` as if installing `appId` (default: the first
    /// library app) hit it. Returns the parsed bundle IDs, or nil when it isn't that error.
    func showAppLimit(_ error: String, appId: String?) -> [String]? {
        guard let blocking = AppLimit.blockingApps(in: error) else { return nil }
        let record = installed.first { $0.id == appId } ?? installed.first
        appLimit = AppLimit(blocking: blocking, appName: record?.name ?? "UTM", retry: .app(id: record?.id ?? appId ?? "-"))
        return blocking
    }
    #endif

    /// What the user calls an app on the device: the library's name, the last app listing's,
    /// or the bundle ID.
    func displayName(of bundleId: String) -> String {
        if bundleId == Bundle.main.bundleIdentifier { return "Apparition" }
        return installed.first { $0.installedBundleId == bundleId }?.name
            ?? probe.deviceApps.first { $0.bundleId == bundleId }?.name
            ?? bundleId
    }

    /// The icon a library app's source lists, for an app on the device.
    func iconURL(of bundleId: String) -> URL? {
        installed.first { $0.installedBundleId == bundleId }
            .flatMap(sourceApp(for:))?.iconURL.flatMap(URL.init(string:))
    }

    /// Chooses what signs an app from its next refresh on (nil: the default).
    func setSigner(_ signer: String?, for id: String) {
        guard !busy, let index = installed.firstIndex(where: { $0.id == id }) else { return }
        installed[index].signWith = signer
        installed[index].needsInstallReview = true
        save()
    }

    /// The source's listing for an installed app (icon, newest version), if its source is still added.
    func sourceApp(for app: InstalledApp) -> Sources.App? {
        sources.first { $0.id == app.source }?.apps.first { $0.id == app.id }
    }

    private static let noUDID = "can't read this device's UDID (paired, and LocalDevVPN or the companion up?)"

    /// Why nothing can be installed right now, when there's no pairing: checked before signing,
    /// so an install or refresh says so at once instead of after signing.
    private var missingPairing: String? {
        probe.refreshPairing()
        return probe.pairingProblem == .missing ? PairingRecord.message(.missing) : nil
    }

    /// Why the device's UDID couldn't be read: the pairing when that's what's wrong.
    private var noUDIDReason: String {
        probe.pairingProblem.map(PairingRecord.message) ?? Self.noUDID
    }

    private func deviceUDID() async -> String? {
        if UserDefaults.standard.string(forKey: "pairedUDID") == nil {
            await probe.identify(host: probe.defaultTunnelHost, port: probe.defaultTunnelPort)
        }
        return UserDefaults.standard.string(forKey: "pairedUDID")
    }

    /// Ends the refresh or install under way (core/src/stop.rs): the sign-in, signing, tunnel or
    /// install running now stops at its next wait, a 2FA prompt is answered with a cancel, and a
    /// refresh run skips the apps it hadn't reached. What the device already received stays.
    func stop() {
        guard busy, !stopping else { return }
        stopping = true
        status = "stopping…"
        if account.twoFactor != nil { account.answerTwoFactor("abort") }
        downloading?.cancel()
        finishInstallReview(nil)
        apparition_stop()
        log.notice("stop requested")
    }

    /// The core's message for work Stop ended (`STOPPED` in core/src/stop.rs).
    nonisolated static let stoppedText = "Stopped before it finished"

    /// Signs in for work that signs with the Apple ID: nil when there's a session, else why not.
    /// Within a refresh run, only the first app tries; the others get the same reason.
    private func signInFailure() async -> String? {
        if let runSignInFailure { return runSignInFailure }
        if await account.signInWithRememberedPassword() { return nil }
        let reason = account.notSignedInReason
        if inRefreshRun { runSignInFailure = reason }
        return reason
    }

    private func signAndInstall(_ record: inout InstalledApp) async -> String {
        if removedWhileBusy.contains(record.id) { return Self.stoppedResult }
        if record.needsInstallReview == true { return fail(&record, "Review Install Options before refreshing with a different signer") }
        if let missingPairing { return fail(&record, missingPairing) }
        guard let udid = await deviceUDID() else {
            return fail(&record, noUDIDReason)
        }
        let name = UserDefaults.standard.string(forKey: "pairedName") ?? ThisDevice.name
        let path = AccountModel.appsDir.appendingPathComponent(record.ipaFile).path
        var signingPath = path
        if let selection = record.selectedExtensions {
            let reply = await Task.detached { AccountModel.call(apparition_install_inventory(path)) }.value
            guard reply["ok"] as? Bool == true, let raw = reply["inventory"],
                  let data = try? JSONSerialization.data(withJSONObject: raw),
                  let inventory = try? JSONDecoder().decode(InstallInventory.self, from: data) else {
                return fail(&record, reply["error"] as? String ?? "Could not inspect extensions")
            }
            let retained = selection.filter { id in inventory.extensions.contains { $0.id == id } }
            let json = String(data: (try? JSONEncoder().encode(retained)) ?? Data(), encoding: .utf8) ?? "[]"
            let prepared = await Task.detached { AccountModel.call(apparition_select_extensions(path, json)) }.value
            guard prepared["ok"] as? Bool == true, let selectedPath = prepared["path"] as? String else {
                return fail(&record, prepared["error"] as? String ?? "Could not prepare extensions")
            }
            signingPath = selectedPath
        }
        let signer = signing.signer(for: record)
        if record.selectedExtensions != nil, let chosen = record.signWith, chosen != signer {
            return fail(&record, "The chosen signer is no longer available. Review Install Options before refreshing.")
        }
        // The copy whose data this install takes over: the one chosen at install time, else the one
        // the library installed before, when the signer changed.
        let previousId = record.selectedExtensions != nil ? record.takeOver : record.takeOver ?? (signing.movesData ? record.installedBundleId : nil)
        if record.takeOver == nil, previousId != record.assignedBundleId { record.takeOver = previousId }
        upsert(record)
        let signed: [String: Any]
        if signer == SigningStore.appleID {
            if let reason = await signInFailure() { return fail(&record, reason) }
            if let target = record.assignedBundleId {
                guard record.assignedTeam == account.team?.id else { return fail(&record, "This App ID is assigned to another team. Review the install with the selected Apple ID first.") }
                let selectedPath = signingPath
                signed = await Task.detached { AccountModel.call(apparition_sign_assigned(selectedPath, udid, name, target)) }.value
            } else {
                signed = await AccountModel.signFile(signingPath, udid: udid, deviceName: name, stripExtensions: record.selectedExtensions == nil && record.stripExtensions)
            }
        } else {
            // An imported certificate signs on its own: no sign-in, no anisette, no slots.
            signed = await SigningStore.signFile(signingPath, identityId: signer, udid: udid, stripExtensions: record.selectedExtensions == nil && record.stripExtensions)
        }
        guard signed["ok"] as? Bool == true, let signedPath = signed["signedPath"] as? String else {
            if signed["code"] as? String == "sharedCertificate" {
                certificateChoice = CertificateChoice(appName: record.name, retry: .app(id: record.id))
                return fail(&record, Self.sharedCertificateMessage, logged: "stopped before making a second certificate")
            }
            return fail(&record, "sign failed: \(signed["error"] as? String ?? "unknown")")
        }
        if signer == SigningStore.appleID { certificateChoice = nil }
        let main = AccountModel.inspect(URL(fileURLWithPath: signedPath), udid: udid).first
        // installd can't replace an app with a copy from another team, and says so cryptically.
        if record.discardDestinationData != true, let bundleId = main?.bundleId, let team = main?.profileTeam,
           let other = await conflictingTeam(bundleId: bundleId, team: team) {
            try? FileManager.default.removeItem(atPath: signedPath)
            return fail(&record, "this \(ThisDevice.noun) already has \(bundleId) signed by team \(other), and \(ThisDevice.os) won't replace it with a copy signed by team \(team). Sign it with team \(other)'s certificate, or delete the app from the \(ThisDevice.home) first (its data goes with it).",
                        logged: "installed copy belongs to another team")
        }
        if let expected = record.reviewedBundleId, main?.bundleId != expected {
            try? FileManager.default.removeItem(atPath: signedPath)
            return fail(&record, "The provisioning profile now uses a different App ID. Review Install Options before installing.")
        }
        if record.discardDestinationData == true, let target = main?.bundleId {
            guard target != Bundle.main.bundleIdentifier else { return fail(&record, "Apparition cannot replace itself with another app") }
            let listing = await probe.listApps(host: probe.defaultTunnelHost, port: probe.defaultTunnelPort)
            guard listing == "ok" else { return fail(&record, "Could not check the destination before deleting data: \(listing)") }
            if installedOnDevice(target) != nil {
                let removed = await probe.uninstall(host: probe.defaultTunnelHost, port: probe.defaultTunnelPort, bundleId: target)
                guard removed == "ok" else { return fail(&record, "Could not discard destination data: \(removed)") }
            }
            record.discardDestinationData = nil
            upsert(record)
        }
        let outcome = await probe.install(host: probe.defaultTunnelHost, port: probe.defaultTunnelPort, appPath: signedPath,
                                          sourceIPA: path)
        try? FileManager.default.removeItem(atPath: signedPath)
        if let blocking = AppLimit.blockingApps(in: outcome) {
            appLimit = AppLimit(blocking: blocking, appName: record.name, retry: .app(id: record.id))
            return fail(&record, Self.appLimitMessage(blocking), logged: "free-profile app limit reached")
        }
        appLimit = nil
        guard outcome == "ok" else {
            return fail(&record, Self.installFailure(outcome))
        }
        if let target = main?.bundleId, let team = main?.profileTeam {
            assignments.complete(target: target, team: team)
        }
        record.installedBundleId = main?.bundleId
        record.signedAt = DebugTime.iso(Date())
        record.expiresAt = main?.profileExpires
        record.signedWith = signing.signerName(signer)
        record.lastError = nil
        var moved = ""
        if let previousId, let newId = main?.bundleId, previousId != newId {
            let transfer = await moveData(from: previousId, to: newId)
            moved = transfer.message
            if transfer.complete { record.takeOver = nil }
        } else {
            record.takeOver = nil
        }
        return "ok, expires \(record.expiresAt ?? "?")" + moved
    }

    /// Copies the data of `previousId`, the app's copy under another bundle ID, to the copy just
    /// installed as `newId`, then removes the old one once everything arrived. The new copy hasn't
    /// run yet. Returns what to add to the install's result:
    /// nothing when the old copy is gone, and the reason when its data stayed behind (the old copy
    /// is then still installed).
    private func moveData(from previousId: String, to newId: String) async -> (message: String, complete: Bool) {
        let own = Bundle.main.bundleIdentifier
        guard previousId != own, newId != own else { return ("", true) }
        let listing = await probe.listApps(host: probe.defaultTunnelHost, port: probe.defaultTunnelPort)
        guard listing == "ok" else { return ("; couldn't check the previous copy: \(listing). Data transfer is pending.", false) }
        guard let old = probe.deviceApps.first(where: { $0.bundleId == previousId }) else { return ("", true) }
        let name = displayName(of: previousId)
        guard old.debuggable else {
            log.notice("old copy isn't development-signed; data left")
            return ("; \(name)'s data stayed in the old copy (\(previousId)): it isn't development-signed, so its data can't be read", false)
        }
        let host = probe.defaultTunnelHost, port = probe.defaultTunnelPort
        let (outcome, move) = await probe.migrateData(host: host, port: port, from: previousId, to: newId)
        guard outcome == "ok", let move else {
            log.error("data move failed")
            return ("; \(name)'s data didn't move: \(outcome.hasPrefix("error: ") ? String(outcome.dropFirst(7)) : outcome). The old copy (\(previousId)) is still installed", false)
        }
        guard move.complete else {
            log.error("data move incomplete: \(move.failed.count, privacy: .public) files")
            return ("; \(move.failed.count) files of \(name)'s data didn't copy (\(move.failed.first ?? "")). The old copy (\(previousId)) is still installed", false)
        }
        let removed = await probe.uninstall(host: host, port: port, bundleId: previousId)
        log.info("moved \(move.summary, privacy: .public) of data to the new copy")
        if removed == "ok" {
            assignments.remove(target: previousId)
            return ("; moved \(move.summary) of data from the copy signed by team \(old.teamId ?? "?") and removed it", true)
        }
        return ("; moved \(move.summary) of data, but couldn't remove the old copy (\(previousId)): delete it from the \(ThisDevice.home)", false)
    }


    // MARK: Apparition itself

    /// What re-signs Apparition: the imported certificate its profile names, else the Apple ID.
    /// Either way it keeps the installed bundle ID and team, or the refresh is refused.
    var selfSigner: String {
        signing.identities.first { !$0.expired && selfApp?.certificateIds.contains($0.id) == true }?.id ?? SigningStore.appleID
    }

    var selfIsDue: Bool {
        guard let expires = selfApp?.expires else { return false }
        return expires.timeIntervalSinceNow < Self.refreshMargin
    }

    /// A newer release this copy can install: one for this platform, signed by a release key.
    var availableUpdate: SelfUpdate.Latest? {
        guard let latest = updateCheck.latest, latest.appSize != nil,
              SelfUpdate.isNewer(latest.version, than: SelfUpdate.currentVersion) else { return nil }
        return latest
    }

    /// Asks apparition.illixion.com for the latest release, at most every 6 hours unless `force`.
    func checkForUpdate(force: Bool = false) async {
        if updateCheck.checking { return }
        if !force, let checked = updateCheck.checkedAt, Date().timeIntervalSince(checked) < 6 * 3600 { return }
        updateCheck.checking = true
        defer { updateCheck.checking = false }
        do {
            updateCheck.latest = try await SelfUpdate.latest()
            updateCheck.error = nil
            log.info("latest release \(self.updateCheck.latest?.version ?? "?", privacy: .public), this is \(SelfUpdate.currentVersion, privacy: .public)")
        } catch {
            // What failed to verify isn't offered, even if an earlier check had it.
            updateCheck.latest = nil
            updateCheck.error = "\(error)"
            log.error("update check failed: \(error, privacy: .private)")
        }
        updateCheck.checkedAt = Date()
    }

    /// Downloads the latest release, signs it like a self-refresh and installs it over this copy,
    /// keeping its data. It applies when Apparition next closes.
    func updateSelf() async {
        await runSelf(trigger: "update", label: "update Apparition") { runStartedAt in
            let downloaded: SelfUpdate.Downloaded
            do {
                downloaded = try await SelfUpdate.download()
            } catch {
                return self.selfFail("couldn't download the update: \(error)")
            }
            defer { SelfUpdate.cleanUp() }
            guard SelfUpdate.isNewer(downloaded.version, than: SelfUpdate.currentVersion) else {
                return self.selfFail("the site's latest release is \(downloaded.version), not newer than this copy (\(SelfUpdate.currentVersion))")
            }
            return await self.signAndInstallSelf(runStartedAt: runStartedAt, update: downloaded)
        }
    }

    /// Re-signs Apparition now, due or not; the update applies when the app next closes.
    func refreshSelf() async {
        await runSelf(trigger: "self", label: "refresh Apparition") { runStartedAt in
            await self.signAndInstallSelf(runStartedAt: runStartedAt)
        }
    }

    private func runSelf(trigger: String, label: String, _ body: @escaping (String) async -> String) async {
        var run = RefreshRun(startedAt: DebugTime.iso(Date()), trigger: trigger)
        refreshRuns.insert(run, at: 0)
        save()
        await exclusive(label) {
            let outcome = await body(run.startedAt)
            run.results[Self.selfResultKey] = outcome
            return outcome
        }
        run.finishedAt = DebugTime.iso(Date())
        if let index = refreshRuns.firstIndex(where: { $0.startedAt == run.startedAt && $0.trigger == trigger }) {
            if run.results.isEmpty { run.results["-"] = "busy" }
            refreshRuns[index] = run
        }
        save()
    }

    /// Copies the installed bundle (or `update`'s, a newer release), signs it with `selfSigner`
    /// under the same bundle ID and team, and installs it over the running copy, which keeps
    /// the app's data.
    private func signAndInstallSelf(runStartedAt: String?, update: SelfUpdate.Downloaded? = nil) async -> String {
        guard let me = selfApp, let team = me.teamId else {
            return selfFail("this copy of Apparition has no provisioning profile to renew")
        }
        if let missingPairing { return selfFail(missingPairing) }
        guard let udid = await deviceUDID() else { return selfFail(noUDIDReason) }
        let name = UserDefaults.standard.string(forKey: "pairedName") ?? ThisDevice.name
        let signer = selfSigner
        var signingId: String?
        if signer == SigningStore.appleID {
            if let reason = await signInFailure() { return selfFail(reason) }
            guard account.team?.id == team else {
                return selfFail("Apparition was installed by team \(team), but the Apple ID signs for team \(account.team?.id ?? "?"), so it would install as a second copy. Sign in with the Apple ID that installed it, or import the certificate that signed it.")
            }
            // isideload appends the team ID itself.
            guard let base = me.appleIDBaseBundleId(team: team) else {
                return selfFail("Apparition's bundle ID (\(me.bundleId)) doesn't end in its team ID, so the Apple ID would rename it. Import the certificate that signed it.")
            }
            signingId = base
        }

        let copy: URL
        do {
            // A release is built as the plain bundle ID; it has to install as this copy's.
            copy = if let update {
                try SelfApp.copyBundle(from: update.appURL, bundleId: signingId ?? me.bundleId)
            } else {
                try SelfApp.copyBundle(bundleId: signingId)
            }
        } catch {
            return selfFail("couldn't copy Apparition for signing: \(error)")
        }
        defer { try? FileManager.default.removeItem(at: copy.deletingLastPathComponent()) }
        let signed: [String: Any] = if signer == SigningStore.appleID {
            await AccountModel.signFile(copy.path, udid: udid, deviceName: name, stripExtensions: false)
        } else {
            // An update is unsigned, so the Keychain groups to keep come from the running copy.
            await SigningStore.signFile(copy.path, identityId: signer, udid: udid, stripExtensions: false,
                                        previousApp: update == nil ? nil : Bundle.main.bundleURL.path)
        }
        guard signed["ok"] as? Bool == true, let signedPath = signed["signedPath"] as? String else {
            if signed["code"] as? String == "sharedCertificate" {
                certificateChoice = CertificateChoice(appName: "Apparition", retry: update == nil ? .selfRefresh : .selfUpdate)
                return selfFail(Self.sharedCertificateMessage, logged: "stopped before making a second certificate")
            }
            return selfFail("sign failed: \(signed["error"] as? String ?? "unknown")")
        }
        if signer == SigningStore.appleID { certificateChoice = nil }
        let main = AccountModel.inspect(URL(fileURLWithPath: signedPath), udid: udid).first
        guard main?.bundleId == me.bundleId, main?.profileTeam == team else {
            return selfFail("signing gave \(main?.bundleId ?? "?") (team \(main?.profileTeam ?? "?")), not \(me.bundleId) (team \(team)), which would install a second copy; not installed")
        }

        let signedWith = signing.signerName(signer)
        selfRecord.pending = PendingSelfRefresh(startedAt: DebugTime.iso(Date()), runStartedAt: runStartedAt,
                                                previousExpires: me.expires.map(DebugTime.iso),
                                                previousStamp: SelfApp.installStamp(), signedWith: signedWith,
                                                version: update?.version)
        save()
        log.info("installing over itself")
        // installd waits for Apparition to close before applying the update, so don't wait for that.
        let outcome = await probe.install(host: probe.defaultTunnelHost, port: probe.defaultTunnelPort, appPath: signedPath,
                                          detachAfterMs: 20_000)
        if outcome == "queued" {
            let what = update.map { "install \($0.version)" } ?? "apply the update"
            return "ok, waiting: close Apparition to \(what) (expires \(main?.profileExpires ?? "?"))"
        }
        selfRecord.pending = nil
        if let blocking = AppLimit.blockingApps(in: outcome) {
            appLimit = AppLimit(blocking: blocking, appName: "Apparition", retry: update == nil ? .selfRefresh : .selfUpdate)
            return selfFail(Self.appLimitMessage(blocking), logged: "free-profile app limit reached")
        }
        appLimit = nil
        guard outcome == "ok" else {
            return selfFail(Self.installFailure(outcome))
        }
        selfRecord.signedAt = DebugTime.iso(Date())
        selfRecord.signedWith = signedWith
        selfRecord.lastError = nil
        save()
        return "ok, expires \(main?.profileExpires ?? "?") (applies when Apparition next opens)"
    }

    /// `logged` replaces the message in the log when it names other apps.
    private func selfFail(_ message: String, logged: String? = nil) -> String {
        if stopping || message.contains(Self.stoppedText) { return Self.stoppedResult }
        selfRecord.lastError = message
        save()
        log.error("Apparition: \(LogScrub.text(logged ?? message), privacy: .public)")
        return "error: \(message)"
    }

    /// After a self-refresh ended the app: the profile now embedded says whether it took.
    private func finishPendingSelfRefresh() {
        guard let pending = selfRecord.pending else { return }
        selfRecord.pending = nil
        let outcome: String
        let replaced = pending.previousStamp.map { $0 != SelfApp.installStamp() }
            ?? Date.iso(pending.previousExpires).map { previous in selfApp?.expires.map { $0 > previous } ?? false } ?? false
        if replaced, let expires = selfApp?.expires {
            selfRecord.signedAt = pending.startedAt
            selfRecord.signedWith = pending.signedWith
            selfRecord.lastError = nil
            outcome = (pending.version == nil ? "ok" : "ok, updated to \(SelfUpdate.currentVersion)") + ", expires \(DebugTime.iso(expires))"
        } else {
            selfRecord.lastError = "the reinstall didn't take: this is still the copy from before"
            outcome = "error: \(selfRecord.lastError!)"
        }
        if let index = refreshRuns.firstIndex(where: { $0.startedAt == pending.runStartedAt }) {
            refreshRuns[index].results[Self.selfResultKey] = outcome
            refreshRuns[index].finishedAt = refreshRuns[index].finishedAt ?? pending.startedAt
        }
        save()
        log.info("self-refresh finished after relaunch: \(outcome.hasPrefix("ok") ? "ok" : "failed", privacy: .public)")
    }

    /// `logged` replaces the message in the log when it names other apps.
    private func fail(_ record: inout InstalledApp, _ message: String, logged: String? = nil) -> String {
        if stopping || message.contains(Self.stoppedText) { return Self.stoppedResult }
        record.lastError = message
        log.error("\(record.name, privacy: .public): \(LogScrub.text(logged ?? message), privacy: .public)")
        return "error: \(message)"
    }

    /// Blames LocalDevVPN or the companion only when the tunnel or the service connection
    /// failed; past that, installd answered and its reason is the whole story.
    private static func installFailure(_ outcome: String) -> String {
        let reason = outcome.hasPrefix("error: ") ? String(outcome.dropFirst(7)) : outcome
        // No pairing, or one the device turned down: the message says what to do.
        if PairingRecord.problem(in: outcome) != nil { return reason }
        return ProbeModel.isUnreachable(reason) ? "install failed (LocalDevVPN connected, or the companion running?): \(reason)" : "install failed: \(reason)"
    }

    private static func appLimitMessage(_ blocking: [String]) -> String {
        "this \(ThisDevice.noun) already has \(blocking.isEmpty ? "the most" : "\(blocking.count)") apps signed by free Apple IDs that \(ThisDevice.os) allows: "
            + (blocking.isEmpty ? "remove one, then try again" : blocking.joined(separator: ", "))
    }

    private func download(_ app: Sources.App) async throws -> String {
        let safe = app.id.replacingOccurrences(of: "/", with: "_").replacingOccurrences(of: ":", with: "_")
        let file = "\(safe)-\(app.version).ipa"
        let destination = AccountModel.appsDir.appendingPathComponent(file)
        if FileManager.default.fileExists(atPath: destination.path) { return file }
        guard let url = URL(string: app.downloadURL) else { throw IDevice.ProbeError("bad download URL") }
        status = "downloading \(app.name) \(app.version)"
        // Held so Stop can cancel it: the core's stop doesn't reach URLSession.
        let fetch = Task { try await URLSession.shared.download(from: url) }
        downloading = fetch
        defer { downloading = nil }
        let (temporary, response) = try await fetch.value
        let code = (response as? HTTPURLResponse)?.statusCode ?? 0
        guard code == 200 else {
            switch code {
            case 404, 410:
                throw IDevice.ProbeError("\(app.name)'s download is gone (HTTP \(code)): the source still lists a file its developer removed. Try another source for it.")
            case 401, 403:
                throw IDevice.ProbeError("the server refused \(app.name)'s download (HTTP \(code)); the source may need its own app or account.")
            default:
                throw IDevice.ProbeError("HTTP \(code) downloading \(app.name)")
            }
        }
        try await Self.keepAsIPA(temporary, to: destination, subject: "\(app.name)'s download")
        account.refreshDownloads()
        return file
    }

    /// Keeps a download as the app's IPA. An IPA is a zip. A .deb package holds the app inside
    /// (Kodi and many older apps are published that way): it's repacked as an IPA, so signing and
    /// refreshing work as for any app. Anything else: say what a source linked instead, rather than
    /// the signer's zip error.
    private static func keepAsIPA(_ file: URL, to destination: URL, subject: String) async throws {
        let handle = try FileHandle(forReadingFrom: file)
        let head = (try? handle.read(upToCount: 512)) ?? Data()
        try? handle.close()
        if head.starts(with: [0x50, 0x4B]) {
            try FileManager.default.moveItem(at: file, to: destination)
            return
        }
        if head.starts(with: Array("!<arch>\n".utf8)) {
            let reply = await Task.detached { AccountModel.call(apparition_convert_package(file.path, destination.path)) }.value
            try? FileManager.default.removeItem(at: file)
            guard reply["ok"] as? Bool == true else {
                throw IDevice.ProbeError("\(subject) is a package with no app Apparition can unpack: \(reply["error"] as? String ?? "unknown")")
            }
            return
        }
        let text = String(decoding: head, as: UTF8.self).lowercased()
        if text.contains("manifestschemaversion") || text.contains("distributionpackage") {
            throw IDevice.ProbeError("\(subject) is an EU App Marketplace (AltStore PAL) package manifest, not an IPA, so it can't be sideloaded.")
        }
        if text.contains("<html") || text.contains("<!doctype") {
            throw IDevice.ProbeError("\(subject) is a web page, not an IPA: the link behind it needs a browser.")
        }
        throw IDevice.ProbeError("\(subject) isn't an IPA or a .deb package.")
    }

    private func upsert(_ record: InstalledApp) {
        guard !removedWhileBusy.contains(record.id) else { return }
        if let index = installed.firstIndex(where: { $0.id == record.id }) {
            installed[index] = record
        } else {
            installed.append(record)
        }
        save()
    }

    private func exclusive(_ label: String, _ body: () async throws -> String) async {
        guard !busy else { status = "busy"; return }
        busy = true
        status = label
        defer { busy = false }
        do {
            status = "\(label): \(try await body())"
        } catch {
            status = stopping ? "\(label): \(Self.stoppedResult)" : "\(label): error: \(error)"
        }
        log.info("\(LogScrub.text(self.status ?? ""), privacy: .public)")
    }

    // MARK: Background refresh

    /// Registers both background tasks. Must run before the app finishes launching.
    func registerBackgroundTasks() {
        for id in [Self.refreshTaskId, Self.processingTaskId] {
            BGTaskScheduler.shared.register(forTaskWithIdentifier: id, using: nil) { [weak self] task in
                Task { @MainActor in self?.runBackground(task) }
            }
        }
    }

    private func runBackground(_ task: BGTask) {
        let trigger = task.identifier == Self.processingTaskId ? "background-processing" : "background"
        let force = UserDefaults.standard.bool(forKey: Self.forceNextBackgroundKey)
        UserDefaults.standard.set(false, forKey: Self.forceNextBackgroundKey)
        let work = Task { await refresh(force: force, trigger: trigger) }
        task.expirationHandler = { [weak self] in
            Task { @MainActor in self?.noteExpired(trigger: trigger) }
            work.cancel()
        }
        Task {
            let run = await work.value
            task.setTaskCompleted(success: run.expiredAt == nil && !run.results.values.contains { $0.hasPrefix("error") })
        }
    }

    private func noteExpired(trigger: String) {
        guard let index = refreshRuns.firstIndex(where: { $0.trigger == trigger && $0.finishedAt == nil })
            ?? refreshRuns.firstIndex(where: { $0.trigger == trigger }) else { return }
        refreshRuns[index].expiredAt = DebugTime.iso(Date())
        save()
        log.error("\(trigger, privacy: .public) run expired")
    }

    /// Asks for background runs: a short one about every 12 hours and a longer one when the
    /// system has time. They only succeed when the tunnel can open (the companion or
    /// LocalDevVPN) and the password is remembered; otherwise the run records why.
    func scheduleRefresh(after delay: TimeInterval = 12 * 3600) {
        let refresh = BGAppRefreshTaskRequest(identifier: Self.refreshTaskId)
        refresh.earliestBeginDate = Date(timeIntervalSinceNow: delay)
        let processing = BGProcessingTaskRequest(identifier: Self.processingTaskId)
        processing.earliestBeginDate = Date(timeIntervalSinceNow: delay)
        processing.requiresNetworkConnectivity = true
        for request in [refresh, processing] as [BGTaskRequest] {
            do {
                try BGTaskScheduler.shared.submit(request)
            } catch {
                log.error("\(request.identifier, privacy: .public) not scheduled: \(String(describing: error), privacy: .public)")
            }
        }
    }

    /// At launch: ask for runs only when none is pending, so a nearer request (from a run
    /// that already rescheduled, or the test hook) isn't pushed back 12 hours.
    func scheduleRefreshIfNeeded() async {
        let pending = await BGTaskScheduler.shared.pendingTaskRequests().map(\.identifier)
        if !pending.contains(Self.refreshTaskId) || !pending.contains(Self.processingTaskId) {
            scheduleRefresh()
        }
    }

    func pendingBackgroundTasks() async -> [[String: String]] {
        await BGTaskScheduler.shared.pendingTaskRequests().map { request in
            ["id": request.identifier, "earliest": request.earliestBeginDate.map(DebugTime.iso) ?? "-"]
        }
    }
}

extension LibraryModel {
    struct InstallExtension: Codable, Identifiable, Sendable {
        let id: String
        let name: String
        let path: String
        let extensionKit: Bool
    }
    struct InstallInventory: Codable, Sendable {
        let id: String
        let name: String
        let extensions: [InstallExtension]
    }
    struct InstallReview: Identifiable {
        let id = UUID()
        let inventory: InstallInventory
        let appleID: Bool
        let target: String
        let selected: [String]
        let unsupported: Set<String>
        let candidates: [DeveloperSlots.AppID]
        let available: Int?
        let signingTeam: String?
        let installed: [ProbeModel.DeviceApp]
        let owners: [String: String]
        let reserved: Set<String>
        let extensionOwners: [String: String]
        let dataSource: String?
    }
    struct InstallDecision {
        let target: String
        let extensions: [String]
        let discard: Bool
    }

    /// Every foreground install reviews the downloaded archive, including updates and file imports.
    /// Refreshes use the persisted plan without presenting UI or enabling newly added extensions.
    private func reviewInstall(_ record: inout InstalledApp) async throws -> Bool {
        guard !removedWhileBusy.contains(record.id) else { return false }
        let signer = signing.signer(for: record)
        let appleID = signer == SigningStore.appleID
        if appleID, let failure = await signInFailure() { throw IDevice.ProbeError(failure) }
        let path = AccountModel.appsDir.appendingPathComponent(record.ipaFile).path
        let reply = await Task.detached { AccountModel.call(apparition_install_inventory(path)) }.value
        guard reply["ok"] as? Bool == true, let raw = reply["inventory"] else {
            throw IDevice.ProbeError(reply["error"] as? String ?? "Could not inspect app")
        }
        let inventory = try JSONDecoder().decode(InstallInventory.self, from: JSONSerialization.data(withJSONObject: raw))
        let team = reply["team"] as? String ?? ""
        if appleID && team.isEmpty { throw IDevice.ProbeError("Sign in and choose a signing team before reviewing App IDs") }
        let ids = appleID ? try await DeveloperSlots.appIDs() : nil
        var certificatePlan: [String: Any] = [:]
        if !appleID {
            guard let udid = await deviceUDID() else { throw IDevice.ProbeError(noUDIDReason) }
            let data = AccountModel.dataDir.path
            certificatePlan = await Task.detached { AccountModel.call(apparition_offline_install_plan(data, path, signer, udid)) }.value
            guard certificatePlan["ok"] as? Bool == true else { throw IDevice.ProbeError(certificatePlan["error"] as? String ?? "Could not resolve provisioning profile") }
        }
        let signingTeam = appleID ? team : certificatePlan["team"] as? String ?? ""
        let listing = await probe.listApps(host: probe.defaultTunnelHost, port: probe.defaultTunnelPort)
        guard listing == "ok" else { throw IDevice.ProbeError("Could not check installed apps before review: \(listing)") }
        let managedTargets = Set(installed.flatMap { [$0.assignedBundleId, $0.reviewedBundleId, $0.installedBundleId, $0.takeOver].compactMap { $0 } })
        assignments.reconcile(installed: Set(probe.deviceApps.map(\.bundleId)), managed: managedTargets)
        save()
        var owners: [String: String] = [:]
        var reserved: Set<String> = [Bundle.main.bundleIdentifier ?? ""]
        var extensionOwners: [String: String] = [:]
        for assignment in assignments.records where assignment.team == signingTeam && assignment.source != record.id {
            owners[assignment.target] = assignment.name
            let children = assignment.claims.subtracting([assignment.target])
            reserved.formUnion(children)
            for child in children { extensionOwners[child] = assignment.target }
        }
        for other in installed where other.id != record.id {
            guard let target = other.assignedBundleId ?? other.reviewedBundleId ?? other.installedBundleId,
                  other.assignedTeam == signingTeam || installedOnDevice(target)?.teamId == signingTeam else { continue }
            owners[target] = other.name
            var source = other.sourceBundleId
            var children = other.selectedExtensions ?? []
            if other.selectedExtensions == nil {
                let oldPath = AccountModel.appsDir.appendingPathComponent(other.ipaFile).path
                let oldReply = await Task.detached { AccountModel.call(apparition_install_inventory(oldPath)) }.value
                if let raw = oldReply["inventory"], let data = try? JSONSerialization.data(withJSONObject: raw),
                   let oldInventory = try? JSONDecoder().decode(InstallInventory.self, from: data) {
                    source = oldInventory.id
                    children = oldInventory.extensions.filter { !other.stripExtensions || !$0.extensionKit }.map(\.id)
                }
            }
            for child in children {
                if let source, child.hasPrefix(source + ".") {
                    let id = target + child.dropFirst(source.count)
                    reserved.insert(id)
                    extensionOwners[id] = target
                }
            }
            if let pending = other.takeOver { reserved.insert(pending) }
        }
        let previousAssignment = assignments.records.last { $0.source == record.id && $0.team == signingTeam }
        let target = appleID ? (record.assignedTeam == team ? record.assignedBundleId : nil) ?? previousAssignment?.target ?? "\(inventory.id).\(team)" : (certificatePlan["target"] as? String ?? inventory.id)
        let supported = Set(certificatePlan["supportedExtensions"] as? [String] ?? [])
        let unsupported = Set(inventory.extensions.filter { appleID ? $0.extensionKit : !supported.contains($0.id) }.map(\.id))
        let review = InstallReview(inventory: inventory, appleID: appleID, target: target,
            selected: record.selectedExtensions ?? (record.installedBundleId == nil ? [] : inventory.extensions.filter { !record.stripExtensions || !$0.extensionKit }.map(\.id)), unsupported: unsupported, candidates: ids?.appIds ?? [], available: ids?.availableQuantity, signingTeam: signingTeam,
            installed: probe.deviceApps, owners: owners, reserved: reserved, extensionOwners: extensionOwners,
            dataSource: record.takeOver ?? record.installedBundleId ?? previousAssignment.flatMap { installedOnDevice($0.target)?.bundleId } ?? installedOnDevice("\(inventory.id).\(team)")?.bundleId)
        guard !removedWhileBusy.contains(record.id) else { return false }
        let decision = await withCheckedContinuation { continuation in
            reviewContinuation = continuation
            installReview = review
        }
        guard let decision, !removedWhileBusy.contains(record.id) else { return false }
        guard !reserved.contains(decision.target),
              (decision.target == target || (appleID && ids?.appIds.contains(where: { $0.identifier == decision.target }) == true)),
              decision.extensions.allSatisfy({ id in inventory.extensions.contains { $0.id == id && !unsupported.contains(id) } }) else {
            throw IDevice.ProbeError("The chosen signing plan is no longer available")
        }
        for child in decision.extensions {
            guard child.hasPrefix(inventory.id + ".") else { throw IDevice.ProbeError("Extension ID does not belong to this app") }
            let assigned = decision.target + child.dropFirst(inventory.id.count)
            guard (!reserved.contains(assigned) || extensionOwners[assigned] == decision.target), owners[assigned] == nil else { throw IDevice.ProbeError("An extension ID is assigned to another app") }
        }
        if appleID, let available = ids?.availableQuantity {
            let registered = Set(ids?.appIds.map(\.identifier) ?? [])
            let requested = [decision.target] + decision.extensions.map { decision.target + $0.dropFirst(inventory.id.count) }
            guard requested.filter({ !registered.contains($0) }).count <= available else {
                throw IDevice.ProbeError("Not enough available App IDs. Remove extensions or reuse registered IDs.")
            }
        }
        // Claim before signing: failed portal operations can still allocate IDs. An app whose
        // identity is reassigned must never refresh over its replacement, for either signer.
        installed.removeAll { $0.id != record.id && ($0.assignedBundleId == decision.target || $0.reviewedBundleId == decision.target || $0.installedBundleId == decision.target) }
        if appleID {
            record.assignedBundleId = decision.target
            record.assignedTeam = team
        } else {
            record.assignedBundleId = nil
            record.assignedTeam = signingTeam
        }
        record.takeOver = !decision.discard && review.dataSource != decision.target ? review.dataSource : nil
        record.reviewedBundleId = decision.target
        record.sourceBundleId = inventory.id
        record.needsInstallReview = nil
        // Pin the reviewed signer so changing the global default cannot silently change identities.
        record.signWith = signer
        record.selectedExtensions = decision.extensions
        record.discardDestinationData = decision.discard
        assignments.reserve(InstallAssignment(source: record.id, bundleId: inventory.id, name: record.name,
            team: signingTeam, target: decision.target, extensions: decision.extensions))
        upsert(record)
        // The assignment must reach disk before any IDs are allocated or an installed copy changes.
        let stored = Stored(sources: sources, installed: installed, refreshRuns: Array(refreshRuns.prefix(20)), selfRecord: selfRecord, assignments: assignments)
        try JSONEncoder().encode(stored).write(to: Self.stateURL, options: .atomic)
        guard StateVault.sync().failed.isEmpty else { throw IDevice.ProbeError("Could not preserve the App ID assignment in the device vault") }
        return true
    }

    func reviewInstalledApp(_ id: String) async {
        guard var record = installed.first(where: { $0.id == id }) else { return }
        await exclusive("install \(record.name)") {
            guard try await self.reviewInstall(&record) else { return "cancelled" }
            let outcome = await self.signAndInstall(&record)
            self.upsert(record)
            return outcome
        }
    }
}
