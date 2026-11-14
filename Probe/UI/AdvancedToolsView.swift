import SwiftUI
#if !os(tvOS)
import UniformTypeIdentifiers
#endif

struct ActivityMonitorView: View {
    let tools: DeviceToolsModel
    @State private var search = ""

    private var shown: [DeviceProcess] {
        let matches = tools.processes.filter { $0.kind != .system || !search.isEmpty }.filter {
            guard !search.isEmpty else { return true }
            return $0.title.localizedCaseInsensitiveContains(search)
                || $0.name.localizedCaseInsensitiveContains(search)
                || ($0.bundleId?.localizedCaseInsensitiveContains(search) ?? false)
                || String($0.pid) == search
        }
        return matches.sorted {
            let left = tools.activityReadings[$0.pid]?.cpu ?? -1
            let right = tools.activityReadings[$1.pid]?.cpu ?? -1
            if left != right { return left > right }
            return $0.title.localizedStandardCompare($1.title) == .orderedAscending
        }
    }

    var body: some View {
        List {
            Section {
                HStack {
                    if tools.activityRunning {
                        Label("Sampling every second", systemImage: "waveform.path")
                            .foregroundStyle(.green)
                        Spacer()
                        Button("Stop", systemImage: "stop.fill", role: .destructive) { tools.stopActivity() }
                    } else {
                        Button(tools.activityStarting ? "Connecting…" : "Start Monitoring", systemImage: "play.fill") {
                            tools.startActivity()
                        }
                        .disabled(tools.activityStarting)
                    }
                }
                if let error = tools.activityError { Note(text: error, kind: .error) }
                if let fields = tools.systemCPUFields {
                    LabeledContent("System CPU fields", value: summary(fields))
                        .font(.caption.monospaced())
                }
            } footer: {
                Text("This uses Instruments' device service. CPU and physical memory fields vary by OS; a dash means the device did not report that value.")
            }
            Section {
                if tools.loadingProcesses && tools.processes.isEmpty {
                    ProgressView("Reading processes…")
                } else if shown.isEmpty {
                    ContentUnavailableView("No processes", systemImage: "cpu", description: Text("Refresh the process list or start monitoring."))
                }
                ForEach(shown) { process in
                    let reading = tools.activityReadings[process.pid]
                    HStack(spacing: 10) {
                        VStack(alignment: .leading, spacing: 3) {
                            Text(process.title).lineLimit(1)
                            Text("\(process.name) · pid \(process.pid)")
                                .font(.caption.monospaced()).foregroundStyle(.secondary).lineLimit(1)
                        }
                        Spacer(minLength: 8)
                        VStack(alignment: .trailing, spacing: 3) {
                            Text(reading?.cpu.map { String(format: "%.1f%%", $0) } ?? "—")
                                .monospacedDigit()
                            Text(memory(reading?.footprint))
                                .font(.caption).monospacedDigit().foregroundStyle(.secondary)
                        }
                    }
                }
            } header: {
                HStack {
                    Text("Processes")
                    Spacer()
                    Button("Refresh", systemImage: "arrow.clockwise") { Task { await tools.refreshProcesses() } }
                        .labelStyle(.iconOnly)
                        .disabled(tools.loadingProcesses)
                }
            } footer: {
                Text("CPU is the percentage reported by Instruments. Memory shows physical footprint where supplied.")
            }
        }
        .navigationTitle("Activity Monitor")
        .searchable(text: $search, prompt: "Process, bundle ID or pid")
        .task { if tools.processes.isEmpty { await tools.refreshProcesses() } }
    }

    private func memory(_ bytes: UInt64?) -> String {
        guard let bytes else { return "—" }
        return ByteCountFormatter.string(fromByteCount: Int64(clamping: bytes), countStyle: .memory)
    }

    private func summary(_ value: ToolJSONValue) -> String {
        switch value {
        case .object(let fields):
            fields.sorted { $0.key < $1.key }.prefix(4).map { "\($0.key)=\(summary($0.value))" }.joined(separator: " · ")
        case .array(let values): values.prefix(4).map(summary).joined(separator: ",")
        case .number(let number): String(format: "%.1f", number)
        case .string(let string): string
        case .bool(let bool): bool ? "yes" : "no"
        case .null: "—"
        }
    }
}

struct NetworkMonitorView: View {
    let tools: DeviceToolsModel

    var body: some View {
        List {
            Section {
                HStack {
                    if tools.networkRunning {
                        Label("Monitoring connections", systemImage: "dot.radiowaves.left.and.right")
                            .foregroundStyle(.green)
                        Spacer()
                        Button("Stop", systemImage: "stop.fill", role: .destructive) { tools.stopNetwork() }
                    } else {
                        Button(tools.networkStarting ? "Connecting…" : "Start Monitoring", systemImage: "play.fill") {
                            tools.startNetwork()
                        }
                        .disabled(tools.networkStarting)
                    }
                }
                if let error = tools.networkError { Note(text: error, kind: .error) }
            } footer: {
                Text("Shows connection endpoints and per-process traffic reported by Instruments. This is not a packet capture; availability depends on the device's developer services.")
            }
            Section {
                if tools.networkConnections.isEmpty {
                    Text(tools.networkRunning ? "Waiting for network activity…" : "Start monitoring to list connections.")
                        .foregroundStyle(.secondary)
                }
                ForEach(tools.networkConnections) { connection in
                    VStack(alignment: .leading, spacing: 5) {
                        HStack {
                            Text(processName(connection.pid)).font(.headline).lineLimit(1)
                            Spacer()
                            Text(connection.pid > 0 ? "pid \(connection.pid)" : "process unavailable")
                                .font(.caption.monospaced()).foregroundStyle(.secondary)
                        }
                        Text("\(connection.local)  →  \(connection.remote)")
                            .font(.caption.monospaced()).lineLimit(1).truncationMode(.middle)
                        HStack(spacing: 12) {
                            Label("↓ \(bytes(connection.rxBytes))", systemImage: "arrow.down")
                            Label("↑ \(bytes(connection.txBytes))", systemImage: "arrow.up")
                            Text("RTT \(connection.averageRTT) · retransmits \(connection.retransmits)")
                        }
                        .font(.caption2.monospaced()).foregroundStyle(.secondary)
                        if let name = connection.interfaceName {
                            Text("\(name) · interface \(connection.interfaceIndex)")
                                .font(.caption2).foregroundStyle(.secondary)
                        }
                    }
                    .padding(.vertical, 3)
                }
            } header: {
                Text("Connections")
            } footer: {
                Text("RTT values use the units supplied by the device service. Connections may disappear when an app closes or the OS rotates its counters.")
            }
        }
        .navigationTitle("Network Monitor")
        .task { if tools.processes.isEmpty { await tools.refreshProcesses() } }
    }

    private func processName(_ pid: Int) -> String {
        tools.processes.first(where: { $0.pid == pid })?.title ?? (pid > 0 ? "Process" : "Unknown process")
    }

    private func bytes(_ count: UInt64) -> String {
        ByteCountFormatter.string(fromByteCount: Int64(clamping: count), countStyle: .file)
    }
}

private struct PendingCondition: Identifiable {
    let group: DeviceConditionGroup
    let profile: DeviceConditionProfile
    var id: String { "\(group.identifier)/\(profile.identifier)" }
}

struct ConnectionConditionsView: View {
    let tools: DeviceToolsModel
    @State private var pending: PendingCondition?

    var body: some View {
        List {
            Section {
                if tools.conditionRunning, let active = tools.activeCondition {
                    Label("Active: \(active)", systemImage: "antenna.radiowaves.left.and.right")
                    Button("Stop Condition", systemImage: "stop.fill", role: .destructive) { tools.stopCondition() }
                } else if tools.conditionStarting {
                    ProgressView("Applying condition…")
                } else if tools.loadingConditions {
                    ProgressView("Reading available profiles…")
                } else if tools.conditionGroups.isEmpty {
                    ContentUnavailableView("No Profiles Available", systemImage: "antenna.radiowaves.left.and.right",
                                           description: Text("This OS build may not expose Instruments' connection-condition service."))
                }
                if let error = tools.conditionError { Note(text: error, kind: .error) }
            } footer: {
                Text("Apparition applies only profiles the device reports. The condition is cleared when you stop it or when the tunnel ends.")
            }
            ForEach(tools.conditionGroups) { group in
                Section(group.identifier) {
                    ForEach(group.profiles) { profile in
                        Button {
                            pending = PendingCondition(group: group, profile: profile)
                        } label: {
                            VStack(alignment: .leading, spacing: 3) {
                                Text(profile.description.isEmpty ? profile.identifier : profile.description)
                                if !profile.description.isEmpty {
                                    Text(profile.identifier).font(.caption.monospaced()).foregroundStyle(.secondary)
                                }
                            }
                        }
                        .disabled(tools.conditionRunning || tools.conditionStarting)
                    }
                }
            }
        }
        .navigationTitle("Connection Conditions")
        .toolbar {
            Button("Refresh", systemImage: "arrow.clockwise") { Task { await tools.refreshConditions() } }
                .disabled(tools.loadingConditions || tools.conditionStarting || tools.conditionRunning)
        }
        .task { if tools.conditionGroups.isEmpty { await tools.refreshConditions() } }
        .confirmationDialog(pending.map {
            let profileName = $0.profile.description.isEmpty ? $0.profile.identifier : $0.profile.description
            return "\($0.group.identifier): \(profileName)"
        } ?? "Apply condition?",
                            isPresented: Binding(get: { pending != nil }, set: { if !$0 { pending = nil } }),
                            titleVisibility: .visible) {
            if let pending {
                Button("Apply on Device") { tools.startCondition(group: pending.group, profile: pending.profile); self.pending = nil }
            }
            Button("Cancel", role: .cancel) { pending = nil }
        } message: {
            Text("This changes network or thermal conditions system-wide for as long as this session remains active.")
        }
    }
}

struct DeveloperServicesView: View {
    let tools: DeviceToolsModel

    var body: some View {
        List {
            Section {
                if tools.loadingServices {
                    ProgressView("Checking paired services…")
                } else if let error = tools.servicesError {
                    Note(text: error, kind: .error)
                } else if let available = tools.instrumentsAvailable {
                    Label(available ? "Instruments service hub available" : "Instruments service hub unavailable",
                          systemImage: available ? "checkmark.circle.fill" : "xmark.circle.fill")
                        .foregroundStyle(available ? .green : .secondary)
                }
                Button("Refresh Services", systemImage: "arrow.clockwise") { Task { await tools.refreshServices() } }
                    .disabled(tools.loadingServices)
            } footer: {
                Text("The service list is what this device advertises to the current pairing. It is a capability snapshot, not a guarantee that every Instruments channel or app container will accept access.")
            }
            if tools.instrumentsAvailable == false {
                Section("When Instruments Is Missing") {
                    Text("Some monitors, location simulation, and connection profiles require Apple's developer services. Have Xcode prepare this device; a restart may unmount its developer image.")
                }
            }
            Section("Advertised Services · \(tools.toolServices.count)") {
                ForEach(tools.toolServices) { service in
                    VStack(alignment: .leading, spacing: 3) {
                        Text(service.name).font(.caption.monospaced())
                        Text("Port \(service.port) · \(service.remoteXpc ? "Remote XPC" : "socket")")
                            .font(.caption2).foregroundStyle(.secondary)
                    }
                }
            }
        }
        .navigationTitle("Developer Services")
        .task { if tools.toolServices.isEmpty { await tools.refreshServices() } }
    }
}

#if !os(tvOS)
struct AppDataAppsView: View {
    let model: ProbeModel
    @State private var loading = false
    @State private var error: String?

    var body: some View {
        List {
            if loading { ProgressView("Listing installed apps…") }
            if let error { Note(text: error, kind: .error) }
            Section {
                ForEach(model.deviceApps.sorted { $0.name.localizedCaseInsensitiveCompare($1.name) == .orderedAscending }) { app in
                    if app.debuggable {
                        NavigationLink(value: ToolRoute.appDataBrowser(app.bundleId)) {
                            appLabel(app)
                        }
                    } else {
                        appLabel(app).foregroundStyle(.secondary)
                    }
                }
            } footer: {
                Text("Only apps whose development signature allows container access can be browsed. System apps and App Store-signed apps may be unavailable under Apple's sandbox rules.")
            }
        }
        .navigationTitle("App Data")
        .toolbar {
            Button("Refresh", systemImage: "arrow.clockwise") { Task { await refresh() } }
                .disabled(loading)
        }
        .task { if model.deviceApps.isEmpty { await refresh() } }
    }

    private func appLabel(_ app: ProbeModel.DeviceApp) -> some View {
        VStack(alignment: .leading, spacing: 3) {
            Text(app.name)
            Text(app.debuggable ? app.bundleId : "Development container access unavailable")
                .font(.caption.monospaced()).lineLimit(1).foregroundStyle(.secondary)
        }
    }

    private func refresh() async {
        loading = true
        defer { loading = false }
        let outcome = await model.listApps(host: model.defaultTunnelHost, port: model.defaultTunnelPort)
        error = outcome == "ok" ? nil : DeviceToolsModel.message(outcome)
    }
}

struct AppDataBrowserView: View {
    let tools: DeviceToolsModel
    let model: ProbeModel
    let bundleId: String
    @State private var shareURL: URL?
    @State private var importingBackup = false
    @State private var pendingRestore: URL?
    @State private var exportingPath: String?

    private var parent: String? {
        guard tools.dataPath != "/Documents", tools.dataPath != "/Library" else { return nil }
        let value = (tools.dataPath as NSString).deletingLastPathComponent
        return value.isEmpty ? "/Documents" : value
    }

    var body: some View {
        List {
            Section {
                if tools.loadingData { ProgressView("Reading app data…") }
                if let error = tools.dataError { Note(text: error, kind: .error) }
                if let status = tools.dataStatus { Label(status, systemImage: "checkmark.circle") }
                HStack {
                    Text("Path").foregroundStyle(.secondary)
                    Spacer()
                    Text(tools.dataBundleId == bundleId ? tools.dataPath : "/Documents")
                        .font(.caption.monospaced()).lineLimit(1).truncationMode(.middle)
                }
                HStack {
                    Button("Documents") { Task { await tools.browseData(bundleId: bundleId, path: "/Documents") } }
                    Button("Library") { Task { await tools.browseData(bundleId: bundleId, path: "/Library") } }
                }
                .buttonStyle(.bordered)
            } footer: {
                Text("The OS grants access only to this app's data container. Caches, saved state, symlinks, and system files are excluded.")
            }
            Section("Items · \(tools.dataEntries.count)") {
                ForEach(tools.dataEntries) { entry in
                    if entry.isDirectory {
                        Button {
                            Task { await tools.browseData(bundleId: bundleId, path: entry.path) }
                        } label: {
                            Label(entry.name, systemImage: "folder")
                        }
                    } else {
                        Button {
                            exportingPath = entry.path
                            Task {
                                shareURL = await tools.exportDataFile(bundleId: bundleId, entry: entry)
                                exportingPath = nil
                            }
                        } label: {
                            HStack {
                                Label(entry.name, systemImage: "doc")
                                Spacer()
                                if exportingPath == entry.path { ProgressView() }
                                else { Text(ByteCountFormatter.string(fromByteCount: Int64(clamping: entry.bytes), countStyle: .file)).font(.caption).foregroundStyle(.secondary) }
                            }
                        }
                        .disabled(exportingPath != nil || tools.loadingData)
                    }
                }
                if tools.dataEntries.isEmpty && !tools.loadingData {
                    Text("This folder is empty or the OS didn't vend it.").foregroundStyle(.secondary)
                }
            }
            Section("Backup") {
                Button {
                    Task { shareURL = await tools.createDataBackup(bundleId: bundleId) }
                } label: {
                    if tools.creatingDataBackup {
                        Label("Creating Backup…", systemImage: "archivebox")
                    } else {
                        Label("Create App Data Backup", systemImage: "archivebox")
                    }
                }
                .disabled(tools.loadingData || exportingPath != nil || tools.creatingDataBackup || tools.restoringDataBackup)
                Button("Restore App Data Backup…", systemImage: "square.and.arrow.down") { importingBackup = true }
                    .disabled(tools.loadingData || exportingPath != nil || tools.creatingDataBackup || tools.restoringDataBackup)
                if tools.restoringDataBackup { ProgressView("Restoring app data…") }
                Text("Close the target app before restoring; Apparition leaves it running. Restore merges into this app, replacing matching files and keeping others. The backup must have the same bundle ID.")
                    .font(.footnote).foregroundStyle(.secondary)
            }
        }
        .navigationTitle(model.deviceApps.first(where: { $0.bundleId == bundleId })?.name ?? "App Data")
        .toolbar {
            if let parent {
                Button("Up", systemImage: "arrow.up") { Task { await tools.browseData(bundleId: bundleId, path: parent) } }
            }
            Button("Refresh", systemImage: "arrow.clockwise") { Task { await tools.browseData(bundleId: bundleId, path: tools.dataPath) } }
                .disabled(tools.loadingData)
        }
        .task {
            if tools.dataBundleId != bundleId { await tools.browseData(bundleId: bundleId, path: "/Documents") }
        }
        .fileImporter(isPresented: $importingBackup, allowedContentTypes: [.item], allowsMultipleSelection: false) { result in
            guard case .success(let urls) = result, let source = urls.first else { return }
            let accessed = source.startAccessingSecurityScopedResource()
            guard accessed else {
                tools.reportDataError("Apparition couldn't access the selected backup file.")
                return
            }
            defer { source.stopAccessingSecurityScopedResource() }
            let destination = FileManager.default.temporaryDirectory.appendingPathComponent("apparition-restore-\(UUID().uuidString).zip")
            do {
                try FileManager.default.copyItem(at: source, to: destination)
                pendingRestore = destination
            } catch {
                tools.reportDataError("Couldn't prepare the backup: \(error.localizedDescription)")
            }
        }
        .confirmationDialog("Restore this app's data?", isPresented: Binding(
            get: { pendingRestore != nil }, set: { if !$0 { discardPendingRestore() } }
        ), titleVisibility: .visible) {
            Button("Restore and Merge", role: .destructive) {
                guard let archive = pendingRestore else { return }
                pendingRestore = nil
                Task {
                    await tools.restoreDataBackup(bundleId: bundleId, archive: archive)
                    try? FileManager.default.removeItem(at: archive)
                }
            }
            Button("Cancel", role: .cancel) { discardPendingRestore() }
        } message: {
            Text("Existing files with matching names will be replaced. The backup must belong to \(bundleId).")
        }
        .shareSheet(url: $shareURL)
        .onDisappear { discardPendingRestore() }
    }

    private func discardPendingRestore() {
        if let pendingRestore { try? FileManager.default.removeItem(at: pendingRestore) }
        pendingRestore = nil
    }
}

struct SysdiagnoseView: View {
    let tools: DeviceToolsModel
    @State private var confirmCapture = false
    @State private var shareURL: URL?

    var body: some View {
        Form {
            Section {
                if tools.capturingSysdiagnose {
                    ProgressView("Capturing and transferring sysdiagnose…")
                } else {
                    Button("Capture Sysdiagnose", systemImage: "waveform.path.ecg") { confirmCapture = true }
                }
                if let status = tools.sysdiagnoseStatus { Label(status, systemImage: "checkmark.circle") }
                if let error = tools.sysdiagnoseError { Note(text: error, kind: .error) }
                if shareURL != nil { Label("Opening share options…", systemImage: "square.and.arrow.up") }
            } footer: {
                Text("A sysdiagnose includes detailed device logs and may contain personal or identifying information. Save it somewhere private and review it before sharing.")
            }
            Section("Availability") {
                Text("Capture is manual. CoreDevice may not offer this service on every device or OS version. A capture can take several minutes and use substantial storage.")
                    .font(.footnote).foregroundStyle(.secondary)
            }
        }
        .navigationTitle("Sysdiagnose")
        .disabled(tools.capturingSysdiagnose)
        .confirmationDialog("Capture a sysdiagnose?", isPresented: $confirmCapture, titleVisibility: .visible) {
            Button("Capture") {
                Task { shareURL = await tools.captureSysdiagnose() }
            }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text("This collects full device diagnostics and writes the archive into Apparition's temporary storage.")
        }
        .shareSheet(url: $shareURL)
    }
}

#else
import SwiftUI

struct AppDataAppsView: View {
    let model: ProbeModel
    var body: some View { EmptyView() }
}
#endif
