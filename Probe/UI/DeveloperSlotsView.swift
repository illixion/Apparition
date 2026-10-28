import RAVEUI
import SwiftUI

/// The signed-in account's development certificates and App IDs, and freeing them. A free
/// account has two certificate slots and ten new App IDs a week; when they're used up, signing
/// fails (error 7460 for certificates) until the user revokes or deletes one here. For
/// certificates the better way comes first: Apparition installs share one (Share Signing, Get
/// Signing), so revoking is for what other tools made or what nobody uses.
struct DeveloperSlotsView: View {
    let account: AccountModel
    let library: LibraryModel
    let onSignIn: () -> Void

    @State private var certificates: [DeveloperSlots.Certificate]?
    @State private var appIDs: DeveloperSlots.AppIDs?
    @State private var certificateError: String?
    @State private var appIDError: String?
    @State private var loading = false
    @State private var working: String?
    @State private var actionError: String?
    @State private var pendingRevoke: DeveloperSlots.Certificate?
    @State private var pendingDelete: DeveloperSlots.AppID?
    @State private var sharing = false
    @State private var receiving = false

    var body: some View {
        Group {
            if let request = account.twoFactor {
                ScrollView {
                    VStack(alignment: .leading, spacing: 16) {
                        RAVEScreenHeading("Certificates & App IDs")
                        Text("Apple wants to confirm it's you before Apparition lists the certificates.")
                            .foregroundStyle(.secondary)
                        TwoFactorPrompt(account: account, request: request)
                    }
                    .padding(24)
                    .frame(maxWidth: 720, alignment: .leading)
                    .frame(maxWidth: .infinity)
                }
            } else if account.team == nil && !loading {
                notSignedIn
            } else {
                list
            }
        }
        .hostsTwoFactorPrompt(account)
        .raveScreenTitle("Certificates & App IDs")
        .toolbar {
            ToolbarItem(placement: .primaryAction) {
                if loading {
                    ProgressView()
                } else {
                    Button {
                        Task { await load() }
                    } label: {
                        Label("Refresh", systemImage: "arrow.clockwise")
                    }
                    .disabled(working != nil)
                }
            }
        }
        .task { await load() }
    }

    /// No session: sign in here with the remembered password, which asks for a 2FA code on this
    /// screen if Apple wants one, or in Setup without one.
    private var notSignedIn: some View {
        let remembered = account.rememberPassword && Keychain.hasPassword
        return ContentUnavailableView {
            Label("Not Signed In", systemImage: "person.crop.circle.badge.questionmark")
        } description: {
            if account.autoSignInPaused {
                Text("Apple asked for a two-factor code when Apparition last signed in on its own. Sign in to enter one and see the certificates and App IDs your Apple ID uses.")
            } else if let error = account.lastError {
                Text(error)
            } else {
                Text("Sign in with your Apple ID to see the certificates and App IDs it uses.")
            }
        } actions: {
            if remembered {
                Button("Sign In") {
                    Task {
                        if await account.signInWithRememberedPassword(byPerson: true) { await load() }
                    }
                }
                .buttonStyle(.borderedProminent)
                .disabled(account.busy)
            } else {
                Button("Sign In…", action: onSignIn)
                    .buttonStyle(.borderedProminent)
            }
        }
    }

    private var list: some View {
        List {
            RAVEScreenHeading("Certificates & App IDs")
            if let working {
                Section {
                    HStack(spacing: 12) {
                        ProgressView()
                        Text(working).foregroundStyle(.secondary)
                    }
                }
            }
            if let actionError {
                Section { Note(text: actionError, kind: .error) }
            }
            certificatesSection
            appIDsSection
        }
        .refreshable { await load() }
    }

    // MARK: Certificates

    private var certificatesSection: some View {
        Section {
            if let certificateError {
                Note(text: certificateError, kind: .error)
            } else if let certificates {
                if certificates.isEmpty {
                    Text("No development certificates. Apparition makes one at the next install or refresh.")
                        .foregroundStyle(.secondary)
                }
                ForEach(certificates) { certificate in
                    certificateRow(certificate)
                }
                sharingRow(certificates)
            } else {
                loadingRow
            }
        } header: {
            HStack {
                Text("Development Certificates")
                Spacer()
                if let certificates {
                    Text("\(certificates.count) of 2 used on a free account").textCase(nil)
                }
            }
        } footer: {
            Text("A free Apple ID can hold two development certificates, across every platform and tool (Xcode, SideStore, Apparition). A certificate signs only where its private key is, so Apparition hands its key between your devices and computers, and they all sign with one: Share Signing sends this \(ThisDevice.noun)'s, Get Signing takes another's. If signing fails with \"maximum number of certificates\" (error 7460), get Apparition's key from the device that has it before revoking anything; revoke only what you no longer use.")
        }
    }

    /// Sharing instead of making (or revoking) one: give this install's key to the others, or
    /// take another Apparition's when this one has none on the account.
    @ViewBuilder
    private func sharingRow(_ certificates: [DeveloperSlots.Certificate]) -> some View {
        let email = account.email ?? account.lastAppleID
        if certificates.contains(where: { $0.inUseHere == true }), let email {
            VStack(alignment: .leading, spacing: 10) {
                Text("Your other devices and computers can sign with this \(ThisDevice.noun)'s certificate too, rather than each making its own.")
                    .foregroundStyle(.secondary)
                Button("Share Signing with Another Device…") { sharing = true }
                    .buttonStyle(.bordered)
            }
            .focusRow()
            .panel(isPresented: $sharing) {
                ShareSigningSheet(signing: library.signing, account: account, email: email, teamId: account.team?.id) { sharing = false }
            }
        } else if certificates.contains(where: { $0.madeByApparition }) {
            VStack(alignment: .leading, spacing: 10) {
                Note(text: "Apparition on another device or computer has a certificate here. Get its key and this \(ThisDevice.noun) signs with it too, with no new certificate and nothing revoked.", kind: .info)
                Button("Get Signing from Another Device…") { receiving = true }
                    .buttonStyle(.borderedProminent)
            }
            .focusRow()
            .panel(isPresented: $receiving) {
                ReceiveSigningSheet(signing: library.signing, email: email) { imported in
                    receiving = false
                    if imported { Task { await load() } }
                }
            }
        }
    }

    private func certificateRow(_ certificate: DeveloperSlots.Certificate) -> some View {
        HStack(alignment: .top, spacing: 16) {
            Image(systemName: icon(for: certificate.platform))
                .font(.title2)
                .frame(width: 40)
            VStack(alignment: .leading, spacing: 6) {
                HStack(spacing: 8) {
                    Text(certificate.name ?? "Development certificate").font(.headline)
                    if certificate.inUseHere == true {
                        StatusPill(text: "Signs on this \(ThisDevice.noun)", systemImage: "checkmark.seal.fill", tint: .green)
                    } else if certificate.madeByApparition {
                        StatusPill(text: "Apparition", systemImage: "sparkles", tint: .purple)
                    }
                }
                Text(certificateDetails(certificate))
                    .font(.callout)
                    .foregroundStyle(.secondary)
                if let expires = Self.date(certificate.expires) {
                    Text(expires < Date() ? "Expired \(expires.relativeToNow)" : "Expires \(expires.relativeToNow)")
                        .font(.callout)
                        .foregroundStyle(.secondary)
                }
            }
            Spacer()
            Button("Revoke", role: .destructive) { pendingRevoke = certificate }
                .confirmation("Revoke \(certificate.name ?? "this certificate")?", item: $pendingRevoke, for: certificate.id) { certificate in
                    Button("Revoke Certificate", role: .destructive) { revoke(certificate) }
                } message: { certificate in
                    Text(revokeMessage(certificate))
                }
                .buttonStyle(.bordered)
                .disabled(certificate.serialNumber == nil || working != nil)
        }
        .focusRow()
        .padding(.vertical, 6)
    }

    private func certificateDetails(_ certificate: DeveloperSlots.Certificate) -> String {
        var parts: [String] = []
        if let machine = certificate.machineName { parts.append("made on \(machine)") }
        if let platform = certificate.platform { parts.append(Self.platformName(platform)) }
        if let type = certificate.typeName, type != certificate.name { parts.append(type) }
        if let status = certificate.status { parts.append(status) }
        return parts.isEmpty ? "No details" : parts.joined(separator: " · ")
    }

    private func revokeMessage(_ certificate: DeveloperSlots.Certificate) -> String {
        var text = "Apps signed with this certificate stop opening until they're signed again. Apparition makes a new certificate at the next install or refresh, and refreshing re-signs your library apps with it."
        if certificate.inUseHere == true {
            text += " It's the one this \(ThisDevice.noun) signs with, so your library apps stop opening until you refresh them, and so do those of any device or computer you shared it with."
        } else if certificate.madeByApparition {
            text += " This \(ThisDevice.noun) doesn't sign with it: Apparition on another device or computer probably does, and its apps stop opening too. Rather than revoke it, get its key from that device (Get Signing from Another Device), and both sign with it."
        } else {
            text += " It wasn't made by Apparition, so the tool that made it (\(certificate.machineName ?? "Xcode or another sideloader")) will need to make a new one too."
        }
        return text
    }

    private func revoke(_ certificate: DeveloperSlots.Certificate) {
        guard let serial = certificate.serialNumber else { return }
        run("Revoking \(certificate.name ?? "certificate")…") {
            try await DeveloperSlots.revokeCertificate(serialNumber: serial)
        }
    }

    // MARK: App IDs

    private var appIDsSection: some View {
        Section {
            if let appIDError {
                Note(text: appIDError, kind: .error)
            } else if let appIDs {
                if let available = appIDs.availableQuantity, let max = appIDs.maxQuantity {
                    Label("\(available) of \(max) available this week", systemImage: "number.circle")
                        .font(.headline)
                        .foregroundStyle(available == 0 ? .orange : .primary)
                }
                if appIDs.appIds.isEmpty {
                    Text("No App IDs registered.").foregroundStyle(.secondary)
                }
                ForEach(appIDs.appIds) { appID in
                    appIDRow(appID)
                }
            } else {
                loadingRow
            }
        } header: {
            Text("App IDs")
        } footer: {
            Text("Each app you sign registers an App ID (and one per extension). A free Apple ID may register ten a week. On a free account Apple may refuse to delete an App ID; it frees its slot on its own 7 days after it was made.")
        }
    }

    private func appIDRow(_ appID: DeveloperSlots.AppID) -> some View {
        let app = libraryApp(for: appID)
        return HStack(alignment: .top, spacing: 16) {
            Image(systemName: "app.badge")
                .font(.title2)
                .frame(width: 40)
            VStack(alignment: .leading, spacing: 6) {
                Text(appID.name).font(.headline)
                Text(appID.identifier)
                    .font(.callout.monospaced())
                    .foregroundStyle(.secondary)
                    .selectableText()
                HStack(spacing: 8) {
                    if let app {
                        StatusPill(text: "Used by \(app.name)", systemImage: "square.stack.3d.up", tint: .blue)
                    }
                    if let expires = Self.date(appID.expires) {
                        Text(expires < Date() ? "Expired \(expires.relativeToNow)" : "Expires \(expires.relativeToNow)")
                            .font(.callout)
                            .foregroundStyle(.secondary)
                    }
                }
            }
            Spacer()
            Button("Delete", role: .destructive) { pendingDelete = appID }
                .confirmation("Delete \(appID.identifier)?", item: $pendingDelete, for: appID.id) { appID in
                    Button("Delete App ID", role: .destructive) { delete(appID) }
                } message: { appID in
                    Text(deleteMessage(appID))
                }
                .buttonStyle(.bordered)
                .disabled(working != nil)
        }
        .focusRow()
        .padding(.vertical, 6)
    }

    /// The library app signed under this App ID: its own, or one of its extensions'.
    private func libraryApp(for appID: DeveloperSlots.AppID) -> LibraryModel.InstalledApp? {
        library.installed.first { app in
            guard let bundleId = app.installedBundleId else { return false }
            return appID.identifier == bundleId || appID.identifier.hasPrefix(bundleId + ".")
        }
    }

    private func deleteMessage(_ appID: DeveloperSlots.AppID) -> String {
        var text = "On a free account Apple may refuse this; an App ID frees its slot by itself 7 days after it was made."
        if let app = libraryApp(for: appID) {
            text = "\(app.name) in your library uses it, and the next refresh registers it again. " + text
        }
        return text
    }

    private func delete(_ appID: DeveloperSlots.AppID) {
        run("Deleting \(appID.identifier)…") {
            try await DeveloperSlots.deleteAppID(appID.appIdId)
        }
    }

    // MARK: Loading

    private var loadingRow: some View {
        HStack(spacing: 12) {
            ProgressView()
            Text("Asking Apple…").foregroundStyle(.secondary)
        }
    }

    private func load() async {
        guard !loading else { return }
        loading = true
        defer { loading = false }
        // A remembered password signs in as a refresh would; Apple's 2FA step, if it asks for
        // one, shows on this screen (or the sign-in waits for the person, if they already
        // left a code unentered).
        if account.team == nil, !(await account.signInWithRememberedPassword()) { return }
        do {
            certificates = try await DeveloperSlots.certificates()
            certificateError = nil
        } catch {
            certificateError = "\(error)"
        }
        do {
            appIDs = try await DeveloperSlots.appIDs()
            appIDError = nil
        } catch {
            appIDError = "\(error)"
        }
    }

    /// Runs a revoke or delete, shows Apple's error text if it refuses, then reloads.
    private func run(_ label: String, _ body: @escaping () async throws -> Void) {
        working = label
        actionError = nil
        Task {
            do {
                try await body()
            } catch {
                actionError = "\(error)"
            }
            working = nil
            await load()
        }
    }

    // MARK: Helpers

    private func icon(for platform: String?) -> String {
        switch platform?.lowercased() {
        case "mac", "macos": "laptopcomputer"
        case "tvos": "appletv"
        case "watchos": "applewatch"
        default: "checkmark.seal"
        }
    }

    static func platformName(_ platform: String) -> String {
        switch platform.lowercased() {
        case "ios": "iOS"
        case "mac", "macos": "macOS"
        case "tvos": "tvOS"
        case "watchos": "watchOS"
        default: platform
        }
    }

    /// Apple's dates arrive as plist XML dates ("2026-10-18T12:00:00Z").
    static func date(_ text: String?) -> Date? {
        guard let text else { return nil }
        return (try? Date(text, strategy: .iso8601)) ?? Date.iso(text)
    }
}

/// A button for next to a slot error: opens the screen above in a sheet.
struct ManageSlotsButton: View {
    let account: AccountModel
    let library: LibraryModel
    let onSignIn: () -> Void
    @State private var showing = false

    var body: some View {
        Button {
            showing = true
        } label: {
            Label("Manage Certificates & App IDs", systemImage: "person.badge.key")
        }
        .buttonStyle(.bordered)
        .panel(isPresented: $showing) {
            NavigationStack {
                DeveloperSlotsView(account: account, library: library, onSignIn: {
                    showing = false
                    onSignIn()
                })
                .toolbar {
                    ToolbarItem(placement: .cancellationAction) {
                        Button("Done") { showing = false }
                    }
                }
            }
            .sheetFrame(minWidth: 820, minHeight: 640)
        }
    }
}
