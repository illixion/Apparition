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
    /// The anisette server in use is a found companion's, shown by name rather than address.
    @AppStorage("anisetteAuto") private var anisetteAuto = false

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            ForEach(ProbeModel.ConnectionMode.offered) { mode in
                ModeOption(mode: mode, selected: model.connectionMode == mode) {
                    account.switchAnisette(from: model.connectionMode, to: mode)
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
        // A companion that runs an anisette server is the likely choice for one: take it when
        // none is set yet.
        .onChange(of: companions.found.map(\.relay)) { _, _ in
            guard model.connectionMode == .companion, account.anisetteURL.isEmpty,
                  let url = companions.found.first(where: { $0.relay == model.relay })?.anisetteURL else { return }
            account.anisetteURL = url
            anisetteAuto = true
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

    /// The anisette server for Apple's sign-in, which goes with the connection: a companion
    /// usually runs one itself, where Local VPN has no computer to run it, so a public server
    /// is the likelier choice. Each mode keeps its own (`AccountModel.switchAnisette`). With a
    /// companion it's chosen from the ones found, by name; typing a URL is the advanced way.
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

            if mode == .companion {
                companionAnisetteChoices
                if !account.anisetteURL.isEmpty {
                    Note(text: anisetteAuto ? "Using \(model.companionName ?? "your companion")'s anisette server." : "Using \(account.anisetteURL).",
                         kind: .info)
                }
                Expander("Advanced: use another server") { anisetteField }
            } else {
                anisetteField
            }

            Expander("Host your own") {
                VStack(alignment: .leading, spacing: 8) {
                    Text("Apparition on your Mac can offer its own (no Docker needed), and on any computer it can run one with your Docker: choose it in its Companion tab, then use Find Companion.")
                    Text("Or, on any machine with Docker, from the Apparition repository:")
                    Text("docker compose -f selfhost/anisette/compose.yaml up -d")
                        .font(.callout.monospaced())
                        .selectableText()
                        .padding(10)
                        .background(.black.opacity(0.25), in: RoundedRectangle(cornerRadius: 10))
                    Text("and enter http://<that machine>:6969 as the server.")
                }
                .font(.callout)
                .foregroundStyle(.secondary)
                .padding(.top, 8)
            }
        }
    }

    private var anisetteField: some View {
        TextField(model.connectionMode == .companion ? "http://your-computer:6969" : "https://your-anisette-server", text: Binding(
            get: { account.anisetteURL },
            set: {
                account.anisetteURL = $0.trimmingCharacters(in: .whitespaces)
                anisetteAuto = false
            }))
            .keyboardType(.URL)
            .textInputAutocapitalization(.never)
            .autocorrectionDisabled()
            .font(.body.monospaced())
            .roundedFieldStyle()
            .wideFieldOnTV()
            .frame(maxWidth: 480)
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
                    Button(companion.name) {
                        account.anisetteURL = companion.anisetteURL ?? ""
                        anisetteAuto = true
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
                    Text("\(companion.name) runs one")
                        .font(.callout)
                    Spacer(minLength: 0)
                    if account.anisetteURL == url {
                        StatusPill(text: "In use", systemImage: "checkmark", tint: .green)
                    } else {
                        Button("Use") {
                            account.anisetteURL = url
                            anisetteAuto = true
                        }
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
            if let name = model.companionName ?? (model.companionAddress.isEmpty ? nil : model.companionAddress) {
                Text("Your companion, \(name), is remembered: choose Companion to switch back.")
                    .font(.callout)
                    .foregroundStyle(.secondary)
            }
        }
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

extension AccountModel {
    /// Each connection mode keeps its own anisette server: a companion's is not what Local VPN
    /// uses, so switching saves the one in use (and whether it was a found companion's) under
    /// the mode being left and brings back the one the other mode had (none the first time).
    func switchAnisette(from old: ProbeModel.ConnectionMode?, to new: ProbeModel.ConnectionMode) {
        guard old != new else { return }
        let defaults = UserDefaults.standard
        if let old {
            defaults.set(anisetteURL, forKey: "anisetteURL." + old.rawValue)
            defaults.set(defaults.bool(forKey: "anisetteAuto"), forKey: "anisetteAuto." + old.rawValue)
        }
        anisetteURL = defaults.string(forKey: "anisetteURL." + new.rawValue) ?? ""
        defaults.set(defaults.bool(forKey: "anisetteAuto." + new.rawValue), forKey: "anisetteAuto")
    }
}
