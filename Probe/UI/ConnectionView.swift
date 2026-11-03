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

/// How Apparition reaches this device's own services, in two modes: a companion (found on the
/// network automatically, or by an address typed under Advanced, which is remembered, so going to
/// Local VPN and back needs no typing) or LocalDevVPN. Used by Setup and Settings.
struct ConnectionView: View {
    let model: ProbeModel
    let account: AccountModel
    let companions: CompanionFinder

    @State private var draft = ""
    @State private var checking = false
    /// Pairing with a companion whose anisette serves paired devices only.
    @State private var pairing = CompanionPairing()

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            ForEach(ProbeModel.ConnectionMode.offered) { mode in
                ModeOption(mode: mode, selected: model.connectionMode == mode) {
                    model.switchMode(to: mode)
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
        .panel(isPresented: Binding(get: { pairing.phase != .idle }, set: { shown in
            guard !shown else { return }
            if case .failed = pairing.phase { pairing.dismissFailure() }
            else { pairing.cancel() }
        })) {
            CompanionAnisettePairingSheet(pairing: pairing)
        }
    }

    // MARK: Companion

    /// Automatic: Find Companion lists the computers running Apparition, by name. The address is
    /// kept out of sight when a found companion is in use. Advanced: type an address, which is
    /// how a companion over Tailscale is reached.
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

            if let relay = model.relay {
                HStack(spacing: 12) {
                    let answered = companions.found.contains { $0.relay == relay }
                    Note(text: "Using \(model.companionName ?? relay)" + (answered ? ", and it answered." : "."),
                         kind: answered ? .success : .info)
                    Button("Check") {
                        checking = true
                        Task { _ = await companions.check(relay: relay); checking = false }
                    }
                    .buttonStyle(.borderless)
                    .disabled(checking)
                    if checking { ProgressView() }
                }
                .focusRow()
            } else {
                Note(text: "Find a companion on your network to use it.", kind: .info)
            }
            if let error = companions.lastError {
                Note(text: error, kind: .warning)
            }

            Expander("Advanced: enter an address") {
                VStack(alignment: .leading, spacing: 8) {
                    TextField("192.168.1.20:49160 or 100.x.y.z:49160", text: $draft)
                        .keyboardType(.URL)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .font(.body.monospaced())
                        .roundedFieldStyle()
                        .wideFieldOnTV()
                        .frame(maxWidth: 380)
                        .onChange(of: draft) { _, typed in
                            model.useCompanion(typed)
                        }
                    Text("For a companion Find Companion can't reach. Over Tailscale there is no Bonjour, so enter its Tailscale address with port 49160. The address is remembered while you use Local VPN.")
                }
                .font(.callout)
                .foregroundStyle(.secondary)
                .padding(.top, 8)
            }
        }
    }

    private func companionRow(_ companion: CompanionFinder.Companion) -> some View {
        HStack(spacing: 14) {
            Image(systemName: "desktopcomputer").font(.title3)
            VStack(alignment: .leading, spacing: 2) {
                Text(companion.name).font(.headline)
                if companion.anisetteURL != nil {
                    Text("Runs an anisette server")
                        .font(.callout)
                        .foregroundStyle(.secondary)
                }
            }
            Spacer(minLength: 0)
            if model.relay == companion.relay {
                StatusPill(text: "In use", systemImage: "checkmark", tint: .green)
            } else {
                Button("Use") {
                    model.useCompanion(companion.relay, name: companion.name)
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

    /// Apple authentication always uses a paired companion, independently of the install route.
    private func anisetteControls(_ mode: ProbeModel.ConnectionMode) -> some View {
        VStack(alignment: .leading, spacing: 12) {
            Divider()
            Label("Apple Sign-In Companion", systemImage: "server.rack")
                .font(.headline)
            Text("Pair with Apparition on your computer for Apple sign-in and signing. Your computer provides anisette and forwards encrypted Apple connections. This \(ThisDevice.noun) verifies Apple's certificate and keeps your Apple ID, password and signing credentials.")
                .font(.callout)
                .foregroundStyle(.secondary)
            companionAnisetteChoices
            if account.anisetteURL.hasPrefix("apparition-anisette://") {
                Note(text: "Paired companion selected. After updating an older companion, pair again to enable Apple connection relay access.", kind: .info)
            }
            Expander("Set up your computer") {
                Text("Open Apparition on your computer and enable anisette in its Companion tab. macOS can provide its own; Windows and Linux can use the local Docker provider. Then use Find Companion here and choose Pair and Use. Local VPN can still handle installs while the paired computer handles Apple connections.")
                    .font(.callout)
                    .foregroundStyle(.secondary)
            }
        }
    }

    private func inUse(_ companion: CompanionFinder.Companion) -> Bool {
        CompanionPairing.isPaired(account.anisetteURL, to: companion)
    }

    private func isFailure(_ phase: CompanionPairing.Phase) -> Bool {
        if case .failed = phase { true } else { false }
    }

    private func useAnisette(of companion: CompanionFinder.Companion) {
        Task {
            guard let url = await pairing.anisetteURL(for: companion) else { return }
            account.anisetteURL = url
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
                    Button(companion.name) { useAnisette(of: companion) }
                }
            } label: {
                let current = offered.first { inUse($0) }
                Label(current.map { "Using \($0.name)'s server" } ?? "Use a Companion's Server", systemImage: "desktopcomputer")
            }
            .focusRow()
        }
        #else
        ForEach(offered) { companion in
            if companion.anisetteURL != nil {
                HStack(spacing: 12) {
                    Image(systemName: "desktopcomputer")
                    Text("\(companion.name) runs one")
                        .font(.callout)
                    Spacer(minLength: 0)
                    if inUse(companion) {
                        StatusPill(text: "In use", systemImage: "checkmark", tint: .green)
                    } else {
                        Button("Pair and Use") { useAnisette(of: companion) }
                            .buttonStyle(.bordered)
                            .disabled(pairing.phase != .idle && !isFailure(pairing.phase))
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
            if let name = model.companionName ?? (model.companionAddress.isEmpty ? nil : model.companionAddress) {
                Text("Your companion, \(name), is remembered: choose Companion to switch back.")
                    .font(.callout)
                    .foregroundStyle(.secondary)
            }
        }
    }
}

/// Shows the companion's matching code as a focused prompt instead of burying it in Connection's
/// setup scroll. The desktop companion shows its approval in a matching modal.
private struct CompanionAnisettePairingSheet: View {
    let pairing: CompanionPairing

    var body: some View {
        NavigationStack {
            VStack(alignment: .leading, spacing: 16) {
                switch pairing.phase {
                case .idle:
                    EmptyView()
                case .waiting(let name):
                    HStack(spacing: 12) {
                        ProgressView()
                        Text("Asking \(name) to pair…").font(.headline)
                    }
                    Text("Approve the request on that computer when its prompt appears.")
                        .foregroundStyle(.secondary)
                case .confirming(let name, let code):
                    Text(code)
                        .font(.system(size: 48, weight: .semibold, design: .rounded).monospacedDigit())
                        .accessibilityLabel("Code \(code)")
                        .frame(maxWidth: .infinity)
                    Text("Check that Apparition on \(name) shows the same code, then choose Allow there.")
                        .foregroundStyle(.secondary)
                case .failed(let reason):
                    Note(text: reason, kind: .warning)
                }
                HStack {
                    Spacer()
                    if case .failed = pairing.phase {
                        Button("Done") { pairing.dismissFailure() }
                            .buttonStyle(.borderedProminent)
                    } else {
                        Button("Cancel", role: .cancel) { pairing.cancel() }
                            .buttonStyle(.bordered)
                    }
                }
                Spacer(minLength: 0)
            }
            .padding(24)
            .frame(maxWidth: 560, alignment: .leading)
            .frame(maxWidth: .infinity)
            .navigationTitle("Pair Companion")
        }
        .sheetFrame(minWidth: 500, minHeight: 300)
    }
}

/// A titled section that opens and closes. Only its header row toggles it: a DisclosureGroup in a
/// list row made the whole row one tap target, so touching anywhere in the section moved it.
/// On tvOS it's `Disclosure`, which opens a page of its own.
private struct Expander<Content: View>: View {
    let title: String
    @ViewBuilder let content: () -> Content
    @State private var open = false

    init(_ title: String, @ViewBuilder content: @escaping () -> Content) {
        self.title = title
        self.content = content
    }

    var body: some View {
        #if os(tvOS)
        Disclosure(title, content: content)
        #else
        VStack(alignment: .leading, spacing: 8) {
            Button {
                withAnimation { open.toggle() }
            } label: {
                HStack {
                    Text(title)
                    Spacer(minLength: 0)
                    Image(systemName: "chevron.right")
                        .font(.footnote.weight(.semibold))
                        .rotationEffect(open ? .degrees(90) : .zero)
                }
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .foregroundStyle(Color.accentColor)
            if open { content() }
        }
        #endif
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
