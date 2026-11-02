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
        NearbyEndpoints.register(signing, account: account)
        let library = LibraryModel(account: account, probe: model, signing: signing)
        _library = State(initialValue: library)
        LibraryEndpoints.register(library, account: account)
        library.registerBackgroundTasks()
        CompanionEndpoints.register(_companions.wrappedValue, probe: model)
        let finder = _companions.wrappedValue
        model.locateCompanion = { [weak finder] name in await finder?.locate(name: name) }
        // An anisette server on the companion moved with it.
        // Only the host changes: a paired device's token stays in the path.
        model.onCompanionMoved = { [weak account] old, found in
            guard let account, let newHost = found.anisetteURL.flatMap(URL.init(string:))?.host,
                  let oldHost = old.split(separator: ":").first,
                  var components = URLComponents(string: account.anisetteURL),
                  components.host == String(oldHost) else { return }
            components.host = newHost
            if let moved = components.string { account.anisetteURL = moved }
        }
        DeveloperSlotsEndpoints.register()
        #if !APPARITION_RELEASE
        ExperimentEndpoints.register(probe: model, account: account)
        DebugScreens.register()
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
                // Asked at first launch, while Apparition is in front: the first update may be found
                // by a Shortcuts run nobody is watching. Skipped when Settings turned it off.
                .task { if UpdateNotice.enabled { await UpdateNotice.requestAccess() } }
                .onChange(of: scenePhase) { _, phase in
                    if phase != .active { StateVault.sync() }
                    // A computer may have handed over a pairing while Apparition was away.
                    if phase == .active { model.refreshPairing() }
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
