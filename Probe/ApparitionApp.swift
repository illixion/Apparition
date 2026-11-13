import DebugTrace
import DebugTraceServer
import DebugTraceUI
import RAVESupport
import SwiftUI

@main
struct ApparitionApp: App {
    @State private var model: ProbeModel
    @State private var account: AccountModel
    @State private var library: LibraryModel
    @State private var companions = CompanionFinder()
    @State private var debugAccess: DebugAccessModel
    @Environment(\.scenePhase) private var scenePhase

    init() {
        DebugTrace.configure(.init(subsystems: [Bundle.main.bundleIdentifier ?? "Apparition"]))
        DebugTraceServer.startIfRequested()
        let debugAccess = DebugAccessModel()
        _debugAccess = State(initialValue: debugAccess)
        // Before any model reads its files: brings back what a tvOS storage purge removed.
        StateVault.sync()
        let model = ProbeModel()
        _model = State(initialValue: model)
        ProbeEndpoints.register(model)
        VerboseLogEndpoints.register()
        let account = AccountModel()
        _account = State(initialValue: account)
        AccountEndpoints.register(account)
        let signing = SigningStore()
        signing.onAppleIDKeyImported = { [weak account] in account?.refreshSession() }
        SigningEndpoints.register(signing)
        NearbyEndpoints.register(signing, account: account)
        let library = LibraryModel(account: account, probe: model, signing: signing)
        _library = State(initialValue: library)
        LibraryEndpoints.register(library, account: account)
        library.registerBackgroundTasks()
        CompanionEndpoints.register(_companions.wrappedValue, probe: model)
        let finder = _companions.wrappedValue
        model.locateCompanion = { [weak finder] name in await finder?.locate(name: name) }
        // A paired anisette channel follows the companion's address; its credential moves in
        // Keychain and never enters the saved URL.
        model.onCompanionMoved = { [weak account] oldHost, newHost in
            account?.anisetteCompanionMoved(from: oldHost, to: newHost)
        }
        DeveloperSlotsEndpoints.register()
        #if !APPARITION_RELEASE
        ExperimentEndpoints.register(probe: model, account: account)
        DebugScreens.register()
        VaultEndpoints.register()
        #endif
        ShortcutActions.register(probe: model, account: account, library: library)
        IntentEndpoints.register(probe: model, account: account, library: library)
        // Debugging from a computer, if the person left it on: an hour that has not run out
        // continues, "until I turn it off" starts again.
        DebugAccessEndpoints.register(debugAccess)
        debugAccess.restore()
    }

    var body: some Scene {
        WindowGroup {
            RootView(model: model, account: account, library: library, companions: companions, debugAccess: debugAccess)
                .debugApprovalPrompts()
                .raveSupportWelcome(RAVESupportConfig(appName: "Apparition", appID: "com.illixion.apparition"))
                .task { await library.scheduleRefreshIfNeeded() }
                // Asked at first launch, while Apparition is in front: the first update may be found
                // by a Shortcuts run nobody is watching. Skipped when Settings turned it off.
                .task { if UpdateNotice.enabled { await UpdateNotice.requestAccess() } }
                .onChange(of: scenePhase) { _, phase in
                    if phase != .active { StateVault.sync() }
                    // A computer may have handed over a pairing while Apparition was away.
                    if phase == .active { model.refreshPairing() }
                    // An hour of debugging from a computer may have run out while Apparition was away.
                    if phase == .active { debugAccess.checkDeadline() }
                    if phase == .background { model.companionMayHaveMoved() }
                }
        }
        #if !os(tvOS)
        .defaultSize(width: 1180, height: 820)
        #endif
        #if os(visionOS)
        DebugApprovalWindow()
        JITRequestWindow(model: model, library: library)
        #endif
    }
}
