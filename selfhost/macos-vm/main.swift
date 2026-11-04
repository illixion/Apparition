// Development-only macOS 26 VM runner; not part of release packaging.
// See README.md for create, install, run and recovery setup commands.
import AppKit
import Foundation
import Network
import ScreenCaptureKit
import Vision
@preconcurrency import Virtualization

func fail(_ msg: String) -> Never {
    FileHandle.standardError.write(Data((msg + "\n").utf8))
    exit(1)
}

struct Bundle_ {
    let dir: URL
    var disk: URL { dir.appendingPathComponent("disk.img") }
    var aux: URL { dir.appendingPathComponent("aux.img") }
    var hwModel: URL { dir.appendingPathComponent("hardware-model") }
    var machineID: URL { dir.appendingPathComponent("machine-identifier") }
}

/// How big one VM is, kept in the bundle's config.json so it stays the same between boots. The
/// APPARITION_VM_MEMORY_MB / _CPUS / _DISPLAY=WxH / _PPI environment variables override it, for experiments.
struct VMSettings: Codable {
    var memoryMB: Int
    var cpus: Int
    var displayWidth: Int
    var displayHeight: Int
    var ppi: Int
    var audio: Bool

    /// The appliance needs 1.5 GiB: macOS 26 doesn't boot in 1.25 GiB, and once its desktop services are trimmed
    /// (appliance-trim.sh) 1.5 GiB runs with no swap. A Mac with 8 GB gets that floor, one with more gets headroom.
    static func defaults() -> VMSettings {
        let small = ProcessInfo.processInfo.physicalMemory <= 9 << 30
        return VMSettings(memoryMB: small ? 1536 : 2048, cpus: 2, displayWidth: 1920, displayHeight: 1200, ppi: 144, audio: true)
    }

    static func url(_ b: Bundle_) -> URL { b.dir.appendingPathComponent("config.json") }

    static func load(_ b: Bundle_) -> VMSettings {
        var settings = defaults()
        if let data = try? Data(contentsOf: url(b)) {
            guard let saved = try? JSONDecoder().decode(VMSettings.self, from: data) else { fail("unreadable \(url(b).path)") }
            settings = saved
        }
        let env = ProcessInfo.processInfo.environment
        if let value = env["APPARITION_VM_MEMORY_MB"].flatMap(Int.init) { settings.memoryMB = value }
        if let value = env["APPARITION_VM_CPUS"].flatMap(Int.init) { settings.cpus = value }
        if let value = env["APPARITION_VM_PPI"].flatMap(Int.init) { settings.ppi = value }
        if let size = env["APPARITION_VM_DISPLAY"]?.split(separator: "x").compactMap({ Int($0) }), size.count == 2 {
            settings.displayWidth = size[0]; settings.displayHeight = size[1]
        }
        return settings
    }

    func save(_ b: Bundle_) throws {
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
        try encoder.encode(self).write(to: Self.url(b), options: .atomic)
    }
}

func makeConfig(_ b: Bundle_, hardwareModel: VZMacHardwareModel, machineID: VZMacMachineIdentifier,
                aux: VZMacAuxiliaryStorage, network: Bool = false, settings: VMSettings) throws -> VZVirtualMachineConfiguration {
    let c = VZVirtualMachineConfiguration()
    let platform = VZMacPlatformConfiguration()
    platform.hardwareModel = hardwareModel
    platform.machineIdentifier = machineID
    platform.auxiliaryStorage = aux
    c.platform = platform
    let statusDirectory = b.dir.appendingPathComponent("guest-status")
    try FileManager.default.createDirectory(at: statusDirectory, withIntermediateDirectories: true)
    c.socketDevices = [VZVirtioSocketDeviceConfiguration()]
    c.bootLoader = VZMacOSBootLoader()
    c.cpuCount = max(settings.cpus, VZVirtualMachineConfiguration.minimumAllowedCPUCount)
    c.memorySize = UInt64(settings.memoryMB) << 20
    print("config: \(c.cpuCount) cpus, \(c.memorySize >> 20) MiB")

    let disk = try VZDiskImageStorageDeviceAttachment(url: b.disk, readOnly: false)
    c.storageDevices = [VZVirtioBlockDeviceConfiguration(attachment: disk)]

    let net = VZVirtioNetworkDeviceConfiguration()
    let addressFile = b.dir.appendingPathComponent("mac-address")
    if FileManager.default.fileExists(atPath: addressFile.path) {
        let text = try String(contentsOf: addressFile, encoding: .utf8).trimmingCharacters(in: .whitespacesAndNewlines)
        guard let address = VZMACAddress(string: text) else { fail("invalid saved MAC address") }
        net.macAddress = address
    } else {
        try net.macAddress.string.write(to: addressFile, atomically: true, encoding: .utf8)
    }
    net.attachment = VZNATNetworkDeviceAttachment()
    c.networkDevices = [net]

    let gfx = VZMacGraphicsDeviceConfiguration()
    gfx.displays = [VZMacGraphicsDisplayConfiguration(widthInPixels: settings.displayWidth, heightInPixels: settings.displayHeight,
                                                      pixelsPerInch: settings.ppi)]
    c.graphicsDevices = [gfx]

    c.keyboards = [VZMacKeyboardConfiguration()]
    c.pointingDevices = [VZMacTrackpadConfiguration()]

    if settings.audio {
        let audio = VZVirtioSoundDeviceConfiguration()
        let out = VZVirtioSoundDeviceOutputStreamConfiguration()
        out.sink = VZHostAudioOutputStreamSink()
        audio.streams = [out]
        c.audioDevices = [audio]
    }

    try c.validate()
    return c
}

// MARK: install

func create(ipsw: URL, dir: URL, diskGB: Int) async throws {
    let b = Bundle_(dir: dir)
    guard diskGB >= 40, diskGB <= 1024 else { fail("diskGB must be 40...1024") }
    guard !FileManager.default.fileExists(atPath: dir.path) else { fail("bundle already exists; refusing to overwrite") }
    print("loading restore image…")
    let image = try await VZMacOSRestoreImage.image(from: ipsw)
    print("image version \(image.operatingSystemVersion) build \(image.buildVersion) supported=\(image.isSupported)")
    guard let req = image.mostFeaturefulSupportedConfiguration else {
        fail("host can't run this image (no supported configuration)")
    }
    guard image.operatingSystemVersion.majorVersion == 26 else { fail("only macOS 26 is supported") }
    try FileManager.default.createDirectory(at: dir.deletingLastPathComponent(), withIntermediateDirectories: true)
    guard mkdir(dir.path, 0o700) == 0 else { fail("could not create a new bundle; refusing to overwrite") }
    try image.buildVersion.write(to: dir.appendingPathComponent("restore-build"), atomically: true, encoding: .utf8)
    let hw = req.hardwareModel
    let mid = VZMacMachineIdentifier()
    try hw.dataRepresentation.write(to: b.hwModel)
    try mid.dataRepresentation.write(to: b.machineID)

    // sparse disk image
    let fd = open(b.disk.path, O_RDWR | O_CREAT | O_EXCL, 0o600)
    guard fd >= 0, ftruncate(fd, off_t(diskGB) * 1024 * 1024 * 1024) == 0 else { fail("disk create failed") }
    close(fd)

    let aux = try VZMacAuxiliaryStorage(creatingStorageAt: b.aux, hardwareModel: hw, options: [.allowOverwrite])
    let settings = VMSettings.defaults()
    try settings.save(b)
    _ = try makeConfig(b, hardwareModel: hw, machineID: mid, aux: aux, settings: settings)
    print("CREATED \(dir.path)")
}

func configuration(_ dir: URL, network: Bool = false, minimumMemoryMB: Int = 0) throws -> VZVirtualMachineConfiguration {
    let b = Bundle_(dir: dir)
    var settings = VMSettings.load(b)
    settings.memoryMB = max(settings.memoryMB, minimumMemoryMB)
    guard let hw = VZMacHardwareModel(dataRepresentation: try Data(contentsOf: b.hwModel)),
          let mid = VZMacMachineIdentifier(dataRepresentation: try Data(contentsOf: b.machineID))
    else { fail("bad bundle") }
    return try makeConfig(b, hardwareModel: hw, machineID: mid,
                          aux: VZMacAuxiliaryStorage(url: b.aux), network: network, settings: settings)
}

@MainActor func install(ipsw: URL, dir: URL) async throws {
    let image = try await VZMacOSRestoreImage.image(from: ipsw)
    guard image.operatingSystemVersion.majorVersion == 26 else { fail("only macOS 26 is supported") }
    guard !FileManager.default.fileExists(atPath: dir.appendingPathComponent("installed").path) else {
        fail("already installed; refusing to erase this VM")
    }
    let expected = try String(contentsOf: dir.appendingPathComponent("restore-build"), encoding: .utf8)
    guard image.buildVersion == expected else { fail("IPSW build differs from the bundle's restore image") }
    let lock = open(dir.appendingPathComponent(".lock").path, O_RDWR | O_CREAT, 0o600)
    guard lock >= 0, flock(lock, LOCK_EX | LOCK_NB) == 0 else { fail("bundle is already in use") }
    defer { close(lock) }
    // Restoring the OS is the one heavy step: give it 4 GiB, whatever the finished appliance uses.
    let config = try configuration(dir, minimumMemoryMB: 4096)

    let vm = VZVirtualMachine(configuration: config)
    let installer = VZMacOSInstaller(virtualMachine: vm, restoringFromImageAt: ipsw)
    let obs = installer.progress.observe(\.fractionCompleted, options: [.new]) { p, _ in
        print(String(format: "install %.1f%%", p.fractionCompleted * 100))
    }
    print("installing…")
    try await withCheckedThrowingContinuation { (k: CheckedContinuation<Void, Error>) in
        DispatchQueue.main.async {
            installer.install { r in
                switch r { case .success: k.resume(); case .failure(let e): k.resume(throwing: e) }
            }
        }
    }
    obs.invalidate()
    try Data().write(to: dir.appendingPathComponent("installed"))
    print("INSTALL OK")
}

// MARK: run

final class Delegate: NSObject, VZVirtualMachineDelegate, NSApplicationDelegate {
    func guestDidStop(_ vm: VZVirtualMachine) { print("guest stopped"); exit(0) }
    func virtualMachine(_ vm: VZVirtualMachine, didStopWithError e: Error) { print("stopped with error: \(e)"); exit(2) }
    func virtualMachine(_ vm: VZVirtualMachine, networkDevice: VZNetworkDevice, attachmentWasDisconnectedWithError error: Error) {
        print("NETWORK ERROR: \(error)")
    }
    func applicationShouldTerminateAfterLastWindowClosed(_ app: NSApplication) -> Bool { true }
}

// A listener belongs to this VM's vsock device; another VM cannot impersonate it.
final class GuestChannel: @unchecked Sendable {
    // POSIX I/O happens off the VM queue; VZ connection.close stays on the main queue.
    let connection: VZVirtioSocketConnection
    init(_ connection: VZVirtioSocketConnection) { self.connection = connection }
}
final class GuestStatus: NSObject, VZVirtioSocketListenerDelegate, @unchecked Sendable {
    let directory: URL
    private let lock = NSLock()
    private var command: UInt8 = 78
    init(directory: URL) { self.directory = directory }
    func requestShutdown() { lock.lock(); command = 83; lock.unlock() }
    func listener(_ listener: VZVirtioSocketListener, shouldAcceptNewConnection connection: VZVirtioSocketConnection,
                  from socketDevice: VZVirtioSocketDevice) -> Bool {
        let channel = GuestChannel(connection)
        let fd = connection.fileDescriptor
        DispatchQueue.global(qos: .utility).async { [self, channel] in
            defer { DispatchQueue.main.async { channel.connection.close() } }
            var timeout = timeval(tv_sec: 5, tv_usec: 0)
            setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, socklen_t(MemoryLayout<timeval>.size))
            var data = Data()
            var buffer = [UInt8](repeating: 0, count: 1024)
            while data.count < 8192 {
                let count = read(fd, &buffer, buffer.count)
                if count <= 0 { break }
                data.append(contentsOf: buffer.prefix(count))
                if data.contains(10) { break }
            }
            guard data.count < 8192,
                  var report = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any],
                  let boot = try? String(contentsOf: directory.appendingPathComponent("boot-id"), encoding: .utf8) else { return }
            report["boot"] = boot
            guard let encoded = try? JSONSerialization.data(withJSONObject: report) else { return }
            try? encoded.write(to: directory.appendingPathComponent("report.json"), options: .atomic)
            lock.lock(); var reply = command; command = 78; lock.unlock()
            _ = write(fd, &reply, 1)
        }
        return true
    }
}

// Host-only lifecycle control. It does not inject input or execute guest code.
// Bind to loopback and require a per-run token kept in a mode-600 bundle file.
final class ControlServer {
    let listener: NWListener
    let vm: VZVirtualMachine
    weak var view: VZVirtualMachineView?
    var guestStatus: GuestStatus?
    let networkAttachment: VZNetworkDeviceAttachment?
    let recovery: Bool
    let token = UUID().uuidString
    let file: URL
    init(vm: VZVirtualMachine, dir: URL, recovery: Bool) throws {
        self.vm = vm
        self.recovery = recovery
        networkAttachment = vm.networkDevices.first?.attachment
        file = dir.appendingPathComponent("control.json")
        let parameters = NWParameters.tcp
        parameters.requiredLocalEndpoint = .hostPort(host: "127.0.0.1", port: .any)
        listener = try NWListener(using: parameters)
        listener.stateUpdateHandler = { [weak self] state in
            guard let self, case .ready = state, let port = self.listener.port else { return }
            do {
                let data = try JSONSerialization.data(withJSONObject: ["port": Int(port.rawValue), "token": self.token])
                try data.write(to: self.file, options: .atomic)
                try FileManager.default.setAttributes([.posixPermissions: 0o600], ofItemAtPath: self.file.path)
            } catch { fail("control endpoint: \(error)") }
        }
        listener.newConnectionHandler = { [weak self] connection in
            connection.start(queue: .main)
            self?.receive(connection, data: Data())
        }
        listener.start(queue: .main)
    }
    enum InputError: Error { case invalid }
    // Physical ANSI keycodes; provisioned guest keyboard layout is ABC.
    func stroke(_ character: Character) throws -> (UInt16, NSEvent.ModifierFlags) {
        let mapping: [Character: UInt16] = ["a": 0, "s": 1, "d": 2, "f": 3, "h": 4, "g": 5, "z": 6, "x": 7, "c": 8, "v": 9, "b": 11, "q": 12, "w": 13, "e": 14, "r": 15, "y": 16, "t": 17, "1": 18, "2": 19, "3": 20, "4": 21, "6": 22, "5": 23, "=": 24, "9": 25, "7": 26, "-": 27, "8": 28, "0": 29, "]": 30, "o": 31, "u": 32, "[": 33, "i": 34, "p": 35, "\r": 36, "l": 37, "j": 38, "'": 39, "k": 40, ";": 41, "\\": 42, ",": 43, "/": 44, "n": 45, "m": 46, ".": 47, "\t": 48, "`": 50]
        if character == " " { return (49, []) }
        if character == "\n" { return (36, []) }
        if let code = mapping[character] { return (code, []) }
        let shifted: [Character: Character] = ["!": "1", "@": "2", "#": "3", "$": "4", "%": "5", "^": "6", "&": "7", "*": "8", "(": "9", ")": "0", "_": "-", "+": "=", "{": "[", "}": "]", "|": "\\", ":": ";", "\"": "'", "<": ",", ">": ".", "?": "/", "~": "`"]
        if let base = shifted[character], let code = mapping[base] { return (code, .shift) }
        let lower = String(character).lowercased()
        if String(character) != lower, lower.count == 1, let code = mapping[Character(lower)] { return (code, .shift) }
        throw InputError.invalid
    }
    func key(_ specification: String) throws {
        var parts = specification.lowercased().split(separator: "+").map(String.init)
        guard let key = parts.popLast() else { throw InputError.invalid }
        var flags: NSEvent.ModifierFlags = []
        for part in parts {
            switch part {
            case "cmd": flags.insert(.command)
            case "shift": flags.insert(.shift)
            case "ctrl": flags.insert(.control)
            case "alt", "option": flags.insert(.option)
            default: throw InputError.invalid
            }
        }
        let named: [String: UInt16] = ["return":36, "tab":48, "space":49, "delete":51, "escape":53,
                                      "left":123, "right":124, "down":125, "up":126]
        if let code = named[key] { try send(code: code, flags: flags) }
        else if key.count == 1 {
            let stroke = try stroke(Character(key))
            try send(code: stroke.0, flags: flags.union(stroke.1))
        } else { throw InputError.invalid }
    }
    func send(code: UInt16, flags: NSEvent.ModifierFlags) throws {
        guard let view, let window = view.window, vm.state == .running else { throw InputError.invalid }
        // Each modifier: its generic flag, its left-hand key code, and the device-dependent bit
        // (NX_DEVICEL*KEYMASK). VZ decides whether a flagsChanged event is a press or a release by
        // testing only the device bit, so without it every modifier press reaches the guest as a release.
        let modifiers: [(NSEvent.ModifierFlags, UInt16, UInt)] = [(.control, 59, 0x1), (.option, 58, 0x20),
                                                                  (.shift, 56, 0x2), (.command, 55, 0x8)]
        var active: UInt = 0
        func event(_ type: NSEvent.EventType, _ key: UInt16) -> NSEvent {
            NSEvent.keyEvent(with: type, location: .zero, modifierFlags: NSEvent.ModifierFlags(rawValue: active),
                             timestamp: ProcessInfo.processInfo.systemUptime,
                             windowNumber: window.windowNumber, context: nil, characters: "",
                             charactersIgnoringModifiers: "", isARepeat: false, keyCode: key)!
        }
        for (flag, key, device) in modifiers where flags.contains(flag) {
            active |= flag.rawValue | device
            view.flagsChanged(with: event(.flagsChanged, key))
        }
        view.keyDown(with: event(.keyDown, code))
        view.keyUp(with: event(.keyUp, code))
        for (flag, key, device) in modifiers.reversed() where flags.contains(flag) {
            active &= ~(flag.rawValue | device)
            view.flagsChanged(with: event(.flagsChanged, key))
        }
    }

    /// Click at a fraction (x, y; origin top-left) of the whole VM window, as `screen` reports it.
    func click(x: Double, y: Double) throws {
        guard let view, let window = view.window, vm.state == .running, (0...1).contains(x), (0...1).contains(y) else { throw InputError.invalid }
        let point = NSPoint(x: x * window.frame.width, y: (1 - y) * window.frame.height)
        func mouse(_ type: NSEvent.EventType) -> NSEvent {
            NSEvent.mouseEvent(with: type, location: point, modifierFlags: [],
                               timestamp: ProcessInfo.processInfo.systemUptime, windowNumber: window.windowNumber,
                               context: nil, eventNumber: 0, clickCount: 1, pressure: type == .leftMouseUp ? 0 : 1)!
        }
        view.mouseMoved(with: mouse(.mouseMoved))
        Thread.sleep(forTimeInterval: 0.1)
        view.mouseDown(with: mouse(.leftMouseDown))
        Thread.sleep(forTimeInterval: 0.08)
        view.mouseUp(with: mouse(.leftMouseUp))
    }

    func respond(_ connection: NWConnection, _ reply: [String: Any]) {
        let out = (try? JSONSerialization.data(withJSONObject: reply)) ?? Data()
        connection.send(content: out + Data([10]), completion: .contentProcessed { _ in connection.cancel() })
    }
    struct Word { let text: String; let box: CGRect }  // box: fractions of the window, origin top-left

    /// Vision text recognition, one entry per word. `.accurate` costs ~0.1 s a frame but keeps
    /// adjacent menu titles apart and reads small grey labels that `.fast` merges or drops.
    nonisolated static func recognize(_ image: CGImage) throws -> [(line: String, words: [Word])] {
        let request = VNRecognizeTextRequest()
        request.recognitionLevel = .accurate
        request.recognitionLanguages = ["en-US"]
        try VNImageRequestHandler(cgImage: image).perform([request])
        return (request.results ?? []).compactMap { observation in
            guard let candidate = observation.topCandidates(1).first else { return nil }
            let line = candidate.string
            var words: [Word] = []
            line.enumerateSubstrings(in: line.startIndex..., options: .byWords) { word, range, _, _ in
                guard let word, let r = try? candidate.boundingBox(for: range)?.boundingBox else { return }
                words.append(Word(text: word, box: CGRect(x: r.minX, y: 1 - r.maxY, width: r.width, height: r.height)))
            }
            return (line, words)
        }
    }

    /// Find `text` (one or more words, case-insensitive) on screen and click its centre.
    @MainActor func clickText(_ text: String, menuBarOnly: Bool) async throws -> [String: Any] {
        guard let number = view?.window?.windowNumber else { throw InputError.invalid }
        let image = try await capture(number)
        let words = try await Task.detached(priority: .userInitiated) { try Self.recognize(image) }.value
            .flatMap { $0.words }.filter { !menuBarOnly || $0.box.midY < 0.08 }
        let wanted = text.lowercased().split(separator: " ").map(String.init)
        guard !wanted.isEmpty else { throw InputError.invalid }
        for start in words.indices where start + wanted.count <= words.count {
            let run = Array(words[start..<start + wanted.count])
            guard zip(run, wanted).allSatisfy({ $0.text.lowercased() == $1 }) else { continue }
            let box = run.dropFirst().reduce(run[0].box) { $0.union($1.box) }
            try click(x: box.midX, y: box.midY)
            return ["ok": true, "x": box.midX, "y": box.midY]
        }
        return ["ok": false, "error": "text not on screen: \(text)"]
    }

    /// This VM window only, at 2x, via ScreenCaptureKit.
    @MainActor func capture(_ number: Int) async throws -> CGImage {
        let content = try await SCShareableContent.excludingDesktopWindows(false, onScreenWindowsOnly: false)
        guard let window = content.windows.first(where: { $0.windowID == CGWindowID(number) && $0.owningApplication?.processID == getpid() })
        else { throw InputError.invalid }
        // Capture at the display's real scale: asking a 1x display for 2x draws the window into the
        // top-left quarter, which throws off every position. Then upscale 1x frames for OCR.
        let scale = view?.window?.backingScaleFactor ?? 2
        let settings = SCStreamConfiguration()
        settings.width = Int(window.frame.width * scale)
        settings.height = Int(window.frame.height * scale)
        settings.showsCursor = false
        let image = try await SCScreenshotManager.captureImage(contentFilter: SCContentFilter(desktopIndependentWindow: window), configuration: settings)
        guard scale < 2 else { return image }
        let width = Int(window.frame.width * 2), height = Int(window.frame.height * 2)
        guard let context = CGContext(data: nil, width: width, height: height, bitsPerComponent: 8, bytesPerRow: 0,
                                      space: CGColorSpaceCreateDeviceRGB(), bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)
        else { return image }
        context.interpolationQuality = .high
        context.draw(image, in: CGRect(x: 0, y: 0, width: width, height: height))
        return context.makeImage() ?? image
    }

    @MainActor func screen(recognize: Bool) async throws -> [String: Any] {
        guard let number = view?.window?.windowNumber else { throw InputError.invalid }
        let cg = try await capture(number)
        let path = file.deletingLastPathComponent().appendingPathComponent("screen.png")
        guard let png = NSBitmapImageRep(cgImage: cg).representation(using: .png, properties: [:]) else { throw InputError.invalid }
        try png.write(to: path, options: .atomic)
        if !recognize { return ["ok": true, "path": path.path] }
        let lines = try await Task.detached(priority: .userInitiated) { try Self.recognize(cg) }.value
        return ["ok": true, "path": path.path, "text": lines.map { $0.line }.joined(separator: "\n"),
                "items": lines.flatMap { $0.words }.map { ["text": $0.text, "x": $0.box.midX, "y": $0.box.midY,
                                                         "w": $0.box.width, "h": $0.box.height] }]
    }

    func receive(_ connection: NWConnection, data: Data) {
        connection.receive(minimumIncompleteLength: 1, maximumLength: 8192) { [weak self] chunk, _, complete, error in
            guard let self else { connection.cancel(); return }
            var data = data
            if let chunk { data.append(chunk) }
            guard data.count <= 8192, error == nil else { connection.cancel(); return }
            if !data.contains(10) {
                if complete { connection.cancel() } else { self.receive(connection, data: data) }
                return
            }
            var reply: [String: Any] = ["ok": false]
            do {
                guard let request = try JSONSerialization.jsonObject(with: data) as? [String: String],
                      request["token"] == self.token else { throw NSError(domain: "control", code: 1) }
                switch request["operation"] {
                case "status": break
                case "screen", "capture":
                    Task { @MainActor in
                        do {
                            let result = try await self.screen(recognize: request["operation"] == "screen")
                            self.respond(connection, result)
                        } catch {
                            self.respond(connection, ["ok": false, "error": "Own-window capture failed; allow Screen Recording for the runner if requested."])
                        }
                    }
                    return
                case "key":
                    guard let key = request["key"] else { throw InputError.invalid }
                    try self.key(key)
                case "click-text":
                    guard let text = request["text"] else { throw InputError.invalid }
                    Task { @MainActor in
                        let reply = (try? await self.clickText(text, menuBarOnly: request["region"] == "menubar"))
                            ?? ["ok": false, "error": "Own-window capture failed; allow Screen Recording for the runner if requested."]
                        self.respond(connection, reply)
                    }
                    return
                case "click":
                    guard let x = request["x"].flatMap(Double.init), let y = request["y"].flatMap(Double.init) else { throw InputError.invalid }
                    try self.click(x: x, y: y)
                case "type":
                    guard let text = request["text"], text.utf8.count <= 2048 else { throw InputError.invalid }
                    // Validate the whole string before sending any events.
                    let strokes = try text.map { try self.stroke($0) }
                    for stroke in strokes { try self.send(code: stroke.0, flags: stroke.1); Thread.sleep(forTimeInterval: 0.01) }
                case "network-on": self.vm.networkDevices.first?.attachment = self.networkAttachment
                case "network-off": self.vm.networkDevices.first?.attachment = nil
                case "shutdown":
                    if self.vm.state == .stopped { DispatchQueue.main.asyncAfter(deadline: .now() + 0.1) { exit(0) } }
                    else { self.guestStatus?.requestShutdown() }
                case "force-stop":
                    if self.vm.state == .stopped { DispatchQueue.main.asyncAfter(deadline: .now() + 0.1) { exit(0) } }
                    else { self.vm.stop { error in
                        if let error { print("STOP ERROR: \(error)") } else { exit(0) }
                    } }
                default: throw NSError(domain: "control", code: 2)
                }
                reply = ["ok": true, "state": self.vm.state.rawValue,
                         "network": self.vm.networkDevices.first?.attachment != nil, "running": self.vm.state == .running, "recovery": self.recovery]
            } catch { reply["error"] = "command failed: \(error.localizedDescription)" }
            self.respond(connection, reply)
        }
    }
}

/// `background`: an automated run. It is an accessory app (no Dock icon, never active) whose window stays behind
/// the others, so a setup that takes minutes does not steal the keyboard from whoever is at the Mac.
@MainActor func run(dir: URL, network: Bool, recovery: Bool, background: Bool = false) throws {
    let lock = open(dir.appendingPathComponent(".lock").path, O_RDWR | O_CREAT, 0o600)
    guard lock >= 0, flock(lock, LOCK_EX | LOCK_NB) == 0 else { fail("bundle is already in use") }
    defer { close(lock) }
    // recoveryOS runs from a RAM disk and shows a black screen forever in 2 GiB (and in 1.5), so recovery gets the same
    // 4 GiB as the install; the appliance itself only needs its small size.
    let config = try configuration(dir, network: network, minimumMemoryMB: recovery ? 4096 : 0)
    let bootID = dir.appendingPathComponent("guest-status/boot-id")
    try UUID().uuidString.write(to: bootID, atomically: true, encoding: .utf8)
    try FileManager.default.setAttributes([.posixPermissions: 0o644], ofItemAtPath: bootID.path)
    let vm = VZVirtualMachine(configuration: config)
    let delegate = Delegate()
    vm.delegate = delegate
    let control = try ControlServer(vm: vm, dir: dir, recovery: recovery)
    let status = GuestStatus(directory: dir.appendingPathComponent("guest-status"))
    control.guestStatus = status
    let statusListener = VZVirtioSocketListener()
    statusListener.delegate = status
    (vm.socketDevices.first as? VZVirtioSocketDevice)?.setSocketListener(statusListener, forPort: 3939)

    let app = NSApplication.shared
    app.setActivationPolicy(background ? .accessory : .regular)
    app.delegate = delegate
    let view = VZVirtualMachineView()
    view.virtualMachine = vm
    control.view = view
    view.capturesSystemKeys = true
    // A fixed display (APPARITION_VM_DISPLAY) keeps its size; otherwise the guest display follows the window.
    view.automaticallyReconfiguresDisplay = ProcessInfo.processInfo.environment["APPARITION_VM_DISPLAY"] == nil
    let win = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 1280, height: 800),
                       styleMask: [.titled, .closable, .resizable, .miniaturizable],
                       backing: .buffered, defer: false)
    win.title = recovery ? "Apparition — Recovery" : "Apparition macOS 26"
    win.contentView = view
    win.makeFirstResponder(view)
    if background {
        win.orderBack(nil)
    } else {
        win.makeKeyAndOrderFront(nil)
        app.activate(ignoringOtherApps: true)
    }
    let options = VZMacOSVirtualMachineStartOptions()
    options.startUpFromMacOSRecovery = recovery
    vm.start(options: options) { error in
        if let error { fail("start failed: \(error)") }
        if !network { vm.networkDevices.first?.attachment = nil }
        print("VM STARTED")
    }
    withExtendedLifetime((control, status, statusListener)) { app.run() }
}

// MARK: main

umask(0o077)
setvbuf(stdout, nil, _IOLBF, 0)
let args = CommandLine.arguments
let usage = "usage: vm create <ipsw> <new-bundle> [diskGB] | install <ipsw> <bundle> | run <bundle> --network=off|on [--background] | setup <bundle> [--background]"
guard args.count >= 3 else { fail(usage) }
switch args[1] {
case "create", "install":
    guard args.count >= 4, args.count <= (args[1] == "create" ? 5 : 4) else { fail(usage) }
    let gb: Int
    if args.count == 5 { guard let value = Int(args[4]) else { fail("invalid diskGB") }; gb = value }
    else { gb = 64 }
    Task {
        do {
            let ipsw = URL(fileURLWithPath: args[2])
            let dir = URL(fileURLWithPath: args[3])
            if args[1] == "create" { try await create(ipsw: ipsw, dir: dir, diskGB: gb) }
            else { try await install(ipsw: ipsw, dir: dir) }
            exit(0)
        } catch { fail("\(args[1]) failed: \(error)") }
    }
    RunLoop.main.run()
case "run", "setup":
    let recovery = args[1] == "setup"
    let background = args.last == "--background"
    let rest = Array(args.dropFirst(2).dropLast(background ? 1 : 0))  // bundle [--network=...]
    guard recovery ? rest.count == 1 : (rest.count == 2 && ["--network=off", "--network=on"].contains(rest[1])) else { fail(usage) }
    // Recovery needs NAT to fetch the setup script. Setup Assistant must use network=off.
    try MainActor.assumeIsolated {
        try run(dir: URL(fileURLWithPath: rest[0]), network: recovery || rest[1] == "--network=on", recovery: recovery, background: background)
    }
default: fail(usage)
}
