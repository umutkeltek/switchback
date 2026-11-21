import AppKit
import Carbon.HIToolbox
import UsagePresentation

/// One system-wide shortcut through the Carbon hot-key API, which needs no Accessibility permission.
/// The app only learns that its own shortcut was pressed; it never observes other keys.
/// `shared` opens the panel; `window` opens the Switchback window. Each has its own hot-key id.
final class GlobalShortcut {
    static let shared = GlobalShortcut(id: 1)
    static let window = GlobalShortcut(id: 2)
    private static var handlerInstalled = false
    private static var byID: [UInt32: GlobalShortcut] { [shared.id: shared, window.id: window] }
    private let id: UInt32
    private var reference: EventHotKeyRef?
    var onPress: () -> Void = {}
    private(set) var registered: ShortcutChoice?

    private init(id: UInt32) { self.id = id }

    /// Replaces any earlier registration. Returns false when macOS refuses it, for example when it is taken.
    @discardableResult func register(_ choice: ShortcutChoice?) -> Bool {
        if let reference { UnregisterEventHotKey(reference) }
        reference = nil
        registered = nil
        guard let choice else { return true }
        if !GlobalShortcut.handlerInstalled {
            var spec = EventTypeSpec(eventClass: OSType(kEventClassKeyboard), eventKind: UInt32(kEventHotKeyPressed))
            let status = InstallEventHandler(GetApplicationEventTarget(), { _, event, _ in
                // Which of the app's shortcuts was pressed; the event says nothing about any other key.
                var pressed = EventHotKeyID()
                let found = GetEventParameter(event, EventParamName(kEventParamDirectObject), EventParamType(typeEventHotKeyID),
                                              nil, MemoryLayout<EventHotKeyID>.size, nil, &pressed)
                let id = found == noErr ? pressed.id : 1
                // Carbon delivers application-target events on the main thread.
                MainActor.assumeIsolated { GlobalShortcut.byID[id]?.onPress() }
                return noErr
            }, 1, &spec, nil, nil)
            guard status == noErr else { return false }
            GlobalShortcut.handlerInstalled = true
        }
        let identifier = EventHotKeyID(signature: OSType(0x5357_4241), id: id) // "SWBA"
        var created: EventHotKeyRef?
        let status = RegisterEventHotKey(choice.keyCode, choice.modifiers, identifier,
                                         GetApplicationEventTarget(), 0, &created)
        guard status == noErr, let created else { return false }
        reference = created
        registered = choice
        return true
    }
}

/// Lets `sb bar open` (and `SwitchbackBar --open-panel`) ask the running app to open its panel.
/// The notification carries no data; any local process may only ask the panel to open.
enum OpenPanelRequest {
    static let name = Notification.Name("dev.switchback.menubar.open-panel")
    static func post() {
        DistributedNotificationCenter.default().postNotificationName(name, object: nil, userInfo: nil,
                                                                     deliverImmediately: true)
    }
}

/// Lets `sb bar window` (and `SwitchbackBar --open-window`) ask the running app to open the Switchback window.
/// Like the panel request, it carries no data.
enum OpenWindowRequest {
    static let name = Notification.Name("dev.switchback.menubar.open-window")
    static func post() {
        DistributedNotificationCenter.default().postNotificationName(name, object: nil, userInfo: nil,
                                                                     deliverImmediately: true)
    }
}
