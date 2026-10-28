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
    let account: AccountModel
    let companions: CompanionFinder

    @State private var draft = ""
    @State private var checking = false

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            ForEach(ProbeModel.ConnectionMode.offered) { mode in
                ModeOption(mode: mode, selected: model.connectionMode == mode) {
                    account.switchAnisette(from: model.connectionMode, to: mode)
                    model.connectionMode = mode
                }
            }
            switch model.connectionMode {
            case .companion: companionControls
            case .localVPN: localVPNNote
            case nil: Note(text: "Choose one to continue.", kind: .info)
            }
            if let mode = model.connectionMode {
                anisetteControls(mode)
            }
        }
        .onAppear { draft = model.companionAddress }
        // A companion that runs an anisette server is the likely choice for one: take it when
        // none is set yet.
        .onChange(of: companions.found.map(\.relay)) { _, _ in
            guard model.connectionMode == .companion, account.anisetteURL.isEmpty,
                  let url = companions.found.first(where: { $0.relay == model.relay })?.anisetteURL else { return }
            account.anisetteURL = url
        }
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

    // MARK: Anisette

    /// The anisette server for Apple's sign-in, which goes with the connection: a companion
    /// usually runs one itself, where Local VPN has no computer to run it, so a public server
    /// is the likelier choice. Each mode keeps its own (`AccountModel.switchAnisette`).
    private func anisetteControls(_ mode: ProbeModel.ConnectionMode) -> some View {
        VStack(alignment: .leading, spacing: 12) {
            Divider()
            Label("Anisette Server", systemImage: "server.rack")
                .font(.headline)
            Text(mode == .companion
                 ? "Signing in to Apple needs an anisette server (v3). Your companion can run one: Apparition on your Mac offers its own. The server sees this \(ThisDevice.noun)'s anisette identity and IP address, never your Apple ID or password."
                 : "Signing in to Apple needs an anisette server (v3), and Local VPN has no computer to run one. Use a public one you trust, or host your own. The server sees this \(ThisDevice.noun)'s anisette identity and IP address, never your Apple ID or password.")
                .font(.callout)
                .foregroundStyle(.secondary)

            if mode == .companion { companionAnisetteChoices }

            TextField(mode == .companion ? "http://your-computer:6969" : "https://your-anisette-server", text: Binding(
                get: { account.anisetteURL },
                set: { account.anisetteURL = $0.trimmingCharacters(in: .whitespaces) }))
                .keyboardType(.URL)
                .textInputAutocapitalization(.never)
                .autocorrectionDisabled()
                .font(.body.monospaced())
                .roundedFieldStyle()
                .wideFieldOnTV()
                .frame(maxWidth: 480)

            Disclosure("Host your own") {
                VStack(alignment: .leading, spacing: 8) {
                    Text("Apparition on your Mac can offer its own (no Docker needed), and on any computer it can run one with your Docker: choose it in its Companion tab, then Find Companion above and use it.")
                    Text("Or, on any machine with Docker, from the Apparition repository:")
                    Text("docker compose -f selfhost/anisette/compose.yaml up -d")
                        .font(.callout.monospaced())
                        .selectableText()
                        .padding(10)
                        .background(.black.opacity(0.25), in: RoundedRectangle(cornerRadius: 10))
                    Text("and enter http://<that machine>:6969 above.")
                }
                .font(.callout)
                .foregroundStyle(.secondary)
                .padding(.top, 8)
            }
        }
    }

    @ViewBuilder
    private var companionAnisetteChoices: some View {
        let offered = companions.found.filter { $0.anisetteURL != nil }
        #if os(tvOS)
        // One menu instead of a row per companion: rows made the step long enough that moving
        // between them scrolled a whole screen.
        if !offered.isEmpty {
            Menu {
                ForEach(offered) { companion in
                    Button("\(companion.name): \(companion.anisetteURL ?? "")") {
                        account.anisetteURL = companion.anisetteURL ?? ""
                    }
                }
            } label: {
                let current = offered.first { $0.anisetteURL == account.anisetteURL }
                Label(current.map { "Using \($0.name)'s server" } ?? "Use a Companion's Server", systemImage: "desktopcomputer")
            }
            .focusRow()
        }
        #else
        ForEach(offered) { companion in
            if let url = companion.anisetteURL {
                HStack(spacing: 12) {
                    Image(systemName: "desktopcomputer")
                    Text("\(companion.name) runs one at \(url)")
                        .font(.callout.monospaced())
                    Spacer(minLength: 0)
                    if account.anisetteURL == url {
                        StatusPill(text: "In use", systemImage: "checkmark", tint: .green)
                    } else {
                        Button("Use") { account.anisetteURL = url }
                            .buttonStyle(.bordered)
                    }
                }
                .focusRow()
            }
        }
        #endif
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

extension AccountModel {
    /// Each connection mode keeps its own anisette server: a companion's is not what Local VPN
    /// uses, so switching saves the one in use under the mode being left and brings back the one
    /// the other mode had (none the first time).
    func switchAnisette(from old: ProbeModel.ConnectionMode?, to new: ProbeModel.ConnectionMode) {
        guard old != new else { return }
        let defaults = UserDefaults.standard
        if let old { defaults.set(anisetteURL, forKey: "anisetteURL." + old.rawValue) }
        anisetteURL = defaults.string(forKey: "anisetteURL." + new.rawValue) ?? ""
    }
}
