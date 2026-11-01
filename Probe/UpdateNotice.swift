import DebugTrace
import Foundation
import UserNotifications

private let log = DebugLogger(subsystem: Bundle.main.bundleIdentifier ?? "Apparition", category: "Updates")

/// The notification that says an update is available: quiet (no sound, passive, so it waits in the
/// notification list), and sent once per update, so a check that finds the same one again says
/// nothing. A person can turn it off in Settings. Only checks that a person asked for can find
/// an update (`LibraryModel.checkExternalUpdates`), so this is also the only thing that can post
/// one. tvOS has no notifications, so there it does nothing.
@MainActor
enum UpdateNotice {
    nonisolated static let enabledKey = "notifyAboutUpdates"
    private static let announcedKey = "announcedUpdates"
    private static let identifier = "updates.available"

    /// On unless a person turned it off.
    static var enabled: Bool { UserDefaults.standard.object(forKey: enabledKey) as? Bool ?? true }

    /// One available update. `key` names that update in particular (the version included), so a
    /// newer one after it is announced again.
    struct Item: Hashable, Sendable {
        let key: String
        /// "UTM 4.6", "Apparition 1.2".
        let title: String
    }

    /// Posts the notification when `items` hold an update that was not announced before, listing
    /// every update still waiting. Returns whether it posted.
    @discardableResult
    static func announce(_ items: [Item]) async -> Bool {
        #if os(tvOS)
        return false
        #else
        guard enabled, !items.isEmpty else { return false }
        let seen = Set(UserDefaults.standard.stringArray(forKey: announcedKey) ?? [])
        guard items.contains(where: { !seen.contains($0.key) }) else { return false }
        // Normally asked at first launch already; asks here only if that never happened.
        guard await requestAccess() else { return false }

        let content = UNMutableNotificationContent()
        content.title = items.count == 1 ? "Update available" : "\(items.count) updates available"
        content.body = items.map(\.title).joined(separator: ", ")
        content.sound = nil
        content.interruptionLevel = .passive
        content.threadIdentifier = identifier
        // One slot: a later announcement replaces this one rather than stacking.
        let request = UNNotificationRequest(identifier: identifier, content: content, trigger: nil)
        do {
            try await UNUserNotificationCenter.current().add(request)
        } catch {
            log.error("can't show the update notification: \(error.localizedDescription, privacy: .public)")
            return false
        }
        UserDefaults.standard.set(Array(seen.union(items.map(\.key))), forKey: announcedKey)
        log.info("announced \(items.count, privacy: .public) available updates")
        return true
        #endif
    }

    /// Whether notifications are allowed, asking if that was never asked (alert only: nothing here
    /// makes a sound). Apparition asks at first launch, and again here when Settings turns the
    /// switch on; a "no" is final until the person changes it in the system's Settings.
    @discardableResult
    static func requestAccess() async -> Bool {
        #if os(tvOS)
        return false
        #else
        let center = UNUserNotificationCenter.current()
        switch await center.notificationSettings().authorizationStatus {
        case .authorized, .provisional, .ephemeral: return true
        case .notDetermined: return (try? await center.requestAuthorization(options: [.alert])) ?? false
        default: return false
        }
        #endif
    }

    /// Whether the person said no to notifications in the system's settings.
    static func isDenied() async -> Bool {
        #if os(tvOS)
        return false
        #else
        await UNUserNotificationCenter.current().notificationSettings().authorizationStatus == .denied
        #endif
    }
}
