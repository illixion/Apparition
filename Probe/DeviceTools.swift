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

/// A value in an Instruments sample. Fields are left dynamic because Apple changes the
/// monitoring dictionary between OS releases.
enum ToolJSONValue: Codable, Sendable {
    case object([String: ToolJSONValue])
    case array([ToolJSONValue])
    case string(String)
    case number(Double)
    case bool(Bool)
    case null

    init(from decoder: Decoder) throws {
        let value = try decoder.singleValueContainer()
        if value.decodeNil() { self = .null }
        else if let bool = try? value.decode(Bool.self) { self = .bool(bool) }
        else if let number = try? value.decode(Double.self) { self = .number(number) }
        else if let string = try? value.decode(String.self) { self = .string(string) }
        else if let object = try? decoder.container(keyedBy: DynamicCodingKey.self) {
            self = .object(try Dictionary(uniqueKeysWithValues: object.allKeys.map { ($0.stringValue, try object.decode(ToolJSONValue.self, forKey: $0)) }))
        } else if var array = try? decoder.unkeyedContainer() {
            var values: [ToolJSONValue] = []
            while !array.isAtEnd { values.append(try array.decode(ToolJSONValue.self)) }
            self = .array(values)
        } else { self = .null }
    }

    func encode(to encoder: Encoder) throws {
        switch self {
        case .object(let value): try value.encode(to: encoder)
        case .array(let value): try value.encode(to: encoder)
        case .string(let value): try value.encode(to: encoder)
        case .number(let value): try value.encode(to: encoder)
        case .bool(let value): try value.encode(to: encoder)
        case .null: var container = encoder.singleValueContainer(); try container.encodeNil()
        }
    }

    var numericValue: Double? {
        switch self {
        case .number(let value): value
        case .object(let values): values.values.compactMap(\.numericValue).first
        case .array(let values): values.compactMap(\.numericValue).first
        default: nil
        }
    }

    func number(matching fragments: [String]) -> Double? {
        guard case .object(let values) = self else { return numericValue }
        for fragment in fragments {
            if let value = values.first(where: { $0.key.lowercased().contains(fragment) })?.value.numericValue { return value }
        }
        return values.values.lazy.compactMap { $0.number(matching: fragments) }.first
    }
}

private struct DynamicCodingKey: CodingKey {
    let stringValue: String
    let intValue: Int? = nil
    init?(stringValue: String) { self.stringValue = stringValue }
    init?(intValue: Int) { return nil }
}

struct DeviceActivityReading: Sendable {
    var cpu: Double?
    var footprint: UInt64?
}

struct DeviceDataEntry: Codable, Identifiable, Hashable, Sendable {
    let name: String
    let path: String
    let kind: String
    let bytes: UInt64
    var id: String { path }
    var isDirectory: Bool { kind == "directory" }
}

struct DeviceConditionProfile: Codable, Identifiable, Hashable, Sendable {
    let identifier: String
    let description: String
    var id: String { identifier }
}

struct DeviceConditionGroup: Codable, Identifiable, Hashable, Sendable {
    let identifier: String
    let profiles: [DeviceConditionProfile]
    var id: String { identifier }
}

struct DeviceToolService: Codable, Identifiable, Hashable, Sendable {
    let name: String
    let port: Int
    let remoteXpc: Bool
    var id: String { name }
}

struct DeviceNetworkConnection: Identifiable, Sendable {
    let serial: UInt64
    var pid: Int
    var local: String
    var remote: String
    var interfaceIndex: UInt32
    var interfaceName: String?
    var rxBytes: UInt64
    var txBytes: UInt64
    var retransmits: UInt64
    var averageRTT: UInt64
    var id: UInt64 { serial }
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

    /// Ends a location, monitor, log or condition session `key`. False when none runs under it.
    func streamActivity(key: String, onSample: @escaping @Sendable (Data) -> Void) async -> String {
        let ended = ToolResult<String>()
        let outcome = await runToolSession(key, action: "activity", host: defaultTunnelHost, port: defaultTunnelPort) { target, file, context in
            let reply = try IDevice.streamActivity(target: target, hostname: Self.hostname, file: file, pin: nil, context: context,
                                                   key: key, onSample: onSample)
            guard reply["ok"] as? Bool == true else { throw IDevice.ProbeError(reply["error"] as? String ?? "the activity monitor failed") }
            ended.value = reply["ended"] as? String
            return { attempt in attempt.detail = "activity monitor \(reply["ended"] ?? "ended")" }
        }
        return outcome == "ok" && ended.value == "lost" ? "lost" : outcome
    }

    func streamNetwork(key: String, onEvent: @escaping @Sendable (Data) -> Void) async -> String {
        let ended = ToolResult<String>()
        let outcome = await runToolSession(key, action: "network-monitor", host: defaultTunnelHost, port: defaultTunnelPort) { target, file, context in
            let reply = try IDevice.streamNetwork(target: target, hostname: Self.hostname, file: file, pin: nil, context: context,
                                                  key: key, onEvent: onEvent)
            guard reply["ok"] as? Bool == true else { throw IDevice.ProbeError(reply["error"] as? String ?? "the network monitor failed") }
            ended.value = reply["ended"] as? String
            return { attempt in attempt.detail = "network monitor \(reply["ended"] ?? "ended")" }
        }
        return outcome == "ok" && ended.value == "lost" ? "lost" : outcome
    }

    func runCondition(key: String, group: String, profile: String,
                      onEnabled: @escaping @MainActor @Sendable () -> Void) async -> (String, Bool) {
        let state = ToolResult<(String?, Bool?)>()
        let outcome = await runToolSession(key, action: "condition", host: defaultTunnelHost, port: defaultTunnelPort) { target, file, context in
            let reply = try IDevice.conditionSession(target: target, hostname: Self.hostname, file: file, pin: nil, context: context,
                                                     key: key, condition: group, profile: profile) {
                Task { @MainActor in onEnabled() }
            }
            guard reply["ok"] as? Bool == true else { throw IDevice.ProbeError(reply["error"] as? String ?? "the device refused this condition") }
            state.value = (reply["ended"] as? String, reply["cleared"] as? Bool)
            return { attempt in attempt.detail = "condition \(reply["ended"] ?? "ended")" }
        }
        return (outcome == "ok" ? state.value?.0 ?? "stopped" : outcome, outcome == "ok" && state.value?.1 == true)
    }

    func listConditionProfiles() async -> (String, [DeviceConditionGroup]?) {
        await prepareRoute(host: defaultTunnelHost)
        let target = tunnelTarget(host: defaultTunnelHost, port: defaultTunnelPort)
        let result = ToolResult<[DeviceConditionGroup]>()
        let outcome = await run(action: "conditions", host: defaultTunnelHost, port: defaultTunnelPort, exclusive: false) { file, context in
            let reply = try IDevice.conditionProfiles(target: target, hostname: Self.hostname, file: file, pin: nil, context: context)
            guard reply["ok"] as? Bool == true, let values = reply["groups"] else {
                throw IDevice.ProbeError(reply["error"] as? String ?? "couldn't read connection conditions")
            }
            result.value = try JSONDecoder().decode([DeviceConditionGroup].self, from: JSONSerialization.data(withJSONObject: values))
            return { attempt in attempt.detail = "\(result.value?.count ?? 0) condition groups" }
        }
        return (outcome, result.value)
    }

    func toolServices() async -> (String, [DeviceToolService]?, Bool?) {
        await prepareRoute(host: defaultTunnelHost)
        let target = tunnelTarget(host: defaultTunnelHost, port: defaultTunnelPort)
        let result = ToolResult<([DeviceToolService], Bool)>()
        let outcome = await run(action: "tool-readiness", host: defaultTunnelHost, port: defaultTunnelPort, exclusive: false) { file, context in
            let reply = try IDevice.toolServiceReadiness(target: target, hostname: Self.hostname, file: file, pin: nil, context: context)
            guard reply["ok"] as? Bool == true, let values = reply["services"] else {
                throw IDevice.ProbeError(reply["error"] as? String ?? "couldn't read device services")
            }
            let services = try JSONDecoder().decode([DeviceToolService].self, from: JSONSerialization.data(withJSONObject: values))
            result.value = (services, reply["instruments"] as? Bool ?? false)
            return { attempt in attempt.detail = "\(services.count) advertised services" }
        }
        return (outcome, result.value?.0, result.value?.1)
    }

    func browseContainer(bundleId: String, path: String) async -> (String, [DeviceDataEntry]?) {
        await prepareRoute(host: defaultTunnelHost)
        let target = tunnelTarget(host: defaultTunnelHost, port: defaultTunnelPort)
        let result = ToolResult<[DeviceDataEntry]>()
        let outcome = await run(action: "app-data", host: defaultTunnelHost, port: defaultTunnelPort, exclusive: false) { file, context in
            let reply = try IDevice.browseContainer(target: target, hostname: Self.hostname, file: file, pin: nil, context: context,
                                                    bundleId: bundleId, path: path)
            guard reply["ok"] as? Bool == true, let values = reply["entries"] else {
                throw IDevice.ProbeError(reply["error"] as? String ?? "couldn't browse this app's data")
            }
            result.value = try JSONDecoder().decode([DeviceDataEntry].self, from: JSONSerialization.data(withJSONObject: values))
            return { attempt in attempt.detail = "\(result.value?.count ?? 0) data entries" }
        }
        return (outcome, result.value)
    }

    func exportContainerFile(bundleId: String, path: String, destination: URL) async -> String {
        await prepareRoute(host: defaultTunnelHost)
        let target = tunnelTarget(host: defaultTunnelHost, port: defaultTunnelPort)
        return await run(action: "app-data-export", host: defaultTunnelHost, port: defaultTunnelPort) { file, context in
            let reply = try IDevice.exportContainerFile(target: target, hostname: Self.hostname, file: file, pin: nil, context: context,
                                                        bundleId: bundleId, devicePath: path, destination: destination.path)
            guard reply["ok"] as? Bool == true else { throw IDevice.ProbeError(reply["error"] as? String ?? "couldn't export this file") }
            return { attempt in attempt.detail = "exported \(reply["bytes"] ?? 0) bytes" }
        }
    }

    func exportContainerBackup(bundleId: String, destination: URL) async -> (String, [String: Any]?) {
        await prepareRoute(host: defaultTunnelHost)
        let target = tunnelTarget(host: defaultTunnelHost, port: defaultTunnelPort)
        let result = ToolResult<[String: Any]>()
        let outcome = await run(action: "app-data-backup", host: defaultTunnelHost, port: defaultTunnelPort) { file, context in
            let reply = try IDevice.exportContainerBackup(target: target, hostname: Self.hostname, file: file, pin: nil, context: context,
                                                          bundleId: bundleId, destination: destination.path)
            guard reply["ok"] as? Bool == true else { throw IDevice.ProbeError(reply["error"] as? String ?? "couldn't back up this app") }
            result.value = reply
            return { attempt in attempt.detail = "\(reply["files"] ?? 0) files · \(reply["bytes"] ?? 0) bytes" }
        }
        return (outcome, result.value)
    }

    func restoreContainerBackup(bundleId: String, archive: URL) async -> (String, [String: Any]?) {
        await prepareRoute(host: defaultTunnelHost)
        let target = tunnelTarget(host: defaultTunnelHost, port: defaultTunnelPort)
        let result = ToolResult<[String: Any]>()
        let outcome = await run(action: "app-data-restore", host: defaultTunnelHost, port: defaultTunnelPort) { file, context in
            let reply = try IDevice.restoreContainerBackup(target: target, hostname: Self.hostname, file: file, pin: nil, context: context,
                                                           bundleId: bundleId, archive: archive.path)
            guard reply["ok"] as? Bool == true else { throw IDevice.ProbeError(reply["error"] as? String ?? "couldn't restore this app backup") }
            result.value = reply
            return { attempt in attempt.detail = "restored \(reply["files"] ?? 0) files" }
        }
        return (outcome, result.value)
    }

    func captureSysdiagnose(destination: URL) async -> (String, [String: Any]?) {
        await prepareRoute(host: defaultTunnelHost)
        let target = tunnelTarget(host: defaultTunnelHost, port: defaultTunnelPort)
        let result = ToolResult<[String: Any]>()
        let outcome = await run(action: "sysdiagnose", host: defaultTunnelHost, port: defaultTunnelPort) { file, context in
            let reply = try IDevice.captureSysdiagnose(target: target, hostname: Self.hostname, file: file,
                                                       pin: nil, context: context, destination: destination.path)
            guard reply["ok"] as? Bool == true else { throw IDevice.ProbeError(reply["error"] as? String ?? "sysdiagnose capture failed") }
            result.value = reply
            return { attempt in attempt.detail = "\(reply["bytes"] ?? 0) bytes" }
        }
        return (outcome, result.value)
    }

    @discardableResult
    nonisolated func stopToolSession(_ key: String) -> Bool {
        AccountModel.call(apparition_tool_stop(key))["stopped"] as? Bool == true
    }

    /// Moves the running location session `key`. False when none runs under it.
    nonisolated func moveLocation(key: String, to place: SimulatedPlace) -> Bool {
        AccountModel.call(apparition_move_location(key, place.latitude, place.longitude))["moved"] as? Bool == true
    }
}

private struct ActivitySampleWire: Decodable, Sendable {
    let type: String?
    let processes: [String: ToolJSONValue]?
    let systemCPUUsage: ToolJSONValue?
}

private struct NetworkAddressWire: Decodable, Sendable {
    let address: String
    let port: UInt16
}

private struct NetworkEventWire: Decodable, Sendable {
    let type: String
    let index: UInt32?
    let name: String?
    let pid: Int?
    let serial: UInt64?
    let interface: UInt32?
    let local: NetworkAddressWire?
    let remote: NetworkAddressWire?
    let rxBytes: UInt64?
    let txBytes: UInt64?
    let retransmits: UInt64?
    let avgRTT: UInt64?
    let minRTT: UInt64?
    let rxPackets: UInt64?
    let txPackets: UInt64?
    let time: UInt64?
}

// MARK: The Tools tab's state

/// What the Tools tab shows and runs through the same connection as JIT (LocalDevVPN or the companion).
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

    // MARK: Activity monitor

    private static let activityKey = "activity"
    private(set) var activityStarting = false
    private(set) var activityRunning = false
    private(set) var activityError: String?
    private(set) var activityReadings: [Int: DeviceActivityReading] = [:]
    private(set) var systemCPUFields: ToolJSONValue?

    func startActivity() {
        guard !activityStarting && !activityRunning else { return }
        activityStarting = true
        activityError = nil
        activityReadings.removeAll()
        systemCPUFields = nil
        Task {
            let outcome = await probe.streamActivity(key: Self.activityKey) { data in
                guard let sample = try? JSONDecoder().decode(ActivitySampleWire.self, from: data) else { return }
                Task { @MainActor in
                    if sample.type == "started" {
                        self.activityStarting = false
                        self.activityRunning = true
                        return
                    }
                    self.systemCPUFields = sample.systemCPUUsage
                    var readings: [Int: DeviceActivityReading] = [:]
                    for (pidText, fields) in sample.processes ?? [:] {
                        guard let pid = Int(pidText) else { continue }
                        let cpu = fields.number(matching: ["cpuusage", "cpu_percent", "cpupct", "cpu"])
                        let memory = fields.number(matching: ["physfootprint", "physicalfootprint", "footprint", "resident", "memory"])
                        let footprint = memory.flatMap { $0.isFinite && $0 >= 0 && $0 < Double(UInt64.max) ? UInt64($0) : nil }
                        readings[pid] = DeviceActivityReading(cpu: cpu, footprint: footprint)
                    }
                    self.activityReadings = readings
                }
            }
            activityStarting = false
            activityRunning = false
            if outcome != "ok" {
                activityError = outcome == "lost" ? "The device ended its activity stream." : Self.message(outcome)
            }
        }
    }

    func stopActivity() {
        guard activityRunning else { return }
        probe.stopToolSession(Self.activityKey)
    }

    // MARK: Network monitor

    private static let networkKey = "network-monitor"
    private(set) var networkStarting = false
    private(set) var networkRunning = false
    private(set) var networkError: String?
    private(set) var networkConnections: [DeviceNetworkConnection] = []
    private var networkBySerial: [UInt64: DeviceNetworkConnection] = [:]
    private var networkInterfaces: [UInt32: String] = [:]

    func startNetwork() {
        guard !networkStarting && !networkRunning else { return }
        networkStarting = true
        networkError = nil
        networkConnections.removeAll()
        networkBySerial.removeAll()
        networkInterfaces.removeAll()
        Task {
            let outcome = await probe.streamNetwork(key: Self.networkKey) { data in
                guard let event = try? JSONDecoder().decode(NetworkEventWire.self, from: data) else { return }
                Task { @MainActor in self.receive(event) }
            }
            networkStarting = false
            networkRunning = false
            if outcome != "ok" {
                networkError = outcome == "lost" ? "The device ended its network stream." : Self.message(outcome)
            }
        }
    }

    private func receive(_ event: NetworkEventWire) {
        switch event.type {
        case "started":
            networkStarting = false
            networkRunning = true
        case "interface":
            if let index = event.index, let name = event.name {
                networkInterfaces[index] = name
                for serial in Array(networkBySerial.keys) where networkBySerial[serial]?.interfaceIndex == index {
                    guard var connection = networkBySerial[serial] else { continue }
                    connection.interfaceName = name
                    networkBySerial[serial] = connection
                }
            }
        case "connection":
            guard let serial = event.serial else { return }
            let address: (NetworkAddressWire?) -> String = { value in
                guard let value else { return "—" }
                let host = value.address.contains(":") ? "[\(value.address)]" : value.address
                return "\(host):\(value.port)"
            }
            networkBySerial[serial] = DeviceNetworkConnection(serial: serial, pid: event.pid ?? 0,
                local: address(event.local), remote: address(event.remote), interfaceIndex: event.interface ?? 0,
                interfaceName: networkInterfaces[event.interface ?? 0], rxBytes: 0, txBytes: 0,
                retransmits: 0, averageRTT: 0)
        case "update":
            guard let serial = event.serial else { return }
            var row = networkBySerial[serial] ?? DeviceNetworkConnection(serial: serial, pid: 0, local: "—", remote: "—",
                interfaceIndex: 0, interfaceName: nil, rxBytes: 0, txBytes: 0, retransmits: 0, averageRTT: 0)
            row.rxBytes = event.rxBytes ?? row.rxBytes
            row.txBytes = event.txBytes ?? row.txBytes
            row.retransmits = event.retransmits ?? row.retransmits
            row.averageRTT = event.avgRTT ?? row.averageRTT
            networkBySerial[serial] = row
        default:
            break
        }
        if networkBySerial.count > 500 {
            let recent = Set(networkBySerial.keys.sorted().suffix(500))
            networkBySerial = networkBySerial.filter { recent.contains($0.key) }
        }
        let totalBytes: (DeviceNetworkConnection) -> UInt64 = { connection in
            let (total, overflow) = connection.rxBytes.addingReportingOverflow(connection.txBytes)
            return overflow ? .max : total
        }
        networkConnections = networkBySerial.values.sorted {
            totalBytes($0) > totalBytes($1)
        }.prefix(100).map { $0 }
    }

    func stopNetwork() {
        guard networkRunning else { return }
        probe.stopToolSession(Self.networkKey)
    }

    // MARK: Connection conditions

    private static let conditionKey = "condition"
    private(set) var conditionGroups: [DeviceConditionGroup] = []
    private(set) var loadingConditions = false
    private(set) var conditionStarting = false
    private(set) var conditionRunning = false
    private(set) var activeCondition: String?
    private(set) var conditionError: String?

    func refreshConditions() async {
        guard !loadingConditions else { return }
        loadingConditions = true
        defer { loadingConditions = false }
        let (outcome, groups) = await probe.listConditionProfiles()
        if let groups {
            conditionGroups = groups
            conditionError = nil
        } else {
            conditionError = Self.message(outcome)
        }
    }

    func startCondition(group: DeviceConditionGroup, profile: DeviceConditionProfile) {
        guard !conditionStarting && !conditionRunning else { return }
        conditionStarting = true
        conditionError = nil
        Task {
            let (ended, cleared) = await probe.runCondition(key: Self.conditionKey, group: group.identifier, profile: profile.identifier) {
                self.conditionStarting = false
                self.conditionRunning = true
                self.activeCondition = profile.description.isEmpty ? profile.identifier : profile.description
            }
            conditionStarting = false
            conditionRunning = false
            activeCondition = nil
            if ended == "lost" {
                conditionError = "The device connection dropped. Apparition asked it to clear the condition; check the target app if the effect remains."
            } else if ended != "stopped" || !cleared {
                conditionError = Self.message(ended == "stopped" ? "The device did not confirm that it cleared the condition." : ended)
            }
        }
    }

    func stopCondition() {
        guard conditionRunning else { return }
        probe.stopToolSession(Self.conditionKey)
    }

    // MARK: Developer services

    private(set) var toolServices: [DeviceToolService] = []
    private(set) var instrumentsAvailable: Bool?
    private(set) var loadingServices = false
    private(set) var servicesError: String?

    func refreshServices() async {
        guard !loadingServices else { return }
        loadingServices = true
        defer { loadingServices = false }
        let (outcome, services, instruments) = await probe.toolServices()
        if let services {
            toolServices = services.sorted { $0.name < $1.name }
            instrumentsAvailable = instruments
            servicesError = nil
        } else {
            servicesError = Self.message(outcome)
        }
    }

    // MARK: App data

    private(set) var dataBundleId: String?
    private(set) var dataPath = "/Documents"
    private(set) var dataEntries: [DeviceDataEntry] = []
    private(set) var loadingData = false
    private(set) var creatingDataBackup = false
    private(set) var restoringDataBackup = false
    private(set) var dataError: String?
    private(set) var dataStatus: String?

    func reportDataError(_ message: String) {
        dataError = message
    }

    func browseData(bundleId: String, path: String) async {
        guard !loadingData else { return }
        loadingData = true
        dataError = nil
        dataStatus = nil
        defer { loadingData = false }
        let (outcome, entries) = await probe.browseContainer(bundleId: bundleId, path: path)
        if let entries {
            dataBundleId = bundleId
            dataPath = path
            dataEntries = entries
        } else {
            dataError = Self.message(outcome)
        }
    }

    private func exportURL(name: String) throws -> URL {
        let folder = FileManager.default.temporaryDirectory.appendingPathComponent("ApparitionExports", isDirectory: true)
        try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
        return folder.appendingPathComponent(name)
    }

    func exportDataFile(bundleId: String, entry: DeviceDataEntry) async -> URL? {
        let name = entry.name.replacingOccurrences(of: "/", with: "_")
        guard let destination = try? exportURL(name: "\(UUID().uuidString)-\(name)") else { return nil }
        let outcome = await probe.exportContainerFile(bundleId: bundleId, path: entry.path, destination: destination)
        guard outcome == "ok" else {
            try? FileManager.default.removeItem(at: destination)
            dataError = Self.message(outcome)
            return nil
        }
        dataError = nil
        return destination
    }

    func createDataBackup(bundleId: String) async -> URL? {
        guard !creatingDataBackup else { return nil }
        creatingDataBackup = true
        dataError = nil
        dataStatus = nil
        defer { creatingDataBackup = false }
        guard let destination = try? exportURL(name: "\(bundleId)-\(UUID().uuidString).apparition-backup.zip") else { return nil }
        let (outcome, result) = await probe.exportContainerBackup(bundleId: bundleId, destination: destination)
        guard outcome == "ok", (result?["failed"] as? [String] ?? []).isEmpty else {
            try? FileManager.default.removeItem(at: destination)
            dataError = Self.message(outcome == "ok" ? "Some app files could not be read; the backup is incomplete." : outcome)
            return nil
        }
        let files = result?["files"] as? Int ?? 0
        let bytes = result?["bytes"] as? Int ?? 0
        dataStatus = "Backed up \(files) files · \(ByteCountFormatter.string(fromByteCount: Int64(bytes), countStyle: .file))"
        dataError = nil
        return destination
    }

    func restoreDataBackup(bundleId: String, archive: URL) async {
        guard !restoringDataBackup else { return }
        restoringDataBackup = true
        dataError = nil
        dataStatus = nil
        defer { restoringDataBackup = false }
        let (outcome, result) = await probe.restoreContainerBackup(bundleId: bundleId, archive: archive)
        if outcome == "ok" {
            let failed = result?["failed"] as? [String] ?? []
            let files = result?["files"] as? Int ?? 0
            await browseData(bundleId: bundleId, path: dataPath)
            dataStatus = "Restored \(files) files" + (failed.isEmpty ? "" : " · \(failed.count) files failed")
            if !failed.isEmpty { dataError = failed.prefix(5).joined(separator: "\n") }
        } else {
            dataError = Self.message(outcome)
        }
    }

    // MARK: Sysdiagnose

    private(set) var capturingSysdiagnose = false
    private(set) var sysdiagnoseError: String?
    private(set) var sysdiagnoseStatus: String?

    func captureSysdiagnose() async -> URL? {
        guard !capturingSysdiagnose else { return nil }
        capturingSysdiagnose = true
        sysdiagnoseError = nil
        sysdiagnoseStatus = nil
        defer { capturingSysdiagnose = false }
        let destination = FileManager.default.temporaryDirectory.appendingPathComponent("apparition-sysdiagnose-\(UUID().uuidString).tar.gz")
        let (outcome, result) = await probe.captureSysdiagnose(destination: destination)
        guard outcome == "ok" else {
            try? FileManager.default.removeItem(at: destination)
            sysdiagnoseError = Self.message(outcome)
            return nil
        }
        let bytes = result?["bytes"] as? Int ?? 0
        sysdiagnoseStatus = "Captured \(ByteCountFormatter.string(fromByteCount: Int64(bytes), countStyle: .file))"
        return destination
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
