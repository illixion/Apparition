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
                Task { await library.signing.receive(url) }
            }
            .panel(isPresented: Binding(get: { library.signing.hasIncoming }, set: { if !$0 { library.signing.dismissIncoming() } })) {
                IncomingSigningSheet(signing: library.signing)
            }
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
        .modifier(ActivityAccessory(active: activityText != nil) { activity })
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

    /// Long work (a refresh, a sign-in, a tunnel) shown past the tabs, so it's visible from any tab.
    @ViewBuilder
    private var activity: some View {
        if let text = activityText {
            #if os(visionOS)
            Divider().frame(height: 28)
            #endif
            HStack(spacing: 10) {
                ProgressView().controlSize(.small)
                Text(text)
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .frame(maxWidth: 320, alignment: .leading)
            }
            .padding(.horizontal, 8)
            .transition(.opacity)
        }
    }

    private var activityText: String? {
        if library.busy { return library.status ?? "Working…" }
        if account.busy { return account.twoFactor != nil ? "Waiting for the 2FA code" : (account.downloadProgress ?? "Signing in…") }
        if model.busy {
            switch model.latestAttempt(model.attempts.first?.action ?? "")?.action {
            case "pair": return model.pinRequested ? "Waiting for the pairing code" : "Pairing…"
            case "jit": return "Serving JIT"
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
            content
        }
    }
}
#endif
