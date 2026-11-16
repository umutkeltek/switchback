import AppKit

/// Observation is scoped to one opening. Global mouse observation leaves the
/// original click untouched and does not activate this background application.
@MainActor final class PopoverDismissal {
    typealias MouseHandler = @Sendable (NSEvent) -> Void
    typealias InstallMouseMonitor = @MainActor (NSEvent.EventTypeMask, @escaping MouseHandler) -> Any?

    private let installMouseMonitor: InstallMouseMonitor
    private let removeMouseMonitor: @MainActor (Any) -> Void
    private let notifications: NotificationCenter
    private let processIdentifier: pid_t
    private let onDismiss: @MainActor () -> Void
    private var mouseMonitor: Any?
    private var activationObserver: NSObjectProtocol?
    private var generation = UUID()

    init(installMouseMonitor: @escaping InstallMouseMonitor = { mask, handler in
        NSEvent.addGlobalMonitorForEvents(matching: mask, handler: handler)
    }, removeMouseMonitor: @escaping @MainActor (Any) -> Void = { NSEvent.removeMonitor($0) },
         notifications: NotificationCenter = NSWorkspace.shared.notificationCenter,
         processIdentifier: pid_t = ProcessInfo.processInfo.processIdentifier,
         onDismiss: @escaping @MainActor () -> Void) {
        self.installMouseMonitor = installMouseMonitor
        self.removeMouseMonitor = removeMouseMonitor
        self.notifications = notifications
        self.processIdentifier = processIdentifier
        self.onDismiss = onDismiss
    }

    var isMonitoring: Bool { mouseMonitor != nil }

    func start() {
        stop()
        let opening = generation
        let ownPID = processIdentifier
        let requestDismissal: @Sendable () -> Void = { [weak self] in
            Task { @MainActor [weak self] in self?.dismiss(opening: opening) }
        }
        mouseMonitor = installMouseMonitor([.leftMouseDown, .rightMouseDown, .otherMouseDown]) { _ in
            requestDismissal()
        }
        activationObserver = notifications.addObserver(
            forName: NSWorkspace.didActivateApplicationNotification, object: nil, queue: .main
        ) { notification in
            guard let app = notification.userInfo?[NSWorkspace.applicationUserInfoKey] as? NSRunningApplication,
                  app.processIdentifier != ownPID else { return }
            requestDismissal()
        }
    }

    func stop() {
        generation = UUID()
        if let mouseMonitor {
            removeMouseMonitor(mouseMonitor)
            self.mouseMonitor = nil
        }
        if let activationObserver {
            notifications.removeObserver(activationObserver)
            self.activationObserver = nil
        }
    }

    private func dismiss(opening: UUID) {
        guard generation == opening, mouseMonitor != nil || activationObserver != nil else { return }
        stop()
        onDismiss()
    }
}
