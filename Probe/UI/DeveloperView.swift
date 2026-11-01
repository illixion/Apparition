import SwiftUI

/// The sections of the Developer screen, the same four the desktop app has.
enum DeveloperSection: String, CaseIterable, Identifiable {
    case projects = "Projects"
    case activity = "Activity"
    case reports = "Reports"
    case service = "Service"
    var id: String { rawValue }
}

/// Where your own projects live: a Mac running appstore builds them, installs them on your
/// devices and collects their crash logs and traces. Before connecting this explains what that
/// is and how to set it up; once connected it shows the projects (each opens its own page),
/// the build queue, reports grouped by app, and the connection itself.
/// A project's page, and the tab to open it on.
struct DeveloperRoute: Hashable {
    var project: String
    var tab: ProjectTab = .overview
}

struct DeveloperView: View {
    let developer: DeveloperModel
    let library: LibraryModel
    /// Projects' pages, so the Library can open one (on a tab).
    @Binding var path: [DeveloperRoute]
    @State private var section: DeveloperSection = .projects
    @State private var resigning: ResignTarget?
    @Environment(\.scenePhase) private var scenePhase

    private var service: BuildService { developer.service }

    var body: some View {
        NavigationStack(path: $path) {
            Group {
                if service.connected { connected } else { DeveloperConnectView(service: service) }
            }
            .navigationTitle("Developer")
            .navigationDestination(for: DeveloperRoute.self) { route in
                ProjectDetailView(developer: developer, library: library, name: route.project, resigning: $resigning,
                                  initialTab: route.tab)
            }
            .toolbar {
                if service.connected { Button("Refresh", systemImage: "arrow.clockwise") { Task { await service.refresh() } } }
            }
        }
        .sheet(item: $resigning) { ResignSheet(service: service, target: $0) }
        .task { await service.restoreConnection() }
        .task(id: service.connected && scenePhase == .active) {
            while service.connected && scenePhase == .active && !Task.isCancelled {
                await service.refresh()
                try? await Task.sleep(for: .seconds(5))
            }
        }
    }

    private var connected: some View {
        VStack(spacing: 0) {
            Picker("Section", selection: $section) {
                ForEach(DeveloperSection.allCases) { item in Text(title(item)).tag(item) }
            }
            .pickerStyle(.segmented)
            .padding(.horizontal, 24)
            .padding(.vertical, 12)
            .accessibilityIdentifier("developerSection")
            switch section {
            case .projects: projects
            case .activity: DeveloperActivityView(service: service)
            case .reports: Form { DeveloperReportsList(service: service, bundleIds: nil) }
            case .service: DeveloperServiceView(service: service)
            }
        }
    }

    private func title(_ item: DeveloperSection) -> String {
        switch item {
        case .activity:
            let running = (service.state?.builds ?? []).filter(\.active).count
            return running > 0 ? "Activity · \(running)" : item.rawValue
        case .reports:
            let total = service.crashLogs.count + service.traces.count
            return total > 0 ? "Reports · \(total)" : item.rawValue
        default: return item.rawValue
        }
    }

    private var projects: some View {
        Form {
            if !service.message.isEmpty {
                Section { Text(service.message).accessibilityIdentifier("buildServiceStatus") }
            }
            Section {
                ForEach(service.state?.repos ?? []) { project in
                    NavigationLink(value: DeveloperRoute(project: project.name)) { ProjectRow(service: service, project: project) }
                }
                if service.state?.repos.isEmpty == true {
                    Text("No projects yet. Register one from Apparition on the Mac, or add it to appstore's config.json.")
                        .foregroundStyle(.secondary)
                }
            } header: {
                Text("Projects")
            } footer: {
                Text("Each project lists the devices it installs on. Open one to build, install, re-sign or read its reports.")
            }
        }
    }
}

/// A project in the list: its checkout, and one line per target saying what is installed there.
private struct ProjectRow: View {
    let service: BuildService
    let project: BuildService.Project

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(alignment: .firstTextBaseline) {
                Text(project.name).font(.headline)
                if let label = project.gitLabel { Text(label).font(.caption.monospaced()).foregroundStyle(.secondary) }
            }
            ForEach(project.platforms, id: \.self) { target in
                Label {
                    Text("\(target.device) · \(target.platform) — \(service.installSummary(project: project, target: target))")
                        .font(.callout).foregroundStyle(.secondary)
                } icon: {
                    Image(systemName: service.device(named: target.device)?.reachable == true ? "circle.fill" : "circle")
                        .foregroundStyle(service.device(named: target.device)?.reachable == true ? .green : .secondary)
                        .imageScale(.small)
                }
            }
            if let job = service.jobs(for: project).first {
                Text("\(job.kind) #\(job.id) · \(job.status)").font(.caption).foregroundStyle(job.active ? .orange : .secondary)
            }
        }
        .padding(.vertical, 6)
    }
}

/// The build queue: running and queued first, then recent ones, each with its log.
private struct DeveloperActivityView: View {
    let service: BuildService
    @State private var showAll = false

    var body: some View {
        let jobs = service.state?.builds ?? []
        let active = jobs.filter(\.active)
        let finished = jobs.filter { !$0.active }
        Form {
            if !active.isEmpty {
                Section("Running") { ForEach(active) { DeveloperJobRow(service: service, job: $0) } }
            }
            Section {
                ForEach(showAll ? finished : Array(finished.prefix(8))) { DeveloperJobRow(service: service, job: $0) }
                if !showAll && finished.count > 8 {
                    Button("Show \(finished.count - 8) older") { showAll = true }
                }
                if jobs.isEmpty { Text("No builds yet. Start one from a project.").foregroundStyle(.secondary) }
            } header: {
                Text("Recent")
            } footer: {
                Text("Builds for one project run one at a time.")
            }
            if !service.detail.isEmpty {
                Section("Log") { DeveloperLogText(text: service.detail) }
            }
        }
    }
}

struct DeveloperJobRow: View {
    let service: BuildService
    let job: BuildService.Job

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(alignment: .firstTextBaseline) {
                Text("\(job.kind) #\(job.id)").font(.headline)
                Text(job.status.capitalized).font(.caption.weight(.semibold))
                    .foregroundStyle(job.active ? .orange : job.status == "success" ? .green : job.status == "failed" ? .red : .secondary)
            }
            Text([job.repo, job.platform, job.device, job.commit.map { "\(job.builtBranch ?? "")@\($0)" }].compactMap { $0 }.joined(separator: " · "))
                .font(.callout).foregroundStyle(.secondary)
            if let error = job.error { Text(error).font(.footnote).foregroundStyle(.red) }
            HStack {
                Button("Read log") { Task { await service.log(job) } }
                if job.cancellable { Button("Cancel", role: .destructive) { Task { await service.cancel(job) } } }
            }
            .buttonStyle(.bordered)
        }
        .padding(.vertical, 4)
    }
}

struct DeveloperLogText: View {
    let text: String
    var body: some View {
        Text(text).font(.system(.caption, design: .monospaced))
            #if !os(tvOS)
            .textSelection(.enabled)
            #endif
    }
}

/// Crash logs and traces, one section per app; `bundleIds` limits it to a project's apps.
struct DeveloperReportsList: View {
    let service: BuildService
    let bundleIds: Set<String>?

    private var apps: [(bundleId: String, name: String)] {
        var seen: [String: String] = [:]
        for crash in service.crashLogs where bundleIds?.contains(crash.bundleId) ?? true { seen[crash.bundleId] = seen[crash.bundleId] ?? crash.appName ?? crash.bundleId }
        for trace in service.traces where bundleIds?.contains(trace.app.bundleId) ?? true { seen[trace.app.bundleId] = seen[trace.app.bundleId] ?? trace.app.name ?? trace.app.bundleId }
        return seen.map { ($0.key, $0.value) }.sorted { $0.name.localizedCaseInsensitiveCompare($1.name) == .orderedAscending }
    }

    var body: some View {
        if apps.isEmpty {
            if bundleIds == nil {
                Section { Text("No reports yet. Crash logs are pulled from your devices, and apps that link DebugTrace upload traces.").foregroundStyle(.secondary) }
            }
        } else {
            ForEach(apps, id: \.bundleId) { app in
                Section("Reports · \(app.name)") { reports(app.bundleId) }
            }
        }
    }

    @ViewBuilder
    private func reports(_ bundleId: String) -> some View {
        ForEach(service.crashLogs.filter { $0.bundleId == bundleId }) { crash in
            VStack(alignment: .leading, spacing: 6) {
                Text(crash.exception ?? "Crash report")
                Text("\(crash.device ?? "Device") · \(crash.build ?? "unknown build")").font(.caption).foregroundStyle(.secondary)
                if crash.symbolicated != nil {
                    Button(service.reportDetailID == "crash:\(crash.id)" ? "Hide report" : "Read symbolicated report") {
                        Task { await service.readCrash(crash) }
                    }
                }
            }
        }
        ForEach(service.traces.filter { $0.app.bundleId == bundleId }) { trace in
            VStack(alignment: .leading, spacing: 6) {
                Text("Trace · \(trace.build?.buildId ?? trace.app.build ?? "unknown build")")
                ForEach(trace.files.filter { [".txt", ".log", ".json", ".md"].contains("." + URL(fileURLWithPath: $0.path).pathExtension.lowercased()) }) { file in
                    let reportID = "trace:\(trace.id):\(file.path)"
                    Button(service.reportDetailID == reportID ? "Hide \(file.path)" : "Read \(file.path)") {
                        Task { await service.readTraceFile(trace, file: file) }
                    }
                }
            }
        }
        if service.reportDetailOwnerBundleId == bundleId, !service.reportDetail.isEmpty {
            if service.reportDetail.contains("? +0x") {
                Note(text: "Some frames are address-only because this report doesn't identify them as symbols from the app binary. The appstore has symbols for kept builds, but they can't resolve frames the crash log attributes to other binaries.", kind: .info)
            }
            Text(service.reportDetail)
                .font(.system(.caption, design: .monospaced))
                .lineLimit(30)
                #if !os(tvOS)
                .textSelection(.enabled)
                #endif
        }
    }
}

/// The connection, the devices the Mac can install to, and what appstore is.
private struct DeveloperServiceView: View {
    @Bindable var service: BuildService

    var body: some View {
        Form {
            Section("Mac build service") {
                LabeledContent("Connected to", value: service.state?.host ?? service.address)
                Toggle("Remember password on this device", isOn: $service.rememberPassword)
                Button("Disconnect", role: .destructive) { Task { await service.disconnect() } }
            }
            Section {
                ForEach(service.state?.devices?.list ?? []) { device in
                    LabeledContent(device.name, value: [device.model, device.reachable ? "reachable" : "not reachable"].compactMap { $0 }.filter { !$0.isEmpty }.joined(separator: " · "))
                }
                if service.state?.devices?.list.isEmpty ?? true {
                    Text("No devices yet. Pair one with the Mac in Xcode.").foregroundStyle(.secondary)
                }
            } header: {
                Text("Devices the Mac installs to")
            } footer: {
                Text("Simulators are not listed.")
            }
            AppstoreAbout()
        }
    }
}

/// Before connecting: what appstore is, how to set it up, and the connection form.
private struct DeveloperConnectView: View {
    @Bindable var service: BuildService
    @State private var password = ""

    var body: some View {
        Form {
            AppstoreAbout(steps: true)
            Section("Connect") {
                TextField("https://your-mac.example.com", text: $service.address)
                    .textContentType(.URL)
                    .autocorrectionDisabled()
                SecureField("Service password", text: $password)
                Button("Connect") {
                    let value = password
                    password = ""
                    Task {
                        if value.isEmpty { await service.connectWithRememberedPassword() }
                        else { await service.connect(password: value) }
                    }
                }
                .disabled(service.busy || (password.isEmpty && !service.hasRememberedPassword) || service.address.isEmpty)
                Toggle("Remember password on this device", isOn: $service.rememberPassword)
                Text("Use the HTTPS address the Mac is reachable at (for example through Tailscale) and the password set with node server.js set-password. Remembered passwords stay in this device's Keychain; Disconnect forgets it.")
                    .font(.footnote).foregroundStyle(.secondary)
                if !service.message.isEmpty { Text(service.message).accessibilityIdentifier("buildServiceStatus") }
            }
        }
    }
}

/// appstore in a few sentences, with where to get it.
private struct AppstoreAbout: View {
    var steps = false
    static let repository = URL(string: "https://github.com/illixion/appstore")!

    var body: some View {
        Section {
            Text("The Developer tools work with appstore, an open-source build service you run on your own Mac. It builds your Xcode projects with your developer certificate, installs them on devices paired with the Mac, keeps every signed build, and collects crash logs and debug traces. From here you can start builds, install kept builds on this \(ThisDevice.noun), re-sign apps with another profile and read their reports.")
            if steps {
                Text("1. Get appstore from GitHub and follow its README. It needs Xcode, Node 18 and a paid developer certificate with a development profile.")
                Text("2. Set a service password on the Mac: node server.js set-password.")
                Text("3. Start it, make it reachable over HTTPS (Tailscale Serve works), and connect below.")
            }
            #if os(tvOS)
            Text("appstore: github.com/illixion/appstore").foregroundStyle(.secondary)
            #else
            Link(destination: Self.repository) { Label("appstore on GitHub", systemImage: "arrow.up.right.square") }
            #endif
        } header: {
            Text(steps ? "Get started" : "About appstore")
        }
    }
}

extension BuildService.Project {
    /// "main@abc1234", with a note when the checkout has uncommitted changes.
    var gitLabel: String? {
        guard let commit = git?.commit, !commit.isEmpty else { return nil }
        return "\(git?.branch ?? "")@\(commit)" + (git?.dirty == true ? " · uncommitted changes" : "")
    }
}

extension BuildService {
    /// What the service installed for a target, in a few words.
    func installSummary(project: Project, target: Target) -> String {
        guard let install = install(for: target) else { return "not installed from the Mac yet" }
        var parts = ["installed \(install.commit ?? install.buildNo ?? install.buildId ?? "a build")"]
        if install.via == "apparition" { parts.append("by Apparition on the device") }
        if let resigned = install.resigned { parts.append("re-signed with \(resigned)") }
        else if install.confirmed == false { parts.append("offered as a download") }
        if let head = project.git?.commit, let commit = install.commit, install.branch == project.git?.branch {
            parts.append(commit == head ? "up to date" : "checkout at \(head)")
        }
        if let at = install.at { parts.append(Date(timeIntervalSince1970: at / 1000).relativeToNow) }
        return parts.joined(separator: " · ")
    }
}
