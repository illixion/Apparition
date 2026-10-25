import SwiftUI

extension ProbeModel {
    /// Becomes the requesting app's debugger; the app comes back to the front once attached.
    func attachJIT(_ request: LibraryModel.JITRequest) async {
        await attachJIT(host: defaultTunnelHost, port: defaultTunnelPort,
                        bundleId: request.bundleId, pid: request.pid)
    }
}

extension LibraryModel.JITRequest {
    /// What the prompt asks of someone: the app's name and why it needs allowing.
    static func message(_ request: LibraryModel.JITRequest) -> String {
        "\(request.bundleId) asks Apparition to be its debugger, which lets it run code it makes. Apparition didn't install it."
    }
}

#if os(visionOS)

/// The "Enable JIT?" prompt for an app Apparition didn't install, in a utility window. A
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
                Text(LibraryModel.JITRequest.message(request))
                    .multilineTextAlignment(.center)
                    .foregroundStyle(.secondary)
                VStack(spacing: 10) {
                    Button("Enable JIT") {
                        library.pendingJITRequest = nil
                        dismissWindow(id: JITRequestWindow.windowID)
                        Task { await model.attachJIT(request) }
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
