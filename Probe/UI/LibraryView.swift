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
    let onBrowse: () -> Void
    let onSetup: () -> Void

    /// The app whose JIT launch this screen started, to show its progress on the right card.
    @State private var jitAppId: String?
    @State private var pendingRemoval: LibraryModel.InstalledApp?
    /// The same removal, asked from the card's own button for an app that is gone from the device.
    @State private var pendingMissingRemoval: LibraryModel.InstalledApp?
    @State private var confirmingSelfRefresh = false
    @State private var confirmingUpdate = false
    @State private var pickingFile = false
    @MainActor private static var listedThisLaunch = false

    private static let columns = [GridItem(.adaptive(minimum: 420, maximum: 640), spacing: 20)]

    /// Debuggable apps on the headset that Apparition didn't install (or no longer manages).
    private var otherApps: [ProbeModel.DeviceApp] {
        let managed = Set(library.installed.compactMap(\.installedBundleId))
        return model.deviceApps.filter {
            $0.debuggable && $0.bundleId != Bundle.main.bundleIdentifier && !managed.contains($0.bundleId)
        }
    }

    private var apps: [LibraryModel.InstalledApp] {
        library.installed.sorted { (LibraryModel.expiryDate($0) ?? .distantPast) < (LibraryModel.expiryDate($1) ?? .distantPast) }
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
            .task { await library.checkForUpdate() }
            .task {
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
            Task { await library.refresh(force: true, trigger: "button") }
        } label: {
            Label("Refresh All", systemImage: "arrow.triangle.2.circlepath")
        }
        .buttonStyle(.borderedProminent)
        .disabled(library.busy || model.busy || library.installed.isEmpty)
        .help("Re-sign and reinstall every app now, so each gets a fresh 7 days")
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
    private var otherGroup: some View {
        HStack(alignment: .firstTextBaseline) {
            groupHeader("Other Apps", subtitle: "Debuggable apps installed by Xcode, build-and-sign or another sideloader. Apparition can launch them with JIT, but doesn't sign or refresh them.")
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
            }
        }
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
                Text("Refreshes itself with \(library.signing.signerName(library.selfSigner)) once due, after the other apps. The update applies the next time Apparition closes.")
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
                          tint: left < LibraryModel.refreshMargin ? .orange : .green)
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
        let tint: Color = left < LibraryModel.refreshMargin ? .orange : .green
        return StatusPill(text: "Expires \(expires.relativeToNow)", systemImage: "clock", tint: tint)
    }

    @ViewBuilder
    private func result(_ app: LibraryModel.InstalledApp) -> some View {
        if let error = app.lastError {
            Note(text: error, kind: .error)
                .lineLimit(4)
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
            Button("Remove from Library", systemImage: "trash", role: .destructive) { pendingMissingRemoval = app }
                .buttonStyle(.bordered)
                .focusRow()
                .confirmation("Remove \(app.name) from the library?", item: $pendingMissingRemoval, for: app.id) { app in
                    Button("Remove", role: .destructive) { library.removeInstalled(app.id) }
                } message: { _ in
                    Text("Apparition stops refreshing it and deletes its kept copy.")
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
            Button("Remove from Library", systemImage: "trash", role: .destructive) { pendingRemoval = app }
        } label: {
            Image(systemName: "ellipsis")
                .frame(width: 32, height: 32)
        }
        .menuStyle(.button)
        // On the menu, so iOS's popover points at the row's button rather than the list.
        .confirmation("Remove \(app.name) from the library?", item: $pendingRemoval, for: app.id) { app in
            Button("Remove", role: .destructive) { library.removeInstalled(app.id) }
        } message: { _ in
            Text("Apparition stops refreshing it and deletes its kept copy. The app stays on this \(ThisDevice.noun) until its profile expires; delete it from the \(ThisDevice.home) to remove it now.")
        }
        .buttonBorderShape(.circle)
        .help("More")
    }
}
