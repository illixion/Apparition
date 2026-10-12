import SwiftUI

/// The signed-in account's development certificates and App IDs, and freeing them. A free
/// account has two certificate slots and ten new App IDs a week; when they're used up, signing
/// fails (error 7460 for certificates) until the user revokes or deletes one here.
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

    var body: some View {
        Group {
            if account.team == nil && !loading {
                ContentUnavailableView {
                    Label("Not Signed In", systemImage: "person.crop.circle.badge.questionmark")
                } description: {
                    Text("Sign in with your Apple ID to see the certificates and App IDs it uses.")
                } actions: {
                    Button("Sign In…", action: onSignIn)
                        .buttonStyle(.borderedProminent)
                }
            } else {
                list
            }
        }
        .navigationTitle("Certificates & App IDs")
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
        .confirmationDialog("Revoke \(pendingRevoke?.name ?? "this certificate")?",
                            isPresented: Binding(get: { pendingRevoke != nil }, set: { if !$0 { pendingRevoke = nil } }),
                            titleVisibility: .visible, presenting: pendingRevoke) { certificate in
            Button("Revoke Certificate", role: .destructive) { revoke(certificate) }
        } message: { certificate in
            Text(revokeMessage(certificate))
        }
        .confirmationDialog("Delete \(pendingDelete?.identifier ?? "this App ID")?",
                            isPresented: Binding(get: { pendingDelete != nil }, set: { if !$0 { pendingDelete = nil } }),
                            titleVisibility: .visible, presenting: pendingDelete) { appID in
            Button("Delete App ID", role: .destructive) { delete(appID) }
        } message: { appID in
            Text(deleteMessage(appID))
        }
    }

    private var list: some View {
        List {
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
            Text("A free Apple ID can hold two development certificates, across every platform and tool (Xcode, SideStore, another Apparition). Each one's private key stays on the device that made it, so Apparition can only sign with the one it made here. If signing fails with \"maximum number of certificates\" (error 7460), revoke one you no longer use.")
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
                .buttonStyle(.bordered)
                .disabled(certificate.serialNumber == nil || working != nil)
        }
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
            text += " It's the one this \(ThisDevice.noun) signs with, so your library apps stop opening until you refresh them."
        } else if certificate.madeByApparition {
            text += " This \(ThisDevice.noun) doesn't sign with it: Apparition on another device or computer probably does, and its apps stop opening too."
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
                    .textSelection(.enabled)
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
                .buttonStyle(.bordered)
                .disabled(working != nil)
        }
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
        // A remembered password signs in quietly, as a refresh would.
        if account.team == nil, !(await account.signInSilently()) { return }
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
        .sheet(isPresented: $showing) {
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
            .frame(minWidth: 820, minHeight: 640)
        }
    }
}
