import AppIntents
import DebugTrace
import Foundation
import Observation

private let log = DebugLogger(subsystem: Bundle.main.bundleIdentifier ?? "Apparition", category: "Shortcuts")

/// What the Shortcuts actions do, apart from App Intents, so the debug server can run the
/// same logic (IntentEndpoints) without Shortcuts.
///
/// The live models reach the intents through App Intents' dependency manager:
/// `register` runs in `ApparitionApp.init`, which the system runs before it performs any
/// intent in the app's process, cold launch included, and the intents and the entity query
/// read the same instances the UI shows through `@Dependency`.
@MainActor
enum ShortcutActions {
    /// How long Launch with JIT waits for the debugger's first JIT request, as the endpoint does.
    static let jitWaitSeconds = 600

    struct Failure: Error, CustomLocalizedStringResourceConvertible {
        let message: String
        var localizedStringResource: LocalizedStringResource { "\(message)" }
    }

    struct RefreshOutcome: Encodable, Sendable {
        /// Apps re-signed and reinstalled by this run.
        var refreshed: Int
        var failed: Int
        /// Whether an install or refresh was already running and this one waited for it.
        var waited: Bool
        /// What the action shows: "Refreshed UTM; expires Oct 18", "Nothing due", the error.
        var summary: String
        var results: [String: String]
    }

    static func register(probe: ProbeModel, account: AccountModel, library: LibraryModel) {
        AppDependencyManager.shared.add(dependency: probe)
        AppDependencyManager.shared.add(dependency: account)
        AppDependencyManager.shared.add(dependency: library)
        ApparitionShortcuts.updateAppShortcutParameters()
        watchInstalledApps(library)
    }

    /// Siri's "Launch <app> with JIT" phrases are spelled from the installed apps; tell the
    /// system again whenever that list changes.
    private static func watchInstalledApps(_ library: LibraryModel) {
        withObservationTracking {
            _ = library.installed.map { "\($0.id)|\($0.name)|\($0.installedBundleId ?? "")" }
        } onChange: {
            Task { @MainActor in
                ApparitionShortcuts.updateAppShortcutParameters()
                watchInstalledApps(library)
            }
        }
    }

    // MARK: Refresh

    /// Refreshes the apps that are due (all with `force`) and waits for the run to finish.
    /// A refresh or install already running is waited for first, never doubled. Cancelling
    /// stops the wait only: an install in flight finishes, so no app is left half-installed.
    static func refresh(force: Bool, trigger: String, library: LibraryModel, probe: ProbeModel,
                        account: AccountModel, progress: Progress? = nil) async throws -> RefreshOutcome {
        let waited = library.busy
        if waited { log.info("refresh waits for a run already in progress") }
        guard try await waitUntil(.seconds(15 * 60), { !library.busy }) else {
            throw Failure(message: "Apparition is still busy installing or refreshing; try again later.")
        }
        // A JIT session or a sign-in holds the tunnel or the account; give it a moment.
        guard try await waitUntil(.seconds(120), { !probe.busy && !account.busy }) else {
            throw Failure(message: "Apparition is busy with another tunnel session (a JIT launch?) or a sign-in.")
        }

        let due = library.installed.filter { force || LibraryModel.isDue($0) }
        progress?.totalUnitCount = Int64(max(due.count, 1))
        progress?.completedUnitCount = 0

        // Unstructured on purpose: the run belongs to the app, not to this caller.
        let box = RunBox()
        Task { box.run = await library.refresh(force: force, trigger: trigger) }
        while box.run == nil {
            if let status = library.status, status.hasPrefix("refreshing "),
               let index = due.firstIndex(where: { "refreshing \($0.name)" == status }) {
                progress?.completedUnitCount = Int64(index)
            }
            try await Task.sleep(for: .milliseconds(300))
        }
        progress?.completedUnitCount = progress?.totalUnitCount ?? 0

        let outcome = describe(box.run!, library: library, waited: waited)
        log.info("refresh (\(trigger, privacy: .public)): \(outcome.refreshed, privacy: .public) refreshed, \(outcome.failed, privacy: .public) failed")
        return outcome
    }

    private final class RunBox {
        var run: LibraryModel.RefreshRun?
    }

    /// Polls `condition` every 300 ms until it holds (true) or `limit` passes (false).
    /// Throws CancellationError when the caller is cancelled.
    private static func waitUntil(_ limit: Duration, _ condition: () -> Bool) async throws -> Bool {
        let deadline = ContinuousClock.now + limit
        while !condition() {
            guard ContinuousClock.now < deadline else { return false }
            try await Task.sleep(for: .milliseconds(300))
        }
        return true
    }

    private static func describe(_ run: LibraryModel.RefreshRun, library: LibraryModel, waited: Bool) -> RefreshOutcome {
        var done: [String] = []
        var errors: [String] = []
        for (id, result) in run.results.sorted(by: { $0.key < $1.key }) where id != "-" {
            let app = library.installed.first { $0.id == id }
            let name = id == LibraryModel.selfResultKey ? "Apparition" : app?.name ?? id
            if id == LibraryModel.selfResultKey, result.hasPrefix("ok") {
                done.append(result.contains("waiting") ? "Apparition (applies when it closes)" : "Apparition")
            } else if result.hasPrefix("ok") {
                done.append("\(name); expires \(shortDate(app?.expiresAt) ?? "?")")
            } else {
                let message = result.hasPrefix("error: ") ? String(result.dropFirst(7)) : result
                errors.append("\(name): \(message)")
            }
        }
        var parts: [String] = []
        if !done.isEmpty { parts.append("Refreshed " + done.joined(separator: ", ")) }
        parts += errors
        if parts.isEmpty {
            switch run.results["-"] {
            case "busy": parts.append("Apparition was busy; nothing refreshed")
            default:
                if library.installed.isEmpty {
                    parts.append("No apps installed through Apparition")
                } else {
                    let next = library.installed.compactMap { app in parseDate(app.expiresAt).map { (app, $0) } }
                        .min { $0.1 < $1.1 }
                    parts.append(next.map { "Nothing due; \($0.0.name) expires \(shortDate($0.0.expiresAt) ?? "?")" }
                                 ?? "Nothing due")
                }
            }
        }
        if waited { parts.insert("Waited for a refresh already running", at: 0) }
        return RefreshOutcome(refreshed: done.count, failed: errors.count, waited: waited,
                              summary: parts.joined(separator: ". "), results: run.results)
    }

    nonisolated static func parseDate(_ text: String?) -> Date? {
        guard let text else { return nil }
        return (try? Date(text, strategy: Date.ISO8601FormatStyle(includingFractionalSeconds: true)))
            ?? (try? Date(text, strategy: Date.ISO8601FormatStyle()))
    }

    nonisolated static func shortDate(_ text: String?) -> String? {
        parseDate(text).map { $0.formatted(.dateTime.month(.abbreviated).day()) }
    }

    // MARK: JIT

    /// Starts `ProbeModel.launchJIT` for an installed app through the configured relay or
    /// LocalDevVPN, and returns once it is clearly under way: the first few seconds catch a
    /// tunnel that can't open. The JIT session itself carries on in the app.
    static func launchJIT(appId: String, library: LibraryModel, probe: ProbeModel) async throws -> String {
        guard let app = library.installed.first(where: { $0.id == appId }) else {
            throw Failure(message: "That app isn't installed through Apparition any more.")
        }
        guard let bundleId = app.installedBundleId else {
            throw Failure(message: "\(app.name) hasn't been installed on this device yet.")
        }
        if let expires = parseDate(app.expiresAt), expires < Date() {
            throw Failure(message: "\(app.name)'s signature expired on \(shortDate(app.expiresAt) ?? "?"); refresh it first.")
        }
        guard try await waitUntil(.seconds(15), { !probe.busy && !library.busy }) else {
            throw Failure(message: "Apparition is busy (a refresh, an install or another JIT session); try again when it's done.")
        }
        let host = probe.defaultTunnelHost
        let route = host == ProbeModel.relayHost ? "the companion" : "LocalDevVPN"
        let startedAt = DebugTime.iso(Date())
        Task { await probe.launchJIT(host: host, port: probe.defaultTunnelPort, bundleId: bundleId, waitSeconds: jitWaitSeconds) }
        log.info("JIT launch started through \(route, privacy: .public)")

        let deadline = ContinuousClock.now + .seconds(8)
        while ContinuousClock.now < deadline {
            try await Task.sleep(for: .milliseconds(300))
            guard let attempt = probe.attempts.first(where: { $0.action == "jit" && $0.startedAt >= startedAt }),
                  let outcome = attempt.outcome else { continue }
            if outcome.hasPrefix("error") {
                let message = outcome.hasPrefix("error: ") ? String(outcome.dropFirst(7)) : outcome
                throw Failure(message: "Couldn't launch \(app.name) with JIT through \(route): \(message)")
            }
            return "Launched \(app.name) with JIT"
        }
        return "Launching \(app.name) with JIT through \(route)"
    }
}
