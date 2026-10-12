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

    static func startLogging(to url: URL) {
        url.path.withCString { path in
            _ = idevice_init_logger(Warn, Debug, UnsafeMutablePointer(mutating: path))
        }
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
    static func withTunnel<T>(_ target: Target, hostname: String, file: PairingFile, pin: PinCallback,
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

    /// Opens a tunnel, launches the app under the debugger and serves its JIT requests.
    static func launchJIT(target: Target, hostname: String, file: PairingFile,
                          pin: PinCallback, context: UnsafeMutableRawPointer,
                          bundleId: String, waitSeconds: UInt32) throws -> [String: Any] {
        try withTunnel(target, hostname: hostname, file: file, pin: pin, context: context) { adapter, handshake in
            return AccountModel.call(apparition_launch_jit(adapter, handshake, bundleId, waitSeconds))
        }
    }

    /// Opens a tunnel and installs the signed app at `appPath` through it, as one IPA
    /// (apparition_install_ipa, reusing `sourceIPA`'s unchanged files), or file by file
    /// (apparition_install) when `asIPA` is off.
    static func install(target: Target, hostname: String, file: PairingFile,
                        pin: PinCallback, context: UnsafeMutableRawPointer, appPath: String,
                        sourceIPA: String? = nil, asIPA: Bool = true, detachAfterMs: UInt64 = 0) throws -> [String: Any] {
        try withTunnel(target, hostname: hostname, file: file, pin: pin, context: context) { adapter, handshake in
            guard asIPA else { return AccountModel.call(apparition_install(adapter, handshake, appPath)) }
            return AccountModel.call(apparition_install_ipa(adapter, handshake, appPath, sourceIPA, detachAfterMs))
        }
    }

    /// Opens a tunnel and lists every user app on the device (apparition_list_apps).
    static func listApps(target: Target, hostname: String, file: PairingFile,
                         pin: PinCallback, context: UnsafeMutableRawPointer) throws -> [String: Any] {
        try withTunnel(target, hostname: hostname, file: file, pin: pin, context: context) { adapter, handshake in
            AccountModel.call(apparition_list_apps(adapter, handshake))
        }
    }

    /// Opens a tunnel and reads the device's UDID and name from lockdown. Pair-verify doesn't
    /// return them (only pair-setup does), and signing needs the UDID.
    static func identify(target: Target, hostname: String, file: PairingFile,
                         pin: PinCallback, context: UnsafeMutableRawPointer) throws -> PeerDevice {
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
                               pin: PinCallback, context: UnsafeMutableRawPointer) throws -> (uuid: String, services: [Service]) {
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
