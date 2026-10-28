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
    @State private var sharingSigning = false
    @State private var receivingSigning = false

    var body: some View {
        NavigationStack {
            Form {
                RAVEScreenHeading("Settings")
                companionSection
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

    /// The connection opens on a screen of its own, like Signing: its two modes, companion list and
    /// anisette server are too much to lay out as one row of this form.
    private var companionSection: some View {
        Section {
            NavigationLink {
                ScrollView {
                    ConnectionView(model: model, account: account, companions: companions)
                        .padding(24)
                        .frame(maxWidth: 720, alignment: .leading)
                        .frame(maxWidth: .infinity)
                }
                .raveScreenTitle("Connection")
            } label: {
                LabeledContent {
                    Text(connectionSummary)
                } label: {
                    Label("Connection", systemImage: "network")
                }
            }
        } footer: {
            Text("How Apparition reaches this \(ThisDevice.noun)'s own services for installs, refreshes and JIT, and the anisette server that goes with it.")
        }
    }

    private var connectionSummary: String {
        switch model.connectionMode {
        case .companion: model.companionName ?? (model.relay == nil ? "Companion" : "Companion (address)")
        case .localVPN: "Local VPN"
        case nil: "Not set"
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
            if let email = account.email ?? account.lastAppleID {
                Button {
                    sharingSigning = true
                } label: {
                    Label("Share Signing with Another Device…", systemImage: "square.and.arrow.up")
                }
                .panel(isPresented: $sharingSigning) {
                    ShareSigningSheet(signing: library.signing, account: account, email: email, teamId: account.team?.id) { sharingSigning = false }
                }
            }
            Button {
                receivingSigning = true
            } label: {
                Label("Get Signing from Another Device…", systemImage: "square.and.arrow.down")
            }
            .panel(isPresented: $receivingSigning) {
                ReceiveSigningSheet(signing: library.signing, email: account.email ?? account.lastAppleID) { _ in receivingSigning = false }
            }
            Toggle(isOn: Binding(get: { account.rememberPassword }, set: { account.rememberPassword = $0 })) {
                Text("Remember password")
                Text(Keychain.hasPassword ? "A password is stored on this \(ThisDevice.noun)." : "Takes effect at the next sign-in.")
            }
        } header: {
            Text("Apple ID")
        } footer: {
            Text("Your devices and computers share one certificate: Share Signing hands this \(ThisDevice.noun)'s to another, and Get Signing takes another's, so a free Apple ID's two are never used up one per device. A remembered password stays in this \(ThisDevice.noun)'s Keychain, never synced or sent anywhere but Apple, so refreshes can sign in on their own. Turning it off deletes it.")
        }
    }

    private var pairingSection: some View {
        Section {
            LabeledContent("Pairing", value: model.pairingProblem == .rejected ? "No longer trusted" : model.hasPairingFile ? "Saved" : "Not paired")
            if let problem = model.pairingProblem {
                Text(PairingRecord.message(problem))
                    .font(.callout)
                    .foregroundStyle(.secondary)
            }
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
                        Text("Installs, refreshes and JIT stop working until you pair again in Setup. This deletes the pairing and the copy kept in the Keychain. Also remove Apparition from Settings > General > Remote Devices.")
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
