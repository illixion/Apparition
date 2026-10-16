import AVFoundation
import CoreLocation
import DebugTrace
import UIKit

private let log = DebugLogger(subsystem: Bundle.main.bundleIdentifier ?? "Apparition", category: "JIT")

/// Keeps Apparition running in the background while it is an app's debugger. The app being
/// debugged is in front, and a suspended Apparition can't answer its JIT requests: the
/// thread that asked would wait for good. A background task alone lasts about 30 seconds,
/// while Wine (Madeira) asks for as long as a game runs, so this also keeps the app running:
/// - iOS: location updates (the `location` background mode), as SSH clients do to keep a
///   session open. The coarsest accuracy (no GPS), and every location is dropped unread, never
///   stored or sent anywhere. The system shows its location indicator while a session runs.
///   Without permission, only the background task is left.
/// - visionOS: silence, played mixed with other apps' audio (the `audio` background mode). An
///   app that opens an immersive space hides every other app's windows, which would suspend
///   Apparition; an app playing mixable audio keeps running behind it (as Longwave's does).
/// - tvOS: the background task only.
@MainActor
final class JITKeepAlive: NSObject {
    private var task: UIBackgroundTaskIdentifier = .invalid
    #if os(visionOS)
    private var engine: AVAudioEngine?
    private var player: AVAudioPlayerNode?
    private var interruption: NSObjectProtocol?
    #endif
    #if os(iOS)
    private let manager = CLLocationManager()
    private var active = false
    private var authorization: CheckedContinuation<Void, Never>?

    override init() {
        super.init()
        manager.delegate = self
        manager.desiredAccuracy = kCLLocationAccuracyThreeKilometers
        manager.distanceFilter = CLLocationDistanceMax
        manager.pausesLocationUpdatesAutomatically = false
    }
    #endif

    /// Asks for location permission if it was never asked, and waits for the answer: the
    /// prompt has to be answered while Apparition is in front, before the debugged app is.
    func prepare() async {
        #if os(iOS)
        guard manager.authorizationStatus == .notDetermined else { return }
        await withCheckedContinuation { continuation in
            authorization = continuation
            manager.requestWhenInUseAuthorization()
        }
        #endif
    }

    func update(active: Bool) {
        if active { start() } else { stop() }
    }

    private func start() {
        if task == .invalid {
            task = UIApplication.shared.beginBackgroundTask(withName: "JIT session") { [weak self] in
                MainActor.assumeIsolated { self?.endTask() }
            }
        }
        #if os(iOS)
        guard !active else { return }
        switch manager.authorizationStatus {
        case .authorizedWhenInUse, .authorizedAlways:
            manager.allowsBackgroundLocationUpdates = true
            manager.showsBackgroundLocationIndicator = true
            manager.startUpdatingLocation()
            active = true
            log.info("keeping Apparition running for the JIT session")
        default:
            log.error("no location permission: Apparition is suspended about 30 s after it leaves the screen, and the debugged app's JIT requests wait until it's back")
        }
        #elseif os(visionOS)
        startSilence()
        #endif
    }

    private func stop() {
        #if os(iOS)
        if active {
            manager.stopUpdatingLocation()
            manager.allowsBackgroundLocationUpdates = false
            active = false
            log.info("JIT sessions over; Apparition may be suspended again")
        }
        #elseif os(visionOS)
        stopSilence()
        #endif
        endTask()
    }

    #if os(visionOS)
    private func startSilence() {
        guard engine == nil else { return }
        do {
            let session = AVAudioSession.sharedInstance()
            try session.setCategory(.playback, options: .mixWithOthers)
            try session.setActive(true)
            let engine = AVAudioEngine()
            let player = AVAudioPlayerNode()
            engine.attach(player)
            let format = engine.mainMixerNode.outputFormat(forBus: 0)
            engine.connect(player, to: engine.mainMixerNode, format: format)
            // A second of zeros, looped.
            if let silence = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: AVAudioFrameCount(format.sampleRate)) {
                silence.frameLength = silence.frameCapacity
                player.scheduleBuffer(silence, at: nil, options: .loops)
            }
            try engine.start()
            player.play()
            self.engine = engine
            self.player = player
            // A call or another app taking the audio session stops the engine; start it again.
            interruption = NotificationCenter.default.addObserver(
                forName: AVAudioSession.interruptionNotification, object: nil, queue: .main
            ) { [weak self] note in
                guard let raw = note.userInfo?[AVAudioSessionInterruptionTypeKey] as? UInt,
                      AVAudioSession.InterruptionType(rawValue: raw) == .ended else { return }
                MainActor.assumeIsolated { self?.resumeSilence() }
            }
            log.info("keeping Apparition running for the JIT session")
        } catch {
            log.error("can't keep Apparition running behind other apps: \(error.localizedDescription, privacy: .public)")
        }
    }

    private func resumeSilence() {
        guard let engine, let player else { return }
        try? AVAudioSession.sharedInstance().setActive(true)
        if !engine.isRunning { try? engine.start() }
        player.play()
    }

    private func stopSilence() {
        if let interruption { NotificationCenter.default.removeObserver(interruption) }
        interruption = nil
        guard let engine else { return }
        engine.stop()
        try? AVAudioSession.sharedInstance().setActive(false, options: .notifyOthersOnDeactivation)
        self.engine = nil
        player = nil
        log.info("JIT sessions over; Apparition may be suspended again")
    }
    #endif

    private func endTask() {
        if task != .invalid { UIApplication.shared.endBackgroundTask(task) }
        task = .invalid
    }
}

#if os(iOS)
extension JITKeepAlive: CLLocationManagerDelegate {
    nonisolated func locationManagerDidChangeAuthorization(_ manager: CLLocationManager) {
        Task { @MainActor in
            guard manager.authorizationStatus != .notDetermined else { return }
            authorization?.resume()
            authorization = nil
        }
    }

    nonisolated func locationManager(_ manager: CLLocationManager, didUpdateLocations locations: [CLLocation]) {
        // Dropped unread: the updates only keep Apparition running.
    }

    nonisolated func locationManager(_ manager: CLLocationManager, didFailWithError error: Error) {}
}
#endif
