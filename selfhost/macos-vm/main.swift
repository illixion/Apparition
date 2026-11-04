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

func makeConfig(_ b: Bundle_, hardwareModel: VZMacHardwareModel, machineID: VZMacMachineIdentifier,
                aux: VZMacAuxiliaryStorage, network: Bool = false, usbKeyboard: Bool = false) throws -> VZVirtualMachineConfiguration {
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
    c.cpuCount = max(2, min(4, ProcessInfo.processInfo.processorCount / 2))
    c.memorySize = 8 * 1024 * 1024 * 1024

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
    gfx.displays = [VZMacGraphicsDisplayConfiguration(widthInPixels: 1920, heightInPixels: 1200, pixelsPerInch: 144)]
    c.graphicsDevices = [gfx]

    // recoveryOS only gets modifier chords (Cmd+Shift+T) from a USB keyboard; the installed guest keeps the Mac one.
    c.keyboards = [usbKeyboard ? VZUSBKeyboardConfiguration() : VZMacKeyboardConfiguration()]
    c.pointingDevices = [VZMacTrackpadConfiguration()]

    let audio = VZVirtioSoundDeviceConfiguration()
    let out = VZVirtioSoundDeviceOutputStreamConfiguration()
    out.sink = VZHostAudioOutputStreamSink()
    audio.streams = [out]
    c.audioDevices = [audio]

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
    _ = try makeConfig(b, hardwareModel: hw, machineID: mid, aux: aux)
    print("CREATED \(dir.path)")
}

func configuration(_ dir: URL, network: Bool = false, usbKeyboard: Bool = false) throws -> VZVirtualMachineConfiguration {
    let b = Bundle_(dir: dir)
    guard let hw = VZMacHardwareModel(dataRepresentation: try Data(contentsOf: b.hwModel)),
          let mid = VZMacMachineIdentifier(dataRepresentation: try Data(contentsOf: b.machineID))
    else { fail("bad bundle") }
    return try makeConfig(b, hardwareModel: hw, machineID: mid,
                          aux: VZMacAuxiliaryStorage(url: b.aux), network: network, usbKeyboard: usbKeyboard)
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
    let config = try configuration(dir)

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
        let modifiers: [(NSEvent.ModifierFlags, UInt16)] = [(.control,59),(.option,58),(.shift,56),(.command,55)]
        var active: NSEvent.ModifierFlags = []
        func event(_ type: NSEvent.EventType, _ key: UInt16) -> NSEvent {
            // flagsChanged can't be made with NSEvent.keyEvent; build it from a CGEvent so the
            // type and modifier state are real.
            if type == .flagsChanged || type == .keyDown || type == .keyUp,
               let cg = CGEvent(keyboardEventSource: nil, virtualKey: CGKeyCode(key), keyDown: type != .keyUp) {
                var cgFlags = CGEventFlags()
                if active.contains(.command) { cgFlags.insert(.maskCommand) }
                if active.contains(.shift) { cgFlags.insert(.maskShift) }
                if active.contains(.control) { cgFlags.insert(.maskControl) }
                if active.contains(.option) { cgFlags.insert(.maskAlternate) }
                cg.flags = cgFlags
                if type == .flagsChanged { cg.type = .flagsChanged }
                if let ns = NSEvent(cgEvent: cg) { return ns }
            }
            return NSEvent.keyEvent(with: type, location: .zero, modifierFlags: active,
                             timestamp: ProcessInfo.processInfo.systemUptime,
                             windowNumber: window.windowNumber, context: nil, characters: "",
                             charactersIgnoringModifiers: "", isARepeat: false, keyCode: key)!
        }
        if !flags.isEmpty {
            // Real events through the app's own queue: the view tracks modifier state from
            // these, whereas calling flagsChanged on it directly leaves the guest without it.
            let pid = ProcessInfo.processInfo.processIdentifier
            func post(_ key: UInt16, down: Bool, _ mask: CGEventFlags, modifier: Bool = false) {
                guard let cg = CGEvent(keyboardEventSource: nil, virtualKey: CGKeyCode(key), keyDown: down) else { return }
                cg.flags = mask
                if modifier { cg.type = .flagsChanged }
                cg.postToPid(pid)
                Thread.sleep(forTimeInterval: 0.05)
            }
            var mask = CGEventFlags()
            let table: [(NSEvent.ModifierFlags, UInt16, CGEventFlags)] = [(.control,59,.maskControl),(.option,58,.maskAlternate),(.shift,56,.maskShift),(.command,55,.maskCommand)]
            for (flag, key, bit) in table where flags.contains(flag) { mask.insert(bit); post(key, down: true, mask, modifier: true) }
            post(code, down: true, mask)
            Thread.sleep(forTimeInterval: 0.05)
            post(code, down: false, mask)
            for (flag, key, bit) in table.reversed() where flags.contains(flag) { mask.remove(bit); post(key, down: false, mask, modifier: true) }
            return
        }
        for (flag, key) in modifiers where flags.contains(flag) {
            active.insert(flag)
            view.flagsChanged(with: event(.flagsChanged, key))
            Thread.sleep(forTimeInterval: 0.05)
        }
        view.keyDown(with: event(.keyDown, code))
        Thread.sleep(forTimeInterval: 0.08)
        view.keyUp(with: event(.keyUp, code))
        Thread.sleep(forTimeInterval: 0.05)
        for (flag, key) in modifiers.reversed() where flags.contains(flag) {
            active.remove(flag)
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
    @MainActor func screen(recognize: Bool) async throws -> [String: Any] {
        let content = try await SCShareableContent.excludingDesktopWindows(false, onScreenWindowsOnly: false)
        guard let number = view?.window?.windowNumber,
              let window = content.windows.first(where: { $0.windowID == CGWindowID(number) && $0.owningApplication?.processID == getpid() }) else { throw InputError.invalid }
        let filter = SCContentFilter(desktopIndependentWindow: window)
        let settings = SCStreamConfiguration()
        settings.width = Int(window.frame.width) * 2
        settings.height = Int(window.frame.height) * 2
        settings.showsCursor = false
        let cg = try await SCScreenshotManager.captureImage(contentFilter: filter, configuration: settings)
        let image = NSBitmapImageRep(cgImage: cg)
        let path = file.deletingLastPathComponent().appendingPathComponent("screen.png")
        guard let png = image.representation(using: .png, properties: [:]) else { throw InputError.invalid }
        try png.write(to: path, options: .atomic)
        if !recognize { return ["ok": true, "path": path.path] }
        let text = try await Task.detached(priority: .userInitiated) {
            let request = VNRecognizeTextRequest()
            request.recognitionLevel = .fast
            request.recognitionLanguages = ["en-US"]
            request.usesLanguageCorrection = false
            try VNImageRequestHandler(cgImage: cg).perform([request])
            let results = request.results ?? []
            let text = results.compactMap { $0.topCandidates(1).first?.string }.joined(separator: "\n")
            let column = results.first(where: { $0.topCandidates(1).first?.string.lowercased() == "continue" })?.boundingBox.midX
            // Positions as fractions of the captured window, origin top-left, so click can use them as is.
            let items: [[String: Any]] = results.compactMap { observation in
                guard let string = observation.topCandidates(1).first?.string else { return nil }
                let box = observation.boundingBox
                return ["text": string, "x": Double(box.midX), "y": Double(1 - box.midY),
                        "w": Double(box.width), "h": Double(box.height)]
            }
            return (text, column.map(Double.init), items)
        }.value
        var result: [String: Any] = ["ok": true, "path": path.path, "text": text.0, "items": text.2]
        if let column = text.1 { result["continueX"] = column }
        return result
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

@MainActor func run(dir: URL, network: Bool, recovery: Bool) throws {
    let lock = open(dir.appendingPathComponent(".lock").path, O_RDWR | O_CREAT, 0o600)
    guard lock >= 0, flock(lock, LOCK_EX | LOCK_NB) == 0 else { fail("bundle is already in use") }
    defer { close(lock) }
    let config = try configuration(dir, network: network, usbKeyboard: recovery)
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
    app.setActivationPolicy(.regular)
    app.delegate = delegate
    let view = VZVirtualMachineView()
    view.virtualMachine = vm
    control.view = view
    view.capturesSystemKeys = true
    view.automaticallyReconfiguresDisplay = true
    let win = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 1280, height: 800),
                       styleMask: [.titled, .closable, .resizable, .miniaturizable],
                       backing: .buffered, defer: false)
    win.title = recovery ? "Apparition — Recovery" : "Apparition macOS 26"
    win.contentView = view
    win.makeFirstResponder(view)
    win.makeKeyAndOrderFront(nil)
    app.activate(ignoringOtherApps: true)
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
let usage = "usage: vm create <ipsw> <new-bundle> [diskGB] | install <ipsw> <bundle> | run <bundle> --network=off|on | setup <bundle>"
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
    guard recovery ? args.count == 3 : (args.count == 4 && ["--network=off", "--network=on"].contains(args[3])) else { fail(usage) }
    // Recovery needs NAT to fetch the setup script. Setup Assistant must use network=off.
    try MainActor.assumeIsolated {
        try run(dir: URL(fileURLWithPath: args[2]), network: recovery || args[3] == "--network=on", recovery: recovery)
    }
default: fail(usage)
}
