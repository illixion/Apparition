import Darwin
import Foundation

/// Thin Swift wrappers over the idevice C FFI calls the probe needs.
/// Every call here blocks: run them off the main thread.
enum IDevice {
    struct FFIError: Error, CustomStringConvertible {
        let code: Int32
        let subCode: Int32
        let message: String
        var description: String { "idevice error \(code)/\(subCode): \(message)" }
    }

    struct ProbeError: Error, CustomStringConvertible {
        let description: String
        init(_ description: String) { self.description = description }
    }

    struct Service: Codable, Sendable {
        let name: String
        let port: Int
        let usesRemoteXPC: Bool
    }

    struct PeerDevice: Codable, Sendable {
        let name: String
        let model: String
        let udid: String
    }

    static func check(_ err: UnsafeMutablePointer<IdeviceFfiError>?) throws {
        guard let err else { return }
        let message = err.pointee.message.map { String(cString: $0) } ?? ""
        let error = FFIError(code: err.pointee.code, subCode: err.pointee.sub_code, message: message)
        idevice_error_free(err)
        throw error
    }

    /// Resolves a numeric host (IPv4, IPv6, or scoped `fe80::1%en0`) and port into a sockaddr.
    static func withSockaddr<T>(host: String, port: UInt16,
                                _ body: (UnsafePointer<sockaddr>, socklen_t) throws -> T) throws -> T {
        var hints = addrinfo()
        hints.ai_flags = AI_NUMERICHOST
        hints.ai_socktype = SOCK_STREAM
        var result: UnsafeMutablePointer<addrinfo>?
        let rc = getaddrinfo(host, String(port), &hints, &result)
        guard rc == 0, let info = result, let addr = info.pointee.ai_addr else {
            throw ProbeError("not a numeric address: \(host) (\(String(cString: gai_strerror(rc))))")
        }
        defer { freeaddrinfo(result) }
        return try body(addr, info.pointee.ai_addrlen)
    }

    /// Owns an RP pairing file handle.
    final class PairingFile {
        let handle: OpaquePointer

        private init(handle: OpaquePointer) { self.handle = handle }

        static func generate(hostname: String) throws -> PairingFile {
            var out: OpaquePointer?
            try check(rp_pairing_file_generate(hostname, &out))
            guard let out else { throw ProbeError("rp_pairing_file_generate returned no handle") }
            return PairingFile(handle: out)
        }

        static func load(_ data: Data) throws -> PairingFile {
            var out: OpaquePointer?
            try data.withUnsafeBytes { bytes in
                try check(rp_pairing_file_from_bytes(bytes.bindMemory(to: UInt8.self).baseAddress, UInt(data.count), &out))
            }
            guard let out else { throw ProbeError("rp_pairing_file_from_bytes returned no handle") }
            return PairingFile(handle: out)
        }

        func serialized() throws -> Data {
            var bytes: UnsafeMutablePointer<UInt8>?
            var length: UInt = 0
            try check(rp_pairing_file_to_bytes(handle, &bytes, &length))
            guard let bytes else { throw ProbeError("rp_pairing_file_to_bytes returned nothing") }
            defer { idevice_data_free(bytes, length) }
            return Data(bytes: bytes, count: Int(length))
        }

        deinit { rp_pairing_file_free(handle) }
    }

    typealias PinCallback = @convention(c) (UnsafeMutableRawPointer?) -> UnsafePointer<CChar>?

    /// Where a tunnel goes: straight to host:port (LocalDevVPN's 10.7.0.1), or through a
    /// companion relay ("ip:port"), which connects back to this device on `port`.
    struct Target: Sendable, CustomStringConvertible {
        let host: String
        let port: UInt16
        let relay: String?
        var description: String { relay.map { "relay \($0) -> :\(port)" } ?? "\(host):\(port)" }
    }

    /// Opens the tunnel (apparition_tunnel_create), runs `body` with its handles, then frees them.
    /// With no `pin` it only pair-verifies, and a pairing the device turns down throws
    /// `PairingRejected` at once; with one, it pair-sets-up instead and asks for the PIN.
    static func withTunnel<T>(_ target: Target, hostname: String, file: PairingFile, pin: PinCallback?,
                              context: UnsafeMutableRawPointer,
                              _ body: (OpaquePointer?, OpaquePointer?) throws -> T) throws -> T {
        var adapter: OpaquePointer?
        var handshake: OpaquePointer?
        let reply = AccountModel.call(apparition_tunnel_create(target.host, target.port, target.relay, hostname, file.handle,
                                                           pin, context, &adapter, &handshake))
        defer {
            if let handshake { rsd_handshake_free(handshake) }
            if let adapter { adapter_free(adapter) }
        }
        guard reply["ok"] as? Bool == true else {
            if reply["code"] as? String == "pairingRejected" { throw PairingRejected() }
            throw ProbeError("tunnel via \(target): \(reply["error"] as? String ?? "failed")")
        }
        return try body(adapter, handshake)
    }

    /// Pair-setup (or pair-verify) against a RemotePairing service, without a tunnel.
    static func pair(host: String, port: UInt16, hostname: String, file: PairingFile,
                     pin: PinCallback, context: UnsafeMutableRawPointer) throws -> PeerDevice? {
        try withSockaddr(host: host, port: port) { addr, length in
            var peer: UnsafeMutablePointer<RpPairingPeerDeviceC>?
            try check(rppairing_pair_network(addr, length, hostname, file.handle, pin, context, &peer))
            guard let peer else { return nil }
            defer { rppairing_peer_device_free(peer) }
            func text(_ pointer: UnsafeMutablePointer<CChar>?) -> String { pointer.map { String(cString: $0) } ?? "" }
            return PeerDevice(name: text(peer.pointee.name), model: text(peer.pointee.model), udid: text(peer.pointee.udid))
        }
    }

    /// Pair-setup (or pair-verify) through the companion relay ("ip:port"), without a tunnel.
    static func pairViaRelay(_ relay: String, port: UInt16, hostname: String, file: PairingFile,
                             pin: PinCallback, context: UnsafeMutableRawPointer) throws {
        let reply = AccountModel.call(apparition_pair_relay(relay, port, hostname, file.handle, pin, context))
        guard reply["ok"] as? Bool == true else { throw ProbeError(reply["error"] as? String ?? "pairing failed") }
    }

    /// A tunnel for a JIT session, with the developer disk image mounted first when nothing has
    /// mounted it. The device lists its services once, when the tunnel opens, and lists the
    /// debugger only while the image is mounted: after mounting it here, the session needs a
    /// new tunnel, whose list has the debugger in it. `body` gets the mount's event line.
    private static func withJITTunnel<T>(_ target: Target, hostname: String, file: PairingFile, pin: PinCallback?,
                                         context: UnsafeMutableRawPointer,
                                         _ body: (OpaquePointer?, OpaquePointer?, String?) throws -> T) throws -> T {
        var disk: DeveloperDiskImage.Outcome?
        let first: T? = try withTunnel(target, hostname: hostname, file: file, pin: pin, context: context) { adapter, handshake in
            disk = DeveloperDiskImage.ensure(adapter: adapter, handshake: handshake)
            if disk?.mountedNow == true { return nil }
            return try body(adapter, handshake, disk?.line)
        }
        if let first { return first }
        return try withTunnel(target, hostname: hostname, file: file, pin: pin, context: context) { adapter, handshake in
            try body(adapter, handshake, disk?.line)
        }
    }

    /// Opens a tunnel, launches the app under the debugger and serves its JIT requests.
    static func launchJIT(target: Target, hostname: String, file: PairingFile,
                          pin: PinCallback?, context: UnsafeMutableRawPointer,
                          bundleId: String, waitSeconds: UInt32) throws -> [String: Any] {
        try withJITTunnel(target, hostname: hostname, file: file, pin: pin, context: context) { adapter, handshake, disk in
            DeveloperDiskImage.annotate(AccountModel.call(apparition_launch_jit(adapter, handshake, bundleId, waitSeconds)), with: disk)
        }
    }

    /// Opens a tunnel, attaches the debugger to the running process `pid` of `bundleId` and
    /// serves its JIT requests, bringing it back to the front.
    static func attachJIT(target: Target, hostname: String, file: PairingFile,
                          pin: PinCallback?, context: UnsafeMutableRawPointer,
                          pid: UInt64, bundleId: String, waitSeconds: UInt32) throws -> [String: Any] {
        try withJITTunnel(target, hostname: hostname, file: file, pin: pin, context: context) { adapter, handshake, disk in
            DeveloperDiskImage.annotate(AccountModel.call(apparition_attach_jit(adapter, handshake, pid, bundleId, waitSeconds)), with: disk)
        }
    }

    /// Opens a tunnel and lends a debugserver session for `bundleId` to a StikDebug JIT script
    /// (JITScript), which serves the app from then on: a new launch with `launch`, else the
    /// running process `pid` (0: the app's running process). Returns the session's reply, with
    /// the script's `log` lines added to its events.
    static func runJITScript(target: Target, hostname: String, file: PairingFile,
                             pin: PinCallback?, context: UnsafeMutableRawPointer,
                             pid: UInt64, bundleId: String, launch: Bool, script: String) throws -> [String: Any] {
        try withJITTunnel(target, hostname: hostname, file: file, pin: pin, context: context) { adapter, handshake, disk in
            var session: OpaquePointer?
            let opened = AccountModel.call(apparition_jit_script_open(adapter, handshake, pid, bundleId, launch, &session))
            guard opened["ok"] as? Bool == true, let session else { return DeveloperDiskImage.annotate(opened, with: disk) }
            let outcome = JITScript.run(script, session: session, pid: opened["pid"] as? Int ?? 0)
            var reply = AccountModel.call(apparition_jit_script_close(session))
            var events = reply["events"] as? [String] ?? []
            events += outcome.lines.map { "script: \($0)" }
            if outcome.droppedLines > 0 { events.append("script: (\(outcome.droppedLines) more lines not kept)") }
            if let exception = outcome.exception { events.append("script threw: \(exception)") }
            if let disk { events.insert(disk, at: 0) }
            reply["events"] = events
            return reply
        }
    }

    /// Opens a tunnel and installs the signed app at `appPath` through it, as one IPA
    /// (apparition_install_ipa, reusing `sourceIPA`'s unchanged files), or file by file
    /// (apparition_install) when `asIPA` is off.
    static func install(target: Target, hostname: String, file: PairingFile,
                        pin: PinCallback?, context: UnsafeMutableRawPointer, appPath: String,
                        sourceIPA: String? = nil, asIPA: Bool = true, detachAfterMs: UInt64 = 0) throws -> [String: Any] {
        try withTunnel(target, hostname: hostname, file: file, pin: pin, context: context) { adapter, handshake in
            guard asIPA else { return AccountModel.call(apparition_install(adapter, handshake, appPath)) }
            return AccountModel.call(apparition_install_ipa(adapter, handshake, appPath, sourceIPA, detachAfterMs))
        }
    }

    /// Opens a tunnel and lists the Apple Watches paired with the iPhone at its other end
    /// (apparition_paired_watches), which signing then registers for a watch app.
    static func pairedWatches(target: Target, hostname: String, file: PairingFile,
                              pin: PinCallback?, context: UnsafeMutableRawPointer) throws -> [String: Any] {
        try withTunnel(target, hostname: hostname, file: file, pin: pin, context: context) { adapter, handshake in
            AccountModel.call(apparition_paired_watches(adapter, handshake))
        }
    }

    /// A box for the callback apparition_install_watch_app makes while the Watch shows its
    /// trust prompt.
    final class WaitingBox {
        let waiting: @Sendable () -> Void
        init(_ waiting: @escaping @Sendable () -> Void) { self.waiting = waiting }
    }

    /// Opens a tunnel and installs the signed watch app at `appPath` on the Watch `watchUDID`
    /// through the iPhone (apparition_install_watch_app). `waiting` runs when the Watch is
    /// showing its trust prompt, the first time.
    static func installWatchApp(target: Target, hostname: String, file: PairingFile,
                                pin: PinCallback?, context: UnsafeMutableRawPointer,
                                watchUDID: String, recordDir: String, appPath: String,
                                waiting: @escaping @Sendable () -> Void) throws -> [String: Any] {
        try withTunnel(target, hostname: hostname, file: file, pin: pin, context: context) { adapter, handshake in
            let box = Unmanaged.passRetained(WaitingBox(waiting))
            defer { box.release() }
            return AccountModel.call(apparition_install_watch_app(adapter, handshake, watchUDID, recordDir, appPath, { raw in
                guard let raw else { return }
                Unmanaged<WaitingBox>.fromOpaque(raw).takeUnretainedValue().waiting()
            }, box.toOpaque()))
        }
    }

    /// Opens a tunnel and removes `bundleId` from the Watch `watchUDID` through the iPhone
    /// (apparition_uninstall_watch_app).
    static func uninstallWatchApp(target: Target, hostname: String, file: PairingFile,
                                  pin: PinCallback?, context: UnsafeMutableRawPointer,
                                  watchUDID: String, recordDir: String, bundleId: String,
                                  waiting: @escaping @Sendable () -> Void) throws -> [String: Any] {
        try withTunnel(target, hostname: hostname, file: file, pin: pin, context: context) { adapter, handshake in
            let box = Unmanaged.passRetained(WaitingBox(waiting))
            defer { box.release() }
            return AccountModel.call(apparition_uninstall_watch_app(adapter, handshake, watchUDID, recordDir, bundleId, { raw in
                guard let raw else { return }
                Unmanaged<WaitingBox>.fromOpaque(raw).takeUnretainedValue().waiting()
            }, box.toOpaque()))
        }
    }

    /// Opens a tunnel and lists every user app on the device (apparition_list_apps).
    static func listApps(target: Target, hostname: String, file: PairingFile,
                         pin: PinCallback?, context: UnsafeMutableRawPointer) throws -> [String: Any] {
        try withTunnel(target, hostname: hostname, file: file, pin: pin, context: context) { adapter, handshake in
            AccountModel.call(apparition_list_apps(adapter, handshake))
        }
    }

    /// Opens a tunnel and deletes `bundleId` and its data from the device (apparition_uninstall).
    static func uninstall(target: Target, hostname: String, file: PairingFile,
                          pin: PinCallback?, context: UnsafeMutableRawPointer, bundleId: String) throws -> [String: Any] {
        try withTunnel(target, hostname: hostname, file: file, pin: pin, context: context) { adapter, handshake in
            AccountModel.call(apparition_uninstall(adapter, handshake, bundleId))
        }
    }

    /// Opens a tunnel and copies the data of the installed app `from` to the installed app `to`
    /// (apparition_migrate_container).
    static func migrateContainer(target: Target, hostname: String, file: PairingFile,
                                 pin: PinCallback?, context: UnsafeMutableRawPointer,
                                 from: String, to: String) throws -> [String: Any] {
        try withTunnel(target, hostname: hostname, file: file, pin: pin, context: context) { adapter, handshake in
            AccountModel.call(apparition_migrate_container(adapter, handshake, from, to))
        }
    }

    /// Opens a tunnel and lists the device's running processes, apps first (apparition_processes).
    static func processes(target: Target, hostname: String, file: PairingFile,
                          pin: PinCallback?, context: UnsafeMutableRawPointer) throws -> [String: Any] {
        try withTunnel(target, hostname: hostname, file: file, pin: pin, context: context) { adapter, handshake in
            AccountModel.call(apparition_processes(adapter, handshake))
        }
    }

    /// Opens a tunnel and sends `signal` to `pid` (apparition_signal_process).
    static func signalProcess(target: Target, hostname: String, file: PairingFile,
                              pin: PinCallback?, context: UnsafeMutableRawPointer,
                              pid: UInt32, signal: UInt32) throws -> [String: Any] {
        try withTunnel(target, hostname: hostname, file: file, pin: pin, context: context) { adapter, handshake in
            AccountModel.call(apparition_signal_process(adapter, handshake, pid, signal))
        }
    }

    /// A box for the callback apparition_simulate_location makes each time the device has taken
    /// a coordinate.
    final class LocationBox {
        let onSet: @Sendable (Double, Double) -> Void
        init(_ onSet: @escaping @Sendable (Double, Double) -> Void) { self.onSet = onSet }
    }

    /// Opens a tunnel and simulates a location for the whole device until
    /// apparition_tool_stop(`key`) or the connection drops (apparition_simulate_location). Blocks
    /// for the whole session, with the tunnel open.
    static func simulateLocation(target: Target, hostname: String, file: PairingFile,
                                 pin: PinCallback?, context: UnsafeMutableRawPointer, key: String,
                                 latitude: Double, longitude: Double,
                                 onSet: @escaping @Sendable (Double, Double) -> Void) throws -> [String: Any] {
        try withTunnel(target, hostname: hostname, file: file, pin: pin, context: context) { adapter, handshake in
            let box = Unmanaged.passRetained(LocationBox(onSet))
            defer { box.release() }
            return AccountModel.call(apparition_simulate_location(adapter, handshake, key, latitude, longitude, { raw, latitude, longitude in
                guard let raw else { return }
                Unmanaged<LocationBox>.fromOpaque(raw).takeUnretainedValue().onSet(latitude, longitude)
            }, box.toOpaque()))
        }
    }

    /// A box for the callback apparition_stream_log makes with each batch of log lines (their JSON).
    final class LogBox {
        let onBatch: @Sendable (Data) -> Void
        init(_ onBatch: @escaping @Sendable (Data) -> Void) { self.onBatch = onBatch }
    }

    /// Opens a tunnel and streams the device's log (only `pid`'s when it isn't negative) until
    /// apparition_tool_stop(`key`) or the device ends it (apparition_stream_log). Blocks for the
    /// whole session, with the tunnel open.
    static func streamLog(target: Target, hostname: String, file: PairingFile,
                          pin: PinCallback?, context: UnsafeMutableRawPointer, key: String,
                          pid: Int64, minimumLevel: UInt32,
                          onBatch: @escaping @Sendable (Data) -> Void) throws -> [String: Any] {
        try withTunnel(target, hostname: hostname, file: file, pin: pin, context: context) { adapter, handshake in
            let box = Unmanaged.passRetained(LogBox(onBatch))
            defer { box.release() }
            return AccountModel.call(apparition_stream_log(adapter, handshake, key, pid, minimumLevel, { raw, json in
                guard let raw, let json else { return }
                // Copied out at once: the text is only valid during the call.
                let data = Data(bytes: json, count: strlen(json))
                Unmanaged<LogBox>.fromOpaque(raw).takeUnretainedValue().onBatch(data)
            }, box.toOpaque()))
        }
    }

    final class JSONEventBox {
        let receive: @Sendable (Data) -> Void
        init(_ receive: @escaping @Sendable (Data) -> Void) { self.receive = receive }
    }

    static func streamActivity(target: Target, hostname: String, file: PairingFile,
                               pin: PinCallback?, context: UnsafeMutableRawPointer, key: String,
                               onSample: @escaping @Sendable (Data) -> Void) throws -> [String: Any] {
        try withTunnel(target, hostname: hostname, file: file, pin: pin, context: context) { adapter, handshake in
            let box = Unmanaged.passRetained(JSONEventBox(onSample))
            defer { box.release() }
            return AccountModel.call(apparition_stream_activity(adapter, handshake, key, { raw, json in
                guard let raw, let json else { return }
                let data = Data(bytes: json, count: strlen(json))
                Unmanaged<JSONEventBox>.fromOpaque(raw).takeUnretainedValue().receive(data)
            }, box.toOpaque()))
        }
    }

    static func streamNetwork(target: Target, hostname: String, file: PairingFile,
                              pin: PinCallback?, context: UnsafeMutableRawPointer, key: String,
                              onEvent: @escaping @Sendable (Data) -> Void) throws -> [String: Any] {
        try withTunnel(target, hostname: hostname, file: file, pin: pin, context: context) { adapter, handshake in
            let box = Unmanaged.passRetained(JSONEventBox(onEvent))
            defer { box.release() }
            return AccountModel.call(apparition_stream_network(adapter, handshake, key, { raw, json in
                guard let raw, let json else { return }
                let data = Data(bytes: json, count: strlen(json))
                Unmanaged<JSONEventBox>.fromOpaque(raw).takeUnretainedValue().receive(data)
            }, box.toOpaque()))
        }
    }

    final class ConditionReadyBox {
        let ready: @Sendable () -> Void
        init(_ ready: @escaping @Sendable () -> Void) { self.ready = ready }
    }

    static func conditionSession(target: Target, hostname: String, file: PairingFile,
                                 pin: PinCallback?, context: UnsafeMutableRawPointer, key: String,
                                 condition: String, profile: String,
                                 onEnabled: @escaping @Sendable () -> Void) throws -> [String: Any] {
        try withTunnel(target, hostname: hostname, file: file, pin: pin, context: context) { adapter, handshake in
            let box = Unmanaged.passRetained(ConditionReadyBox(onEnabled))
            defer { box.release() }
            return AccountModel.call(apparition_condition_session(adapter, handshake, key, condition, profile, { raw in
                guard let raw else { return }
                Unmanaged<ConditionReadyBox>.fromOpaque(raw).takeUnretainedValue().ready()
            }, box.toOpaque()))
        }
    }

    static func conditionProfiles(target: Target, hostname: String, file: PairingFile,
                                  pin: PinCallback?, context: UnsafeMutableRawPointer) throws -> [String: Any] {
        try withTunnel(target, hostname: hostname, file: file, pin: pin, context: context) { adapter, handshake in
            AccountModel.call(apparition_condition_profiles(adapter, handshake))
        }
    }

    static func toolServiceReadiness(target: Target, hostname: String, file: PairingFile,
                                     pin: PinCallback?, context: UnsafeMutableRawPointer) throws -> [String: Any] {
        try withTunnel(target, hostname: hostname, file: file, pin: pin, context: context) { adapter, handshake in
            AccountModel.call(apparition_tool_service_readiness(adapter, handshake))
        }
    }

    static func captureSysdiagnose(target: Target, hostname: String, file: PairingFile,
                                   pin: PinCallback?, context: UnsafeMutableRawPointer,
                                   destination: String) throws -> [String: Any] {
        try withTunnel(target, hostname: hostname, file: file, pin: pin, context: context) { adapter, handshake in
            AccountModel.call(apparition_capture_sysdiagnose(adapter, handshake, destination))
        }
    }

    static func browseContainer(target: Target, hostname: String, file: PairingFile,
                                pin: PinCallback?, context: UnsafeMutableRawPointer,
                                bundleId: String, path: String) throws -> [String: Any] {
        try withTunnel(target, hostname: hostname, file: file, pin: pin, context: context) { adapter, handshake in
            AccountModel.call(apparition_container_browse(adapter, handshake, bundleId, path))
        }
    }

    static func exportContainerFile(target: Target, hostname: String, file: PairingFile,
                                    pin: PinCallback?, context: UnsafeMutableRawPointer,
                                    bundleId: String, devicePath: String, destination: String) throws -> [String: Any] {
        try withTunnel(target, hostname: hostname, file: file, pin: pin, context: context) { adapter, handshake in
            AccountModel.call(apparition_container_export_file(adapter, handshake, bundleId, devicePath, destination))
        }
    }

    static func exportContainerBackup(target: Target, hostname: String, file: PairingFile,
                                      pin: PinCallback?, context: UnsafeMutableRawPointer,
                                      bundleId: String, destination: String) throws -> [String: Any] {
        try withTunnel(target, hostname: hostname, file: file, pin: pin, context: context) { adapter, handshake in
            AccountModel.call(apparition_container_export_backup(adapter, handshake, bundleId, destination))
        }
    }

    static func restoreContainerBackup(target: Target, hostname: String, file: PairingFile,
                                       pin: PinCallback?, context: UnsafeMutableRawPointer,
                                       bundleId: String, archive: String) throws -> [String: Any] {
        try withTunnel(target, hostname: hostname, file: file, pin: pin, context: context) { adapter, handshake in
            AccountModel.call(apparition_container_restore_backup(adapter, handshake, bundleId, archive))
        }
    }

    /// Opens a tunnel and reads the device's UDID and name from lockdown. Pair-verify doesn't
    /// return them (only pair-setup does), and signing needs the UDID.
    static func identify(target: Target, hostname: String, file: PairingFile,
                         pin: PinCallback?, context: UnsafeMutableRawPointer) throws -> PeerDevice {
        try withTunnel(target, hostname: hostname, file: file, pin: pin, context: context) { adapter, handshake in
            var client: OpaquePointer?
            try check(lockdownd_connect_rsd(adapter, handshake, &client))
            defer { if let client { lockdownd_client_free(client) } }
            func value(_ key: String) throws -> String {
                var node: plist_t?
                try check(lockdownd_get_value(client, key, nil, &node))
                guard let node else { return "" }
                defer { plist_free(node) }
                var text: UnsafeMutablePointer<CChar>?
                plist_get_string_val(node, &text)
                defer { if let text { plist_mem_free(text) } }
                return text.map { String(cString: $0) } ?? ""
            }
            return PeerDevice(name: try value("DeviceName"), model: try value("ProductType"), udid: try value("UniqueDeviceID"))
        }
    }

    /// Pair-verify (pairing first if needed), open the CDTunnel and run the RSD handshake.
    /// Returns the device UUID and its RSD service list; the tunnel is closed afterwards.
    static func tunnelServices(target: Target, hostname: String, file: PairingFile,
                               pin: PinCallback?, context: UnsafeMutableRawPointer) throws -> (uuid: String, services: [Service]) {
        try withTunnel(target, hostname: hostname, file: file, pin: pin, context: context) { adapter, handshake in
            guard let handshake else { throw ProbeError("tunnel_create_rppairing returned no handshake") }

            var uuidPointer: UnsafeMutablePointer<CChar>?
            try check(rsd_get_uuid(handshake, &uuidPointer))
            let uuid = uuidPointer.map { String(cString: $0) } ?? ""
            if let uuidPointer { rsd_free_string(uuidPointer) }

            var array: UnsafeMutablePointer<CRsdServiceArray>?
            try check(rsd_get_services(handshake, &array))
            guard let array else { return (uuid, []) }
            defer { rsd_free_services(array) }
            let services = (0..<Int(array.pointee.count)).map { index in
                let service = array.pointee.services[index]
                return Service(name: String(cString: service.name), port: Int(service.port),
                               usesRemoteXPC: service.uses_remote_xpc)
            }
            return (uuid, services.sorted { $0.name < $1.name })
        }
    }
}
