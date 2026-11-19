/// The account-list layout the panel draws. Unset or unknown draws the ledger.
public enum PanelStyle: String, Sendable, CaseIterable {
    /// One table row per account, windows in aligned columns. The default.
    case ledger = "a"
    /// The earlier account cards, still reachable through the preference.
    case cards
    /// Exploration: one tile per account with two concentric arcs (5-hour inside, weekly outside).
    case rings
    /// Exploration: one segmented fuel gauge per provider, one segment per account.
    case gauge
    /// Exploration: one coloured line per account under a strip of upcoming resets.
    case strip

    /// UserDefaults key, for example `defaults write dev.switchback.menubar panelStyle cards`.
    public static let preferenceKey = "panelStyle"

    public init(preference: String?) {
        self = preference.flatMap(PanelStyle.init(rawValue:)) ?? .ledger
    }
    /// The three visual explorations share the compact direction panel.
    public var isDirection: Bool { [.rings, .gauge, .strip].contains(self) }
}
