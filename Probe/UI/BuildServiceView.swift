import SwiftUI

struct BuildServiceView: View {
    @Bindable var service: BuildService
    @State private var password = ""
    @Environment(\.scenePhase) private var scenePhase

    var body: some View {
        NavigationStack {
            Form {
                Section("Mac build service") {
                    if service.connected {
                        LabeledContent("Connected to", value: service.state?.host ?? service.address)
                        Button("Disconnect") { Task { await service.disconnect() } }
                    } else {
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
                        Text("Use the Mac service password. This is separate from the companion relay and Apple ID.")
                            .font(.footnote).foregroundStyle(.secondary)
                    }
                    Toggle("Remember password on this device", isOn: $service.rememberPassword)
                    Text("Remembered passwords stay in this device's Keychain. Turning this off forgets them; Disconnect forgets the password for this service.")
                        .font(.footnote).foregroundStyle(.secondary)
                    if !service.message.isEmpty { Text(service.message).accessibilityIdentifier("buildServiceStatus") }
                }
                if service.connected {
                    Section("Projects") {
                        ForEach(service.state?.repos ?? []) { project in
                            BuildProjectView(service: service, project: project)
                        }
                        if service.state?.repos.isEmpty == true {
                            Text("Add a project to the Mac service configuration to start building.")
                        }
                    }
                    Section("Builds") {
                        ForEach(service.state?.builds ?? []) { job in
                            VStack(alignment: .leading, spacing: 8) {
                                Text("#\(job.id) \(job.repo) · \(job.platform)").font(.headline)
                                Text("\(job.device ?? "") · \(job.status)").foregroundStyle(.secondary)
                                HStack {
                                    Button("Read log") { Task { await service.log(job) } }
                                    if job.cancellable {
                                        Button("Cancel", role: .destructive) { Task { await service.cancel(job) } }
                                    }
                                }
                            }
                        }
                    }
                    Section("Registered builds") {
                        ForEach(service.artifacts) { artifact in
                            VStack(alignment: .leading) {
                                Text(artifact.app.name ?? artifact.app.bundleId).font(.headline)
                                Text("\(artifact.platform) · \(artifact.app.build ?? artifact.buildId)")
                                Text(artifact.buildId).font(.caption).foregroundStyle(.secondary)
                            }
                        }
                    }
                    if !service.detail.isEmpty {
                        Section("Build log") {
                            Text(service.detail).font(.system(.caption, design: .monospaced))
                                #if !os(tvOS)
                                .textSelection(.enabled)
                                #endif
                        }
                    }
                }
            }
            .navigationTitle("Developer")
            .task { await service.restoreConnection() }
            .toolbar {
                if service.connected { Button("Refresh", systemImage: "arrow.clockwise") { Task { await service.refresh() } } }
            }
            .task(id: service.connected && scenePhase == .active) {
                while service.connected && scenePhase == .active && !Task.isCancelled {
                    await service.refresh()
                    try? await Task.sleep(for: .seconds(5))
                }
            }
        }
    }
}

private struct BuildProjectView: View {
    let service: BuildService
    let project: BuildService.Project
    @State private var target = 0
    @State private var branch = "current"
    @State private var launch = false
    @State private var debug = true
    @State private var patch = false

    var body: some View {
        #if os(tvOS)
        VStack(alignment: .leading, spacing: 12) {
            Text(project.name).font(.headline)
            controls
        }
        #else
        DisclosureGroup(project.name) { controls }
        #endif
    }

    private var controls: some View {
        VStack(alignment: .leading, spacing: 24) {
            Picker("Target", selection: $target) {
                ForEach(Array(project.platforms.enumerated()), id: \.offset) { index, item in
                    Text("\(item.platform) · \(item.device)").tag(index)
                }
            }
            Picker("Branch", selection: $branch) {
                Text("Current checkout").tag("current")
                ForEach((project.branches ?? []).filter { $0 != "current" }, id: \.self) { Text($0).tag($0) }
            }
            Toggle("Debug access", isOn: $debug)
            Toggle("Live patching", isOn: $patch)
            Text("Live patching adds instrumentation and can reduce performance. Debug access still requires approval on the target device. These options do not enable JIT.")
                .font(.footnote).foregroundStyle(.secondary)
            VStack(alignment: .leading, spacing: 12) {
                Text("Build without installing").font(.headline)
                Button("Build only") { submit(deploy: false) }
                    .buttonStyle(.bordered)
                    .accessibilityIdentifier("developerBuildOnly")
            }
            Divider()
            VStack(alignment: .leading, spacing: 12) {
                Text("Install on device").font(.headline)
                Toggle("Launch after installation", isOn: $launch)
                Button("Build & install on \(selectedDevice)") { submit(deploy: true) }
                    .buttonStyle(.bordered)
                    .accessibilityIdentifier("developerBuildAndInstall")
            }
        }
        .padding(.vertical, 12)
        .disabled(service.busy || !project.platforms.indices.contains(target))
    }

    private var selectedDevice: String {
        project.platforms.indices.contains(target) ? project.platforms[target].device : "selected device"
    }

    private func submit(deploy: Bool) {
        guard project.platforms.indices.contains(target) else { return }
        Task { await service.build(project: project, target: project.platforms[target], branch: branch,
                                   deploy: deploy, launch: launch, debug: debug, patch: patch) }
    }
}
