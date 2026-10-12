import SwiftUI

/// Sources and the apps they offer, with Install. A source is an AltStore/SideStore JSON URL
/// or `github:owner/repo` (the newest release with a matching .ipa asset).
struct BrowseView: View {
    let model: ProbeModel
    let account: AccountModel
    let library: LibraryModel
    let onSetup: () -> Void

    @State private var addingSource = false
    @State private var refreshing = false
    @State private var pendingRemoval: Sources.Source?

    var body: some View {
        NavigationStack {
            Group {
                if library.sources.isEmpty {
                    ContentUnavailableView {
                        Label("No Sources", systemImage: "tray")
                    } description: {
                        Text("Add an AltStore or SideStore source URL, or github:owner/repo for apps published only as release assets (UTM's \(ThisDevice.os) build is github:utmapp/UTM).")
                    } actions: {
                        Button("Add Source") { addingSource = true }
                            .buttonStyle(.borderedProminent)
                    }
                } else {
                    list
                }
            }
            .navigationTitle("Browse")
            .toolbar {
                ToolbarItemGroup(placement: .primaryAction) {
                    Button {
                        refreshing = true
                        Task { await library.refreshSources(); refreshing = false }
                    } label: {
                        if refreshing { ProgressView() } else { Label("Reload Sources", systemImage: "arrow.clockwise") }
                    }
                    .disabled(refreshing || library.sources.isEmpty)
                    .help("Fetch every source again")
                    Button {
                        addingSource = true
                    } label: {
                        Label("Add Source", systemImage: "plus")
                    }
                    .help("Add a source")
                }
            }
            .sheet(isPresented: $addingSource) {
                AddSourceSheet(library: library)
            }
            .confirmationDialog("Remove \(pendingRemoval?.name ?? pendingRemoval?.id ?? "source")?",
                                isPresented: Binding(get: { pendingRemoval != nil }, set: { if !$0 { pendingRemoval = nil } }),
                                titleVisibility: .visible, presenting: pendingRemoval) { source in
                Button("Remove Source", role: .destructive) { library.removeSource(source.id) }
            } message: { _ in
                Text("Apps you installed from it stay in your library and keep being refreshed.")
            }
        }
    }

    private var list: some View {
        List {
            if library.busy, let status = library.status {
                Section {
                    HStack(spacing: 12) {
                        ProgressView()
                        Text(status).foregroundStyle(.secondary)
                    }
                }
            } else if let status = library.status, status.contains("error") {
                Section {
                    Note(text: status, kind: .error)
                    if DeveloperSlots.isSlotError(status) {
                        ManageSlotsButton(account: account, library: library, onSignIn: onSetup)
                    }
                }
            }
            ForEach(library.sources) { source in
                Section {
                    if let error = source.error {
                        Note(text: error, kind: .error)
                    }
                    if source.apps.isEmpty && source.error == nil {
                        Text(source.fetchedAt == nil ? "Not fetched yet" : "This source lists no apps")
                            .foregroundStyle(.secondary)
                    }
                    ForEach(source.apps) { app in
                        row(app, in: source)
                    }
                } header: {
                    header(source)
                }
            }
        }
    }

    private func header(_ source: Sources.Source) -> some View {
        HStack(alignment: .firstTextBaseline) {
            VStack(alignment: .leading, spacing: 2) {
                Text(source.name ?? source.id)
                    .font(.title3.weight(.semibold))
                    .foregroundStyle(.primary)
                Text(source.id)
                    .font(.caption.monospaced())
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .truncationMode(.middle)
            }
            Spacer()
            Button(role: .destructive) {
                pendingRemoval = source
            } label: {
                Label("Remove", systemImage: "trash")
                    .labelStyle(.iconOnly)
            }
            .buttonStyle(.borderless)
            .help("Remove this source")
        }
        .textCase(nil)
    }

    private func row(_ app: Sources.App, in source: Sources.Source) -> some View {
        let installed = library.installed.first { $0.id == app.id }
        return HStack(spacing: 16) {
            AppIconView(url: app.iconURL.flatMap(URL.init(string:)), name: app.name, size: 56)
            VStack(alignment: .leading, spacing: 4) {
                Text(app.name).font(.headline)
                Text(details(app))
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
            Spacer()
            installButton(app, source: source, installed: installed)
        }
        .padding(.vertical, 6)
    }

    private func details(_ app: Sources.App) -> String {
        var parts = [app.version]
        if let developer = app.developer { parts.append(developer) }
        if let size = app.size { parts.append(ByteCountFormatter.string(fromByteCount: Int64(size), countStyle: .file)) }
        if let minOS = app.minOSVersion { parts.append("needs \(minOS)+") }
        return parts.joined(separator: " · ")
    }

    @ViewBuilder
    private func installButton(_ app: Sources.App, source: Sources.Source, installed: LibraryModel.InstalledApp?) -> some View {
        let label: String = switch installed {
        case nil: "Install"
        case let record? where record.version != app.version: "Update"
        default: "Reinstall"
        }
        HStack(spacing: 12) {
            if installed?.version == app.version {
                Label("Installed", systemImage: "checkmark.circle.fill")
                    .labelStyle(.iconOnly)
                    .foregroundStyle(.green)
                    .help("Installed")
            }
            let button = Button(label) {
                // Free accounts can't sign app extensions, so they're dropped.
                Task { await library.install(sourceId: source.id, appId: app.id, stripExtensions: installed?.stripExtensions ?? true) }
            }
            .disabled(library.busy || model.busy)
            if installed == nil {
                button.buttonStyle(.borderedProminent)
            } else {
                button.buttonStyle(.bordered)
            }
        }
    }
}

/// Adding a source: a URL or github:owner/repo, plus the release-asset options github: takes.
struct AddSourceSheet: View {
    let library: LibraryModel
    @Environment(\.dismiss) private var dismiss
    @State private var url = ""
    @State private var assetPattern = ThisDevice.utmAssetPattern
    @State private var prereleases = true
    @State private var adding = false

    private var trimmed: String { url.trimmingCharacters(in: .whitespacesAndNewlines) }
    private var isGitHub: Bool { trimmed.hasPrefix("github:") }
    private var isValid: Bool {
        if isGitHub { return trimmed.dropFirst("github:".count).split(separator: "/").count == 2 }
        return URL(string: trimmed).map { $0.scheme == "https" || $0.scheme == "http" } ?? false
    }

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    TextField("https://example.com/source.json or github:owner/repo", text: $url)
                        .keyboardType(.URL)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .textFieldTapTarget()
                        .onSubmit(add)
                } header: {
                    Text("Source")
                } footer: {
                    Text("An AltStore or SideStore source URL, or github: followed by a repository for apps published only as GitHub release assets.")
                }
                if isGitHub {
                    Section {
                        TextField("Asset name contains", text: $assetPattern)
                            .textInputAutocapitalization(.never)
                            .autocorrectionDisabled()
                            .textFieldTapTarget()
                        Toggle("Include prereleases", isOn: $prereleases)
                    } header: {
                        Text("Release asset")
                    } footer: {
                        Text("The newest release with an .ipa whose name contains this text is used; the shortest match wins, so \"visionOS\" picks UTM-visionOS.ipa over UTM-Remote-visionOS.ipa. A name ending in .ipa matches exactly.")
                    }
                }
                Section("Suggestions") {
                    Button("UTM for \(ThisDevice.os) (github:utmapp/UTM)") {
                        url = "github:utmapp/UTM"
                        assetPattern = ThisDevice.utmAssetPattern
                        prereleases = true
                    }
                }
            }
            .navigationTitle("Add Source")
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") { dismiss() }
                }
                ToolbarItem(placement: .confirmationAction) {
                    if adding {
                        ProgressView()
                    } else {
                        Button("Add", action: add).disabled(!isValid)
                    }
                }
            }
        }
        .frame(minWidth: 640, minHeight: 520)
    }

    private func add() {
        guard isValid, !adding else { return }
        adding = true
        let pattern = assetPattern.trimmingCharacters(in: .whitespaces)
        Task {
            await library.addSource(trimmed, assetPattern: isGitHub && !pattern.isEmpty ? pattern : nil,
                                    prereleases: isGitHub && prereleases)
            adding = false
            dismiss()
        }
    }
}
