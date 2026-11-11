import RAVEUI
import SwiftUI

/// Shared by iOS, iPadOS, visionOS and tvOS; shown after the archive is read and before signing.
struct InstallReviewView: View {
    let library: LibraryModel
    let review: LibraryModel.InstallReview
    var embedded = false
    @State private var target = ""
    @State private var selected: Set<String> = []
    @State private var dataChoice = ""
    @State private var profile = ""
    private var discard: Bool { dataChoice == "discard" }
    private var option: LibraryModel.ProfileOption? { review.profiles.first { $0.uuid == profile } }
    /// Extensions that can't be signed: ExtensionKit ones with the Apple ID, those the chosen profile doesn't cover with a certificate.
    private var unsupported: Set<String> {
        guard !review.appleID, let option else { return review.unsupported }
        return Set(review.inventory.extensions.map(\.id).filter { !option.supportedExtensions.contains($0) })
    }

    private var destination: ProbeModel.DeviceApp? { review.installed.first { $0.bundleId == target } }
    private var cost: Int {
        guard review.appleID else { return 0 }
        let registered = Set(review.candidates.map(\.identifier))
        let childIDs = selected.map { target + $0.dropFirst(review.inventory.id.count) }
        return ([target] + childIDs).filter { !registered.contains($0) }.count
    }
    private var problem: String? {
        if !review.appleID, !review.profiles.isEmpty, option == nil { return "Choose a provisioning profile." }
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

    private func profileDetail(_ option: LibraryModel.ProfileOption) -> String {
        var parts = [option.kind?.capitalized ?? "Profile", option.debuggable ? "debuggable (JIT works)" : "not debuggable"]
        if let expires = SigningStore.date(option.expires) { parts.append("expires \(expires.formatted(date: .abbreviated, time: .omitted))") }
        let names = ["com.apple.developer.kernel.increased-memory-limit": "increased memory limit",
                     "com.apple.developer.kernel.extended-virtual-addressing": "extended virtual addressing",
                     "com.apple.developer.kernel.increased-debugging-memory-limit": "increased debugging memory limit"]
        if !option.grants.isEmpty { parts.append("grants " + option.grants.map { names[$0] ?? $0 }.joined(separator: ", ")) }
        return parts.joined(separator: " · ")
    }

    var body: some View {
        if embedded {
            reviewForm
        } else {
            NavigationStack {
                reviewForm.raveScreenTitle("Review Install")
            }
            .sheetFrame(minWidth: 600, minHeight: 520)
        }
    }

    private var reviewForm: some View {
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
                Section {
                    if !review.profiles.isEmpty {
                        Picker("Profile", selection: $profile) {
                            ForEach(review.profiles) { option in
                                Text("\(option.name) — \(option.appIdPattern)").tag(option.uuid)
                            }
                        }
                    }
                    if let option {
                        LabeledContent("For App ID", value: "\(option.teamId).\(option.appIdPattern)")
                        Text(profileDetail(option)).font(.caption).foregroundStyle(.secondary)
                        if option.uuid == review.profile, review.profileSource == "hint" {
                            Text("The profile this app's build was made for, or the one chosen for it before.").font(.caption)
                        } else if option.uuid == review.profile, review.profileSource == "installed" {
                            Text("For the same App ID as the copy on this \(ThisDevice.noun), so it installs as an update of it.").font(.caption)
                        }
                    }
                    LabeledContent("Bundle ID", value: target)
                    if target == review.inventory.id {
                        Text("Keeps the app's original bundle ID while signing with the imported profile. This can install an older build over the existing app as a downgrade, subject to Apple's install checks. Some Apple services may still depend on the profile's App ID.")
                    }
                } header: {
                    Text("App Identity")
                } footer: {
                    Text("Capabilities come from the profile. A paid team's profile for another App ID still signs this app, and gives it that ID's entitlements, such as the increased memory limit. The chosen profile stays assigned for updates and refreshes.")
                }
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
                            if item.watch == true {
                                Text("Apple Watch app: installed on the Watch paired with this iPhone, through it. Needs an App ID of its own.").font(.caption)
                            }
                            if unsupported.contains(item.id) {
                                Text(review.appleID ? "ExtensionKit signing requires imported provisioning profiles." : "No imported profile covers this extension on this device.").font(.caption)
                            }
                        }
                    }
                    .disabled(unsupported.contains(item.id))
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
                    library.finishInstallReview(.init(target: target, extensions: selected.sorted(), discard: discard,
                                                      profile: review.appleID ? nil : option?.uuid))
                }
                .disabled(target.isEmpty || problem != nil || ((destination != nil || review.dataSource != nil) && dataChoice.isEmpty))
                Button("Cancel", role: .cancel) { library.finishInstallReview(nil) }
            }
        }
        .onChange(of: target) { _, _ in dataChoice = "" }
        .onChange(of: profile) { _, _ in
            guard let option else { return }
            target = option.target
            selected = selected.filter { option.supportedExtensions.contains($0) }
        }
        .onAppear {
            profile = review.profile ?? ""
            target = review.appleID ? review.target : (review.profiles.first { $0.uuid == review.profile }?.target ?? review.target)
            selected = Set(review.selected.filter { id in
                review.inventory.extensions.contains { $0.id == id && !unsupported.contains(id) }
            })
        }
    }
}
