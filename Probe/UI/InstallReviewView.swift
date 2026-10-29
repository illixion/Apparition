import RAVEUI
import SwiftUI

/// Shared by iOS, iPadOS, visionOS and tvOS; shown after the archive is read and before signing.
struct InstallReviewView: View {
    let library: LibraryModel
    let review: LibraryModel.InstallReview
    @State private var target = ""
    @State private var selected: Set<String> = []
    @State private var dataChoice = ""
    private var discard: Bool { dataChoice == "discard" }

    private var destination: ProbeModel.DeviceApp? { review.installed.first { $0.bundleId == target } }
    private var cost: Int {
        guard review.appleID else { return 0 }
        let registered = Set(review.candidates.map(\.identifier))
        let childIDs = selected.map { target + $0.dropFirst(review.inventory.id.count) }
        return ([target] + childIDs).filter { !registered.contains($0) }.count
    }
    private var problem: String? {
        if review.reserved.contains(target) { return "This ID is reserved for Apparition, an extension, or a pending data transfer." }
        if let destination, let team = review.signingTeam, let oldTeam = destination.teamId, team != oldTeam, !discard {
            return "The installed copy belongs to another signing team. Choose Start with empty data to delete it before installing."
        }
        for child in selected {
            guard child.hasPrefix(review.inventory.id + ".") else { return "This extension's ID does not belong to the app." }
            let id = target + child.dropFirst(review.inventory.id.count)
            if (review.reserved.contains(id) && review.extensionOwners[id] != target) || review.owners[id] != nil { return "The ID for this extension is assigned to another app. Deselect it or choose another App ID." }
        }
        if let available = review.available, cost > available { return "This selection needs \(cost) new App IDs; your account has \(available) available. Remove extensions or reuse another ID." }
        return nil
    }

    var body: some View {
        NavigationStack {
            Form {
                RAVEScreenHeading("Review \(review.inventory.name)")
                if review.appleID {
                    Section {
                        Picker("Install under App ID", selection: $target) {
                            Text(review.target).tag(review.target)
                            ForEach(review.candidates.filter { $0.identifier != review.target && !review.reserved.contains($0.identifier) }) { candidate in
                                Text("\(candidate.name) — \(candidate.identifier)").tag(candidate.identifier)
                            }
                        }
                        if let owner = review.owners[target] {
                            Note(text: "This ID is assigned to \(owner). Installing replaces that assignment and stops refreshing the previous app.", kind: .warning)
                        }
                        Text("\(cost) new App ID\(cost == 1 ? "" : "s")\(review.available.map { "; \($0) available" } ?? "")")
                    } header: {
                        Text("App Identity")
                    } footer: {
                        Text("Reuse an ID already registered with your Apple account, or use this app's ID. The chosen identity stays assigned for updates and refreshes. Assignments are stored on this device.")
                    }
                }
                if !review.appleID {
                    Section { LabeledContent("Install under App ID", value: review.target) }
                }
                Section {
                    if review.inventory.extensions.isEmpty { Text("This app has no extensions.").foregroundStyle(.secondary) }
                    ForEach(review.inventory.extensions) { item in
                        Toggle(isOn: Binding(get: { selected.contains(item.id) }, set: { enabled in
                            if enabled { selected.insert(item.id) } else { selected.remove(item.id) }
                        })) {
                            VStack(alignment: .leading) {
                                Text(item.name)
                                Text(item.id).font(.caption).foregroundStyle(.secondary)
                                if review.unsupported.contains(item.id) {
                                    Text(review.appleID ? "ExtensionKit signing requires imported provisioning profiles." : "No imported profile covers this extension on this device.").font(.caption)
                                }
                            }
                        }
                        .disabled(review.unsupported.contains(item.id))
                    }
                } header: {
                    Text("App Extensions")
                } footer: {
                    Text("Each selected extension can use another App ID. Leave optional widgets, sharing helpers, and Top Shelf extensions off to save slots. Features supplied by excluded extensions will be unavailable. New extensions stay off during automatic refreshes.")
                }
                if destination != nil || review.dataSource != nil {
                    Section {
                        Picker("App data", selection: $dataChoice) {
                            Text("Choose how to handle app data").tag("")
                            Text("Keep app data").tag("keep")
                            Text("Start with empty data").tag("discard")
                        }
                        if let destination {
                            Note(text: "This installs over \(destination.name) (\(target)). Keeping data lets the replacement app access that copy's files. Starting empty deletes that copy and its data before installing.", kind: .warning)
                        }
                    } header: {
                        Text("App Data")
                    } footer: {
                        Text("When changing IDs, Keep copies Documents and Library to the new copy, then removes the old copy only after every file arrives. Keychain items and app group data cannot be copied or cleared here and may remain after deletion. Starting empty leaves a copy under a different ID in place.")
                    }
                }
                Section {
                    if let problem { Note(text: problem, kind: .warning) }
                    Button(discard && destination != nil ? "Delete Existing Data and Install" : "Install \(review.inventory.name)", role: discard && destination != nil ? .destructive : nil) {
                        library.finishInstallReview(.init(target: target, extensions: selected.sorted(), discard: discard))
                    }
                    .disabled(target.isEmpty || problem != nil || ((destination != nil || review.dataSource != nil) && dataChoice.isEmpty))
                    Button("Cancel", role: .cancel) { library.finishInstallReview(nil) }
                }
            }
            .raveScreenTitle("Review Install")
        }
        .sheetFrame(minWidth: 600, minHeight: 520)
        .onChange(of: target) { _, _ in dataChoice = "" }
        .onAppear {
            target = review.target
            selected = Set(review.selected.filter { id in
                review.inventory.extensions.contains { $0.id == id && !review.unsupported.contains(id) }
            })
        }
    }
}
