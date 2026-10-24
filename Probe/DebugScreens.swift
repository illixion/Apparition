#if !APPARITION_RELEASE
import DebugTrace
import Observation

/// Opens a tab, or one of the signing panels, from the debug server (`showScreen`), so they can
/// be looked at with `_screenshot` on a device nobody is steering: an Apple TV across the room.
/// Not in official releases.
@MainActor @Observable
final class DebugScreens {
    static let shared = DebugScreens()

    enum Panel: String, CaseIterable, Identifiable {
        case getSigning, shareSigning, certificates
        var id: String { rawValue }
    }

    /// The tab to switch to; RootView takes it and clears it.
    var tab: AppTab?
    var panel: Panel?

    static func register() {
        DebugSurface.shared.register([
            .command("showScreen", "Switch to a tab and/or present a signing panel, as tapping there would (panel none closes it)",
                     parameters: [
                        .string("tab", "Tab to show", choices: AppTab.allCases.map(\.rawValue)),
                        .string("panel", "Panel to present", choices: Panel.allCases.map(\.rawValue) + ["none"]),
                     ]) { args in
                if let tab = args.string("tab") { shared.tab = AppTab(rawValue: tab) }
                if let panel = args.string("panel") { shared.panel = Panel(rawValue: panel) }
                return ["tab": args.string("tab") ?? "-", "panel": shared.panel?.rawValue ?? "none"]
            },
        ])
    }
}
#endif
