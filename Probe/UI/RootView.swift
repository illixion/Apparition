import RAVEUI
import SwiftUI

/// The main window's tabs: in RAVEUI's ornament tab bar on visionOS, a TabView elsewhere.
enum AppTab: String, CaseIterable {
    case library = "Library"
    case browse = "Browse"
    case setup = "Setup"
    case settings = "Settings"
    case diagnostics = "Diagnostics"

    var systemImage: String {
        switch self {
        case .library: "square.grid.2x2"
        case .browse: "magnifyingglass"
        case .setup: "checklist"
        case .settings: "gearshape"
        case .diagnostics: "stethoscope"
        }
    }

}

#if os(visionOS)
extension AppTab: RAVETabItem {
    /// Pinned to the case name, not the display title.
    var accessibilityIdentifier: String { RAVEA11y.tab(String(describing: self)) }
}
#endif

/// The main window: one screen per tab, the tab bar below it, and what's running beside the tabs.
struct RootView: View {
    static let setupFinishedKey = "setupFinished"
    static let showDiagnosticsKey = "showDiagnostics"

    let model: ProbeModel
    let account: AccountModel
    let library: LibraryModel
    let companions: CompanionFinder

    @AppStorage(RootView.showDiagnosticsKey) private var showDiagnostics = false
    @AppStorage(RootView.setupFinishedKey) private var setupFinished = false
    /// Opens on the Library once setup is complete, on Setup on first run or while anything
    /// is missing (whether or not Setup was dismissed with Finish Later).
    @State private var tab: AppTab
    @Environment(\.scenePhase) private var scenePhase
    #if os(visionOS)
    @Environment(\.openWindow) private var openWindow
    #endif

    init(model: ProbeModel, account: AccountModel, library: LibraryModel, companions: CompanionFinder) {
        self.model = model
        self.account = account
        self.library = library
        self.companions = companions
        _tab = State(initialValue: SetupStatus.opensOnLibrary(model: model, account: account) ? .library : .setup)
    }

    private var visibleTabs: [AppTab] {
        AppTab.allCases.filter { $0 != .diagnostics || showDiagnostics }
    }

    var body: some View {
        tabs
            .onChange(of: showDiagnostics) { _, shown in
                if !shown && tab == .diagnostics { tab = .settings }
            }
            .onAppear { model.startBrowsing() }
            // Certificates and profiles: put in Files > Apparition > Signing, or shared or opened here.
            .onChange(of: scenePhase, initial: true) { _, phase in
                if phase == .active { Task { await library.signing.scanInbox() } }
            }
            .onOpenURL { url in
                if let request = LibraryModel.jitRequest(url) {
                    if library.isManaged(request.bundleId) {
                        attach(request)
                    } else {
                        library.pendingJITRequest = request
                        #if os(visionOS)
                        openWindow(id: JITRequestWindow.windowID)
                        #endif
                    }
                } else if let source = LibraryModel.sourceLink(url) {
                    library.linkedSource = source
                    tab = .browse
                } else if LibraryModel.isAppFile(url) {
                    // An .ipa or .deb from Files or Safari's share sheet: the library offers to install it.
                    tab = .library
                    Task { await library.receiveAppFile(url) }
                } else {
                    Task { await library.signing.receive(url) }
                }
            }
            #if !os(visionOS)
            // visionOS asks in JITRequestWindow: a dialog there can land out of reach.
            .confirmationDialog("Enable JIT for \(library.pendingJITRequest.map { library.displayName(of: $0.bundleId) } ?? "")?",
                                isPresented: Binding(get: { library.pendingJITRequest != nil }, set: { if !$0 { library.pendingJITRequest = nil } }),
                                titleVisibility: .visible, presenting: library.pendingJITRequest) { request in
                Button("Enable JIT") { attach(request) }
                Button("Cancel", role: .cancel) {}
            } message: { request in
                Text(LibraryModel.JITRequest.message(request))
            }
            #endif
            .panel(isPresented: Binding(get: { library.incoming != nil }, set: { if !$0 { library.discardIncoming() } })) {
                IncomingAppSheet(library: library) { tab = .library }
            }
            .panel(isPresented: Binding(get: { library.signing.hasIncoming }, set: { if !$0 { library.signing.dismissIncoming() } })) {
                IncomingSigningSheet(signing: library.signing)
            }
            // A sign-in started away from Setup (an install, a refresh) asks for its 2FA code here,
            // unless the screen it started from shows the prompt itself. Closing the sheet cancels it.
            .panel(isPresented: Binding(get: { account.twoFactor != nil && account.twoFactorHosts == 0 },
                                        set: { if !$0, account.twoFactor != nil { account.answerTwoFactor("abort") } })) {
                TwoFactorSheet(account: account)
            }
            #if !APPARITION_RELEASE
            .onChange(of: DebugScreens.shared.tab) { _, wanted in
                guard let wanted else { return }
                tab = wanted
                DebugScreens.shared.tab = nil
            }
            .panel(isPresented: Binding(get: { DebugScreens.shared.panel != nil }, set: { if !$0 { DebugScreens.shared.panel = nil } })) {
                debugPanel
            }
            #endif
    }

    #if !APPARITION_RELEASE
    /// What `showScreen` asked for, as the buttons that open it present it.
    @ViewBuilder
    private var debugPanel: some View {
        let close = { DebugScreens.shared.panel = nil }
        switch DebugScreens.shared.panel {
        case .getSigning:
            ReceiveSigningSheet(signing: library.signing, email: account.email ?? account.lastAppleID) { _ in close() }
        case .shareSigning:
            ShareSigningSheet(signing: library.signing, email: account.email ?? account.lastAppleID ?? "", teamId: account.team?.id, onDone: close)
        case .certificates:
            NavigationStack {
                DeveloperSlotsView(account: account, library: library, onSignIn: close)
                    .toolbar {
                        ToolbarItem(placement: .cancellationAction) { Button("Done", action: close) }
                    }
            }
        case nil:
            EmptyView()
        }
    }
    #endif

    private func attach(_ request: LibraryModel.JITRequest) {
        Task { await model.attachJIT(request) }
    }

    #if os(visionOS)
    private var tabs: some View {
        screen(tab)
            .id(tab)
            .transition(.opacity)
            .animation(.smooth(duration: 0.25), value: tab)
            .ornament(attachmentAnchor: .scene(.bottomFront), contentAlignment: .top) {
                RAVETabBar(tabs: visibleTabs, selection: $tab) {
                    activity
                }
            }
    }
    #else
    private var tabs: some View {
        TabView(selection: $tab) {
            ForEach(visibleTabs, id: \.self) { item in
                Tab(item.rawValue, systemImage: item.systemImage, value: item) {
                    screen(item)
                }
            }
        }
        #if os(iOS)
        .modifier(ActivityAccessory(active: activityText != nil || !model.jitSessions.isEmpty) { activity })
        #endif
    }
    #endif

    @ViewBuilder
    private func screen(_ shown: AppTab) -> some View {
            switch shown {
            case .library:
                LibraryView(model: model, account: account, library: library,
                            onBrowse: { tab = .browse }, onSetup: { tab = .setup })
            case .browse:
                BrowseView(model: model, account: account, library: library, onSetup: { tab = .setup })
            case .setup:
                SetupView(model: model, account: account, companions: companions) {
                    setupFinished = true
                    tab = .library
                }
            case .settings:
                SettingsView(model: model, account: account, library: library, companions: companions, onSetup: { tab = .setup })
            case .diagnostics:
                DiagnosticsView(model: model, account: account, library: library, companions: companions)
            }
    }

    /// Long work (a refresh, a sign-in, a tunnel) shown past the tabs, so it's visible from any
    /// tab, and the apps Apparition is debugging, each with a Stop.
    @ViewBuilder
    private var activity: some View {
        if activityText != nil || !model.jitSessions.isEmpty {
            #if os(visionOS)
            Divider().frame(height: 28)
            #endif
            HStack(spacing: 10) {
                if let text = activityText {
                    ProgressView().controlSize(.small)
                    Text(text)
                        .font(.callout)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                        .frame(maxWidth: 320, alignment: .leading)
                }
                if !model.jitSessions.isEmpty {
                    jitSessions
                }
            }
            .padding(.horizontal, 8)
            .transition(.opacity)
        }
    }

    /// "Debugging Madeira" and a Stop for it; with several apps, Stop asks which.
    @ViewBuilder
    private var jitSessions: some View {
        let sessions = model.jitSessions.sorted()
        Label(sessions.count == 1 ? "Debugging \(library.displayName(of: sessions[0]))" : "Debugging \(sessions.count) apps",
              systemImage: "bolt.fill")
            .font(.callout)
            .lineLimit(1)
            .accessibilityIdentifier("jitSession")
        if sessions.count == 1 {
            Button("Stop", role: .destructive) { model.stopJIT(bundleId: sessions[0]) }
                .buttonStyle(.bordered)
                .controlSize(.small)
                .accessibilityIdentifier("stopJIT")
        } else {
            Menu("Stop") {
                ForEach(sessions, id: \.self) { bundleId in
                    Button(library.displayName(of: bundleId), role: .destructive) { model.stopJIT(bundleId: bundleId) }
                }
            }
            .buttonStyle(.bordered)
            .controlSize(.small)
            .accessibilityIdentifier("stopJIT")
        }
    }

    private var activityText: String? {
        if library.busy { return library.status ?? "Working…" }
        if account.busy { return account.twoFactor != nil ? "Waiting for the 2FA code" : (account.downloadProgress ?? "Signing in…") }
        if model.busy {
            switch model.latestAttempt(model.attempts.first?.action ?? "")?.action {
            case "pair": return model.pinRequested ? "Waiting for the pairing code" : "Pairing…"
            case "install": return "Installing…"
            default: return "Talking to the \(ThisDevice.noun)…"
            }
        }
        return nil
    }
}

#if os(iOS)
/// Long work shown above the tab bar, where iOS 26.1 can hide the accessory while idle.
private struct ActivityAccessory<Accessory: View>: ViewModifier {
    let active: Bool
    @ViewBuilder let accessory: () -> Accessory

    func body(content: Content) -> some View {
        if #available(iOS 26.1, *) {
            content.tabViewBottomAccessory(isEnabled: active) { accessory() }
        } else {
            // No accessory before 26.1: a bar above the tab bar instead.
            content.safeAreaInset(edge: .bottom) {
                if active {
                    accessory()
                        .padding(.vertical, 8)
                        .frame(maxWidth: .infinity)
                        .background(.bar)
                }
            }
        }
    }
}
#endif
