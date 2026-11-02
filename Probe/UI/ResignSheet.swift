import SwiftUI

/// What to re-sign (a kept build, or a project target the Mac builds first), and the devices a
/// project installs that app on.
struct ResignTarget: Identifiable {
    let source: BuildService.ResignSource
    let devices: [String]

    init(artifact: BuildService.Artifact, devices: [String]) { source = .kept(artifact); self.devices = devices }
    init(source: BuildService.ResignSource, devices: [String]) { self.source = source; self.devices = devices }

    var id: String {
        switch source {
        case .kept(let artifact): artifact.buildId
        case .build(let project, let target): "\(project.name)|\(target.platform)|\(target.device)"
        }
    }
    var name: String {
        switch source {
        case .kept(let artifact): artifact.app.name ?? artifact.app.bundleId
        case .build(let project, _): project.name
        }
    }
    var platform: String {
        switch source {
        case .kept(let artifact): artifact.platform
        case .build(_, let target): target.platform
        }
    }
    var bundleId: String? {
        switch source {
        case .kept(let artifact): artifact.app.bundleId
        case .build(_, let target): target.bundleId
        }
    }
    /// Where the IPA comes from, in a sentence.
    var sourceText: String {
        switch source {
        case .kept(let artifact): "The build the Mac kept: \(artifact.app.build ?? artifact.buildId)."
        case .build(let project, _): "The Mac kept no build of this app, so it builds \(project.name) from its current checkout first, without installing it, and re-signs that build."
        }
    }
}

/// Signs an app with another of the Mac's profiles and installs it over the copy on a device. A
/// device can't give an installed app back, so the IPA is the one the Mac kept, or one it builds
/// for the purpose; the Mac (Apparition Store, `lib/resign.js`) does the signing, and for a different App
/// ID the backup, reinstall and restore of the app's data, which installd needs because it
/// refuses that update.
struct ResignSheet: View {
    let service: BuildService
    let target: ResignTarget
    @Environment(\.dismiss) private var dismiss
    @State private var profiles: [BuildService.ResignProfile] = []
    @State private var loaded = false
    @State private var profileID = ""
    @State private var device = ""
    @State private var keepData = true
    @State private var confirming = false
    @State private var queued: BuildService.Job?

    private var profile: BuildService.ResignProfile? { profiles.first { $0.id == profileID } }
    private var queuedStatus: String? { queued.flatMap { job in service.state?.builds.first { $0.id == job.id }?.status } }
    private var running: Bool { queuedStatus == "queued" || queuedStatus == "running" }
    private var ready: Bool { profile != nil && target.devices.contains(device) && !service.busy && !running }

    var body: some View {
        NavigationStack {
            Form {
                Section("App") {
                    Text(target.name)
                    Text(target.platform).foregroundStyle(.secondary)
                    if let bundleId = target.bundleId { Text(bundleId).font(.caption.monospaced()).foregroundStyle(.secondary) }
                    Text(target.sourceText).font(.footnote).foregroundStyle(.secondary)
                }
                Section("Sign and install") {
                    if target.devices.isEmpty {
                        Text("No configured project installs this app on a \(target.platform) device, so there is nothing to sign it for.")
                    } else {
                        Picker("Device", selection: $device) { ForEach(target.devices, id: \.self) { Text($0).tag($0) } }
                    }
                    if loaded && profiles.isEmpty {
                        Text("No usable provisioning profiles in the Mac's build-signing folder.")
                    } else {
                        Picker("Profile", selection: $profileID) { ForEach(profiles) { Text($0.label).tag($0.id) } }
                    }
                }
                Section("Data") {
                    Toggle("Keep the app's data", isOn: $keepData)
                    Text("A profile for the same App ID updates the app in place and keeps its data. A different App ID can't be installed over the old copy: the Mac then copies Documents and Library off the device, reinstalls, and puts them back (the backup stays on the Mac). Keychain items and app-group data do not move. The device must be reachable from the Mac.")
                        .font(.footnote).foregroundStyle(.secondary)
                }
                Section {
                    Button("Check first") { submit(dryRun: true) }
                        .disabled(!ready)
                    Button("Re-sign…", role: .destructive) { confirming = true }
                        .disabled(!ready)
                    Text("Check first only tests that the app's data can be reached. Nothing is changed.")
                        .font(.footnote).foregroundStyle(.secondary)
                }
                if let queued {
                    Section("Job #\(queued.id)") {
                        Text(queuedStatus ?? queued.status)
                        if !service.message.isEmpty { Text(service.message).font(.footnote) }
                    }
                } else if !service.message.isEmpty {
                    Section { Text(service.message) }
                }
            }
            .navigationTitle("Re-sign")
            .toolbar { ToolbarItem(placement: .cancellationAction) { Button("Close") { dismiss() } } }
            .confirmationDialog("Re-sign \(target.name) on \(device)?", isPresented: $confirming, titleVisibility: .visible) {
                Button(keepData ? "Re-sign and keep data" : "Re-sign and discard data", role: .destructive) { submit(dryRun: false) }
                Button("Cancel", role: .cancel) {}
            } message: {
                Text(keepData ? "If the device needs a reinstall, its data is backed up on the Mac first." : "The app's data is not backed up and may be lost.")
            }
            .task {
                device = target.devices.first ?? ""
                profiles = await service.resignProfiles()
                profileID = profiles.first?.id ?? ""
                loaded = true
            }
            // The Developer screen refreshes on its own; this sheet may be open without it.
            .task(id: queued?.id) {
                while running, !Task.isCancelled {
                    try? await Task.sleep(for: .seconds(3))
                    await service.refresh()
                }
            }
        }
    }

    private func submit(dryRun: Bool) {
        guard let profile else { return }
        Task {
            queued = await service.resign(target.source, device: device, profile: profile, keepData: keepData, dryRun: dryRun)
        }
    }
}
