import DebugTrace
import Foundation

/// Spike 5 endpoints: sources, installing from them and refreshing what was installed.
/// Install and refresh return at once; poll libraryState.
@MainActor
enum LibraryEndpoints {
    struct State: Encodable {
        let busy: Bool
        let status: String?
        let sources: [Sources.Source]
        let installed: [LibraryModel.InstalledApp]
        let refreshRuns: [LibraryModel.RefreshRun]
        let rememberPassword: Bool
        let hasStoredPassword: Bool
        let apparition: SelfState?
    }

    struct SelfState: Encodable {
        let bundleId: String
        let teamId: String?
        let profileName: String?
        let expires: String?
        let due: Bool
        let signer: String
        let record: LibraryModel.SelfRecord
    }

    struct UpdateState: Encodable {
        let current: String
        let latest: SelfUpdate.Latest?
        let available: String?
        let error: String?
        let site: String?
        let testSigners: Bool
    }

    static func state(_ model: LibraryModel, _ account: AccountModel) -> State {
        State(busy: model.busy, status: model.status, sources: model.sources, installed: model.installed,
              refreshRuns: Array(model.refreshRuns.prefix(5)), rememberPassword: account.rememberPassword,
              hasStoredPassword: Keychain.hasPassword,
              apparition: model.selfApp.map { me in
                  SelfState(bundleId: me.bundleId, teamId: me.teamId, profileName: me.profileName,
                            expires: me.expires.map(DebugTime.iso), due: model.selfIsDue,
                            signer: model.signing.signerName(model.selfSigner), record: model.selfRecord)
              })
    }

    static func register(_ model: LibraryModel, account: AccountModel) {
        DebugSurface.shared.register([
            .query("libraryState", "Sources and their apps, installed apps with expiry, recent refresh runs") { _ in
                state(model, account)
            },
            .command("addSource", "Add and fetch a source: an AltStore/SideStore source URL, or github:owner/repo",
                     parameters: [
                        .string("url", "https://… source JSON, or github:owner/repo", required: true),
                        .string("assetPattern", "github: only, text the .ipa asset name must contain, e.g. visionOS"),
                        .boolean("prereleases", "github: only, also consider prereleases", default: false),
                     ], timeout: .seconds(60)) { args in
                await model.addSource(try args.requireString("url"), assetPattern: args.string("assetPattern"),
                                      prereleases: args.bool("prereleases") ?? false)
                return state(model, account)
            },
            .command("removeSource", "Forget a source (installed apps stay)",
                     parameters: [.string("url", "The source's id as listed", required: true)]) { args in
                model.removeSource(try args.requireString("url"))
                return state(model, account)
            },
            .command("refreshSources", "Fetch every source again", timeout: .seconds(120)) { _ in
                await model.refreshSources()
                return state(model, account)
            },
            .command("installFromSource", "Download, sign and install an app from a source through the LocalDevVPN tunnel. Returns at once; poll libraryState",
                     parameters: [
                        .string("sourceId", "The source's id", required: true),
                        .string("appId", "The app's id in that source (bundle id or github:owner/repo)", required: true),
                        .boolean("stripExtensions", "Drop app extensions, which a free account can't sign", default: true),
                     ]) { args in
                let source = try args.requireString("sourceId")
                let app = try args.requireString("appId")
                let strip = args.bool("stripExtensions") ?? true
                Task { await model.install(sourceId: source, appId: app, stripExtensions: strip) }
                try await Task.sleep(for: .seconds(1))
                return state(model, account)
            },
            .command("refreshApps", "Re-sign and reinstall installed apps that expire within 3 days (all with force). Returns at once; poll libraryState",
                     parameters: [.boolean("force", "Refresh every installed app", default: false)]) { args in
                let force = args.bool("force") ?? false
                Task { await model.refresh(force: force, trigger: "manual") }
                try await Task.sleep(for: .seconds(1))
                return state(model, account)
            },
            .command("refreshSelf", "Re-sign Apparition with selfSigner and hand the update to installd, which applies it when the app closes; relaunch and read libraryState.apparition",
                     timeout: .seconds(5)) { _ in
                Task { await model.refreshSelf() }
                try await Task.sleep(for: .seconds(1))
                return state(model, account)
            },
            .command("checkUpdate", "Ask the release site for the latest Apparition, verified against the release keys",
                     timeout: .seconds(60)) { _ in
                await model.checkForUpdate(force: true)
                return UpdateState(current: SelfUpdate.currentVersion, latest: model.updateCheck.latest,
                                   available: model.availableUpdate?.version, error: model.updateCheck.error,
                                   site: SelfUpdate.site, testSigners: SelfUpdate.signers != nil)
            },
            .command("updateSelf", "Download the latest release, sign it with selfSigner and hand it to installd, which installs it when the app closes. Returns at once; poll libraryState.apparition",
                     timeout: .seconds(5)) { _ in
                Task { await model.updateSelf() }
                try await Task.sleep(for: .seconds(1))
                return state(model, account)
            },
            .command("setReleaseSite", "Development builds only (not official releases): check another release site, trusting other keys (an allowed-signers file's text). Both empty: back to apparition.illixion.com and the release keys",
                     parameters: [
                        .string("site", "https://… base URL serving release.json"),
                        .string("signers", "allowed-signers lines: releases@apparition.illixion.com ssh-ed25519 AAAA… comment"),
                     ]) { args in
                for (key, value) in [(SelfUpdate.siteKey, args.string("site")), (SelfUpdate.signersKey, args.string("signers"))] {
                    if let value, !value.isEmpty {
                        UserDefaults.standard.set(value, forKey: key)
                    } else {
                        UserDefaults.standard.removeObject(forKey: key)
                    }
                }
                return UpdateState(current: SelfUpdate.currentVersion, latest: nil, available: nil, error: nil,
                                   site: SelfUpdate.site, testSigners: SelfUpdate.signers != nil)
            },
            .query("backgroundTasks", "Background refresh requests the system still holds") { _ in
                await model.pendingBackgroundTasks()
            },
            .command("scheduleBackgroundRefresh", "Test hook: ask for both background runs after `minutes`; with force, the next one refreshes everything",
                     parameters: [
                        .integer("minutes", "Earliest start, from now", default: 15),
                        .boolean("force", "Refresh every app on the next background run, due or not", default: true),
                     ]) { args in
                UserDefaults.standard.set(args.bool("force") ?? true, forKey: LibraryModel.forceNextBackgroundKey)
                model.scheduleRefresh(after: TimeInterval((args.int("minutes") ?? 15) * 60))
                return await model.pendingBackgroundTasks()
            },
        ])
    }
}
