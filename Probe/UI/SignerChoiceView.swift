import RAVEUI
import SwiftUI

/// What signs a new app, and what happens to a copy of it already on the device: shown before
/// the first install, because an installed app can't be replaced by a copy signed by another team.
struct SignerChoiceSection: View {
    let library: LibraryModel
    let appName: String
    /// The app's own bundle ID, when known; without it the checks against the device's apps are skipped.
    let bundleId: String?
    @Binding var signer: String
    /// The bundle ID of a copy on the device whose data the new install takes over, or nil to keep both.
    @Binding var takeOver: String?

    private var options: [SigningStore.SignerOption] { library.signerOptions }
    private var usable: [SigningStore.SignerOption] { options.filter { $0.unusable == nil } }

    private var selected: SigningStore.SignerOption? { options.first { $0.id == signer } }

    /// The bundle ID and team the chosen signer would install under, if known.
    private var prediction: (bundleId: String, teamId: String?)? {
        guard let bundleId, let selected else { return nil }
        return library.signing.prediction(of: selected, bundleId: bundleId)
    }

    /// The copy installd would refuse to replace: the same bundle ID, signed by another team.
    private var conflict: ProbeModel.DeviceApp? {
        guard let prediction, let team = prediction.teamId,
              let copy = library.installedOnDevice(prediction.bundleId), let other = copy.teamId, other != team else { return nil }
        return copy
    }

    /// Copies of the app under another bundle ID, whose data could come along.
    private var copies: [ProbeModel.DeviceApp] {
        guard let bundleId else { return [] }
        return library.otherCopies(of: bundleId).filter { $0.bundleId != prediction?.bundleId }
    }

    private var footer: String {
        var parts: [String] = []
        if let selected {
            parts.append(selected.addsTeamToBundleId
                ? "The Apple ID adds its team ID to the app's bundle ID, so this copy has data of its own, next to a copy signed by another team."
                : "Your certificate keeps the app's bundle ID.")
        }
        for option in options where option.unusable != nil {
            parts.append("\(option.title) isn't offered: \(option.unusable ?? "").")
        }
        return parts.joined(separator: " ")
    }

    var body: some View {
        Section {
            Picker("Sign with", selection: $signer) {
                ForEach(usable) { option in
                    VStack(alignment: .leading, spacing: 2) {
                        Text(option.title)
                        Text(option.detail).font(.caption).foregroundStyle(.secondary)
                    }
                    .tag(option.id)
                }
            }
            .pickerStyle(.inline)
            .labelsHidden()
            if let conflict {
                Note(text: "This \(ThisDevice.noun) already has \(conflict.bundleId) signed by team \(conflict.teamId ?? "?"), and \(ThisDevice.os) won't replace it with a copy from team \(prediction?.teamId ?? "?"). Sign with team \(conflict.teamId ?? "?")'s certificate, or delete the app first (its data goes with it).",
                     kind: .warning)
            }
        } header: {
            Text("Sign With")
        } footer: {
            Text(footer)
        }
        if !copies.isEmpty {
            Section {
                Picker("Existing copy", selection: $takeOver) {
                    Text("Keep both, each with its own data").tag(String?.none)
                    ForEach(copies) { copy in
                        Text("Move data from \(copy.name) (team \(copy.teamId ?? "?")) and remove it").tag(String?.some(copy.bundleId))
                    }
                }
                .pickerStyle(.inline)
                .labelsHidden()
            } header: {
                Text("Copy on This \(ThisDevice.noun.capitalized)")
            } footer: {
                Text("\(appName) is already installed, signed by another team. Moving copies its files to the new copy through the tunnel, then removes the old one; nothing is removed unless every file arrived. Keychain items and data shared through app groups stay behind.")
            }
        }
    }
}

/// Asks what signs an app from a source before its first install (`LibraryModel.requestInstall`).
struct InstallSignerSheet: View {
    let library: LibraryModel
    let choice: LibraryModel.SignerChoice

    @State private var signer = ""
    @State private var takeOver: String?

    var body: some View {
        NavigationStack {
            Form {
                RAVEScreenHeading("Install \(choice.appName)")
                SignerChoiceSection(library: library, appName: choice.appName, bundleId: choice.bundleId,
                                    signer: $signer, takeOver: $takeOver)
                Section {
                    Button("Install \(choice.appName)") {
                        let signer = signer, takeOver = takeOver
                        Task { await library.confirmSignerChoice(signer: signer, takeOver: takeOver) }
                    }
                    .disabled(signer.isEmpty)
                    Button("Cancel", role: .cancel) { library.cancelSignerChoice() }
                }
            }
            .raveScreenTitle("Install \(choice.appName)")
        }
        .sheetFrame(minWidth: 560, minHeight: 420)
        .task {
            signer = library.signing.preferredSigner(in: library.signerOptions)
            await library.refreshDeviceApps()
        }
    }
}
