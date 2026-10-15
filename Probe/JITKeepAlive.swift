import AVFoundation
import DebugTrace
import UIKit

private let log = DebugLogger(subsystem: Bundle.main.bundleIdentifier ?? "Apparition", category: "JIT")

/// Keeps Apparition running in the background while it is an app's debugger. The app being
/// debugged is in front, and a suspended Apparition can't answer its JIT requests: the
/// thread that asked would wait for good. A background task alone lasts about 30 seconds,
/// while Wine (Madeira) asks for as long as a game runs, so this also plays silence (the
/// `audio` background mode), mixed with whatever else plays.
@MainActor
final class JITKeepAlive {
    private var engine: AVAudioEngine?
    private var task: UIBackgroundTaskIdentifier = .invalid
    private var interruption: NSObjectProtocol?

    func update(active: Bool) {
        if active { start() } else { stop() }
    }

    private func start() {
        if task == .invalid {
            task = UIApplication.shared.beginBackgroundTask(withName: "JIT session") { [weak self] in
                MainActor.assumeIsolated { self?.endTask() }
            }
        }
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
            // A call or another app's audio stops the engine; start it again afterwards.
            interruption = NotificationCenter.default.addObserver(
                forName: AVAudioSession.interruptionNotification, object: nil, queue: .main
            ) { [weak self] note in
                guard let raw = note.userInfo?[AVAudioSessionInterruptionTypeKey] as? UInt,
                      AVAudioSession.InterruptionType(rawValue: raw) == .ended else { return }
                MainActor.assumeIsolated { self?.resume() }
            }
            log.info("keeping Apparition running for the JIT session")
        } catch {
            log.error("can't keep Apparition running in the background: \(error.localizedDescription, privacy: .public)")
        }
    }

    private func resume() {
        guard let engine, !engine.isRunning else { return }
        try? AVAudioSession.sharedInstance().setActive(true)
        try? engine.start()
        (engine.attachedNodes.first { $0 is AVAudioPlayerNode } as? AVAudioPlayerNode)?.play()
    }

    private func stop() {
        if let interruption { NotificationCenter.default.removeObserver(interruption) }
        interruption = nil
        if let engine {
            engine.stop()
            try? AVAudioSession.sharedInstance().setActive(false, options: .notifyOthersOnDeactivation)
            log.info("JIT sessions over; Apparition may be suspended again")
        }
        engine = nil
        endTask()
    }

    private func endTask() {
        if task != .invalid { UIApplication.shared.endBackgroundTask(task) }
        task = .invalid
    }
}
