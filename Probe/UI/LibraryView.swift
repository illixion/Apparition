import DebugTrace
import RAVEUI
import SwiftUI
import UniformTypeIdentifiers

private let log = DebugLogger(subsystem: Bundle.main.bundleIdentifier ?? "Apparition", category: "UI")

/// The default tab: what Apparition installed and keeps signed, with when each runs out.
struct LibraryView: View {
    let model: ProbeModel
    let account: AccountModel
    let library: LibraryModel
    let developer: DeveloperModel
    let onDeveloper: () -> Void
    /// Opens a project's page on the Developer tab, on a section of it.
    let onOpenProject: (String, ProjectTab) -> Void
    let onBrowse: () -> Void
    let onSetup: () -> Void

    /// The app whose JIT launch this screen started, to show its progress on the right card.
    @State private var jitAppId: String?
    /// The app being linked to a project (the sheet).
    @State private var linking: ProbeModel.DeviceApp?
    @State private var pendingRemoval: RemovalConfirmation?
    @State private var removalError: String?
    @State private var confirmingSelfRefresh = false
    @State private var confirmingUpdate = false
    @State private var pickingFile = false
    @MainActor private static var listedThisLaunch = false

    private static let columns = [GridItem(.adaptive(minimum: 420, maximum: 640), spacing: 20)]

    private struct RemovalConfirmation {
        let appID: String
        let token = UUID()
        let deadline = Date().addingTimeInterval(10)
    }

    private var service: BuildService { developer.service }

    /// Apps that belong to a configured project: linked to it by the person, or matched on the bundle
    /// identifier (exactly, or modulo a team or debug suffix). A linked app shows even when it isn't
    /// debuggable, because the person said so.
    private var appstoreApps: [(app: ProbeModel.DeviceApp, project: BuildService.Project, target: BuildService.Target, link: ProjectLink?)] {
        model.deviceApps.compactMap { app in
            guard app.bundleId != Bundle.main.bundleIdentifier else { return nil }
            guard let match = developer.project(for: app), app.debuggable || match.link?.chosen == true else { return nil }
            return (app, match.project, match.target, match.link)
        }
    }

    /// Debuggable apps not managed by Apparition or a configured appstore project.
    private var otherApps: [ProbeModel.DeviceApp] {
        let known = Set(library.installed.compactMap(\.installedBundleId) + appstoreApps.map { $0.app.bundleId })
        return model.deviceApps.filter {
            $0.debuggable && $0.bundleId != Bundle.main.bundleIdentifier && !known.contains($0.bundleId)
        }
    }

    private var apps: [LibraryModel.InstalledApp] {
        let appstoreManaged = Set(appstoreApps.compactMap { app in
            library.installed.first { $0.installedBundleId == app.app.bundleId }?.id
        })
        return library.installed.filter { !appstoreManaged.contains($0.id) }
            .sorted { (LibraryModel.expiryDate($0) ?? .distantPast) < (LibraryModel.expiryDate($1) ?? .distantPast) }
    }

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(alignment: .leading, spacing: 0) {
                    RAVEScreenHeading("Library")
                    #if os(tvOS)
                    // tvOS's toolbar squeezes its title ("Ref…h All") and is far from the focus: here it's the first thing.
                    refreshAllButton
                        .padding(.bottom, 24)
                    #endif
                    content
                }
                .padding(32)
            }
            .task(id: pendingRemoval?.token) {
                guard let confirmation = pendingRemoval else { return }
                try? await Task.sleep(for: .seconds(10))
                guard !Task.isCancelled, pendingRemoval?.token == confirmation.token else { return }
                pendingRemoval = nil
            }
            .onDisappear { pendingRemoval = nil }
            .task {
                await service.restoreConnection()
                if service.connected { await service.refresh() }
                // Once per launch: who else installed debuggable apps here. Refresh by hand after.
                guard !Self.listedThisLaunch, model.hasPairingFile, !model.busy, !library.busy else { return }
                Self.listedThisLaunch = true
                await model.listApps(host: model.defaultTunnelHost, port: model.defaultTunnelPort)
            }
            .raveScreenTitle("Library")
            #if !os(tvOS)
            .toolbar {
                ToolbarItem(placement: .primaryAction) { installFromFileButton }
                ToolbarItem(placement: .primaryAction) { refreshAllButton }
            }
            // An .ipa, or a .deb package like Kodi's: Safari's downloads and Files both lead here.
            .fileImporter(isPresented: $pickingFile, allowedContentTypes: [.item]) { result in
                guard case .success(let url) = result else { return }
                Task { await library.receiveAppFile(url) }
            }
            #endif
            .confirmationDialog("Update Apparition to \(library.availableUpdate?.version ?? "")?", isPresented: $confirmingUpdate, titleVisibility: .visible) {
                Button("Download and Install") { Task { await library.updateSelf() } }
            } message: {
                Text("Apparition downloads the official release, checks its signature, signs it with \(library.signing.signerName(library.selfSigner)) and hands it to \(ThisDevice.os), keeping its data. It installs when you close Apparition, like an App Store update.")
            }
            .confirmationDialog("Refresh Apparition now?", isPresented: $confirmingSelfRefresh, titleVisibility: .visible) {
                Button("Refresh and Reinstall") { Task { await library.refreshSelf() } }
            } message: {
                Text("Apparition signs itself again with \(library.signing.signerName(library.selfSigner)) and hands the update to \(ThisDevice.os), keeping its data. The update applies when you close Apparition, like an App Store update.")
            }
            .sheet(item: $linking) { app in
                LinkProjectSheet(developer: developer, app: app) { linking = nil }
            }
            .alert("Removal Incomplete", isPresented: Binding(get: { removalError != nil }, set: { if !$0 { removalError = nil } })) {
                Button("OK", role: .cancel) {}
            } message: {
                Text(removalError ?? "")
            }
        }
    }

    #if !os(tvOS)
    private var installFromFileButton: some View {
        Button {
            pickingFile = true
        } label: {
            Label("Install from a File", systemImage: "doc.badge.plus")
        }
        .disabled(library.busy || model.busy)
        .help("Install an .ipa, or the app in a .deb package, from Files")
    }
    #endif

    private var refreshAllButton: some View {
        Button {
            Task { await library.refresh(force: false, trigger: "button") }
        } label: {
            Label("Refresh Expiring", systemImage: "arrow.triangle.2.circlepath")
        }
        .buttonStyle(.borderedProminent)
        .disabled(library.busy || model.busy || !library.hasDue)
        .help("Re-sign and reinstall the apps with 3 days or less left. Each app's own Refresh button refreshes it whatever it has left.")
    }

    @ViewBuilder
    private var content: some View {
        VStack(alignment: .leading, spacing: 24) {
            if let limit = library.appLimit, !library.busy {
                Card { AppLimitView(model: model, library: library, limit: limit) }
            }
            if let choice = library.certificateChoice, !library.busy {
                Card { SharedCertificateView(account: account, library: library, choice: choice) }
            }
            if library.installed.isEmpty && otherApps.isEmpty && model.deviceAppsFetchedAt == nil {
                if let me = library.selfApp { selfCard(me) }
                emptyState
            } else {
                managedGroup
                appstoreGroup
                otherGroup
                    .padding(.top, 16)
            }
        }
    }

    // MARK: Groups

    private func groupHeader(_ title: String, subtitle: String) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(title).font(.title.weight(.semibold))
            Text(subtitle).font(.callout).foregroundStyle(.secondary)
        }
    }

    @ViewBuilder
    private var managedGroup: some View {
        groupHeader("Managed by Apparition", subtitle: "Signed and kept refreshed by Apparition.")
        if let me = library.selfApp {
            selfCard(me)
        }
        if library.installed.isEmpty {
            HStack(spacing: 16) {
                Text("No apps yet.").foregroundStyle(.secondary)
                Button("Browse Sources", action: onBrowse)
                    .buttonStyle(.bordered)
                #if !os(tvOS)
                installFromFileButton
                    .buttonStyle(.bordered)
                #endif
            }
            .focusRow()
        } else {
            summary
            LazyVGrid(columns: Self.columns, spacing: 20) {
                ForEach(apps) { app in
                    card(app)
                }
            }
        }
    }

    @ViewBuilder
    private var appstoreGroup: some View {
        VStack(alignment: .leading, spacing: 16) {
            groupHeader("Your Projects", subtitle: "Apps on this \(ThisDevice.noun) that belong to a project on your Mac's appstore. Install builds and updates here, from the project's page, or link an app that isn't recognised.")
            if service.connected {
                LazyVGrid(columns: Self.columns, spacing: 20) {
                    ForEach(appstoreApps, id: \.app.bundleId) { match in
                        appstoreCard(match.app, project: match.project, target: match.target, link: match.link)
                    }
                }
                if appstoreApps.isEmpty {
                    Text(model.deviceAppsFetchedAt == nil ? "Check this device's apps to find configured project builds." : "No installed apps match a configured project. Link one from Other Apps below.")
                        .foregroundStyle(.secondary)
                }
            } else {
                Text("Building your own apps on a Mac? Connect to its appstore to see them here with their updates and reports.")
                    .foregroundStyle(.secondary)
                Button("Set Up in Developer", action: onDeveloper)
                    .buttonStyle(.bordered)
            }
        }
        .padding(.top, 16)
    }

    @ViewBuilder
    private var otherGroup: some View {
        HStack(alignment: .firstTextBaseline) {
            groupHeader("Other Apps", subtitle: "Apps outside Apparition and appstore projects. Apparition can launch debuggable apps with JIT.")
            Spacer()
            if model.busy && model.latestAttempt("apps")?.outcome == nil && model.attempts.first?.action == "apps" {
                ProgressView()
            } else {
                Button {
                    Task { await model.listApps(host: model.defaultTunnelHost, port: model.defaultTunnelPort) }
                } label: {
                    Label("Check Again", systemImage: "arrow.clockwise")
                }
                .buttonStyle(.bordered)
                .disabled(model.busy || library.busy)
            }
        }
        .focusRow()
        if let attempt = model.latestAttempt("apps"), let outcome = attempt.outcome, outcome.hasPrefix("error") {
            Note(text: "Couldn't list this \(ThisDevice.noun)'s apps: \(outcome.dropFirst(7))", kind: .warning)
        }
        if otherApps.isEmpty {
            Text(model.deviceAppsFetchedAt == nil ? "Not checked yet." : "No other debuggable apps on this \(ThisDevice.noun).")
                .foregroundStyle(.secondary)
        } else {
            LazyVGrid(columns: Self.columns, spacing: 20) {
                ForEach(otherApps) { app in
                    otherCard(app)
                }
            }
        }
        let others = model.deviceApps.filter { !$0.debuggable }.count
        if let fetched = Date.iso(model.deviceAppsFetchedAt) {
            Text("Checked \(fetched.relativeToNow)" + (others > 0 ? ". \(others) more app\(others == 1 ? " isn't" : "s aren't") debuggable (App Store or distribution signed), so JIT can't attach to \(others == 1 ? "it" : "them")." : "."))
                .font(.footnote)
                .foregroundStyle(.tertiary)
        }
    }

    private func otherCard(_ app: ProbeModel.DeviceApp) -> some View {
        let jitId = "device:\(app.bundleId)"
        return Card {
            VStack(alignment: .leading, spacing: 16) {
                HStack(alignment: .top, spacing: 16) {
                    AppIconView(url: nil, name: app.name, size: 72)
                    VStack(alignment: .leading, spacing: 6) {
                        Text(app.name)
                            .font(.title3.weight(.semibold))
                            .lineLimit(1)
                        if let version = app.version, !version.isEmpty {
                            Text("Version \(version)").font(.callout).foregroundStyle(.secondary)
                        }
                        Text(app.bundleId)
                            .font(.caption.monospaced())
                            .foregroundStyle(.secondary)
                            .lineLimit(1)
                            .selectableText()
                        if let signer = app.signer {
                            Text(signer).font(.caption).foregroundStyle(.tertiary).lineLimit(1)
                        }
                    }
                    Spacer(minLength: 0)
                }
                if jitAppId == jitId, let jit = jitNote() { jit }
                Button {
                    jitAppId = jitId
                    log.info("JIT launch of another app from the library")
                    Task { await model.launchJIT(host: model.defaultTunnelHost, port: model.defaultTunnelPort, bundleId: app.bundleId, waitSeconds: 600) }
                } label: {
                    Label("Launch with JIT", systemImage: "bolt.fill")
                }
                .buttonStyle(.bordered)
                .disabled(model.busy || library.busy)
                if service.connected && !(service.state?.repos.isEmpty ?? true) {
                    Button { linking = app } label: { Label("Link to Project…", systemImage: "link") }
                        .buttonStyle(.bordered)
                }
            }
        }
    }

    private func appstoreCard(_ app: ProbeModel.DeviceApp, project: BuildService.Project, target: BuildService.Target,
                              link: ProjectLink?) -> some View {
        let jitId = "device:\(app.bundleId)"
        let projectBundleId = target.bundleId ?? library.installed.first { $0.installedBundleId == app.bundleId }?.id ?? app.bundleId
        let artifact = service.artifact(for: projectBundleId, installedBuild: app.build)
        let latestArtifact = service.latestArtifact(project: project, target: target, bundleId: projectBundleId)
        let updateAvailable = latestArtifact.map {
            app.build == nil || ($0.app.build ?? $0.buildId) != app.build
        } ?? false
        let traces = service.traces(for: app.bundleId, installedBuild: app.build)
        let crashes = service.crashLogs(for: app.bundleId, installedBuild: app.build)
        return Card {
            VStack(alignment: .leading, spacing: 16) {
                HStack(alignment: .top, spacing: 16) {
                    AppIconView(url: nil, name: app.name, size: 72)
                    VStack(alignment: .leading, spacing: 6) {
                        HStack(spacing: 8) {
                            Text(app.name).font(.title3.weight(.semibold)).lineLimit(1)
                            if updateAvailable {
                                Text("Update available")
                                    .font(.caption.weight(.semibold))
                                    .foregroundStyle(.orange)
                                    .padding(.horizontal, 8)
                                    .padding(.vertical, 4)
                                    .background(.orange.opacity(0.15), in: Capsule())
                                    .fixedSize()
                            }
                        }
                        Text("\(project.name) · \(ThisDevice.appCompatibilityLabel(deviceFamily: app.deviceFamily))")
                            .font(.callout).foregroundStyle(.secondary)
                        Text("\(link?.chosen == true ? "Linked to" : "Built on the Mac for") \(target.device) · \(target.platform)")
                            .font(.caption).foregroundStyle(.secondary)
                        if let version = app.version, !version.isEmpty {
                            Text("Version \(version)").font(.callout).foregroundStyle(.secondary)
                        }
                        if let build = app.build, !build.isEmpty {
                            Text("Installed build \(build)").font(.caption).foregroundStyle(.secondary)
                        }
                        if let artifact {
                            Text("\(artifact.app.build == app.build ? "Kept build" : "Latest kept build") \(artifact.app.build ?? artifact.buildId)")
                                .font(.caption).foregroundStyle(.secondary)
                        }
                        Text(app.bundleId).font(.caption.monospaced()).foregroundStyle(.tertiary).lineLimit(1).selectableText()
                    }
                    Spacer(minLength: 0)
                }
                appstoreUpdateControls(app: app, project: project, target: target, bundleId: projectBundleId)
                HStack(spacing: 12) {
                    Button {
                        jitAppId = jitId
                        Task { await model.launchJIT(host: model.defaultTunnelHost, port: model.defaultTunnelPort, bundleId: app.bundleId, waitSeconds: 600) }
                    } label: { Label("Launch with JIT", systemImage: "bolt.fill") }
                        .disabled(model.busy || library.busy)
                    Button { onOpenProject(project.name, .overview) } label: {
                        Label("Open Project", systemImage: "hammer")
                    }
                    if !traces.isEmpty || !crashes.isEmpty {
                        Button { onOpenProject(project.name, .reports) } label: {
                            Label("\(crashes.count) crash\(crashes.count == 1 ? "" : "es"), \(traces.count) trace\(traces.count == 1 ? "" : "s")",
                                  systemImage: "exclamationmark.triangle")
                        }
                    }
                    Menu {
                        Button("Link to a Different Project…", systemImage: "link") { linking = app }
                        Button("Unlink from \(project.name)", systemImage: "link.badge.plus", role: .destructive) { developer.unlink(app.bundleId) }
                    } label: { Image(systemName: "ellipsis").frame(width: 32, height: 32) }
                        .menuStyle(.button)
                        .buttonBorderShape(.circle)
                }
                .buttonStyle(.bordered)
                if jitAppId == jitId, let jit = jitNote() { jit }
            }
        }
        .onChange(of: service.artifacts.map(\.id)) { _, _ in
            guard UserDefaults.standard.bool(forKey: "appstoreAutoUpdate-\(projectBundleId)"),
                  let latest = service.latestArtifact(project: project, target: target, bundleId: projectBundleId),
                  app.build == nil || (latest.app.build ?? latest.buildId) != app.build else { return }
            guard developer.installOptions(target).canInstallHere else { return }
            prepareUpdate(latest, project: project, target: target)
        }
    }

    /// Updating this device's copy from a build the Mac kept: Apparition downloads it and signs it
    /// like any other install. Building and re-signing are on the project's page.
    @ViewBuilder
    private func appstoreUpdateControls(app: ProbeModel.DeviceApp, project: BuildService.Project,
                                        target: BuildService.Target, bundleId: String) -> some View {
        let latest = service.latestArtifact(project: project, target: target, bundleId: bundleId)
        let hasUpdate = latest.map { app.build == nil || ($0.app.build ?? $0.buildId) != app.build } ?? false
        let phase = developer.phase(project, target)
        let canInstallHere = developer.installOptions(target).canInstallHere
        let preparesUpdates = Binding(get: {
            UserDefaults.standard.bool(forKey: "appstoreAutoUpdate-\(bundleId)")
        }, set: { enabled in
            UserDefaults.standard.set(enabled, forKey: "appstoreAutoUpdate-\(bundleId)")
            if enabled, canInstallHere, let latest, hasUpdate { prepareUpdate(latest, project: project, target: target) }
        })
        VStack(alignment: .leading, spacing: 12) {
            if let phase, phase.isWorking {
                switch phase {
                case .building: ProgressView("Building…")
                case .downloading: ProgressView("Downloading kept build…")
                default: ProgressView(library.status ?? "Installing…")
                }
            } else if case .failed(let reason, _)? = phase {
                Note(text: reason, kind: .error).lineLimit(4)
            } else if let latest, hasUpdate, canInstallHere {
                Button {
                    prepareUpdate(latest, project: project, target: target)
                } label: {
                    Label("Install update · \(latest.app.build ?? latest.buildId)", systemImage: "arrow.down.app")
                }
                .buttonStyle(.borderedProminent)
                .disabled(service.busy || library.busy)
            } else if let latest, hasUpdate {
                Button { onOpenProject(project.name, .overview) } label: {
                    Label("Update ready · \(latest.app.build ?? latest.buildId)", systemImage: "arrow.down.app")
                }
                .buttonStyle(.bordered)
            }
            if canInstallHere {
                Toggle("Automatically install updates", isOn: preparesUpdates)
                if preparesUpdates.wrappedValue {
                    Text("When the Mac keeps a newer build, Apparition downloads it, signs it with this \(ThisDevice.noun)'s own identity and installs it, using the signing plan you chose the first time.")
                        .font(.caption).foregroundStyle(.secondary)
                }
            }
        }
    }

    private func prepareUpdate(_ artifact: BuildService.Artifact, project: BuildService.Project, target: BuildService.Target) {
        guard developer.phase(project, target)?.isWorking != true, !library.busy else { return }
        Task { await developer.install(artifact, project: project, target: target) }
    }

    // MARK: Summary and progress

    @ViewBuilder
    private var summary: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(spacing: 12) {
                Text("^[\(library.installed.count) app](inflect: true)")
                    .font(.title3.weight(.semibold))
                if let next = apps.compactMap(LibraryModel.expiryDate).first {
                    Text("·").foregroundStyle(.tertiary)
                    Text(next < Date() ? "one has expired" : "next expiry \(next.relativeToNow)")
                        .font(.title3)
                        .foregroundStyle(.secondary)
                }
                // From the stored expiry dates: nothing is asked of Apple or anyone else.
                TimelineView(.periodic(from: .now, by: 60)) { _ in
                    if library.dueCount > 0 {
                        HStack(spacing: 12) {
                            Text("·").foregroundStyle(.tertiary)
                            Text("^[\(library.dueCount) app](inflect: true) due for refresh")
                                .font(.title3)
                                .foregroundStyle(.orange)
                        }
                    }
                }
            }
            if library.busy {
                HStack(spacing: 12) {
                    ProgressView()
                    Text(library.status ?? "Working…")
                        .font(.callout)
                        .foregroundStyle(.secondary)
                        .lineLimit(2)
                    Spacer(minLength: 0)
                    StopButton(library: library)
                }
                .focusRow()
                #if os(visionOS)
                // visionOS doesn't replace an app that has a window open: the install waits, and
                // the refresh with it, until every one of them is closed.
                Note(text: "Close every window of an app before it's refreshed. Vision Pro won't replace an app that's open, so the refresh waits until it closes.", kind: .info)
                #endif
            } else if let run = library.refreshRuns.first, let finished = Date.iso(run.finishedAt) {
                lastRun(run, finished: finished)
            }
            if !model.routeChosen {
                Button(action: onSetup) {
                    Note(text: "Apparition doesn't know how to reach this \(ThisDevice.noun) yet. Choose Companion or Local VPN in Setup.", kind: .warning)
                }
                .buttonStyle(.plain)
            } else if let problem = account.anisetteProblem, problem.url == account.anisetteURL {
                ConnectionProblemNote(model: model, account: account, problem: .anisette(problem), onSettings: onSetup)
            } else if let mode = model.unreachableMode, mode == model.connectionMode {
                ConnectionProblemNote(model: model, account: account, problem: .unreachable(mode), onSettings: onSetup)
            }
        }
    }

    private func lastRun(_ run: LibraryModel.RefreshRun, finished: Date) -> some View {
        let failed = run.results.values.filter { $0.hasPrefix("error") }.count
        let ok = run.results.values.filter { $0.hasPrefix("ok") }.count
        let stopped = run.results.values.contains(LibraryModel.stoppedResult)
        let text: String
        if run.results["-"] == "nothing due" {
            text = "Last check \(finished.relativeToNow): nothing was due"
        } else if stopped {
            text = "Last refresh \(finished.relativeToNow): stopped, \(ok) refreshed" + (failed > 0 ? ", \(failed) failed" : "")
        } else if failed > 0 {
            text = "Last refresh \(finished.relativeToNow): \(ok) refreshed, \(failed) failed"
        } else {
            text = "Last refresh \(finished.relativeToNow): \(ok) refreshed"
        }
        return Note(text: text, kind: failed > 0 ? .warning : .info)
    }

    private var emptyState: some View {
        ContentUnavailableView {
            Label("No Apps Yet", systemImage: "square.stack.3d.up.slash")
        } description: {
            Text("Install apps from AltStore or SideStore sources, from a GitHub repository's releases, or from an .ipa or .deb file. Apparition signs them with your Apple ID and keeps them refreshed.")
        } actions: {
            Button("Browse Sources", action: onBrowse)
                .buttonStyle(.borderedProminent)
            #if !os(tvOS)
            installFromFileButton
            #endif
            Button("Finish Setup", action: onSetup)
        }
        .frame(maxWidth: .infinity, minHeight: 420)
    }

    // MARK: Apparition's own card

    /// Apparition itself: it refreshes last in each run once due, or here by hand.
    private func selfCard(_ me: SelfApp) -> some View {
        Card {
            VStack(alignment: .leading, spacing: 16) {
                HStack(alignment: .top, spacing: 16) {
                    AppIconView(url: nil, name: "Apparition", size: 72)
                    VStack(alignment: .leading, spacing: 6) {
                        HStack(spacing: 8) {
                            Text("Apparition")
                                .font(.title3.weight(.semibold))
                            StatusPill(text: "This app", systemImage: "sparkles", tint: .accentColor)
                        }
                        Text(["Version \(SelfUpdate.currentVersion)", me.teamName, me.teamId.map { "Team \($0)" }].compactMap { $0 }.joined(separator: " · "))
                            .font(.callout)
                            .foregroundStyle(.secondary)
                        TimelineView(.periodic(from: .now, by: 60)) { _ in
                            selfExpiryPill(me)
                        }
                    }
                    Spacer(minLength: 0)
                }
                if library.selfRecord.pending != nil && !library.busy {
                    Note(text: "The refreshed copy is ready. Close Apparition to apply it, then open it again.", kind: .info)
                } else if let error = library.selfRecord.lastError {
                    Note(text: error, kind: .error).lineLimit(4)
                    FailureAge(at: library.selfRecord.lastErrorAt)
                } else if let signed = Date.iso(library.selfRecord.signedAt) {
                    Note(text: "Refreshed itself \(signed.relativeToNow)" + (library.selfRecord.signedWith.map { " with \($0)" } ?? ""), kind: .success)
                }
                if let update = library.availableUpdate {
                    VStack(alignment: .leading, spacing: 8) {
                        Note(text: "Apparition \(update.version) is available.", kind: .info)
                        if let notes = update.notes, !notes.isEmpty {
                            Text(notes)
                                .font(.callout)
                                .foregroundStyle(.secondary)
                                .lineLimit(6)
                        }
                    }
                }
                if library.availableUpdate == nil, library.updateCheck.checkedAt != nil {
                    if let error = library.updateCheck.error {
                        Note(text: "Couldn't check for an update: \(error)", kind: .error).lineLimit(3)
                    } else {
                        Note(text: "Apparition is up to date.", kind: .success)
                    }
                }
                Text("Refreshes itself with \(library.signing.signerName(library.selfSigner)) once due, after the other apps. It never looks for a newer Apparition on its own: Check for Update asks, and so does the Refresh Apps shortcut's Check for Updates option. The update applies the next time Apparition closes.")
                    .font(.callout)
                    .foregroundStyle(.secondary)
                HStack(spacing: 12) {
                    if let update = library.availableUpdate {
                        Button {
                            confirmingUpdate = true
                        } label: {
                            Label("Update to \(update.version)", systemImage: "arrow.down.circle")
                        }
                        .buttonStyle(.borderedProminent)
                        .disabled(library.busy || model.busy)
                    } else {
                        Button {
                            Task { await library.checkForUpdate() }
                        } label: {
                            if library.updateCheck.checking {
                                ProgressView()
                            } else {
                                Label("Check for Update", systemImage: "arrow.down.circle.dotted")
                            }
                        }
                        .buttonStyle(.bordered)
                        .disabled(library.updateCheck.checking)
                    }
                    Button {
                        confirmingSelfRefresh = true
                    } label: {
                        Label("Refresh", systemImage: "arrow.clockwise")
                    }
                    .buttonStyle(.bordered)
                    .disabled(library.busy || model.busy)
                }
                .focusRow()
            }
        }
    }

    private func selfExpiryPill(_ me: SelfApp) -> StatusPill {
        guard let expires = me.expires else {
            return StatusPill(text: "No expiry", systemImage: "clock.badge.questionmark", tint: .secondary)
        }
        let left = expires.timeIntervalSinceNow
        if left <= 0 {
            return StatusPill(text: "Expired \(expires.relativeToNow)", systemImage: "xmark.circle.fill", tint: .red)
        }
        return StatusPill(text: "Expires \(expires.relativeToNow)", systemImage: "clock",
                          tint: left <= LibraryModel.refreshMargin ? .orange : .green)
    }

    // MARK: App card

    private func card(_ app: LibraryModel.InstalledApp) -> some View {
        let listing = library.sourceApp(for: app)
        return Card {
            VStack(alignment: .leading, spacing: 16) {
                HStack(alignment: .top, spacing: 16) {
                    AppIconView(url: listing?.iconURL.flatMap(URL.init(string:)), name: app.name, size: 72)
                    VStack(alignment: .leading, spacing: 6) {
                        Text(app.name)
                            .font(.title3.weight(.semibold))
                            .lineLimit(1)
                        Text("Version \(app.version)")
                            .font(.callout)
                            .foregroundStyle(.secondary)
                        TimelineView(.periodic(from: .now, by: 60)) { _ in
                            expiryPill(app)
                        }
                    }
                    Spacer(minLength: 0)
                    moreMenu(app, listing: listing)
                }
                .focusRow()
                result(app)
                if library.isMissingFromDevice(app) { missingFromDevice(app) }
                else if pendingRemoval?.appID == app.id { removalButton(app) }
                if let jit = jitStatus(app) { jit }
                actions(app, listing: listing)
            }
        }
    }

    private func expiryPill(_ app: LibraryModel.InstalledApp) -> some View {
        guard let expires = LibraryModel.expiryDate(app) else {
            return StatusPill(text: "Not signed yet", systemImage: "clock.badge.questionmark", tint: .secondary)
        }
        let left = expires.timeIntervalSinceNow
        if left <= 0 {
            return StatusPill(text: "Expired \(expires.relativeToNow)", systemImage: "xmark.circle.fill", tint: .red)
        }
        let tint: Color = left <= LibraryModel.refreshMargin ? .orange : .green
        return StatusPill(text: "Expires \(expires.relativeToNow)", systemImage: "clock", tint: tint)
    }

    @ViewBuilder
    private func result(_ app: LibraryModel.InstalledApp) -> some View {
        if let error = app.lastError {
            Note(text: error, kind: .error)
                .lineLimit(4)
            FailureAge(at: app.lastErrorAt)
            if DeveloperSlots.isSlotError(error) {
                SlotErrorActions(account: account, library: library, error: error, onSetup: onSetup) {
                    Task { await library.refresh(appId: app.id) }
                }
            }
        } else if let signed = Date.iso(app.signedAt) {
            Note(text: "Refreshed \(signed.relativeToNow)" + (app.signedWith.map { " with \($0)" } ?? ""), kind: .success)
        }
    }

    /// An app deleted from the Home Screen: it can be taken out of the library right here, not
    /// only from the menu. A refresh would install it again.
    private func missingFromDevice(_ app: LibraryModel.InstalledApp) -> some View {
        VStack(alignment: .leading, spacing: 10) {
            Note(text: "\(app.name) isn't on this \(ThisDevice.noun) any more. Refresh installs it again; remove it to stop keeping it.", kind: .warning)
            removalButton(app)
        }
    }

    private func removalButton(_ app: LibraryModel.InstalledApp) -> some View {
        let confirming = pendingRemoval?.appID == app.id
        return VStack(alignment: .leading, spacing: 8) {
            Button(role: .destructive) {
                if let confirmation = pendingRemoval, confirmation.appID == app.id,
                   Date() < confirmation.deadline {
                    pendingRemoval = nil
                    Task { removalError = await library.removeFromDeviceAndLibrary(app.id) }
                } else {
                    pendingRemoval = RemovalConfirmation(appID: app.id)
                }
            } label: {
                Label(confirming ? "Are you sure? Remove" : (library.isMissingFromDevice(app) ? "Remove from Library" : "Remove App"), systemImage: "trash")
                    .foregroundStyle(.white)
            }
            .buttonStyle(.borderedProminent)
            .tint(.red)
            .focusRow()
            if confirming {
                Text(library.isMissingFromDevice(app)
                     ? "Apparition stops refreshing it and deletes its kept copy."
                     : "Apparition deletes the app and its data from this \(ThisDevice.noun) (and its watch app from the Apple Watch), stops refreshing it and deletes its kept copy. If the device can't be reached, the app still leaves this list.")
                    .font(.callout)
                    .foregroundStyle(.secondary)
            }
        }
    }

    private func jitStatus(_ app: LibraryModel.InstalledApp) -> Note? {
        guard jitAppId == app.id else { return nil }
        return jitNote()
    }

    private func jitNote() -> Note? {
        guard let attempt = model.latestAttempt("jit") else { return nil }
        switch attempt.outcome {
        case nil: return Note(text: "Serving JIT until the app closes. Stop it here or from Apparition's notification.", kind: .info)
        case "ok": return Note(text: "JIT session finished: \(attempt.detail ?? "ok")", kind: .success)
        case let outcome?: return Note(text: "JIT launch failed: \(outcome)", kind: .error)
        }
    }

    private func actions(_ app: LibraryModel.InstalledApp, listing: Sources.App?) -> some View {
        HStack(spacing: 12) {
            Button {
                Task { await library.refresh(appId: app.id) }
            } label: {
                Label("Refresh", systemImage: "arrow.clockwise")
            }
            .disabled(library.busy || model.busy)

            if let bundleId = app.installedBundleId, model.jitSessions.contains(bundleId) {
                Button(role: .destructive) {
                    model.stopJIT(bundleId: bundleId)
                } label: {
                    Label("Stop JIT", systemImage: "stop.fill")
                }
                .help("Detaches the debugger: \(app.name) keeps running, without JIT")
            } else if let bundleId = app.installedBundleId {
                Button {
                    jitAppId = app.id
                    log.info("JIT launch from the library")
                    Task { await model.launchJIT(host: model.defaultTunnelHost, port: model.defaultTunnelPort, bundleId: bundleId, waitSeconds: 600) }
                } label: {
                    Label("Launch with JIT", systemImage: "bolt.fill")
                }
                .disabled(model.busy || library.busy)
                .help("Opens \(app.name) under a debugger so it can use JIT (UTM needs this to run VMs fast)")
            }

            if let listing, listing.version != app.version {
                Button {
                    Task { await library.install(sourceId: app.source, appId: app.id, stripExtensions: app.stripExtensions) }
                } label: {
                    Label("Update to \(listing.version)", systemImage: "arrow.down.circle")
                }
                .disabled(library.busy || model.busy)
            }
        }
        .focusRow()
        .buttonStyle(.bordered)
    }

    private func moreMenu(_ app: LibraryModel.InstalledApp, listing: Sources.App?) -> some View {
        Menu {
            if let bundleId = app.installedBundleId {
                Text(bundleId)
            }
            Text("From \(app.source)")
            Button("Review Install Options", systemImage: "slider.horizontal.3") {
                Task { await library.reviewInstalledApp(app.id) }
            }
            .disabled(library.busy || model.busy)
            Divider()
            Picker(selection: Binding(get: { app.signWith ?? "" },
                                      set: { library.setSigner($0.isEmpty ? nil : $0, for: app.id) })) {
                Text("Default (\(library.signing.signerName(library.signing.defaultSigner)))").tag("")
                Text("Apple ID").tag(SigningStore.appleID)
                ForEach(library.signing.identities) { identity in
                    Text(identity.name).tag(identity.id)
                }
            } label: {
                Label("Sign With", systemImage: "signature")
            }
            .pickerStyle(.menu)
            Divider()
            Button("Remove App", systemImage: "trash", role: .destructive) {
                pendingRemoval = RemovalConfirmation(appID: app.id)
            }
        } label: {
            Image(systemName: "ellipsis")
                .frame(width: 32, height: 32)
        }
        .menuStyle(.button)
        .buttonBorderShape(.circle)
        .help("More")
    }
}

/// When a failure shown on a card happened: it stays until a refresh succeeds, so it can be old.
private struct FailureAge: View {
    let at: String?

    var body: some View {
        Text(Date.iso(at).map { "Failed \($0.relativeToNow)" } ?? "Failed earlier")
            .font(.caption)
            .foregroundStyle(.secondary)
    }
}
