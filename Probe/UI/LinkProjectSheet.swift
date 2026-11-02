import SwiftUI

/// Choose which project target an app on this device is. The app keeps the link whatever its bundle
/// identifier looks like, so an Xcode debug build, or an app a free Apple ID gave another team's
/// suffix, reads as the project's.
struct LinkProjectSheet: View {
    let developer: DeveloperModel
    let app: ProbeModel.DeviceApp
    let onDone: () -> Void

    private var service: BuildService { developer.service }

    /// Targets for this device first, those whose identifier best matches before the rest.
    private var choices: [(project: BuildService.Project, target: BuildService.Target)] {
        let all = (service.state?.repos ?? []).flatMap { project in project.platforms.map { (project: project, target: $0) } }
        func rank(_ item: (project: BuildService.Project, target: BuildService.Target)) -> Int {
            var score = 0
            if developer.isThisDevice(item.target) { score -= 2 }
            if let id = item.target.bundleId, BundleIdentity.matches(installed: app.bundleId, target: id) { score -= 4 }
            return score
        }
        return all.sorted { rank($0) < rank($1) }
    }

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    LabeledContent("App", value: app.name)
                    Text(app.bundleId).font(.caption.monospaced()).foregroundStyle(.secondary)
                } footer: {
                    Text("Linking shows this app with its project's builds, updates and reports, and installs new builds over it.")
                }
                Section("Project") {
                    ForEach(choices, id: \.target) { item in
                        Button {
                            developer.link(app.bundleId, to: item.project, target: item.target)
                            onDone()
                        } label: {
                            VStack(alignment: .leading, spacing: 4) {
                                Text(item.project.name).font(.headline)
                                Text("\(item.target.device) · \(item.target.platform)" + (item.target.bundleId.map { " · \($0)" } ?? ""))
                                    .font(.caption).foregroundStyle(.secondary)
                            }
                        }
                    }
                    if choices.isEmpty {
                        Text("The build service has no projects yet. Register one from Apparition on the Mac, or add it to Apparition Store's config.json.")
                            .foregroundStyle(.secondary)
                    }
                }
            }
            .navigationTitle("Link to Project")
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Cancel", action: onDone) }
            }
        }
    }
}
