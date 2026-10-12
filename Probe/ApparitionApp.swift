import DebugTrace
import DebugTraceServer
import DebugTraceUI
import SwiftUI

@main
struct ApparitionApp: App {
    @State private var model: ProbeModel
    @State private var account: AccountModel
    @State private var library: LibraryModel
    @State private var companions = CompanionFinder()
    @Environment(\.scenePhase) private var scenePhase

    init() {
        DebugTrace.configure(.init(subsystems: [Bundle.main.bundleIdentifier ?? "Apparition"]))
        DebugTraceServer.startIfRequested()
        // Before any model reads its files: brings back what a tvOS storage purge removed.
        StateVault.sync()
        let model = ProbeModel()
        _model = State(initialValue: model)
        ProbeEndpoints.register(model)
        let account = AccountModel()
        _account = State(initialValue: account)
        AccountEndpoints.register(account)
        let signing = SigningStore()
        signing.onAppleIDKeyImported = { [weak account] in account?.refreshSession() }
        SigningEndpoints.register(signing)
        let library = LibraryModel(account: account, probe: model, signing: signing)
        _library = State(initialValue: library)
        LibraryEndpoints.register(library, account: account)
        library.registerBackgroundTasks()
        CompanionEndpoints.register(_companions.wrappedValue, probe: model)
        DeveloperSlotsEndpoints.register()
        #if !APPARITION_RELEASE
        ExperimentEndpoints.register(probe: model, account: account)
        VaultEndpoints.register()
        #endif
        ShortcutActions.register(probe: model, account: account, library: library)
        IntentEndpoints.register(probe: model, account: account, library: library)
    }

    var body: some Scene {
        WindowGroup {
            RootView(model: model, account: account, library: library, companions: companions)
                .debugApprovalPrompts()
                .task { await library.scheduleRefreshIfNeeded() }
                .onChange(of: scenePhase) { _, phase in
                    if phase != .active { StateVault.sync() }
                }
        }
        #if !os(tvOS)
        .defaultSize(width: 1180, height: 820)
        #endif
        #if os(visionOS)
        DebugApprovalWindow()
        #endif
    }
}
