import RAVEUI
import SwiftUI

/// Everything Setup configured, editable in place, plus the account, pairing and about.
struct SettingsView: View {
    let model: ProbeModel
    let account: AccountModel
    let library: LibraryModel
    let companions: CompanionFinder
    let onSetup: () -> Void

    @AppStorage("pairedUDID") private var pairedUDID: String?
    @AppStorage("pairedName") private var pairedName: String?
    @AppStorage(RootView.showDiagnosticsKey) private var showDiagnostics = false
    @State private var confirmForget = false
    @State private var confirmSignOut = false
    @State private var checking = false

    var body: some View {
        NavigationStack {
            Form {
                RAVEScreenHeading("Settings")
                companionSection
                anisetteSection
                accountSection
                pairingSection
                shortcutsSection
                advancedSection
                aboutSection
            }
            .raveScreenTitle("Settings")
        }
    }

    // MARK: Sections

    private var companionSection: some View {
        Section {
            LabeledContent("Relay") {
                TextField("ip:49160, empty for LocalDevVPN",
                          text: Binding(get: { model.relay ?? "" },
                                        set: { model.relay = $0.trimmingCharacters(in: .whitespaces).isEmpty ? nil : $0.trimmingCharacters(in: .whitespaces) }))
                    .keyboardType(.URL)
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
                    .multilineTextAlignment(.trailing)
                    .font(.body.monospaced())
                    .textFieldTapTarget()
                    .wideFieldOnTV()
            }
            HStack {
                Button(companions.searching ? "Searching…" : "Find Companion") { Task { await companions.search() } }
                    .disabled(companions.searching)
                Spacer()
                if let relay = model.relay {
                    if checking { ProgressView() }
                    Button("Check \(relay)") {
                        checking = true
                        Task { _ = await companions.check(relay: relay); checking = false }
                    }
                    .disabled(checking)
                }
            }
            .focusRow()
            ForEach(companions.found) { companion in
                HStack {
                    VStack(alignment: .leading) {
                        Text(companion.name)
                        Text(companion.relay).font(.caption.monospaced()).foregroundStyle(.secondary)
                    }
                    Spacer()
                    if model.relay == companion.relay {
                        Image(systemName: "checkmark").foregroundStyle(.green)
                    } else {
                        Button("Use as Relay") { model.relay = companion.relay }
                            .buttonStyle(.borderless)
                    }
                }
                .focusRow()
            }
            if let error = companions.lastError { Note(text: error, kind: .warning) }
        } header: {
            Text("Companion")
        } footer: {
            Text("Apparition Companion relays the connection back to this \(ThisDevice.noun). On Tailscale, type its Tailscale address.")
        }
    }

    private var anisetteSection: some View {
        Section {
            TextField("http://your-server:6969",
                      text: Binding(get: { account.anisetteURL }, set: { account.anisetteURL = $0.trimmingCharacters(in: .whitespaces) }))
                .keyboardType(.URL)
                .textInputAutocapitalization(.never)
                .autocorrectionDisabled()
                .font(.body.monospaced())
                .textFieldTapTarget()
            ForEach(companions.found.filter { $0.anisetteURL != nil && $0.anisetteURL != account.anisetteURL }) { companion in
                Button("Use \(companion.name)'s anisette server") { account.anisetteURL = companion.anisetteURL ?? "" }
            }
        } header: {
            Text("Anisette Server")
        } footer: {
            Text("An anisette-v3 server for Apple sign-in. It sees this \(ThisDevice.noun)'s anisette identity and IP address, never your Apple ID or password.")
        }
    }

    private var accountSection: some View {
        Section {
            if let team = account.team {
                LabeledContent("Apple ID", value: account.email ?? "Signed in")
                LabeledContent("Team", value: "\(team.name ?? "Personal") (\(team.id))")
                Button("Sign Out", role: .destructive) { confirmSignOut = true }
                    .disabled(account.busy)
                    .confirmationDialog("Sign out?", isPresented: $confirmSignOut, titleVisibility: .visible) {
                        Button("Sign Out", role: .destructive) { account.signOut() }
                    } message: {
                        Text("The remembered password is deleted too, so refreshes stop until you sign in again.")
                    }
            } else {
                LabeledContent("Apple ID", value: account.lastAppleID ?? "Not signed in")
                Button("Sign In…", action: onSetup)
                if account.lastAppleID != nil && Keychain.hasPassword {
                    Button("Forget Apple ID", role: .destructive) { account.signOut() }
                }
            }
            NavigationLink {
                SigningView(account: account, library: library, onSignIn: onSetup)
            } label: {
                LabeledContent {
                    Text(library.signing.signerName(library.signing.defaultSigner))
                } label: {
                    Label("Signing", systemImage: "signature")
                }
            }
            NavigationLink {
                DeveloperSlotsView(account: account, library: library, onSignIn: onSetup)
            } label: {
                Label("Certificates & App IDs", systemImage: "person.badge.key")
            }
            Toggle(isOn: Binding(get: { account.rememberPassword }, set: { account.rememberPassword = $0 })) {
                Text("Remember password")
                Text(Keychain.hasPassword ? "A password is stored on this \(ThisDevice.noun)." : "Takes effect at the next sign-in.")
            }
        } header: {
            Text("Apple ID")
        } footer: {
            Text("A remembered password stays in this \(ThisDevice.noun)'s Keychain, never synced or sent anywhere but Apple, so refreshes can sign in on their own. Turning it off deletes it.")
        }
    }

    private var pairingSection: some View {
        Section {
            LabeledContent("Pairing", value: model.hasPairingFile ? "Saved" : "Not paired")
            if let pairedName { LabeledContent("Device", value: pairedName) }
            if let pairedUDID {
                LabeledContent("UDID") {
                    Text(pairedUDID).font(.caption.monospaced()).selectableText()
                }
            }
            if model.hasPairingFile {
                Button("Forget Pairing", role: .destructive) { confirmForget = true }
                    .disabled(model.busy)
                    .confirmationDialog("Forget this pairing?", isPresented: $confirmForget, titleVisibility: .visible) {
                        Button("Forget Pairing", role: .destructive) { model.forgetPairing() }
                    } message: {
                        Text("Installs, refreshes and JIT stop working until you pair again in Setup. Also remove Apparition from Settings > General > Remote Devices.")
                    }
            } else {
                Button("Pair in Setup…", action: onSetup)
            }
        } header: {
            Text("This \(ThisDevice.title)")
        }
    }

    private var shortcutsSection: some View {
        Section {
            Text("Free-account apps expire after 7 days. To keep them fresh without opening Apparition, make a personal automation in the Shortcuts app (for example daily, at a time the \(ThisDevice.noun) is usually on and the companion is reachable) that runs Apparition's refresh action.")
                .foregroundStyle(.secondary)
                .focusableText()
        } header: {
            Text("Shortcuts")
        } footer: {
            Text("Unattended refreshes need Remember password on and the companion reachable.")
        }
    }

    private var advancedSection: some View {
        Section("Advanced") {
            Button("Run Setup Again", action: onSetup)
            Toggle("Show Diagnostics tab", isOn: $showDiagnostics)
        }
    }

    private var aboutSection: some View {
        Section {
            LabeledContent("Version", value: Self.version)
            Text("Apparition installs apps from AltStore-format sources and GitHub releases, signs them with your own Apple ID on this \(ThisDevice.noun), keeps them refreshed and launches them with JIT. MIT licensed.")
                .font(.callout)
                .foregroundStyle(.secondary)
                .focusableText()
        } header: {
            Text("About")
        }
    }

    static var version: String {
        let info = Bundle.main.infoDictionary
        let short = info?["CFBundleShortVersionString"] as? String ?? "?"
        let build = info?["CFBundleVersion"] as? String ?? "?"
        return "\(short) (\(build))"
    }
}
