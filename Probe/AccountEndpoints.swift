import DebugTrace
import Foundation

/// Spike 2 endpoints. Sign-in itself has no endpoint on purpose: the password is typed
/// on the device only. A 2FA code (one-time) can be relayed through `twoFactor`.
@MainActor
enum AccountEndpoints {
    struct State: Encodable {
        let busy: Bool
        let signedIn: Bool
        let team: AccountModel.Team?
        let teams: [AccountModel.Team]
        let anisetteURL: String
        let lastError: String?
        let twoFactorPending: Bool
        let twoFactorSMS: Bool?
        let downloads: [String]
        let downloadProgress: String?
        let pairedUDID: String?
        let pairedName: String?
        let signs: [AccountModel.SignResult]
    }

    static func state(_ model: AccountModel) -> State {
        State(busy: model.busy, signedIn: model.team != nil, team: model.team, teams: model.teams,
              anisetteURL: model.anisetteURL, lastError: model.lastError,
              twoFactorPending: model.twoFactor != nil, twoFactorSMS: model.twoFactor?.sms,
              downloads: model.downloads, downloadProgress: model.downloadProgress,
              pairedUDID: UserDefaults.standard.string(forKey: "pairedUDID"),
              pairedName: UserDefaults.standard.string(forKey: "pairedName"),
              signs: model.signs)
    }

    static func register(_ model: AccountModel) {
        DebugSurface.shared.register([
            .query("accountState", "Sign-in state, team, anisette server, downloaded IPAs and signing results") { _ in
                state(model)
            },
            .command("setAnisette", "Set the anisette-v3 server URL used for the next sign-in",
                     parameters: [.string("url", "An anisette-v3 server, e.g. http://<host>:6969", required: true)]) { args in
                model.anisetteURL = try args.requireString("url")
                return state(model)
            },
            .command("twoFactor", "Answer a pending 2FA request: code:<digits>, devices, sms:<id>, resend or abort",
                     parameters: [.string("answer", "The response", required: true)]) { args in
                model.answerTwoFactor(try args.requireString("answer"))
                try await Task.sleep(for: .seconds(1))
                return state(model)
            },
            .command("download", "Download an IPA into Documents/apps. Returns at once; poll accountState",
                     parameters: [.string("url", "Direct URL of the .ipa", required: true)]) { args in
                let url = try args.requireString("url")
                Task { await model.download(url) }
                try await Task.sleep(for: .seconds(1))
                return state(model)
            },
            .command("sign", "Register the device and sign a downloaded IPA. Returns at once; poll accountState",
                     parameters: [
                        .string("file", "File name in Documents/apps", required: true),
                        .string("udid", "Device UDID (defaults to the paired device's)"),
                        .string("deviceName", "Name to register the device under (defaults to the paired device's)"),
                        .boolean("stripExtensions", "Remove Extensions/ (ExtensionKit) before signing", default: false),
                     ]) { args in
                let file = try args.requireString("file")
                guard let udid = args.string("udid") ?? UserDefaults.standard.string(forKey: "pairedUDID") else {
                    throw DebugError.failedPrecondition("No UDID known", hint: "Pair first, or pass udid")
                }
                let name = args.string("deviceName") ?? UserDefaults.standard.string(forKey: "pairedName") ?? ThisDevice.name
                let strip = args.bool("stripExtensions") ?? false
                Task { await model.sign(file: file, udid: udid, deviceName: name, stripExtensions: strip) }
                try await Task.sleep(for: .seconds(1))
                return state(model)
            },
        ])
    }
}
