#if !os(tvOS)
import MapKit
#endif
import SwiftUI

/// The Tools tab's pages.
enum ToolRoute: Hashable {
    case processes
    case location
    case console
}

/// Device tools beside JIT, the ones StikDebug users know: what runs on this device, a simulated
/// location, and the device's log. They reach the device the way JIT does (LocalDevVPN or the
/// companion), so each says so when that connection is missing.
struct ToolsView: View {
    let model: ProbeModel
    let account: AccountModel
    let tools: DeviceToolsModel
    let onSetup: () -> Void
    @State private var path: [ToolRoute] = []

    var body: some View {
        NavigationStack(path: $path) {
            Form {
                ToolsConnectionNote(model: model, account: account, onSetup: onSetup)
                Section {
                    NavigationLink(value: ToolRoute.processes) {
                        ToolRow(title: "Processes", systemImage: "list.bullet.rectangle",
                                detail: "What runs on this \(ThisDevice.noun), and ending it.")
                    }
                    #if !os(tvOS)
                    NavigationLink(value: ToolRoute.location) {
                        ToolRow(title: "Location", systemImage: "location",
                                detail: "Make this \(ThisDevice.noun) report a place of your choosing, for every app.")
                    }
                    #endif
                    NavigationLink(value: ToolRoute.console) {
                        ToolRow(title: "Console", systemImage: "terminal",
                                detail: "The \(ThisDevice.noun)'s log as it's written, one app's or everything.")
                    }
                } footer: {
                    Text("These reach this \(ThisDevice.noun) the way JIT does: through LocalDevVPN or your computer.")
                }
                #if !os(tvOS)
                if let place = tools.location.place {
                    Section("Simulated Location") {
                        LabeledContent(place.name, value: place.coordinates)
                        Button("Stop Simulating", systemImage: "location.slash", role: .destructive) { tools.stopLocation() }
                    }
                }
                #endif
            }
            .navigationTitle("Tools")
            .navigationDestination(for: ToolRoute.self) { route in
                switch route {
                case .processes:
                    ProcessesView(tools: tools) { process in
                        tools.startConsole(for: process)
                        path.append(.console)
                    }
                case .location:
                    #if os(tvOS)
                    EmptyView()
                    #else
                    LocationView(tools: tools)
                    #endif
                case .console:
                    ConsoleView(tools: tools)
                }
            }
        }
    }
}

private struct ToolRow: View {
    let title: String
    let systemImage: String
    let detail: String

    var body: some View {
        Label {
            VStack(alignment: .leading, spacing: 2) {
                Text(title)
                Text(detail).font(.caption).foregroundStyle(.secondary)
            }
        } icon: {
            Image(systemName: systemImage)
        }
    }
}

/// Why the tools can't reach the device right now, as the Library says it: no pairing, no route
/// chosen, or the route not answering.
struct ToolsConnectionNote: View {
    let model: ProbeModel
    let account: AccountModel
    let onSetup: () -> Void

    var body: some View {
        if let problem = model.pairingProblem {
            Section {
                Button(action: onSetup) { Note(text: PairingRecord.message(problem), kind: .warning) }
                    .buttonStyle(.plain)
            }
        } else if !model.routeChosen {
            Section {
                Button(action: onSetup) {
                    Note(text: "Apparition doesn't know how to reach this \(ThisDevice.noun) yet. Choose Companion or Local VPN in Setup.", kind: .warning)
                }
                .buttonStyle(.plain)
            }
        } else if let mode = model.unreachableMode, mode == model.connectionMode {
            Section {
                ConnectionProblemNote(model: model, account: account, problem: .unreachable(mode), onSettings: onSetup)
            }
        }
    }
}

// MARK: Processes

struct ProcessesView: View {
    let tools: DeviceToolsModel
    /// Opens the console on this process's log.
    let onShowLog: (DeviceProcess) -> Void
    @State private var search = ""
    @State private var showSystem = false
    @State private var chosen: DeviceProcess?

    private var shown: [DeviceProcess] {
        guard !search.isEmpty else { return tools.processes }
        return tools.processes.filter {
            $0.title.localizedCaseInsensitiveContains(search) || $0.name.localizedCaseInsensitiveContains(search)
                || ($0.bundleId?.localizedCaseInsensitiveContains(search) ?? false) || String($0.pid) == search
        }
    }

    var body: some View {
        List {
            if let error = tools.processError {
                Section { Note(text: error, kind: .error) }
            }
            group("Apps", kinds: [.app, .systemApp])
            group("App Extensions", kinds: [.extension])
            if showSystem || !search.isEmpty {
                group("System", kinds: [.system])
            }
            if !tools.processes.isEmpty {
                Section {
                    Toggle("Show System Processes", isOn: $showSystem)
                } footer: {
                    if let loaded = tools.processesLoadedAt {
                        Text("\(tools.processes.count) processes, listed \(loaded.formatted(date: .omitted, time: .standard)).")
                    }
                }
            }
        }
        .overlay {
            if tools.loadingProcesses && tools.processes.isEmpty {
                ProgressView("Listing processes…")
            }
        }
        .searchable(text: $search, prompt: "Name, bundle ID or pid")
        .navigationTitle("Processes")
        .toolbar {
            Button("Refresh", systemImage: "arrow.clockwise") { Task { await tools.refreshProcesses() } }
                .disabled(tools.loadingProcesses)
        }
        #if !os(tvOS)
        .refreshable { await tools.refreshProcesses() }
        #endif
        .task { await tools.refreshProcesses() }
    }

    @ViewBuilder
    private func group(_ title: String, kinds: Set<DeviceProcess.Kind>) -> some View {
        let members = shown.filter { kinds.contains($0.kind) }
        if !members.isEmpty {
            Section(title) {
                ForEach(members) { process in row(process) }
            }
        }
    }

    private func row(_ process: DeviceProcess) -> some View {
        Button {
            chosen = process
        } label: {
            HStack {
                VStack(alignment: .leading, spacing: 2) {
                    Text(process.title)
                    Text(subtitle(process)).font(.caption).foregroundStyle(.secondary).lineLimit(1).truncationMode(.middle)
                }
                Spacer()
                Text(String(process.pid)).monospacedDigit().foregroundStyle(.secondary)
            }
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .confirmation(process.title, item: $chosen, for: process.id) { process in
            Button("Show Log") { onShowLog(process) }
            if !isApparition(process) {
                Button("End Process", role: .destructive) { Task { await tools.end(process) } }
            }
            Button("Cancel", role: .cancel) {}
        } message: { process in
            Text(message(process))
        }
    }

    private func subtitle(_ process: DeviceProcess) -> String {
        switch process.kind {
        case .extension: process.appName.map { "In \($0)" } ?? process.path
        case .app, .systemApp: process.bundleId ?? process.path
        case .system: process.path
        }
    }

    private func isApparition(_ process: DeviceProcess) -> Bool {
        process.bundleId == Bundle.main.bundleIdentifier && process.kind != .extension
    }

    private func message(_ process: DeviceProcess) -> String {
        let where_ = "pid \(process.pid) · \(process.path)"
        switch process.kind {
        case .system:
            return "\(where_)\n\nEnding a system process can make this \(ThisDevice.noun) misbehave until the system starts it again."
        case .app, .systemApp:
            return isApparition(process) ? where_ : "\(where_)\n\nEnding an app loses what it hasn't saved, as when it's closed from the app switcher."
        case .extension:
            return where_
        }
    }
}

// MARK: Location

#if !os(tvOS)
struct LocationView: View {
    let tools: DeviceToolsModel
    @State private var camera: MapCameraPosition = .automatic
    /// What the map shows now, so placing a pin doesn't move it (the automatic camera frames
    /// whatever is on the map).
    @State private var visible: MKCoordinateRegion?
    @State private var chosen: SimulatedPlace?
    @State private var query = ""
    @State private var results: [SimulatedPlace] = []
    @State private var searching = false
    @State private var searchError: String?
    @State private var typed = ""

    var body: some View {
        VStack(spacing: 0) {
            map
                .frame(minHeight: 260, idealHeight: 340, maxHeight: 420)
            Form {
                status
                search
                if !tools.recentPlaces.isEmpty {
                    Section {
                        ForEach(tools.recentPlaces, id: \.self) { place in
                            Button { choose(place) } label: { LabeledContent(place.name, value: place.coordinates) }
                        }
                    } header: {
                        Text("Recent")
                    } footer: {
                        Button("Forget Recent Places") { tools.forgetRecentPlaces() }.font(.caption)
                    }
                }
                Section {
                    HStack {
                        TextField("Latitude, longitude", text: $typed)
                            .autocorrectionDisabled()
                            #if os(iOS)
                            .keyboardType(.numbersAndPunctuation)
                            #endif
                            .onSubmit(useTyped)
                        Button("Use", action: useTyped).disabled(SimulatedPlace.parse(typed) == nil)
                    }
                } header: {
                    Text("Coordinates")
                } footer: {
                    Text("In degrees, like 59.43696, 24.75353. Tap the map to choose a place there instead.")
                }
            }
        }
        .navigationTitle("Location")
        #if os(iOS)
        // A large title would sit on the map.
        .navigationBarTitleDisplayMode(.inline)
        #endif
        .onAppear {
            if let place = tools.location.place ?? tools.recentPlaces.first { focus(on: place, choosing: tools.location.place == nil) }
        }
    }

    private var map: some View {
        MapReader { proxy in
            Map(position: $camera) {
                if let simulated = tools.location.place {
                    Annotation(simulated.name, coordinate: simulated.coordinate) {
                        Image(systemName: "location.circle.fill")
                            .font(.title)
                            .foregroundStyle(.white, .blue)
                    }
                }
                if let chosen, chosen != tools.location.place {
                    Marker(chosen.name, coordinate: chosen.coordinate)
                }
            }
            .onMapCameraChange { context in visible = context.region }
            // Beside the map's own gestures (pan, zoom), which a plain tap gesture loses to.
            .simultaneousGesture(SpatialTapGesture().onEnded { tap in
                guard let coordinate = proxy.convert(tap.location, from: .local) else { return }
                if let visible { camera = .region(visible) }
                chosen = SimulatedPlace(name: String(format: "%.5f, %.5f", coordinate.latitude, coordinate.longitude),
                                        latitude: coordinate.latitude, longitude: coordinate.longitude)
            })
        }
    }

    @ViewBuilder
    private var status: some View {
        Section {
            switch tools.location {
            case .off:
                Text("This \(ThisDevice.noun) reports its real location.").foregroundStyle(.secondary)
            case .starting(let place):
                HStack(spacing: 10) {
                    ProgressView()
                    Text("Starting at \(place.name)…")
                }
            case .on(let place):
                Label {
                    VStack(alignment: .leading) {
                        Text("Every app sees \(place.name)")
                        Text(place.coordinates).font(.caption).foregroundStyle(.secondary)
                    }
                } icon: {
                    Image(systemName: "location.fill").foregroundStyle(.blue)
                }
            case .failed(let message):
                Note(text: message, kind: .error)
            }
            if let chosen {
                Button(tools.location.running ? "Move to \(chosen.name)" : "Simulate \(chosen.name)", systemImage: "location.fill") {
                    tools.simulate(chosen)
                }
                .disabled(chosen == tools.location.place)
            }
            if tools.location.running {
                Button("Stop and Use the Real Location", systemImage: "location.slash", role: .destructive) { tools.stopLocation() }
            }
        } footer: {
            Text("The simulated location holds while Apparition runs (it stays running in the background meanwhile) and goes back to the real one when you stop it, or when the connection drops. It needs the developer disk image, which Xcode mounts and a restart removes.")
        }
    }

    @ViewBuilder
    private var search: some View {
        Section("Search") {
            HStack {
                TextField("A place or address", text: $query)
                    .onSubmit { Task { await find() } }
                if searching { ProgressView() }
            }
            if let searchError { Note(text: searchError, kind: .warning) }
            ForEach(results, id: \.self) { place in
                Button { choose(place) } label: { LabeledContent(place.name, value: place.coordinates) }
            }
        }
    }

    private func useTyped() {
        guard let place = SimulatedPlace.parse(typed) else { return }
        choose(place)
    }

    private func choose(_ place: SimulatedPlace) {
        focus(on: place, choosing: true)
    }

    private func focus(on place: SimulatedPlace, choosing: Bool) {
        if choosing { chosen = place }
        camera = .region(MKCoordinateRegion(center: place.coordinate, latitudinalMeters: 4000, longitudinalMeters: 4000))
    }

    private func find() async {
        let text = query.trimmingCharacters(in: .whitespaces)
        guard !text.isEmpty else { return }
        searching = true
        searchError = nil
        defer { searching = false }
        let request = MKLocalSearch.Request()
        request.naturalLanguageQuery = text
        do {
            let response = try await MKLocalSearch(request: request).start()
            results = response.mapItems.prefix(6).map { item in
                let coordinate = item.placemark.coordinate
                return SimulatedPlace(name: item.name ?? text, latitude: coordinate.latitude, longitude: coordinate.longitude)
            }
            if results.isEmpty { searchError = "Nothing found for \(text)." }
            if let first = results.first { focus(on: first, choosing: true) }
        } catch {
            results = []
            searchError = "Search failed: \(error.localizedDescription)"
        }
    }
}
#endif

// MARK: Console

struct ConsoleView: View {
    let tools: DeviceToolsModel
    @State private var filter = ""
    @State private var follow = true

    private var shown: [DeviceLogLine] {
        filter.isEmpty ? tools.lines : tools.lines.filter { DeviceToolsModel.matches($0, filter) }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            controls
                .padding(.horizontal)
                .padding(.vertical, 10)
            if let error = tools.consoleError {
                Note(text: error, kind: .error).padding(.horizontal).padding(.bottom, 8)
            }
            Divider()
            lineList
        }
        .navigationTitle(tools.consoleProcess.map { "Console · \($0.title)" } ?? "Console")
        .searchable(text: $filter, prompt: "Filter")
        .task {
            if tools.processes.isEmpty { await tools.refreshProcesses() }
        }
    }

    private var controls: some View {
        HStack(spacing: 12) {
            Menu {
                Button("Everything on This \(ThisDevice.title)") { tools.startConsole(for: nil) }
                let apps = tools.processes.filter { $0.kind == .app || $0.kind == .systemApp }
                if !apps.isEmpty {
                    Section("Apps") {
                        ForEach(apps) { process in
                            Button("\(process.title) (\(process.pid))") { tools.startConsole(for: process) }
                        }
                    }
                }
            } label: {
                Label(tools.consoleProcess?.title ?? "Everything", systemImage: "app.badge")
                    .labelStyle(.titleAndIcon)
                    .lineLimit(1)
            }
            Picker("Level", selection: Binding(get: { tools.consoleLevel }, set: { level in
                tools.consoleLevel = level
                if tools.consoleRunning { tools.startConsole(for: tools.consoleProcess) }
            })) {
                ForEach(ConsoleLevel.allCases) { Text($0.title).tag($0) }
            }
            .pickerStyle(.menu)
            .fixedSize()
            Spacer()
            if tools.consoleRunning {
                Button(tools.paused ? "Resume" : "Pause", systemImage: tools.paused ? "play.fill" : "pause.fill") { tools.paused.toggle() }
                Button("Stop", systemImage: "stop.fill", role: .destructive) { tools.stopConsole() }
            } else {
                Button("Start", systemImage: "play.fill") { tools.startConsole(for: tools.consoleProcess) }
            }
            Menu {
                Toggle("Follow New Lines", isOn: $follow)
                #if !os(tvOS)
                Button("Copy Shown Lines", systemImage: "doc.on.doc") {
                    UIPasteboard.general.string = tools.consoleText(matching: filter)
                }
                #endif
                Button("Clear", systemImage: "trash", role: .destructive) { tools.clearConsole() }
            } label: {
                Label("More", systemImage: "ellipsis.circle")
            }
        }
        .labelStyle(.iconOnly)
        .focusRow()
    }

    private var lineList: some View {
        ScrollViewReader { proxy in
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 4) {
                    ForEach(shown) { line in
                        LogLineRow(line: line).id(line.id)
                    }
                }
                .padding(.horizontal)
                .padding(.vertical, 8)
            }
            .overlay {
                if tools.lines.isEmpty {
                    Text(tools.consoleRunning ? "Waiting for lines…" : "Start the console to read the \(ThisDevice.noun)'s log.")
                        .foregroundStyle(.secondary)
                }
            }
            .onChange(of: tools.lines.last?.id) { _, last in
                guard follow, !tools.paused, let last = shown.last?.id ?? last else { return }
                proxy.scrollTo(last, anchor: .bottom)
            }
        }
    }
}

private struct LogLineRow: View {
    let line: DeviceLogLine

    var body: some View {
        VStack(alignment: .leading, spacing: 1) {
            HStack(spacing: 6) {
                Circle().fill(color).frame(width: 7, height: 7)
                Text(DeviceLogLine.timeFormat.string(from: line.time))
                Text("\(line.process)[\(line.pid)]").fontWeight(.semibold)
                if let subsystem = line.subsystem {
                    Text([subsystem, line.category].compactMap { $0 }.joined(separator: ":")).lineLimit(1)
                }
            }
            .font(.caption2.monospaced())
            .foregroundStyle(.secondary)
            Text(line.message)
                .font(.caption.monospaced())
                .foregroundStyle(line.level == .error || line.level == .fault ? color : .primary)
                .selectableText()
        }
        .accessibilityElement(children: .combine)
    }

    private var color: Color {
        switch line.level {
        case .fault: .red
        case .error: .orange
        case .notice: .secondary
        case .info: .blue
        case .debug: .gray
        }
    }
}
