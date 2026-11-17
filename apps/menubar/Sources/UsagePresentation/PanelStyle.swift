/// Which account-list layout the panel draws. Unset or unknown keeps the current cards.
public enum PanelStyle: String, Sendable, CaseIterable {
    case current
    /// A: one table row per account, windows in aligned columns.
    case ledger = "a"
    /// B: a hero block for the account with the most room, compact rows for the rest.
    case focus = "b"

    /// UserDefaults key, for example `defaults write dev.switchback.menubar panelStyle a`.
    public static let preferenceKey = "panelStyle"

    public init(preference: String?) {
        self = preference.flatMap(PanelStyle.init(rawValue:)) ?? .current
    }
}
