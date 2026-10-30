import SwiftUI

extension ProbeModel {
    /// Becomes the requesting app's debugger, as StikDebug would: a request naming a process
    /// is attached to, and that app comes back to the front; one naming none launches the app
    /// anew under the debugger. With a script, the script is the debugger.
    func answerJIT(_ request: LibraryModel.JITRequest) async {
        if let script = request.script {
            await runJITScript(host: defaultTunnelHost, port: defaultTunnelPort, bundleId: request.bundleId,
                               pid: request.pid, launch: request.pid == 0, script: script)
        } else if request.pid == 0 {
            await launchJIT(host: defaultTunnelHost, port: defaultTunnelPort, bundleId: request.bundleId,
                            waitSeconds: 0, reportsFailure: true)
        } else {
            await attachJIT(host: defaultTunnelHost, port: defaultTunnelPort,
                            bundleId: request.bundleId, pid: request.pid)
        }
    }
}

extension LibraryModel.JITRequest {
    /// Whether the person is asked first. A script is: it can read and change everything in
    /// the app, and any app can send one, naming any app.
    func needsAsking(managed: Bool) -> Bool {
        !managed || script != nil
    }

    /// What the prompt asks of someone: the app's name and why it needs allowing.
    static func message(_ request: LibraryModel.JITRequest, managed: Bool) -> String {
        let installed = managed ? "" : " Apparition didn't install it."
        guard request.script != nil else {
            return "\(request.bundleId) asks Apparition to be its debugger, which lets it run code it makes.\(installed)"
        }
        return "A JIT script for \(request.bundleId) asks to be its debugger. The script can read and change everything in the app, so allow it only if you added it yourself, as in LiveContainer.\(installed)"
    }
}

#if os(visionOS)

/// The "Enable JIT?" prompt for an app Apparition didn't install, or a JIT script, in a utility window. A
/// confirmation dialog on visionOS is a popover anchored to the main window, which could land far
/// to its right and out of reach, leaving the request to time out. iOS and tvOS keep the dialog.
/// It also says why a request couldn't be served (no pairing, a pairing the headset turned down,
/// no companion), which iOS and tvOS show in an alert.
struct JITRequestWindow: Scene {
    static let windowID = "jit-request"

    let model: ProbeModel
    let library: LibraryModel

    var body: some Scene {
        Window("Enable JIT", id: Self.windowID) {
            JITRequestView(model: model, library: library)
        }
        .windowResizability(.contentSize)
        // A utility panel opens close to the person and in front of the app's other windows,
        // as DebugTrace's connection prompt does.
        .defaultWindowPlacement { _, _ in WindowPlacement(.utilityPanel) }
        // Only ever opened for a request: never at launch, and never restored empty.
        .defaultLaunchBehavior(.suppressed)
        .restorationBehavior(.disabled)
    }
}

private struct JITRequestView: View {
    let model: ProbeModel
    let library: LibraryModel
    @Environment(\.dismissWindow) private var dismissWindow

    var body: some View {
        VStack(spacing: 16) {
            if let failure = model.jitFailure {
                Image(systemName: failure.problem == nil ? "exclamationmark.triangle.fill" : "link.badge.plus")
                    .font(.system(size: 40))
                    .foregroundStyle(.orange)
                Text("Can't Enable JIT for \(library.displayName(of: failure.bundleId))")
                    .font(.title2.bold())
                    .multilineTextAlignment(.center)
                Text(failure.message)
                    .multilineTextAlignment(.center)
                    .foregroundStyle(.secondary)
                    .accessibilityIdentifier("jitFailure")
                Button("OK") {
                    model.jitFailure = nil
                    dismissWindow(id: JITRequestWindow.windowID)
                }
                .buttonStyle(.borderedProminent)
            } else if let request = library.pendingJITRequest {
                Image(systemName: "bolt.fill")
                    .font(.system(size: 40))
                Text("Enable JIT for \(library.displayName(of: request.bundleId))?")
                    .font(.title2.bold())
                    .multilineTextAlignment(.center)
                Text(LibraryModel.JITRequest.message(request, managed: library.isManaged(request.bundleId)))
                    .multilineTextAlignment(.center)
                    .foregroundStyle(.secondary)
                VStack(spacing: 10) {
                    Button("Enable JIT") {
                        library.pendingJITRequest = nil
                        dismissWindow(id: JITRequestWindow.windowID)
                        Task { await model.answerJIT(request) }
                    }
                    .buttonStyle(.borderedProminent)
                    .accessibilityIdentifier("enableJIT")
                    Button("Cancel", role: .cancel) {
                        library.pendingJITRequest = nil
                        dismissWindow(id: JITRequestWindow.windowID)
                    }
                }
            } else {
                Text("No request is waiting.")
                    .foregroundStyle(.secondary)
            }
        }
        .padding(32)
        .frame(width: 460)
        // Closing the window with the request still up is a no.
        .onDisappear {
            library.pendingJITRequest = nil
            model.jitFailure = nil
        }
    }
}

#endif
