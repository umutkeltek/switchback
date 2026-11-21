import Foundation

/// Where the status item is placed when it is created. macOS keeps each status item's place in the
/// app's own preferences under `NSStatusItem Preferred Position <autosave name>`: the item's distance
/// from the right end of the menu bar, read once when the item is created. An item without one is
/// placed as a new item, at the left end of the status items, which is the first place macOS hides
/// when the frontmost app's menus are wide. The app therefore writes a place toward the right end,
/// beside the system's own items, before it creates the item, and keeps any place the owner chose
/// by Command-dragging the item.
public enum StatusItemPlacement {
    /// The app's place: its distance in points from the right end of the menu bar.
    public static let position = 280.0
    /// The status item's autosave name in the installed app.
    public static let autosaveName = "SwitchbackBar"
    /// An Info.plist key a test copy sets so its item and its place stay apart from the installed app's.
    public static let autosaveNameInfoKey = "SBStatusItemAutosaveName"
    /// The app's own record of the place it wrote, to tell it apart from a place the owner chose.
    public static let placedKey = "statusItemPlacedPosition"

    /// The preferences key macOS reads for the item's place.
    public static func positionKey(_ autosaveName: String) -> String {
        "NSStatusItem Preferred Position \(autosaveName)"
    }

    /// The autosave name from the bundle's Info.plist value, or the installed app's.
    public static func autosaveName(info: String?) -> String {
        guard let info, !info.isEmpty, info.allSatisfy({ $0.isLetter || $0.isNumber || $0 == "-" || $0 == "." })
        else { return autosaveName }
        return info
    }

    public enum Decision: Equatable, Sendable {
        /// Write this place before the item is created (and record it as the app's).
        case place(Double)
        /// Leave the stored place alone.
        case keep
    }

    /// What to do before the item is created.
    /// - `stored`: the place macOS has for the item (`nil`: none, the item was never placed).
    /// - `placed`: the place the app wrote last time (`nil`: never, an install from before this rule).
    ///
    /// No place: the app's. The app's own place, unchanged: kept, or moved to a new app place. Any
    /// other place after the app wrote one: the owner Command-dragged the item (or macOS moved it), so
    /// it is kept. A place from before this rule is kept when it is already at or right of the app's
    /// place; one further left is the new-item place macOS gave, and gets the app's place once.
    public static func decide(stored: Double?, placed: Double?, target: Double = position) -> Decision {
        guard let stored, stored.isFinite else { return .place(target) }
        if let placed {
            if stored == placed { return placed == target ? .keep : .place(target) }
            return .keep
        }
        return stored > target ? .place(target) : .keep
    }

    /// Reads `UserDefaults`, applies the decision and records the app's place. Call before the
    /// status item is created: macOS reads the place only then.
    @discardableResult
    public static func apply(_ defaults: UserDefaults, autosaveName: String, target: Double = position) -> Decision {
        let key = positionKey(autosaveName)
        let stored = (defaults.object(forKey: key) as? NSNumber)?.doubleValue
        let placed = (defaults.object(forKey: placedKey) as? NSNumber)?.doubleValue
        let decision = decide(stored: stored, placed: placed, target: target)
        if case .place(let value) = decision {
            defaults.set(value, forKey: key)
            defaults.set(value, forKey: placedKey)
        }
        return decision
    }
}
