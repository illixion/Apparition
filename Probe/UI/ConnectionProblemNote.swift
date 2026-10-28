import SwiftUI

/// Shown in the Library when the chosen connection last failed: its tunnel didn't reach this
/// device, or its anisette server didn't serve Apple's sign-in. Says what to check, with a retry,
/// the other mode when it can be used here (for the companion, only once an address is
/// remembered), and Connection Settings.
struct ConnectionProblemNote: View {
    enum Problem: Equatable {
        case unreachable(ProbeModel.ConnectionMode)
        case anisette(AccountModel.AnisetteProblem)
    }

    let model: ProbeModel
    let account: AccountModel
    let problem: Problem
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
            .disabled(model.busy || account.busy)
            .focusRow()
        }
    }

    private var mode: ProbeModel.ConnectionMode? {
        switch problem {
        case .unreachable(let mode): mode
        case .anisette: model.connectionMode
        }
    }

    private var text: String {
        switch problem {
        case .unreachable(.localVPN):
            "Apparition can't reach this \(ThisDevice.noun) through LocalDevVPN. Open LocalDevVPN and check it's connected: only one VPN runs at a time, so turning on another (Tailscale, say) turns it off."
        case .unreachable(.companion):
            "Apparition can't reach \(companionLabel). Check that Apparition is open on that computer and that this \(ThisDevice.noun) can reach it: on the same network, or both on Tailscale."
        case .anisette(let problem):
            problem.message + " Until then, apps can't be refreshed."
        }
    }

    private var companionLabel: String {
        model.companionName.map { "the companion \($0)" } ?? "the companion at \(model.companionAddress)"
    }

    /// The other mode, when this device offers it and it has what it needs: a companion
    /// address, and for an anisette problem, an anisette server of its own.
    private var other: ProbeModel.ConnectionMode? {
        guard let mode else { return nil }
        let other: ProbeModel.ConnectionMode = mode == .localVPN ? .companion : .localVPN
        guard ProbeModel.ConnectionMode.offered.contains(other) else { return nil }
        if other == .companion && model.companionAddress.isEmpty { return nil }
        if case .anisette = problem,
           (UserDefaults.standard.string(forKey: "anisetteURL." + other.rawValue) ?? "").isEmpty { return nil }
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
        Task {
            if case .anisette = problem {
                _ = await account.signInWithRememberedPassword(byPerson: true)
            } else {
                await model.listApps(host: model.defaultTunnelHost, port: model.defaultTunnelPort)
            }
        }
    }
}
