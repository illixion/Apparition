import SwiftUI

/// Shown in the Library when the chosen connection last failed to reach this device: what to
/// check for that mode, a retry, and the other mode when it can be used here (for the companion,
/// only once an address is remembered; otherwise Connection Settings is the way there).
struct ConnectionProblemNote: View {
    let model: ProbeModel
    let account: AccountModel
    let mode: ProbeModel.ConnectionMode
    let onSettings: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Note(text: text, kind: .warning)
            HStack(spacing: 12) {
                Button {
                    retry()
                } label: {
                    Label("Try Again", systemImage: "arrow.clockwise")
                }
                if let other {
                    Button(otherLabel(other)) { use(other) }
                }
                Button("Connection Settings", action: onSettings)
            }
            .buttonStyle(.bordered)
            .disabled(model.busy)
            .focusRow()
        }
    }

    private var text: String {
        switch mode {
        case .localVPN:
            "Apparition can't reach this \(ThisDevice.noun) through LocalDevVPN. Open LocalDevVPN and check it's connected: only one VPN runs at a time, so turning on another (Tailscale, say) turns it off."
        case .companion:
            "Apparition can't reach \(companionLabel). Check that Apparition is open on that computer and that this \(ThisDevice.noun) can reach it: on the same network, or both on Tailscale."
        }
    }

    private var companionLabel: String {
        model.companionName.map { "the companion \($0)" } ?? "the companion at \(model.companionAddress)"
    }

    /// The other mode, when this device offers it and it has what it needs.
    private var other: ProbeModel.ConnectionMode? {
        let other: ProbeModel.ConnectionMode = mode == .localVPN ? .companion : .localVPN
        guard ProbeModel.ConnectionMode.offered.contains(other) else { return nil }
        if other == .companion && model.companionAddress.isEmpty { return nil }
        return other
    }

    private func otherLabel(_ other: ProbeModel.ConnectionMode) -> String {
        switch other {
        case .localVPN: "Use LocalDevVPN"
        case .companion: "Use \(model.companionName ?? "Companion")"
        }
    }

    private func use(_ other: ProbeModel.ConnectionMode) {
        account.switchAnisette(from: model.connectionMode, to: other)
        model.switchMode(to: other)
        retry()
    }

    private func retry() {
        Task { await model.listApps(host: model.defaultTunnelHost, port: model.defaultTunnelPort) }
    }
}
