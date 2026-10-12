import DebugTrace
import Foundation

/// Lets an agent drive the probe through the debug server. The pairing file itself
/// (it holds a private key) is never returned, only whether it exists.
@MainActor
enum ProbeEndpoints {
    struct State: Encodable {
        let browserStates: [String: String]
        let interfaces: [String]
        let advertised: [ProbeModel.Advertised]
        let reachability: [ProbeModel.Reachability]
        let attempts: [ProbeModel.Attempt]
        let busy: Bool
        let pinRequested: Bool
        let hasPairingFile: Bool
        let relay: String?
    }

    static func state(_ model: ProbeModel) -> State {
        State(browserStates: model.browserStates, interfaces: model.interfaces,
              advertised: model.advertised.values.sorted { $0.id < $1.id },
              reachability: model.reachability, attempts: model.attempts,
              busy: model.busy, pinRequested: model.pinRequested, hasPairingFile: model.hasPairingFile,
              relay: model.relay)
    }

    static func register(_ model: ProbeModel) {
        let target: [DebugParameter] = [
            .string("host", "Numeric address to connect to (10.7.0.1 for LocalDevVPN), or \"relay\" to go through the companion set with setRelay", required: true),
            .integer("port", "TCP port of the RemotePairing service", required: true, range: 1...65535),
        ]
        DebugSurface.shared.register([
            .query("probeState", "Bonjour results, own interface addresses, reachability, pairing/tunnel attempts") { _ in
                state(model)
            },
            .command("browse", "Restart Bonjour browsing for the RemotePairing services; returns the state") { _ in
                model.startBrowsing()
                try await Task.sleep(for: .seconds(3))
                return state(model)
            },
            .command("scan", "TCP-connect every advertised service on every candidate host; returns the state",
                     timeout: .seconds(120)) { _ in
                await model.scan()
                return state(model)
            },
            .command("pair", "Start a pair-setup against host:port (the manual-pairing service). Returns at once; poll probeState and send the PIN with submitPin",
                     parameters: target) { args in
                let host = try args.requireString("host")
                let port = args.int("port") ?? 0
                Task { await model.pair(host: host, port: port) }
                try await Task.sleep(for: .seconds(2))
                return state(model)
            },
            .command("tunnel", "Pair-verify against host:port (the paired service, usually 49152), open the tunnel and list RSD services. Returns at once; poll probeState",
                     parameters: target) { args in
                let host = try args.requireString("host")
                let port = args.int("port") ?? 0
                Task { await model.tunnel(host: host, port: port) }
                try await Task.sleep(for: .seconds(2))
                return state(model)
            },
            .command("install", "Open the tunnel at host:port and install a signed app directory through it. Returns at once; poll probeState",
                     parameters: target + [.string("path", "Absolute path of the signed .app (accountState.signs[].signedPath)", required: true)]) { args in
                let host = try args.requireString("host")
                let port = args.int("port") ?? 0
                let path = try args.requireString("path")
                Task { await model.install(host: host, port: port, appPath: path) }
                try await Task.sleep(for: .seconds(2))
                return state(model)
            },
            .command("launchJIT", "Launch an app under the debugger through the tunnel at host:port and serve its JIT requests (UTM asks when a VM starts). Returns at once; poll probeState",
                     parameters: target + [
                        .string("bundleId", "Installed bundle ID, e.g. com.utmapp.UTM.<team>", required: true),
                        .integer("waitSeconds", "How long to wait for the first JIT request", default: 600, range: 10...3600),
                     ]) { args in
                let host = try args.requireString("host")
                let port = args.int("port") ?? 0
                let bundleId = try args.requireString("bundleId")
                let wait = args.int("waitSeconds") ?? 600
                Task { await model.launchJIT(host: host, port: port, bundleId: bundleId, waitSeconds: wait) }
                try await Task.sleep(for: .seconds(2))
                return state(model)
            },
            .command("identify", "Read the device's UDID and name through the tunnel at host:port and save them for signing. Returns at once; poll probeState",
                     parameters: target) { args in
                let host = try args.requireString("host")
                let port = args.int("port") ?? 0
                Task { await model.identify(host: host, port: port) }
                try await Task.sleep(for: .seconds(2))
                return state(model)
            },
.command("listApps", "List every user app on the headset through the default tunnel (relay or LocalDevVPN), whoever installed it, and keep the list for the Library",
                     timeout: .seconds(90)) { _ in
                let outcome = await model.listApps(host: model.defaultTunnelHost, port: model.defaultTunnelPort)
                return DeviceAppsReply(outcome: outcome, fetchedAt: model.deviceAppsFetchedAt, apps: model.deviceApps)
            },
                        .command("setRelay", "Set the Apparition Companion relay (ip:port), or clear it with an empty string",
                     parameters: [.string("relay", "e.g. 172.20.48.198:49160", required: true)]) { args in
                let relay = try args.requireString("relay").trimmingCharacters(in: .whitespaces)
                model.relay = relay.isEmpty ? nil : relay
                return state(model)
            },
            .command("submitPin", "Answer a pending PIN request with the 6-digit code shown in Settings",
                     parameters: [.string("pin", "The code shown on the device", required: true)]) { args in
                model.submitPin(try args.requireString("pin"))
                try await Task.sleep(for: .seconds(1))
                return state(model)
            },
            .command("forgetPairing", "Delete the saved pairing file", destructive: true) { _ in
                model.forgetPairing()
                return state(model)
            },
        ])
    }
}

struct DeviceAppsReply: Encodable {
    let outcome: String
    let fetchedAt: String?
    let apps: [ProbeModel.DeviceApp]
}
