import RAVEUI
import SwiftUI

extension ProbeModel.ConnectionMode {
    var title: String {
        switch self {
        case .companion: "Companion"
        case .localVPN: "Local VPN"
        }
    }

    var systemImage: String {
        switch self {
        case .companion: "desktopcomputer"
        case .localVPN: "lock.shield"
        }
    }

    var summary: String {
        switch self {
        case .companion:
            "Apparition on your Mac or PC connects back to this \(ThisDevice.noun) for you, over your local network or Tailscale. Leaves the VPN slot free."
        case .localVPN:
            #if os(visionOS)
            "LocalDevVPN loops the connection back on this \(ThisDevice.noun), with no computer. It takes the device's only VPN slot, and the headset may reset these connections: if installs fail, use a companion."
            #else
            "LocalDevVPN loops the connection back on this \(ThisDevice.noun), with no computer. It takes the device's only VPN slot."
            #endif
        }
    }
}

/// How Apparition reaches this device's own services, in two modes: a companion (its address is
/// remembered, so going to Local VPN and back needs no typing) or LocalDevVPN. Used by Setup and
/// Settings.
struct ConnectionView: View {
    let model: ProbeModel
    let companions: CompanionFinder

    @State private var draft = ""
    @State private var checking = false

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            ForEach(ProbeModel.ConnectionMode.offered) { mode in
                ModeOption(mode: mode, selected: model.connectionMode == mode) {
                    model.connectionMode = mode
                }
            }
            switch model.connectionMode {
            case .companion: companionControls
            case .localVPN: localVPNNote
            case nil: Note(text: "Choose one to continue.", kind: .info)
            }
        }
        .onAppear { draft = model.companionAddress }
    }

    // MARK: Companion

    private var companionControls: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(spacing: 12) {
                Button {
                    Task { await companions.search() }
                } label: {
                    Label(companions.searching ? "Searching…" : "Find Companion", systemImage: "dot.radiowaves.left.and.right")
                }
                .buttonStyle(.borderedProminent)
                .disabled(companions.searching)
                if companions.searching { ProgressView() }
            }
            .focusRow()

            ForEach(companions.found) { companion in
                companionRow(companion)
            }

            VStack(alignment: .leading, spacing: 8) {
                Text("Or type its address").font(.headline)
                HStack(spacing: 12) {
                    TextField("192.168.1.20:49160 or 100.x.y.z:49160", text: $draft)
                        .keyboardType(.URL)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .font(.body.monospaced())
                        .roundedFieldStyle()
                        .wideFieldOnTV()
                        .frame(maxWidth: 380)
                        .onChange(of: draft) { _, typed in
                            model.companionAddress = typed.trimmingCharacters(in: .whitespaces)
                        }
                    if checking { ProgressView() }
                }
                .focusRow()
                Text("On your local network, Find Companion finds it. Over Tailscale there is no Bonjour, so enter its Tailscale address with port 49160. The address is remembered while you use Local VPN.")
                    .font(.callout)
                    .foregroundStyle(.secondary)
            }

            if let relay = model.relay {
                HStack(spacing: 12) {
                    if companions.found.contains(where: { $0.relay == relay }) {
                        Note(text: "Using \(relay), and it answered.", kind: .success)
                    } else {
                        Note(text: "Using \(relay).", kind: .info)
                    }
                    Button("Check") {
                        checking = true
                        Task { _ = await companions.check(relay: relay); checking = false }
                    }
                    .buttonStyle(.borderless)
                    .disabled(checking)
                }
                .focusRow()
            } else {
                Note(text: "Find a companion or type its address to use it.", kind: .info)
            }
            if let error = companions.lastError {
                Note(text: error, kind: .warning)
            }
        }
    }

    private func companionRow(_ companion: CompanionFinder.Companion) -> some View {
        HStack(spacing: 14) {
            Image(systemName: "desktopcomputer").font(.title3)
            VStack(alignment: .leading, spacing: 2) {
                Text(companion.name).font(.headline)
                Text(companion.relay + (companion.anisetteURL != nil ? " · runs an anisette server" : ""))
                    .font(.callout.monospaced())
                    .foregroundStyle(.secondary)
            }
            Spacer(minLength: 0)
            if model.relay == companion.relay {
                StatusPill(text: "In use", systemImage: "checkmark", tint: .green)
            } else {
                Button("Use") {
                    model.useCompanion(companion.relay)
                    draft = companion.relay
                }
                .buttonStyle(.bordered)
            }
        }
        .focusRow()
        .padding(14)
        .background(.thinMaterial, in: RoundedRectangle(cornerRadius: 18, style: .continuous))
    }

    // MARK: Local VPN

    private var localVPNNote: some View {
        VStack(alignment: .leading, spacing: 8) {
            Note(text: "Turn LocalDevVPN on whenever Apparition installs, refreshes or enables JIT.", kind: .info)
            if !model.companionAddress.isEmpty {
                Text("Your companion, \(model.companionAddress), is remembered: choose Companion to switch back.")
                    .font(.callout)
                    .foregroundStyle(.secondary)
            }
        }
    }
}

/// One of the two modes: its icon, name and what it means, marked when chosen.
private struct ModeOption: View {
    let mode: ProbeModel.ConnectionMode
    let selected: Bool
    let choose: () -> Void

    var body: some View {
        Button(action: choose) {
            HStack(alignment: .top, spacing: 14) {
                Image(systemName: mode.systemImage)
                    .font(.title2)
                    .frame(width: 36)
                    .foregroundStyle(selected ? Color.accentColor : .secondary)
                VStack(alignment: .leading, spacing: 4) {
                    Text(mode.title).font(.headline)
                    Text(mode.summary)
                        .font(.callout)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
                Spacer(minLength: 0)
                Image(systemName: selected ? "checkmark.circle.fill" : "circle")
                    .font(.title3)
                    .foregroundStyle(selected ? Color.accentColor : .secondary)
            }
            .padding(14)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(.thinMaterial, in: RoundedRectangle(cornerRadius: 18, style: .continuous))
            .overlay {
                RoundedRectangle(cornerRadius: 18, style: .continuous)
                    .strokeBorder(selected ? Color.accentColor : .clear, lineWidth: 2)
            }
            .contentShape(RoundedRectangle(cornerRadius: 18, style: .continuous))
        }
        .buttonStyle(.plain)
        .accessibilityAddTraits(selected ? .isSelected : [])
    }
}
