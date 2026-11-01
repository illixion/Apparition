import SwiftUI

/// The sections of one project's page. Which ones show depends on what the connected service and
/// this device can do, so a person with a plain Xcode project and no Mac service sees fewer.
enum ProjectTab: String, CaseIterable, Identifiable {
    case overview = "Overview"
    case builds = "Builds"
    case reports = "Reports"
    case signing = "Signing"
    var id: String { rawValue }
}

/// One project: a header saying where its target stands, then tabs for installing it, building it,
/// reading its reports and how it is signed.
struct ProjectDetailView: View {
    let developer: DeveloperModel
    let library: LibraryModel
    let name: String
    @Binding var resigning: ResignTarget?
    /// The tab to open on; a Library badge asks for Reports, say.
    var initialTab: ProjectTab = .overview
    @State private var tab: ProjectTab?
    @State private var targetKey: String?
    @State private var options = BuildOptions()
    @State private var downloadingKeptBuild: String?

    private var service: BuildService { developer.service }

    var body: some View {
        Group {
            if let project = service.project(named: name) {
                content(project)
            } else {
                Form { Text("\(name) is no longer configured on the Mac.").foregroundStyle(.secondary) }
            }
        }
        .navigationTitle(name)
    }

    // MARK: Layout

    private func targetID(_ target: BuildService.Target) -> String { "\(target.device)|\(target.platform)" }

    /// The chosen target, else the one for this device, else the first.
    private func selectedTarget(_ project: BuildService.Project) -> BuildService.Target? {
        project.platforms.first { targetID($0) == targetKey }
            ?? project.platforms.first { developer.isThisDevice($0) }
            ?? project.platforms.first
    }

    private func tabs(_ project: BuildService.Project) -> [ProjectTab] {
        [.overview, .builds, .reports, .signing]
    }

    private func title(_ item: ProjectTab, _ project: BuildService.Project) -> String {
        switch item {
        case .builds:
            let running = service.jobs(for: project).filter(\.active).count
            return running > 0 ? "Builds · \(running)" : item.rawValue
        case .reports:
            let count = service.reportCount(for: project)
            return count.crashes + count.traces > 0 ? "Reports · \(count.crashes + count.traces)" : item.rawValue
        default: return item.rawValue
        }
    }

    @ViewBuilder
    private func content(_ project: BuildService.Project) -> some View {
        if let target = selectedTarget(project) {
            let shown = tabs(project)
            let current = tab.flatMap { shown.contains($0) ? $0 : nil } ?? (shown.contains(initialTab) ? initialTab : .overview)
            VStack(spacing: 0) {
                header(project, target)
                Picker("Section", selection: Binding(get: { current }, set: { tab = $0 })) {
                    ForEach(shown) { Text(title($0, project)).tag($0) }
                }
                .pickerStyle(.segmented)
                .padding(.horizontal, 24)
                .padding(.vertical, 12)
                .accessibilityIdentifier("projectSection")
                Form {
                    if !service.message.isEmpty { Section { Text(service.message) } }
                    switch current {
                    case .overview: overview(project, target)
                    case .builds: builds(project, target)
                    case .reports: DeveloperReportsList(service: service, bundleIds: Set(project.platforms.compactMap(\.bundleId)))
                    case .signing: signing(project, target)
                    }
                }
            }
        } else {
            Form { Text("\(name) has no targets yet.").foregroundStyle(.secondary) }
        }
    }

    private func header(_ project: BuildService.Project, _ target: BuildService.Target) -> some View {
        let badge = service.badge(project: project, target: target)
        return VStack(alignment: .leading, spacing: 8) {
            HStack(spacing: 12) {
                if project.platforms.count > 1 {
                    Picker("Target", selection: Binding(get: { targetID(target) }, set: { targetKey = $0 })) {
                        ForEach(project.platforms, id: \.self) { Text("\($0.device) · \($0.platform)").tag(targetID($0)) }
                    }
                    .pickerStyle(.menu)
                    .accessibilityIdentifier("projectTarget")
                } else {
                    Text("\(target.device) · \(target.platform)").font(.headline)
                }
                StatusPill(text: badge.text, systemImage: badge.symbol, tint: badge.tint)
                Spacer(minLength: 0)
            }
            Text(service.installSummary(project: project, target: target)).font(.callout).foregroundStyle(.secondary)
            if let label = project.gitLabel { Text("Checkout \(label)").font(.caption.monospaced()).foregroundStyle(.tertiary) }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.horizontal, 24)
        .padding(.top, 8)
    }

    // MARK: Overview

    @ViewBuilder
    private func overview(_ project: BuildService.Project, _ target: BuildService.Target) -> some View {
        let choices = developer.installOptions(target)
        let working = developer.phase(project, target)?.isWorking == true || service.busy
        Section {
            if let route = choices.preferred {
                Button {
                    Task { await developer.buildAndInstall(project: project, target: target, route: route, options: options) }
                } label: {
                    Label(title(route, target), systemImage: symbol(route))
                }
                .buttonStyle(.borderedProminent)
                .disabled(working)
                .accessibilityIdentifier("developerBuildAndInstall")
                Text(explanation(route, target)).font(.footnote).foregroundStyle(.secondary)
                if let other = choices.secondary {
                    Button {
                        Task { await developer.buildAndInstall(project: project, target: target, route: other, options: options) }
                    } label: {
                        Label(title(other, target), systemImage: symbol(other))
                    }
                    .buttonStyle(.bordered)
                    .disabled(working)
                    .accessibilityIdentifier("developerBuildAndInstallSecondary")
                }
                // The way to the device route, when only the tunnel can install right now.
                if route == .xcodeTunnel, let why = choices.thisDeviceProblem, developer.isThisDevice(target) {
                    Text(why).font(.footnote).foregroundStyle(.secondary)
                }
            } else if let why = choices.unavailable {
                Note(text: why, kind: .warning)
            }
            if let phase = developer.phase(project, target) { phaseRow(phase, project, target, choices) }
            if choices.canInstallHere, let latest = pendingUpdate(project, target) {
                keptBuildButton(latest, project: project, target: target, title: "Install kept build \(latest.app.build ?? latest.buildId) here")
            }
        } header: {
            Text("Install on \(target.device)")
        }
        Section("Installed") {
            LabeledContent("Checkout") { Text(project.gitLabel ?? "unknown").font(.callout.monospaced()) }
            if let install = service.install(for: target) {
                LabeledContent("Installed", value: install.commit ?? install.buildNo ?? install.buildId ?? "a build")
                LabeledContent("By", value: install.via == "apparition" ? "Apparition on the device" : install.resigned != nil ? "Re-sign from the Mac" : "The Mac")
            }
            if let device = service.device(named: target.device) {
                LabeledContent("Mac to device", value: device.reachable ? "Reachable" : "Not reachable")
            }
        }
    }

    private func title(_ route: InstallRoute, _ target: BuildService.Target) -> String {
        switch route {
        case .thisDevice: "Build & Install on this \(ThisDevice.noun)"
        case .xcodeTunnel: "Build & Install on \(target.device) via Xcode tunnel"
        }
    }

    private func symbol(_ route: InstallRoute) -> String {
        route == .thisDevice ? "arrow.down.app" : "laptopcomputer.and.arrow.down"
    }

    private func explanation(_ route: InstallRoute, _ target: BuildService.Target) -> String {
        switch route {
        case .thisDevice:
            "The Mac builds and keeps the app; Apparition downloads it from the build service and signs it with this \(ThisDevice.noun)'s own identity. It doesn't need the Mac to reach \(target.device), so it works away from home, and with a free Apple ID."
        case .xcodeTunnel:
            "The Mac installs it through Xcode's tunnel with its own certificate. Quick for updates, and the only route that can launch the app from here, but the Mac has to reach \(target.device) and Apparition isn't involved."
        }
    }

    /// The kept build newer than what is installed here, when this device's copy came from a build.
    private func pendingUpdate(_ project: BuildService.Project, _ target: BuildService.Target) -> BuildService.Artifact? {
        guard developer.phase(project, target)?.isWorking != true else { return nil }
        return developer.update(for: project, target: target, installedBuild: installedBuild(target))
    }

    private func installedBuild(_ target: BuildService.Target) -> String? {
        guard let bundleId = target.bundleId,
              let installedId = library.installed.first(where: { $0.id == bundleId })?.installedBundleId else { return nil }
        return library.installedOnDevice(installedId)?.build
    }

    @ViewBuilder
    private func phaseRow(_ phase: DeveloperModel.Phase, _ project: BuildService.Project, _ target: BuildService.Target,
                          _ choices: InstallOptions) -> some View {
        switch phase {
        case .building(let job):
            ProgressView(job == 0 ? "Queueing the build…" : "Building #\(job)…")
        case .downloading:
            ProgressView("Downloading the build…")
        case .installing:
            ProgressView(library.status ?? "Installing…")
        case .done(let text):
            Note(text: text, kind: .success)
            Button("Dismiss") { developer.dismissPhase(project, target) }.buttonStyle(.bordered)
        case .failed(let reason, let kept):
            Note(text: reason, kind: .error)
            if let kept, choices.canInstallHere, let artifact = service.artifact(buildId: kept) {
                keptBuildButton(artifact, project: project, target: target, title: "Install the kept build here instead")
            }
            Button("Dismiss") { developer.dismissPhase(project, target) }.buttonStyle(.bordered)
        }
    }

    private func keptBuildButton(_ artifact: BuildService.Artifact, project: BuildService.Project,
                                 target: BuildService.Target, title: String) -> some View {
        Button {
            Task { await developer.install(artifact, project: project, target: target, launch: options.launch) }
        } label: {
            Label(title, systemImage: "arrow.down.app")
        }
        .buttonStyle(.bordered)
        .disabled(developer.phase(project, target)?.isWorking == true || library.busy || service.busy)
    }

    // MARK: Builds

    @ViewBuilder
    private func builds(_ project: BuildService.Project, _ target: BuildService.Target) -> some View {
        let choices = developer.installOptions(target)
        Section {
            Picker("Branch", selection: $options.branch) {
                Text("Current checkout").tag("current")
                ForEach((project.branches ?? []).filter { $0 != "current" }, id: \.self) { Text($0).tag($0) }
            }
            Toggle("Launch after installing", isOn: $options.launch)
            Toggle("Debug access", isOn: $options.debug)
            Toggle("Live patching", isOn: $options.patch)
            if options.patch && service.capabilities.contains("entry-patching") {
                Picker("Patch backend", selection: $options.injectBackend) {
                    Text("Swift dynamic replacement").tag("dynamic")
                    Text("Apparition entry boundaries (experimental)").tag("apparition")
                }
            }
        } header: {
            Text("Build options")
        } footer: {
            Text("These apply to every build started here. Live patching is a build setting: it adds instrumentation, so the app runs slower. You patch the running app from the Mac (the `inject` tool), which needs no tunnel. Debug access still asks for approval on the device.")
        }
        Section {
            Button("Build only") { Task { await developer.buildOnly(project: project, target: target, options: options) } }
                .buttonStyle(.bordered)
                .disabled(service.busy)
                .accessibilityIdentifier("developerBuildOnly")
        } header: {
            Text("Build without installing")
        } footer: {
            Text("Keeps a signed IPA on the Mac to install here, or re-sign. Nothing is installed on any device.")
        }
        let jobs = service.jobs(for: project)
        if !jobs.isEmpty {
            Section("Recent builds") {
                ForEach(jobs.prefix(5)) { DeveloperJobRow(service: service, job: $0) }
                if !service.detail.isEmpty { DeveloperLogText(text: service.detail) }
            }
        }
        let kept = service.artifacts(for: project)
        if !kept.isEmpty {
            Section {
                ForEach(kept.prefix(8)) { artifact in keptRow(project, artifact, choices) }
            } header: {
                Text("Kept builds")
            } footer: {
                Text("Every signed build the Mac made, kept exactly as signed.")
            }
        }
    }

    private func keptRow(_ project: BuildService.Project, _ artifact: BuildService.Artifact, _ choices: InstallOptions) -> some View {
        let target = project.platforms.first { $0.platform.caseInsensitiveCompare(artifact.platform) == .orderedSame && developer.isThisDevice($0) }
            ?? project.platforms.first { $0.platform.caseInsensitiveCompare(artifact.platform) == .orderedSame }
        return VStack(alignment: .leading, spacing: 8) {
            Text("\(artifact.app.name ?? artifact.app.bundleId) · \(artifact.platform)").font(.headline)
            Text(artifact.app.build ?? artifact.buildId).font(.caption.monospaced()).foregroundStyle(.secondary)
            HStack {
                if let target, developer.installOptions(target).canInstallHere {
                    keptBuildButton(artifact, project: project, target: target, title: "Install here")
                }
                if service.canResign {
                    Button("Re-sign…") { resigning = ResignTarget(artifact: artifact, devices: service.devices(for: artifact)) }
                }
            }
            .buttonStyle(.bordered)
        }
        .padding(.vertical, 4)
    }

    // MARK: Signing

    @ViewBuilder
    private func signing(_ project: BuildService.Project, _ target: BuildService.Target) -> some View {
        let record = target.bundleId.flatMap { id in library.installed.first { $0.id == id } }
        Section {
            if let record {
                LabeledContent("Signed with", value: record.signedWith ?? "not signed yet")
                if let expires = LibraryModel.expiryDate(record) { LabeledContent("Expires", value: expires.relativeToNow) }
                if let error = record.lastError { Note(text: error, kind: .error) }
                Button("Refresh now") { Task { await library.refresh(appId: record.id) } }
                    .buttonStyle(.bordered)
                    .disabled(library.busy)
            } else {
                Text("This \(ThisDevice.noun) hasn't installed it from here yet. When it does, Apparition keeps it signed and refreshed like its other apps.")
                    .foregroundStyle(.secondary)
            }
        } header: {
            Text("On this \(ThisDevice.noun)")
        }
        if let source = service.resignSource(for: project, target: target) {
            let device = service.device(named: target.device)
            Section {
                Button { resigning = ResignTarget(source: source, devices: [target.device]) } label: {
                    Label("Re-sign with another profile…", systemImage: "signature")
                }
                .disabled(device?.reachable == false)
                if device?.reachable == false {
                    Text("The Mac can't reach \(target.device) right now, and re-signing replaces the app in place through the Xcode tunnel.")
                        .font(.footnote).foregroundStyle(.secondary)
                }
            } header: {
                Text("From the Mac")
            } footer: {
                Text("Signs the app with another of the Mac's provisioning profiles and installs it over the copy on \(target.device).")
            }
        }
    }
}

extension BuildService {
    /// Where a target stands against its checkout, as a badge.
    func badge(project: Project, target: Target) -> (text: String, symbol: String, tint: Color) {
        guard let install = install(for: target) else { return ("Not installed", "circle.dashed", .secondary) }
        if let head = project.git?.commit, let commit = install.commit, install.branch == project.git?.branch {
            return commit == head ? ("Up to date", "checkmark.circle.fill", .green) : ("Behind checkout", "arrow.triangle.2.circlepath", .orange)
        }
        return ("Installed", "checkmark.circle", .blue)
    }
}
