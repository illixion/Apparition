import DebugTrace
import Foundation
import UserNotifications

private let log = DebugLogger(subsystem: Bundle.main.bundleIdentifier ?? "Apparition", category: "JIT")

/// The notification that stays up while Apparition is an app's debugger: it says so while
/// Apparition is out of sight, and its Stop action ends the session from anywhere (the app
/// keeps running, without a debugger). Removed when the session ends. tvOS has no
/// notification banners or actions, so there it does nothing.
@MainActor
final class JITSessionNotice: NSObject {
    static let category = "jit-session"
    static let stopAction = "stop"

    /// Ends the session serving a bundle ID (ProbeModel.stopJIT).
    var onStop: ((String) -> Void)?

    override init() {
        super.init()
        #if !os(tvOS)
        let center = UNUserNotificationCenter.current()
        center.delegate = self
        let stop = UNNotificationAction(identifier: Self.stopAction, title: "Stop", options: [.destructive])
        center.setNotificationCategories([
            UNNotificationCategory(identifier: Self.category, actions: [stop], intentIdentifiers: [])
        ])
        #endif
    }

    /// Asks for permission to notify if it was never asked: while Apparition is in front.
    func prepare() async {
        #if !os(tvOS)
        let center = UNUserNotificationCenter.current()
        guard await center.notificationSettings().authorizationStatus == .notDetermined else { return }
        _ = try? await center.requestAuthorization(options: [.alert])
        #endif
    }

    func show(bundleId: String, name: String) {
        #if !os(tvOS)
        let content = UNMutableNotificationContent()
        content.title = "Apparition is \(name)'s debugger"
        content.body = "It serves \(name)'s JIT requests until \(name) closes. Stop leaves \(name) running without JIT."
        content.categoryIdentifier = Self.category
        content.userInfo = ["bundleId": bundleId]
        content.interruptionLevel = .passive
        content.threadIdentifier = Self.category
        let request = UNNotificationRequest(identifier: Self.identifier(bundleId), content: content, trigger: nil)
        UNUserNotificationCenter.current().add(request) { error in
            if let error { log.error("can't show the JIT session notification: \(error.localizedDescription, privacy: .public)") }
        }
        #endif
    }

    func remove(bundleId: String) {
        #if !os(tvOS)
        let center = UNUserNotificationCenter.current()
        center.removeDeliveredNotifications(withIdentifiers: [Self.identifier(bundleId)])
        center.removePendingNotificationRequests(withIdentifiers: [Self.identifier(bundleId)])
        #endif
    }

    private static func identifier(_ bundleId: String) -> String { "jit-session.\(bundleId)" }
}

#if !os(tvOS)
extension JITSessionNotice: UNUserNotificationCenterDelegate {
    /// In the list, but no banner over Apparition itself: its own screen says it already.
    nonisolated func userNotificationCenter(_ center: UNUserNotificationCenter, willPresent notification: UNNotification) async -> UNNotificationPresentationOptions {
        [.list]
    }

    nonisolated func userNotificationCenter(_ center: UNUserNotificationCenter, didReceive response: UNNotificationResponse) async {
        guard response.notification.request.content.categoryIdentifier == Self.category,
              response.actionIdentifier == Self.stopAction,
              let bundleId = response.notification.request.content.userInfo["bundleId"] as? String else { return }
        await MainActor.run {
            log.info("JIT session stopped from its notification")
            onStop?(bundleId)
        }
    }
}
#endif
