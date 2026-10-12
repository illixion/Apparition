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
        var lastError: String?
        /// What signs it: `SigningStore.appleID`, an imported certificate's id, or nil for the default.
        var signWith: String?
        /// What signed the installed copy, for the card: "Apple ID" or the certificate's name.
        var signedWith: String?
    }

    /// Apparition's own refreshes: it re-signs and reinstalls itself (`refreshSelf`).
    struct SelfRecord: Codable, Sendable {
        var signedAt: String?
        var signedWith: String?
        var lastError: String?
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
    private(set) var busy = false
    private(set) var status: String?
    private(set) var refreshRuns: [RefreshRun] = []
    private(set) var selfRecord = SelfRecord()
    private(set) var updateCheck = UpdateCheck()
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
    }

    init(account: AccountModel, probe: ProbeModel, signing: SigningStore) {
        self.account = account
        self.probe = probe
        self.signing = signing
        if let data = try? Data(contentsOf: Self.stateURL),
           let stored = try? JSONDecoder().decode(Stored.self, from: data) {
            sources = stored.sources
            installed = stored.installed
            refreshRuns = stored.refreshRuns ?? []
            selfRecord = stored.selfRecord ?? SelfRecord()
        }
        finishPendingSelfRefresh()
    }

    private func save() {
        let stored = Stored(sources: sources, installed: installed, refreshRuns: Array(refreshRuns.prefix(20)),
                            selfRecord: selfRecord)
        guard let data = try? JSONEncoder().encode(stored) else { return }
        try? data.write(to: Self.stateURL, options: .atomic)
    }

    // MARK: Sources

    func addSource(_ id: String, assetPattern: String?, prereleases: Bool) async {
        let id = id.trimmingCharacters(in: .whitespacesAndNewlines)
        if !sources.contains(where: { $0.id == id }) {
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
    func install(sourceId: String, appId: String, stripExtensions: Bool) async {
        guard let app = sources.first(where: { $0.id == sourceId })?.apps.first(where: { $0.id == appId }) else {
            status = "no app \(appId) in \(sourceId)"
            return
        }
        await exclusive("install \(app.name)") {
            let file = try await self.download(app)
            var record = self.installed.first { $0.id == app.id }
                ?? InstalledApp(id: app.id, source: sourceId, name: app.name, version: app.version,
                                ipaFile: file, stripExtensions: stripExtensions)
            record.version = app.version
            record.ipaFile = file
            record.stripExtensions = stripExtensions
            let outcome = await self.signAndInstall(&record)
            self.upsert(record)
            return outcome
        }
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
            for var record in due {
                status = "refreshing \(record.name)"
                run.results[record.id] = await signAndInstall(&record)
                upsert(record)
            }
            if selfDue {
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

    /// Forgets an installed app: Apparition stops refreshing it and deletes its kept IPA. The
    /// app itself stays on the device until its profile expires (or the user deletes it).
    func removeInstalled(_ id: String) {
        guard !busy, let record = installed.first(where: { $0.id == id }) else { return }
        installed.removeAll { $0.id == id }
        try? FileManager.default.removeItem(at: AccountModel.appsDir.appendingPathComponent(record.ipaFile))
        account.refreshDownloads()
        save()
        log.info("removed an app from the library")
    }

    /// Chooses what signs an app from its next refresh on (nil: the default).
    func setSigner(_ signer: String?, for id: String) {
        guard let index = installed.firstIndex(where: { $0.id == id }) else { return }
        installed[index].signWith = signer
        save()
    }

    /// The source's listing for an installed app (icon, newest version), if its source is still added.
    func sourceApp(for app: InstalledApp) -> Sources.App? {
        sources.first { $0.id == app.source }?.apps.first { $0.id == app.id }
    }

    private static let noUDID = "can't read this device's UDID (paired, and LocalDevVPN or the companion up?)"

    private func deviceUDID() async -> String? {
        if UserDefaults.standard.string(forKey: "pairedUDID") == nil {
            await probe.identify(host: probe.defaultTunnelHost, port: probe.defaultTunnelPort)
        }
        return UserDefaults.standard.string(forKey: "pairedUDID")
    }

    private func signAndInstall(_ record: inout InstalledApp) async -> String {
        guard let udid = await deviceUDID() else {
            return fail(&record, Self.noUDID)
        }
        let name = UserDefaults.standard.string(forKey: "pairedName") ?? ThisDevice.name
        let path = AccountModel.appsDir.appendingPathComponent(record.ipaFile).path
        let signer = signing.signer(for: record)
        let signed: [String: Any]
        if signer == SigningStore.appleID {
            guard await account.signInSilently() else {
                return fail(&record, "not signed in (sign in, or turn on Remember password for unattended refreshes)")
            }
            signed = await AccountModel.signFile(path, udid: udid, deviceName: name, stripExtensions: record.stripExtensions)
        } else {
            // An imported certificate signs on its own: no sign-in, no anisette, no slots.
            signed = await SigningStore.signFile(path, identityId: signer, udid: udid, stripExtensions: record.stripExtensions)
        }
        guard signed["ok"] as? Bool == true, let signedPath = signed["signedPath"] as? String else {
            return fail(&record, "sign failed: \(signed["error"] as? String ?? "unknown")")
        }
        let main = AccountModel.inspect(URL(fileURLWithPath: signedPath), udid: udid).first
        let outcome = await probe.install(host: probe.defaultTunnelHost, port: probe.defaultTunnelPort, appPath: signedPath,
                                          sourceIPA: path)
        try? FileManager.default.removeItem(atPath: signedPath)
        guard outcome == "ok" else {
            return fail(&record, "install failed (LocalDevVPN connected, or the companion running?): \(outcome)")
        }
        record.installedBundleId = main?.bundleId
        record.signedAt = DebugTime.iso(Date())
        record.expiresAt = main?.profileExpires
        record.signedWith = signing.signerName(signer)
        record.lastError = nil
        return "ok, expires \(record.expiresAt ?? "?")"
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
        guard let udid = await deviceUDID() else { return selfFail(Self.noUDID) }
        let name = UserDefaults.standard.string(forKey: "pairedName") ?? ThisDevice.name
        let signer = selfSigner
        var signingId: String?
        if signer == SigningStore.appleID {
            guard await account.signInSilently() else {
                return selfFail("not signed in (sign in, or turn on Remember password for unattended refreshes)")
            }
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
            return selfFail("sign failed: \(signed["error"] as? String ?? "unknown")")
        }
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
        guard outcome == "ok" else {
            return selfFail("install failed (LocalDevVPN connected, or the companion running?): \(outcome)")
        }
        selfRecord.signedAt = DebugTime.iso(Date())
        selfRecord.signedWith = signedWith
        selfRecord.lastError = nil
        save()
        return "ok, expires \(main?.profileExpires ?? "?") (applies when Apparition next opens)"
    }

    private func selfFail(_ message: String) -> String {
        selfRecord.lastError = message
        save()
        log.error("Apparition: \(message, privacy: .public)")
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

    private func fail(_ record: inout InstalledApp, _ message: String) -> String {
        record.lastError = message
        log.error("\(record.name, privacy: .public): \(message, privacy: .public)")
        return "error: \(message)"
    }

    private func download(_ app: Sources.App) async throws -> String {
        let safe = app.id.replacingOccurrences(of: "/", with: "_").replacingOccurrences(of: ":", with: "_")
        let file = "\(safe)-\(app.version).ipa"
        let destination = AccountModel.appsDir.appendingPathComponent(file)
        if FileManager.default.fileExists(atPath: destination.path) { return file }
        guard let url = URL(string: app.downloadURL) else { throw IDevice.ProbeError("bad download URL") }
        status = "downloading \(app.name) \(app.version)"
        let (temporary, response) = try await URLSession.shared.download(from: url)
        guard (response as? HTTPURLResponse)?.statusCode == 200 else {
            throw IDevice.ProbeError("HTTP \((response as? HTTPURLResponse)?.statusCode ?? 0) downloading \(app.name)")
        }
        try FileManager.default.moveItem(at: temporary, to: destination)
        // Keep one version per app.
        for other in (try? FileManager.default.contentsOfDirectory(atPath: AccountModel.appsDir.path)) ?? []
        where other.hasPrefix("\(safe)-") && other != file {
            try? FileManager.default.removeItem(at: AccountModel.appsDir.appendingPathComponent(other))
        }
        account.refreshDownloads()
        return file
    }

    private func upsert(_ record: InstalledApp) {
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
            status = "\(label): error: \(error)"
        }
        log.info("\(self.status ?? "", privacy: .public)")
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
