import Foundation
import CoreGraphics

/// Whether macOS shows the status item. A running process and a created item do not prove it:
/// macOS hides items that do not fit beside the frontmost app's menus, and the menu bar can auto-hide.
public enum StatusItemVisibility {
    /// True only when the item's window has a size, its centre lies on a screen, and macOS reports it visible.
    public static func onScreen(frame: CGRect?, screens: [CGRect], occlusionVisible: Bool) -> Bool {
        guard let frame, frame.width > 0, frame.height > 0, occlusionVisible else { return false }
        let centre = CGPoint(x: frame.midX, y: frame.midY)
        return screens.contains { $0.contains(centre) }
    }
    /// Appends "time visible" or "time hidden" only when the state changes; keeps the newest `limit` entries.
    public static func record(_ history: [String], visible: Bool, at date: Date, limit: Int = 12) -> [String] {
        let word = visible ? "visible" : "hidden"
        guard history.last?.hasSuffix(" " + word) != true else { return history }
        let entry = ISO8601DateFormatter().string(from: date) + " " + word
        return Array((history + [entry]).suffix(limit))
    }
}

/// A global shortcut that opens the panel even when macOS hides the menu bar item.
public struct ShortcutChoice: Sendable, Equatable, Identifiable {
    public let id: String
    public let title: String
    /// Virtual key code (kVK_ANSI_*).
    public let keyCode: UInt32
    /// Carbon modifier mask: command 1<<8, option 1<<11, control 1<<12.
    public let modifiers: UInt32

    static let command: UInt32 = 1 << 8, option: UInt32 = 1 << 11, control: UInt32 = 1 << 12
    public static let all = [
        ShortcutChoice(id: "control-option-command-l", title: "⌃⌥⌘L", keyCode: 0x25, modifiers: control | option | command),
        ShortcutChoice(id: "control-option-command-b", title: "⌃⌥⌘B", keyCode: 0x0B, modifiers: control | option | command),
        ShortcutChoice(id: "control-option-command-s", title: "⌃⌥⌘S", keyCode: 0x01, modifiers: control | option | command),
    ]
    public static let off = "off"
    /// UserDefaults key for the selected shortcut id.
    public static let preferenceKey = "openPanelShortcut"
    /// An unset or unknown preference means the default shortcut; "off" means none.
    public static func selected(_ preference: String?) -> ShortcutChoice? {
        if preference == off { return nil }
        return all.first { $0.id == preference } ?? all[0]
    }
}
