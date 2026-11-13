import DebugTrace
import DebugTraceUI
import RAVESupport
import RAVEUI
import SwiftUI

/// Everything Setup configured, editable in place, plus the account, pairing and about.
struct SettingsView: View {
    let model: ProbeModel
    let account: AccountModel
    let library: LibraryModel
    let companions: CompanionFinder
    let debugAccess: DebugAccessModel
    let onSetup: () -> Void

    @AppStorage("pairedUDID") private var pairedUDID: String?
    @AppStorage("pairedName") private var pairedName: String?
    @AppStorage(RootView.showDiagnosticsKey) private var showDiagnostics = false
    @AppStorage(UpdateNotice.enabledKey) private var notifyAboutUpdates = true
    @State private var verboseLog = VerboseLog.isOn
    @State private var verboseBytes = VerboseLog.bytes
    @State private var notificationsDenied = false
    @State private var confirmForget = false
    @State private var confirmSignOut = false
    @State private var sharingSigning = false
    @State private var receivingSigning = false

    var body: some View {
        NavigationStack {
            Form {
                RAVEScreenHeading("Settings")
                companionSection
                certificatesSection
                accountSection
                pairingSection
                shortcutsSection
                #if !os(tvOS)
                notificationsSection
                #endif
                advancedSection
                troubleshootingSection
                reportSection
                traceSection
                developerSection
                Section { RAVESupportCard(RAVESupportConfig(appName: "Apparition", appID: "com.illixion.apparition")) }
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

    /// Signing with your own certificate (.p12) and profiles: its own screen and section, apart
    /// from the Apple ID's. Nothing in it signs in or asks Apple for anything.
    private var certificatesSection: some View {
        Section {
            NavigationLink {
                SigningView(library: library)
            } label: {
                LabeledContent {
                    Text(library.signing.signerName(library.signing.defaultSigner))
                } label: {
                    Label("Certificates & Profiles", systemImage: "signature")
                }
            }
        } header: {
            Text("Your Certificates")
        } footer: {
            Text("Import a certificate (.p12) and provisioning profiles to sign offline, with no Apple ID and no requests to Apple. This also sets what signs apps by default.")
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
            Text("Apparition asks Apple only when you sign in, install or refresh with the Apple ID, or open Certificates & App IDs and press Load. Your devices and computers share one certificate: Share Signing hands this \(ThisDevice.noun)'s to another, and Get Signing takes another's, so a free Apple ID's two are never used up one per device. A remembered password stays in this \(ThisDevice.noun)'s Keychain, never synced or sent anywhere but Apple, so refreshes can sign in on their own. Turning it off deletes it.")
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

    #if !os(tvOS)
    private var notificationsSection: some View {
        Section {
            Toggle("Notify about available updates", isOn: $notifyAboutUpdates)
                .onChange(of: notifyAboutUpdates) { _, on in
                    guard on else { return }
                    Task { await UpdateNotice.requestAccess(); notificationsDenied = await UpdateNotice.isDenied() }
                }
        } header: {
            Text("Notifications")
        } footer: {
            Text(notificationsDenied && notifyAboutUpdates
                 ? "Notifications are off for Apparition in the system's Settings, so this can't show anything."
                 : "A quiet notification, with no sound, once for each update that a check finds. Apparition looks for updates only when you ask (Check for Update, Reload Sources) or when the Refresh Apps shortcut's Check for Updates is on.")
        }
        .task { notificationsDenied = await UpdateNotice.isDenied() }
    }
    #endif

    private var advancedSection: some View {
        Section("Advanced") {
            Button("Run Setup Again", action: onSetup)
            Toggle("Show Diagnostics tab", isOn: $showDiagnostics)
        }
    }

    /// The record of a sign-in's requests to Apple (Probe/VerboseLog.swift): off until chosen.
    private var troubleshootingSection: some View {
        Section {
            Toggle("Record sign-in requests", isOn: $verboseLog)
                .onChange(of: verboseLog) { _, on in
                    VerboseLog.set(on)
                    verboseBytes = VerboseLog.bytes
                }
            if verboseBytes > 0 {
                #if !os(tvOS)
                ShareLink("Share the record", item: VerboseLog.url)
                #endif
                Button("Clear the record", role: .destructive) {
                    VerboseLog.clear()
                    verboseBytes = 0
                }
            }
        } header: {
            Text("Troubleshooting")
        } footer: {
            #if os(tvOS)
            Text("For finding out what Apple changed. Writes each request a sign-in sends to Apple, and the answer, as structure and sizes only: no password, token, key or anisette value. Apple's error text can still name a team or an app ID, so read it before sharing it. A connected computer can read it with the debug server's verboseLog. Off until you turn it on.")
            #else
            Text("For finding out what Apple changed. Writes each request a sign-in sends to Apple, and the answer, as structure and sizes only: no password, token, key or anisette value. Apple's error text can still name a team or an app ID, so read apparition-verbose.log (in Apparition's folder in the Files app) before sharing it. Off until you turn it on.")
            #endif
        }
        .onAppear { verboseBytes = VerboseLog.bytes }
    }

    /// A problem report for Apparition's public issue tracker: a trace the person reads through
    /// first, redacted as a released app's is, and encrypted to the maintainer so the file can
    /// be attached to a public issue (docs/design/PUBLIC_DEBUGGING_PLAN.md, section 1).
    private var reportSection: some View {
        Section {
            DebugTraceReportButton("Report a Problem…", configuration: ProblemReport.configuration)
        } footer: {
            Text("Collects Apparition's recent log, what it was doing and its state, with personal details left out, and shows you every file before anything leaves this \(ThisDevice.noun). You choose what stays in. The report is encrypted so that only Apparition's maintainer can open it, even attached to a public issue on GitHub. Nothing is sent on its own.")
        }
    }

    /// Letting a computer the person paired debug this copy (DebugAccess.swift).
    private var developerSection: some View {
        Section {
            NavigationLink {
                DebugAccessView(model: debugAccess)
            } label: {
                LabeledContent {
                    Text(debugAccessSummary)
                } label: {
                    Label("Debugging from a Computer", systemImage: "laptopcomputer.and.iphone")
                }
            }
        } header: {
            Text("Developer")
        } footer: {
            Text("For finding a problem with a computer's help, or working on Apparition itself. Off unless you turn it on.")
        }
    }

    private var debugAccessSummary: String {
        switch debugAccess.status {
        case .off: "Off"
        case .starting: "Starting…"
        case .failed: "Couldn't start"
        case .on(let until): until == nil ? "On" : "On for an hour"
        }
    }

    /// A debug trace of this run, sent to the Apparition Store that built this copy. Only a copy
    /// with an upload destination offers it (build-and-sign and `apparition build` embed one). Copies
    /// from the release site log without redaction and can't preview a trace before it leaves, so
    /// they wait for Report a Problem (docs/design/PUBLIC_DEBUGGING_PLAN.md).
    @ViewBuilder
    private var traceSection: some View {
        if DebugTrace.canUpload {
            Section {
                DebugTraceButton()
            } footer: {
                Text("This copy was built by an Apparition Store, so a debug trace of this run (its log, breadcrumbs and app state) goes back to that Store, where you can read it next to the build. Copies from the release site don't offer this.")
            }
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
