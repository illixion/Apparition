import DebugTrace
import Foundation

/// The Shortcuts actions' logic, without Shortcuts: what Refresh Apps and Launch with JIT
/// would do and say.
@MainActor
enum IntentEndpoints {
    struct App: Encodable {
        let id: String
        let name: String
        let expiresAt: String?
        let display: String
    }

    static func register(probe: ProbeModel, account: AccountModel, library: LibraryModel) {
        DebugSurface.shared.register([
            .query("intentApps", "The apps the Launch with JIT action offers (installed, with a bundle id)") { _ in
                library.installed.filter { $0.installedBundleId != nil }.map { app in
                    App(id: app.id, name: app.name, expiresAt: app.expiresAt,
                        display: ShortcutActions.shortDate(app.expiresAt).map { "\(app.name), expires \($0)" } ?? app.name)
                }
            },
            .command("intentRefresh", "Run the Refresh Apps action's logic and wait for it (up to 5 min): waits for a run already going, then returns the count and the summary the action would show",
                     parameters: [.boolean("allApps", "Refresh every app, not only those expiring within 3 days", default: false),
                                  .boolean("checkForUpdates", "Also ask the release site and the installed apps' sources for updates, alongside the signing", default: false)],
                     timeout: .seconds(300)) { args in
                try await ShortcutActions.refresh(force: args.bool("allApps") ?? false,
                                                  checkForUpdates: args.bool("checkForUpdates") ?? false, trigger: "intent-endpoint",
                                                  library: library, probe: probe, account: account)
            },
            .command("intentLaunchJIT", "Run the Launch with JIT action's logic for an installed app (intentApps id). Returns after the first seconds of the launch; the JIT session continues, poll probeState",
                     parameters: [.string("appId", "The installed app's id, as intentApps lists it", required: true)],
                     timeout: .seconds(60)) { args in
                ["message": try await ShortcutActions.launchJIT(appId: try args.requireString("appId"),
                                                                library: library, probe: probe)]
            },
        ])
    }
}
