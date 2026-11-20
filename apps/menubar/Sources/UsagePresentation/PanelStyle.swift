/// The account-list layout the panel draws. Unset or unknown draws the gauge.
public enum PanelStyle: String, Sendable, CaseIterable {
    /// One table row per account, windows in aligned columns.
    case ledger = "a"
    /// The earlier account cards, still reachable through the preference.
    case cards
    /// One tile per account with two concentric arcs (5-hour inside, weekly outside).
    case rings
    /// One segmented gauge per provider, one segment per account. The default.
    case gauge
    /// One coloured line per account under a strip of upcoming resets.
    case strip

    /// UserDefaults key, for example `defaults write dev.switchback.menubar panelStyle cards`.
    public static let preferenceKey = "panelStyle"
    public static let standard = PanelStyle.gauge

    /// A saved preference is kept; no preference, or one this version does not know, draws the gauge.
    public init(preference: String?) {
        self = preference.flatMap(PanelStyle.init(rawValue:)) ?? .standard
    }
    /// The visual looks share the compact direction panel.
    public var isDirection: Bool { [.rings, .gauge, .strip].contains(self) }
    /// The choices Options › Look offers, in menu order. The card look stays reachable through the preference.
    public static let looks: [PanelStyle] = [.gauge, .ledger, .rings, .strip]
    public var title: String {
        switch self {
        case .ledger: "Ledger"
        case .cards: "Cards"
        case .rings: "Rings"
        case .gauge: "Gauge"
        case .strip: "Strip"
        }
    }
}
