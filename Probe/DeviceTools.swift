import CoreLocation
import DebugTrace
import Foundation
import Observation

private let log = DebugLogger(subsystem: Bundle.main.bundleIdentifier ?? "Apparition", category: "Tools")

/// One process running on the device, as apparition_processes lists it.
struct DeviceProcess: Codable, Identifiable, Hashable, Sendable {
    enum Kind: String, Codable, Sendable {
        case app, systemApp, `extension`, system
    }

    let pid: Int
    let name: String
    let path: String
    let kind: Kind
    let bundleId: String?
    let appName: String?

    var id: Int { pid }

    /// What a person calls it: the app's name, an extension's own (its app is the subtitle).
    var title: String { kind == .extension ? name : (appName ?? name) }
}

/// One line of the device's log, as apparition_stream_log sends it.
struct DeviceLogLine: Identifiable, Sendable {
    enum Level: String, Codable, Sendable, CaseIterable {
        case debug, info, notice, error, fault
    }

    let id: Int
    let time: Date
    let pid: Int
    let process: String
    let sender: String
    let level: Level
    let subsystem: String?
    let category: String?
    let message: String

    struct Wire: Decodable, Sendable {
        let time: Double
        let pid: Int
        let process: String
        let sender: String
        let level: Level
        let subsystem: String?
        let category: String?
        let message: String
    }

    /// One line as text, the way `log stream` prints it.
    var text: String {
        let label = [subsystem, category].compactMap { $0 }.joined(separator: ":")
        return "\(Self.timeFormat.string(from: time)) \(process)[\(pid)] <\(level.rawValue)>\(label.isEmpty ? "" : " [\(label)]") \(message)"
    }

    static let timeFormat: DateFormatter = {
        let format = DateFormatter()
        format.dateFormat = "HH:mm:ss.SSS"
        return format
    }()
}

/// The least important lines the console shows, as `log stream --level` chooses.
enum ConsoleLevel: UInt32, CaseIterable, Identifiable, Sendable {
    case debug = 0, info = 1, standard = 2, errors = 3
    var id: UInt32 { rawValue }
    var title: String {
        switch self {
        case .debug: "Everything"
        case .info: "Info and Above"
        case .standard: "Default"
        case .errors: "Errors Only"
        }
    }
}

/// A place to simulate, with what to call it in the list of recent places.
struct SimulatedPlace: Codable, Equatable, Hashable, Sendable {
    var name: String
    var latitude: Double
    var longitude: Double

    var coordinate: CLLocationCoordinate2D { CLLocationCoordinate2D(latitude: latitude, longitude: longitude) }

    var coordinates: String { String(format: "%.5f, %.5f", latitude, longitude) }

    /// "59.437, 24.7536" (or with a space or semicolon between), within Earth's ranges.
    static func parse(_ text: String) -> SimulatedPlace? {
        let numbers = text.split(whereSeparator: { ",; ".contains($0) }).compactMap { Double($0) }
        guard numbers.count == 2, (-90...90).contains(numbers[0]), (-180...180).contains(numbers[1]) else { return nil }
        return SimulatedPlace(name: String(format: "%.5f, %.5f", numbers[0], numbers[1]), latitude: numbers[0], longitude: numbers[1])
    }
}

// MARK: The calls, over the same tunnel as JIT

private final class ToolResult<T>: @unchecked Sendable {
    var value: T?
}

extension ProbeModel {
    /// The device's running processes, apps first. Returns "ok" and the list, or what failed.
    func listProcesses() async -> (outcome: String, processes: [DeviceProcess]?) {
        await prepareRoute(host: defaultTunnelHost)
        let target = tunnelTarget(host: defaultTunnelHost, port: defaultTunnelPort)
        let result = ToolResult<[DeviceProcess]>()
        let outcome = await run(action: "processes", host: defaultTunnelHost, port: defaultTunnelPort, exclusive: false) { file, context in
            let reply = try IDevice.processes(target: target, hostname: Self.hostname, file: file, pin: nil, context: context)
            guard reply["ok"] as? Bool == true, let list = reply["processes"] else {
                throw IDevice.ProbeError(reply["error"] as? String ?? "couldn't list the processes")
            }
            let processes = try JSONDecoder().decode([DeviceProcess].self, from: JSONSerialization.data(withJSONObject: list))
            result.value = processes
            return { attempt in attempt.detail = "\(processes.count) processes" }
        }
        return (outcome, result.value)
    }

    /// Sends `signal` (9 ends a process, 15 asks it to end) to `pid`. Returns "ok" or what failed.
    func signalProcess(pid: Int, signal: UInt32) async -> String {
        await prepareRoute(host: defaultTunnelHost)
        let target = tunnelTarget(host: defaultTunnelHost, port: defaultTunnelPort)
        return await run(action: "signal", host: defaultTunnelHost, port: defaultTunnelPort, exclusive: false) { file, context in
            let reply = try IDevice.signalProcess(target: target, hostname: Self.hostname, file: file, pin: nil, context: context,
                                                  pid: UInt32(clamping: pid), signal: signal)
            guard reply["ok"] as? Bool == true else {
                throw IDevice.ProbeError(reply["error"] as? String ?? "couldn't end the process")
            }
            return { attempt in attempt.detail = "signal \(signal)" }
        }
    }

    /// Simulates `place` for the whole device until `stopToolSession(key)`, or until the
    /// connection drops. `onSet` runs (on the main actor) each time the device has taken a
    /// coordinate. Returns "ok", "lost" when the connection dropped, or what failed.
    func simulateLocation(key: String, place: SimulatedPlace, onSet: @escaping @MainActor @Sendable (Double, Double) -> Void) async -> String {
        let ended = ToolResult<String>()
        let outcome = await runToolSession(key, action: "location", host: defaultTunnelHost, port: defaultTunnelPort) { target, file, context in
            let reply = try IDevice.simulateLocation(target: target, hostname: Self.hostname, file: file, pin: nil, context: context,
                                                     key: key, latitude: place.latitude, longitude: place.longitude) { latitude, longitude in
                Task { @MainActor in onSet(latitude, longitude) }
            }
            guard reply["ok"] as? Bool == true else {
                throw IDevice.ProbeError(reply["error"] as? String ?? "location simulation failed")
            }
            ended.value = reply["ended"] as? String
            let detail = "\(reply["ended"] ?? "ended") after \(reply["seconds"] ?? 0) s, \(reply["moves"] ?? 0) moves"
            return { attempt in attempt.detail = detail }
        }
        return outcome == "ok" && ended.value == "lost" ? "lost" : outcome
    }

    /// Streams the device's log (`pid`'s only, when given) until `stopToolSession(key)` or the
    /// device ends it. `onBatch` gets each batch's JSON on a background thread. Returns "ok",
    /// "lost" when the device ended it, or what failed.
    func streamLog(key: String, pid: Int?, minimum: ConsoleLevel, onBatch: @escaping @Sendable (Data) -> Void) async -> String {
        let ended = ToolResult<String>()
        let outcome = await runToolSession(key, action: "log", host: defaultTunnelHost, port: defaultTunnelPort) { target, file, context in
            let reply = try IDevice.streamLog(target: target, hostname: Self.hostname, file: file, pin: nil, context: context,
                                              key: key, pid: Int64(pid ?? -1), minimumLevel: minimum.rawValue, onBatch: onBatch)
            guard reply["ok"] as? Bool == true else {
                throw IDevice.ProbeError(reply["error"] as? String ?? "the log stream failed")
            }
            ended.value = reply["ended"] as? String
            let detail = "\(reply["ended"] ?? "ended"), \(reply["lines"] ?? 0) lines"
            return { attempt in attempt.detail = detail }
        }
        return outcome == "ok" && ended.value == "lost" ? "lost" : outcome
    }

    /// Ends the location or log session `key`. False when none runs under it.
    @discardableResult
    nonisolated func stopToolSession(_ key: String) -> Bool {
        AccountModel.call(apparition_tool_stop(key))["stopped"] as? Bool == true
    }

    /// Moves the running location session `key`. False when none runs under it.
    nonisolated func moveLocation(key: String, to place: SimulatedPlace) -> Bool {
        AccountModel.call(apparition_move_location(key, place.latitude, place.longitude))["moved"] as? Bool == true
    }
}

// MARK: The Tools tab's state

/// What the Tools tab shows and runs: the device's processes, a simulated location and the
/// device's log. Each goes through the same connection as JIT (LocalDevVPN or the companion).
@MainActor @Observable
final class DeviceToolsModel {
    let probe: ProbeModel

    init(probe: ProbeModel) {
        self.probe = probe
        recentPlaces = Self.loadRecentPlaces()
    }

    /// An outcome as a person reads it: without the "error: " the probe puts in front.
    static func message(_ outcome: String) -> String {
        if let problem = PairingRecord.problem(in: outcome) { return PairingRecord.message(problem) }
        return outcome.hasPrefix("error: ") ? String(outcome.dropFirst(7)) : outcome
    }

    // MARK: Processes

    private(set) var processes: [DeviceProcess] = []
    private(set) var processesLoadedAt: Date?
    private(set) var loadingProcesses = false
    var processError: String?

    func refreshProcesses() async {
        guard !loadingProcesses else { return }
        loadingProcesses = true
        defer { loadingProcesses = false }
        let (outcome, list) = await probe.listProcesses()
        if let list {
            processes = list
            processesLoadedAt = Date()
            processError = nil
        } else {
            processError = Self.message(outcome)
        }
    }

    /// Ends `process` (SIGKILL), then lists the processes again. Returns what failed, or nil.
    @discardableResult
    func end(_ process: DeviceProcess) async -> String? {
        let outcome = await probe.signalProcess(pid: process.pid, signal: 9)
        guard outcome == "ok" else {
            processError = Self.message(outcome)
            return processError
        }
        log.info("ended a \(process.kind.rawValue, privacy: .public) process")
        processes.removeAll { $0.pid == process.pid }
        // The system may start it again (a daemon, an app's extension) with a new pid.
        try? await Task.sleep(for: .milliseconds(600))
        await refreshProcesses()
        return nil
    }

    // MARK: Location

    enum LocationState: Equatable {
        case off
        case starting(SimulatedPlace)
        case on(SimulatedPlace)
        case failed(String)

        var place: SimulatedPlace? {
            switch self {
            case .starting(let place), .on(let place): place
            case .off, .failed: nil
            }
        }

        var running: Bool { place != nil }
    }

    private static let locationKey = "location"
    private(set) var location: LocationState = .off
    private(set) var recentPlaces: [SimulatedPlace] = []

    /// Starts simulating `place`, or moves the running simulation there.
    func simulate(_ place: SimulatedPlace) {
        remember(place)
        switch location {
        case .on:
            if probe.moveLocation(key: Self.locationKey, to: place) {
                location = .on(place)
                return
            }
        case .starting:
            // Not taken yet: the session takes the move when it's ready.
            if probe.moveLocation(key: Self.locationKey, to: place) { location = .starting(place) }
            return
        case .off, .failed:
            break
        }
        location = .starting(place)
        Task {
            // The model lives as long as the window, so the session holds it as the task does.
            let outcome = await probe.simulateLocation(key: Self.locationKey, place: place) { latitude, longitude in
                guard var current = self.location.place else { return }
                if abs(current.latitude - latitude) > 1e-9 || abs(current.longitude - longitude) > 1e-9 {
                    current = SimulatedPlace(name: String(format: "%.5f, %.5f", latitude, longitude), latitude: latitude, longitude: longitude)
                }
                self.location = .on(current)
            }
            switch outcome {
            case "ok": location = .off
            case "lost": location = .failed("The connection to this \(ThisDevice.noun) dropped, so it's back at its real location. Start it again to go on.")
            default: location = .failed(Self.message(outcome))
            }
        }
    }

    /// Puts the device's real location back.
    func stopLocation() {
        if !probe.stopToolSession(Self.locationKey), case .failed = location { location = .off }
    }

    func clearLocationError() {
        if case .failed = location { location = .off }
    }

    private static let recentPlacesKey = "tools.recentPlaces"

    private static func loadRecentPlaces() -> [SimulatedPlace] {
        guard let data = UserDefaults.standard.data(forKey: recentPlacesKey) else { return [] }
        return (try? JSONDecoder().decode([SimulatedPlace].self, from: data)) ?? []
    }

    private func remember(_ place: SimulatedPlace) {
        recentPlaces.removeAll { abs($0.latitude - place.latitude) < 1e-5 && abs($0.longitude - place.longitude) < 1e-5 }
        recentPlaces.insert(place, at: 0)
        recentPlaces = Array(recentPlaces.prefix(8))
        if let data = try? JSONEncoder().encode(recentPlaces) {
            UserDefaults.standard.set(data, forKey: Self.recentPlacesKey)
        }
    }

    func forgetRecentPlaces() {
        recentPlaces = []
        UserDefaults.standard.removeObject(forKey: Self.recentPlacesKey)
    }

    // MARK: Console

    /// Lines kept: older ones go as new ones come.
    static let consoleLimit = 5000
    private static let consoleKey = "console"

    private(set) var lines: [DeviceLogLine] = []
    private(set) var consoleRunning = false
    var consoleError: String?
    /// Whose log: one process's, or the whole device's (nil).
    private(set) var consoleProcess: DeviceProcess?
    var consoleLevel: ConsoleLevel = .standard
    /// Lines keep arriving while paused and are shown when it resumes (up to the limit).
    var paused = false {
        didSet { if !paused { flushPaused() } }
    }
    private var held: [DeviceLogLine] = []
    private var nextLineID = 0
    /// The session the arriving batches belong to; a batch from an earlier one is dropped.
    private var consoleGeneration = 0

    /// Starts the console for `process` (nil: the whole device), ending one already running.
    func startConsole(for process: DeviceProcess?) {
        consoleProcess = process
        if consoleRunning {
            probe.stopToolSession(Self.consoleKey)
            // The running session ends shortly; start once it has.
            Task {
                while consoleRunning { try? await Task.sleep(for: .milliseconds(50)) }
                beginConsole()
            }
        } else {
            beginConsole()
        }
    }

    private func beginConsole() {
        consoleError = nil
        consoleRunning = true
        consoleGeneration += 1
        let generation = consoleGeneration
        let pid = consoleProcess?.pid
        let level = consoleLevel
        Task {
            let outcome = await probe.streamLog(key: Self.consoleKey, pid: pid, minimum: level) { data in
                guard let wire = try? JSONDecoder().decode([DeviceLogLine.Wire].self, from: data), !wire.isEmpty else { return }
                Task { @MainActor in self.receive(wire, generation: generation) }
            }
            consoleRunning = false
            switch outcome {
            case "ok": break
            case "lost":
                consoleError = consoleProcess == nil
                    ? "The \(ThisDevice.noun) ended the log stream."
                    : "The log stream ended: \(consoleProcess?.title ?? "the process") may have quit."
            default: consoleError = Self.message(outcome)
            }
        }
    }

    func stopConsole() {
        probe.stopToolSession(Self.consoleKey)
    }

    func clearConsole() {
        lines.removeAll()
        held.removeAll()
    }

    private func receive(_ wire: [DeviceLogLine.Wire], generation: Int) {
        guard generation == consoleGeneration else { return }
        let arrived = wire.map { line -> DeviceLogLine in
            nextLineID += 1
            return DeviceLogLine(id: nextLineID, time: Date(timeIntervalSince1970: line.time / 1000), pid: line.pid,
                                 process: line.process, sender: line.sender, level: line.level, subsystem: line.subsystem,
                                 category: line.category, message: line.message)
        }
        if paused {
            held.append(contentsOf: arrived)
            if held.count > Self.consoleLimit { held.removeFirst(held.count - Self.consoleLimit) }
        } else {
            append(arrived)
        }
    }

    private func flushPaused() {
        append(held)
        held.removeAll()
    }

    private func append(_ arrived: [DeviceLogLine]) {
        lines.append(contentsOf: arrived)
        if lines.count > Self.consoleLimit { lines.removeFirst(lines.count - Self.consoleLimit) }
    }

    /// The lines shown, as text, for sharing.
    func consoleText(matching filter: String) -> String {
        lines.filter { Self.matches($0, filter) }.map(\.text).joined(separator: "\n")
    }

    nonisolated static func matches(_ line: DeviceLogLine, _ filter: String) -> Bool {
        guard !filter.isEmpty else { return true }
        return line.message.localizedCaseInsensitiveContains(filter)
            || line.process.localizedCaseInsensitiveContains(filter)
            || line.sender.localizedCaseInsensitiveContains(filter)
            || (line.subsystem?.localizedCaseInsensitiveContains(filter) ?? false)
            || (line.category?.localizedCaseInsensitiveContains(filter) ?? false)
    }
}
