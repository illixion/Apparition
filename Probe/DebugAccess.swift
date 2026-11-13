import DebugTrace
import DebugTraceServer
import Foundation
import Observation
import RAVECompanion
import RAVEDebugAccess

private let log = DebugLogger(subsystem: Bundle.main.bundleIdentifier ?? "Apparition", category: "DebugAccess")

/// Settings > Developer > Debugging from a Computer: letting a computer the person paired
/// reach this copy's debug server (docs/design/PUBLIC_DEBUGGING_PLAN.md, section 2).
///
/// Off until turned on, and asked each time for how long: an hour, which ends by the clock
/// and is not picked up again by a relaunch past the deadline, or until turned off, which
/// starts again at every launch. A paired computer comes in over TLS with a key of its own
/// (RAVEDebugAccess); the plain-HTTP listener with a password is an advanced extra, for
/// Tailscale.
///
/// A copy the launch already started a server for (built by an Apparition Store, or launched
/// with `DEBUGTRACE_SERVER=1`) keeps that server and its build token untouched. There this
/// only adds the paired listener beside it, on a server of its own.
@MainActor
@Observable
final class DebugAccessModel {
    enum RunFor: String {
        case hour
        case forever
    }

    enum Status: Equatable {
        case off
        case starting
        /// `until` nil: until turned off.
        case on(until: Date?)
        case failed(String)
    }

    private enum Key {
        static let runFor = "debugAccess.runFor"
        static let until = "debugAccess.until"
        static let password = "debugAccess.password"
        static let personalDetail = "debugAccess.personalDetail"
    }

    static let hour: TimeInterval = 3600

    private(set) var status: Status = .off
    /// Where paired computers connect, while on.
    private(set) var pairedPort: UInt16?
    /// The plain-HTTP listener's port, while the password listener is on.
    private(set) var passwordPort: UInt16?
    /// The password, read from the Keychain once the password listener is wanted.
    private(set) var password: String?
    let access: RAVEDebugAccess

    /// The launch already runs a debug server with the build's own token. The password
    /// listener and the privacy switch are that server's business, so they are not offered.
    let launchServer = DebugTraceServer.requestedAtLaunch

    /// The plain-HTTP listener, with the password as its bearer token.
    var passwordAccess: Bool {
        didSet {
            guard passwordAccess != oldValue else { return }
            defaults.set(passwordAccess, forKey: Key.password)
            restartIfOn()
        }
    }

    /// Off: a connected computer gets what a release build gives. On: the system log,
    /// debug-level lines, screen capture and full snapshots too.
    var sharesPersonalDetail: Bool {
        didSet {
            guard sharesPersonalDetail != oldValue else { return }
            defaults.set(sharesPersonalDetail, forKey: Key.personalDetail)
            restartIfOn()
        }
    }

    private let defaults: UserDefaults
    private let passwordStore = RAVEDebugKeychainStorage(service: "com.illixion.apparition.debug-access", account: "password")
    @ObservationIgnored private var server: DebugTraceServer?
    @ObservationIgnored private var deadline: Task<Void, Never>?

    init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
        passwordAccess = defaults.bool(forKey: Key.password)
        sharesPersonalDetail = defaults.bool(forKey: Key.personalDetail)
        // The device's generic name, never the one its owner gave it: a computer learns this
        // name as soon as it starts a pairing, before anyone has allowed it.
        access = RAVEDebugAccess(
            hostName: { "Apparition on \(ThisDevice.name)" },
            storage: RAVEDebugKeychainStorage(service: "com.illixion.apparition.debug-access"))
        access.onChange = { [weak self] in
            // A listener's keys are fixed when it is made.
            Task { @MainActor [weak self] in
                await self?.server?.reloadTransports()
                self?.readPorts()
            }
        }
    }

    var isOn: Bool {
        switch status {
        case .on, .starting: true
        case .off, .failed: false
        }
    }

    /// What the last launch left: "until turned off" starts again, an hour that has not
    /// run out continues, and one that has is over.
    func restore() {
        guard let runFor = defaults.string(forKey: Key.runFor).flatMap(RunFor.init(rawValue:)) else { return }
        switch runFor {
        case .forever:
            Task { await start(until: nil) }
        case .hour:
            let until = Date(timeIntervalSince1970: defaults.double(forKey: Key.until))
            if until > Date() {
                Task { await start(until: until) }
            } else {
                forget()
            }
        }
    }

    func turnOn(_ runFor: RunFor) {
        let until = runFor == .hour ? Date().addingTimeInterval(Self.hour) : nil
        defaults.set(runFor.rawValue, forKey: Key.runFor)
        defaults.set(until?.timeIntervalSince1970 ?? 0, forKey: Key.until)
        Task { await start(until: until) }
    }

    func turnOff() {
        forget()
        stop()
        status = .off
        log.notice("debugging from a computer is off")
    }

    /// The app came to the front: an hour may have run out while it was suspended.
    func checkDeadline() {
        if case .on(let until?) = status, until <= Date() { expire() }
    }

    private func expire() {
        guard case .on(_?) = status else { return }
        log.notice("debugging from a computer ended after its hour")
        turnOff()
    }

    private func forget() {
        defaults.removeObject(forKey: Key.runFor)
        defaults.removeObject(forKey: Key.until)
    }

    // MARK: The server

    private func configuration() -> DebugTraceServer.Configuration {
        // Beside a launch server: the paired listener only. Its plain listener and token are the launch's.
        let plain = passwordAccess && !launchServer
        return DebugTraceServer.Configuration(
            binding: plain ? .network : .none,
            authentication: plain ? .token(currentPassword()) : .none,
            // The person turned it on in Settings; the privacy mode says what it shares.
            allowedInRelease: true,
            transports: [access.transport])
    }

    private func start(until: Date?) async {
        stop()
        status = .starting
        if !launchServer { DebugTrace.setPrivacy(sharesPersonalDetail ? .development : .release) }
        do {
            let configuration = configuration()
            if launchServer {
                let own = DebugTraceServer(configuration: configuration)
                try await own.start()
                server = own
            } else {
                server = try await DebugTraceServer.start(configuration: configuration)
            }
        } catch {
            stop()
            forget()
            status = .failed("Couldn't start listening: \(error.localizedDescription)")
            log.error("debug access failed to start: \(String(describing: error), privacy: .public)")
            return
        }
        readPorts()
        status = .on(until: until)
        log.notice("debugging from a computer is on (\(until == nil ? "until turned off" : "for an hour", privacy: .public))")
        if let until {
            deadline = Task { [weak self] in
                // The continuous clock keeps counting while the app is suspended, so the hour
                // ends by the clock; `checkDeadline` catches it the moment the app is back.
                try? await Task.sleep(for: .seconds(max(until.timeIntervalSinceNow, 0)), clock: .continuous)
                guard !Task.isCancelled else { return }
                self?.expire()
            }
        }
    }

    private func stop() {
        deadline?.cancel()
        deadline = nil
        if launchServer {
            server?.stop()
        } else if server != nil {
            DebugTraceServer.stopShared()
            DebugTrace.setPrivacy(.detected)
        }
        server = nil
        pairedPort = nil
        passwordPort = nil
    }

    private func restartIfOn() {
        guard case .on(let until) = status else { return }
        Task { await start(until: until) }
    }

    private func readPorts() {
        pairedPort = server?.transportPorts["paired"]
        passwordPort = server?.port
    }

    // MARK: The password

    /// The password, made on first use. Random and never chosen: it is the only thing
    /// between the plain listener and whoever can reach this device.
    private func currentPassword() -> String {
        if let password { return password }
        if let stored = passwordStore.load().map({ String(decoding: $0, as: UTF8.self) }), !stored.isEmpty {
            password = stored
            return stored
        }
        return makePassword()
    }

    @discardableResult
    private func makePassword() -> String {
        // 256 bits from the system's generator, URL-safe.
        let fresh = (try? RAVECompanionCredential.generateToken()) ?? UUID().uuidString + UUID().uuidString
        do {
            try passwordStore.save(Data(fresh.utf8))
        } catch {
            log.error("the debug password could not be kept: \(String(describing: error), privacy: .public)")
        }
        password = fresh
        return fresh
    }

    /// Shows the password in Settings without waiting for the listener to start.
    func loadPassword() {
        if passwordAccess { _ = currentPassword() }
    }

    /// Replaces the password. Whoever was allowed "always" with the old one is asked again.
    func newPassword() {
        makePassword()
        DebugTraceServer.forgetRememberedClients()
        log.notice("the debug password was replaced")
        restartIfOn()
    }

    /// This device's addresses, for a computer that has to be told where it is.
    var addresses: [String] { RAVEDebugAccess.localAddresses() }

    /// Where a computer sends plain-HTTP requests, while the password listener is on.
    var passwordURL: String? {
        guard let passwordPort, let address = addresses.first else { return nil }
        return "http://\(address):\(passwordPort)"
    }
}

// MARK: - Debug endpoint

enum DebugAccessEndpoints {
    /// What a tester needs to see without the screen: nothing here names a computer,
    /// an address or a secret.
    @MainActor
    static func register(_ model: DebugAccessModel, on surface: DebugSurface? = nil) {
        let surface = surface ?? .shared
        surface.register(.query("debugAccess", "Settings > Developer > Debugging from a Computer: whether it is on and for how long, the listeners' ports, how many computers are paired, and the state of a pairing in progress. No names, addresses or secrets.",
                                releaseSafe: true) { _ -> JSONValue in
            var state: [String: JSONValue] = [
                "launchServer": .bool(model.launchServer),
                "passwordAccess": .bool(model.passwordAccess),
                "sharesPersonalDetail": .bool(model.sharesPersonalDetail),
                "pairedComputers": .int(model.access.computers.count),
            ]
            switch model.status {
            case .off: state["status"] = "off"
            case .starting: state["status"] = "starting"
            case .failed: state["status"] = "failed"
            case .on(let until):
                state["status"] = "on"
                state["secondsLeft"] = until.map { .int(max(Int($0.timeIntervalSinceNow), 0)) } ?? .null
            }
            if let port = model.pairedPort { state["pairedPort"] = .int(Int(port)) }
            if let port = model.passwordPort { state["passwordPort"] = .int(Int(port)) }
            switch model.access.pairing {
            case .idle: state["pairing"] = "idle"
            case .waiting: state["pairing"] = "waiting"
            case .confirming: state["pairing"] = "confirming"
            case .paired: state["pairing"] = "paired"
            case .failed: state["pairing"] = "failed"
            }
            if let port = model.access.pairingPort { state["pairingPort"] = .int(Int(port)) }
            return .object(state)
        })
        #if !APPARITION_RELEASE
        surface.register(.command("debugAccessControl", "Drives Settings > Developer > Debugging from a Computer without the screen, for tests: on (runFor: hour or forever), off, beginPairing, endPairing. In the Simulator only, also answer (allow: true or false) to a pairing's code, which on a device is the person's to tap.",
                                  parameters: [
                                    .string("action", "what to do", required: true,
                                            choices: ["on", "off", "beginPairing", "endPairing", "answer"]),
                                    .string("runFor", "for action=on", default: "hour", choices: ["hour", "forever"]),
                                    .boolean("allow", "for action=answer", default: false),
                                  ]) { arguments -> JSONValue in
            switch try arguments.requireString("action") {
            case "on": model.turnOn(DebugAccessModel.RunFor(rawValue: try arguments.requireString("runFor")) ?? .hour)
            case "off": model.turnOff()
            case "beginPairing": model.access.beginPairing()
            case "endPairing": model.access.endPairing()
            case "answer":
                #if targetEnvironment(simulator)
                model.access.answer(try arguments.requireBool("allow"))
                #else
                throw DebugError(.forbidden, "a pairing is answered on the device",
                                 hint: "compare the code on the device with the computer's, and tap Allow there")
                #endif
            default: break
            }
            return ["done": true]
        })
        #endif
    }
}
