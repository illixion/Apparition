import AppIntents
import Foundation

/*
 Shortcuts and Siri actions.

 A free Apple ID's apps expire after 7 days, and background tasks are not a reliable way to
 refresh them, so the supported route is a Shortcuts personal automation (Time of Day, daily)
 that runs Refresh Apps. A refresh takes 40-150 s (sign-in, signing, uploading the app
 through the tunnel), longer than an intent gets in the background, so every action here
 opens Apparition and runs in its process, where the live models are (ShortcutActions).

 Foreground: `openAppWhenRun` for the iOS 17 floor, and `supportedModes = .foreground` where
 the 26 SDKs replace it (both are in the iOS and visionOS 27 SDKs).
 */

// MARK: Refresh Apps

struct RefreshAppsIntent: AppIntent, ProgressReportingIntent {
    static let title: LocalizedStringResource = "Refresh Apps"
    static let description = IntentDescription(
        "Re-signs and reinstalls the apps Apparition installed that expire within 3 days, or all of them. Can also look for updates while it signs. Opens Apparition and waits for the refresh to finish, which can take a few minutes. Returns how many apps were refreshed.",
        categoryName: "Apps"
    )
    static let openAppWhenRun = true
    @available(iOS 26.0, macOS 26.0, visionOS 26.0, *)
    static var supportedModes: IntentModes { .foreground }

    @Parameter(title: "All Apps", description: "Refresh every app, not only those expiring within 3 days", default: false)
    var allApps: Bool

    @Parameter(title: "Check for Updates", description: "Also ask the release site and the sources your apps came from whether anything is newer, while signing. Off, Apparition contacts none of them.", default: false)
    var checkForUpdates: Bool

    static var parameterSummary: some ParameterSummary {
        When(\.$allApps, .equalTo, true) {
            Summary("Refresh all apps") { \.$allApps; \.$checkForUpdates }
        } otherwise: {
            Summary("Refresh apps that are due") { \.$allApps; \.$checkForUpdates }
        }
    }

    @Dependency private var library: LibraryModel
    @Dependency private var probe: ProbeModel
    @Dependency private var account: AccountModel

    @MainActor
    func perform() async throws -> some IntentResult & ReturnsValue<Int> & ProvidesDialog {
        let outcome = try await ShortcutActions.refresh(force: allApps, checkForUpdates: checkForUpdates, trigger: "shortcut", library: library,
                                                        probe: probe, account: account, progress: progress)
        // Nothing went through and something failed: fail the action, so an automation
        // reports it rather than looking like it worked.
        if outcome.refreshed == 0 && outcome.failed > 0 {
            throw ShortcutActions.Failure(message: outcome.summary)
        }
        return .result(value: outcome.refreshed, dialog: "\(outcome.summary)")
    }
}

// MARK: Launch with JIT

struct InstalledAppEntity: AppEntity {
    static let typeDisplayRepresentation: TypeDisplayRepresentation = "Installed App"
    static let defaultQuery = InstalledAppQuery()

    /// The library's id: the source app's bundle identifier, or `github:owner/repo`.
    let id: String
    let name: String
    let expiresAt: String?

    var displayRepresentation: DisplayRepresentation {
        if let date = ShortcutActions.shortDate(expiresAt) {
            return DisplayRepresentation(title: "\(name)", subtitle: "Expires \(date)")
        }
        return DisplayRepresentation(title: "\(name)")
    }

    init(_ app: LibraryModel.InstalledApp) {
        id = app.id
        name = app.name
        expiresAt = app.expiresAt
    }
}

/// The installed apps that can be launched: those with an installed bundle id.
struct InstalledAppQuery: EntityStringQuery {
    @Dependency private var library: LibraryModel

    func entities(for identifiers: [String]) async throws -> [InstalledAppEntity] {
        let all = await launchable()
        return identifiers.compactMap { id in all.first { $0.id == id } }
    }

    func entities(matching string: String) async throws -> [InstalledAppEntity] {
        await launchable().filter { $0.name.localizedCaseInsensitiveContains(string) }
    }

    func suggestedEntities() async throws -> [InstalledAppEntity] {
        await launchable()
    }

    private func launchable() async -> [InstalledAppEntity] {
        let library = library
        return await MainActor.run {
            library.installed.filter { $0.installedBundleId != nil }.map(InstalledAppEntity.init)
        }
    }
}

struct LaunchWithJITIntent: AppIntent {
    static let title: LocalizedStringResource = "Launch with JIT"
    static let description = IntentDescription(
        "Opens Apparition and launches an app it installed under the debugger, so the app can use JIT (UTM, emulators). Uses the companion when one is set, otherwise LocalDevVPN.",
        categoryName: "Apps"
    )
    static let openAppWhenRun = true
    @available(iOS 26.0, macOS 26.0, visionOS 26.0, *)
    static var supportedModes: IntentModes { .foreground }

    @Parameter(title: "App", requestValueDialog: "Which app?")
    var app: InstalledAppEntity

    static var parameterSummary: some ParameterSummary {
        Summary("Launch \(\.$app) with JIT")
    }

    @Dependency private var library: LibraryModel
    @Dependency private var probe: ProbeModel

    @MainActor
    func perform() async throws -> some IntentResult & ProvidesDialog {
        let message = try await ShortcutActions.launchJIT(appId: app.id, library: library, probe: probe)
        return .result(dialog: "\(message)")
    }
}

// MARK: Open Apparition

struct OpenApparitionIntent: AppIntent {
    static let title: LocalizedStringResource = "Open Apparition"
    static let description = IntentDescription("Opens Apparition.")
    static let openAppWhenRun = true
    @available(iOS 26.0, macOS 26.0, visionOS 26.0, *)
    static var supportedModes: IntentModes { .foreground }

    func perform() async throws -> some IntentResult {
        .result()
    }
}

// MARK: App Shortcuts

struct ApparitionShortcuts: AppShortcutsProvider {
    static var appShortcuts: [AppShortcut] {
        AppShortcut(
            intent: RefreshAppsIntent(),
            phrases: [
                "Refresh apps in \(.applicationName)",
                "Refresh my \(.applicationName) apps",
                "Re-sign apps with \(.applicationName)",
            ],
            shortTitle: "Refresh Apps",
            systemImageName: "arrow.clockwise.circle"
        )
        AppShortcut(
            intent: LaunchWithJITIntent(),
            phrases: [
                "Launch \(\.$app) with JIT in \(.applicationName)",
                "Start \(\.$app) with \(.applicationName) JIT",
                "Launch an app with JIT in \(.applicationName)",
            ],
            shortTitle: "Launch with JIT",
            systemImageName: "bolt.circle"
        )
        AppShortcut(
            intent: OpenApparitionIntent(),
            phrases: ["Open \(.applicationName)", "Show \(.applicationName)"],
            shortTitle: "Open Apparition",
            systemImageName: "app.badge"
        )
    }
}
