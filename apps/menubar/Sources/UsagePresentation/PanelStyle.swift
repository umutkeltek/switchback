/// Which account-list layout the panel draws. Unset or unknown draws the ledger.
public enum PanelStyle: String, Sendable, CaseIterable {
    /// One table row per account, windows in aligned columns. The default.
    case ledger = "a"
    /// The earlier account cards, still reachable through the preference.
    case cards

    /// UserDefaults key, for example `defaults write dev.switchback.menubar panelStyle cards`.
    public static let preferenceKey = "panelStyle"

    public init(preference: String?) {
        self = preference == PanelStyle.cards.rawValue ? .cards : .ledger
    }
}
