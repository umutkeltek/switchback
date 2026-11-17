import Foundation
import UserNotifications
import UsagePresentation

/// Thin post-only adapter. The crossing rule is `Presenter.limitNotices` in UsagePresentation.
enum LimitNotifier {
    // UNUserNotificationCenter needs a bundled app; inspection modes and bare builds never touch it.
    private static var available: Bool { Bundle.main.bundleIdentifier != nil }
    /// Called only when the person switches the option on, never at launch.
    static func requestPermission() {
        guard available else { return }
        Task { _ = try? await UNUserNotificationCenter.current().requestAuthorization(options: [.alert, .sound]) }
    }
    static func post(_ notices: [LimitNotice]) {
        guard available, !notices.isEmpty else { return }
        Task {
            for notice in notices {
                let content = UNMutableNotificationContent()
                content.title = notice.title
                content.body = notice.body
                content.sound = .default
                try? await UNUserNotificationCenter.current().add(
                    UNNotificationRequest(identifier: notice.id, content: content, trigger: nil))
            }
        }
    }
}
