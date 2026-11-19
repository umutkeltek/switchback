import AppKit
import Carbon.HIToolbox
import UsagePresentation

/// One system-wide shortcut through the Carbon hot-key API, which needs no Accessibility permission.
/// The app only learns that its own shortcut was pressed; it never observes other keys.
final class GlobalShortcut {
    static let shared = GlobalShortcut()
    private var reference: EventHotKeyRef?
    private var handlerInstalled = false
    var onPress: () -> Void = {}
    private(set) var registered: ShortcutChoice?

    /// Replaces any earlier registration. Returns false when macOS refuses it, for example when it is taken.
    @discardableResult func register(_ choice: ShortcutChoice?) -> Bool {
        if let reference { UnregisterEventHotKey(reference) }
        reference = nil
        registered = nil
        guard let choice else { return true }
        if !handlerInstalled {
            var spec = EventTypeSpec(eventClass: OSType(kEventClassKeyboard), eventKind: UInt32(kEventHotKeyPressed))
            let status = InstallEventHandler(GetApplicationEventTarget(), { _, _, _ in
                // Carbon delivers application-target events on the main thread.
                MainActor.assumeIsolated { GlobalShortcut.shared.onPress() }
                return noErr
            }, 1, &spec, nil, nil)
            guard status == noErr else { return false }
            handlerInstalled = true
        }
        let identifier = EventHotKeyID(signature: OSType(0x5357_4241), id: 1) // "SWBA"
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
