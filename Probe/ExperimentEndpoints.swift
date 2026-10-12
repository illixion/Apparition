import DebugTrace
import Foundation

/// Experiments that aren't features yet, reachable only through the debug server.
///
/// Profile reuse: build-and-sign found that a paid account's explicit profile installs apps
/// under any bundle ID. If a free profile does too, one App ID could sign many apps, past the
/// free account's ten App IDs a week (installd's three-apps limit would still apply).
@MainActor
enum ExperimentEndpoints {
    struct ReuseReply: Encodable {
        let signed: String
        let install: String
        var profileEntitlements: [String]? = nil
        var signedEntitlements: [String]? = nil
    }

    static func register(probe: ProbeModel, account: AccountModel) {
        DebugSurface.shared.register([
            .command("experimentReuseProfile",
                     "Experiment: sign a downloaded IPA with an existing App ID's free profile (no App ID registered), keeping its own bundle ID or bundleId, and install it. Waits for the install",
                     parameters: [
                        .string("file", "File name in Documents/apps", required: true),
                        .string("profileAppId", "An App ID the team has, e.g. com.utmapp.UTM.<team>", required: true),
                        .string("bundleId", "Install under this bundle ID instead of the app's own"),
                        .boolean("minimal", "Sign with only application-identifier, team, get-task-allow and one keychain group", default: false),
                        .string("identityId", "Control: sign with this imported certificate instead, and profileAppId is an imported profile's UUID"),
                     ], timeout: .seconds(600)) { args in
                let path = AccountModel.appsDir.appendingPathComponent(try args.requireString("file")).path
                let profileAppId = try args.requireString("profileAppId")
                let bundleId = args.string("bundleId")
                let minimal = args.bool("minimal") ?? false
                let reply: [String: Any]
                if let identity = args.string("identityId") {
                    // Control: an imported certificate and profile (profileAppId is then the profile's UUID).
                    let dataDir = AccountModel.dataDir.path
                    reply = await Task.detached {
                        AccountModel.call(apparition_sign_offline_keeping_id(dataDir, path, identity, profileAppId, bundleId, minimal))
                    }.value
                } else {
                    guard await account.signInSilently() else {
                        throw DebugError.failedPrecondition("Not signed in", hint: "Sign in on the headset with Remember password on")
                    }
                    reply = await Task.detached {
                        AccountModel.call(apparition_sign_with_existing_profile(path, profileAppId, bundleId, minimal))
                    }.value
                }
                guard reply["ok"] as? Bool == true, let signedPath = reply["signedPath"] as? String else {
                    return ReuseReply(signed: "error: \(reply["error"] as? String ?? "unknown")", install: "-")
                }
                defer { try? FileManager.default.removeItem(atPath: signedPath) }
                let install = await probe.install(host: probe.defaultTunnelHost, port: probe.defaultTunnelPort, appPath: signedPath)
                let detail = probe.latestAttempt("install").flatMap { $0.outcome == "ok" ? nil : $0.outcome } ?? install
                return ReuseReply(signed: "ok: \(reply["bundleId"] as? String ?? "?") with \(reply["profileAppId"] as? String ?? "?")'s profile",
                                  install: detail,
                                  profileEntitlements: reply["profileEntitlements"] as? [String],
                                  signedEntitlements: reply["signedEntitlements"] as? [String])
            },
        ])
    }
}
