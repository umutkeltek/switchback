import Foundation

/// Geometry of the gauge look. Pure numbers, so the wrapping and fitting rules are testable without AppKit.
public enum GaugeLayout {
    public static let panelWidth = 360.0
    public static let padding = 12.0
    public static var contentWidth: Double { panelWidth - padding * 2 }
    /// Most segments in one row; more wrap to the next row.
    public static let maxColumns = 5
    public static let segmentGap = 4.0
    public static let rowGap = 8.0

    // Heights, in points, of the parts the view draws at fixed sizes.
    public static let header = 48.0, footer = 30.0, searchBar = 40.0
    public static let hero = 70.0, sectionTitle = 20.0, sectionGap = 14.0
    public static let segmentBar = 24.0, modelBar = 4.0, modelBarGap = 2.0
    /// Name, number and reset chip under each segment.
    public static let labels = 46.0
    public static let legend = 16.0
    /// Point sizes of the number under a segment: room, or a prepaid balance.
    public static let numberFont = 15.0, balanceFont = 13.0
    public static let emptyState = 120.0

    /// Columns for a group of `count` segments: the fewest rows of at most `maxColumns`,
    /// then the same number of columns in every row, so every segment has the same width.
    /// 8 accounts are 2 rows of 4, 7 are 4 and 3, 5 stay in one row.
    public static func columns(_ count: Int) -> Int {
        guard count > 0 else { return 1 }
        let rows = (count + maxColumns - 1) / maxColumns
        return (count + rows - 1) / rows
    }
    /// The segments of each row, in order.
    public static func rows<T>(_ items: [T]) -> [[T]] {
        let width = columns(items.count)
        return stride(from: 0, to: items.count, by: width).map { Array(items[$0..<min(items.count, $0 + width)]) }
    }
    /// One segment's width when a group wraps into `columns` columns.
    public static func segmentWidth(columns: Int) -> Double {
        let columns = Double(max(1, columns))
        return (contentWidth - segmentGap * (columns - 1)) / columns
    }
    /// Height of one segment row: the bar, the model bar when the group has one, and the labels.
    public static func rowHeight(modelBar hasModel: Bool) -> Double {
        segmentBar + (hasModel ? modelBarGap + modelBar : 0) + 4 + labels
    }
    public static func sectionHeight(_ section: DirectionSection) -> Double {
        let count = Double(max(1, rows(section.seats).count))
        return sectionTitle + count * rowHeight(modelBar: section.hasModelBar) + (count - 1) * rowGap
            + (section.legend == nil ? 0 : legend)
    }
    /// Height of the open detail card for one seat.
    public static func detailHeight(_ detail: SeatDetail) -> Double {
        // Vertical padding 12, title 20, a line per limit, balance and issue, spend 18, pin row 28.
        var height = 12 + 20 + Double(detail.lines.count) * 22 + Double(detail.balances.count) * 20
        height += Double(detail.issues.count) * 18
        if detail.spend != nil { height += 18 }
        return height + 28
    }
    /// The whole panel: header, optional search bar, hero, sections, an open detail, footer.
    /// The caller caps it (600 points) and scrolls the rest.
    public static func height(_ data: DirectionPresentation, searching: Bool, detail: SeatDetail?, showBest: Bool) -> Double {
        var content = padding * 2
        if showBest && (data.best != nil || data.noRoom != nil) { content += hero + sectionGap }
        for (index, section) in data.sections.enumerated() {
            if index > 0 { content += sectionGap }
            content += sectionHeight(section)
        }
        if data.sections.isEmpty { content += emptyState }
        if let detail { content += rowGap + detailHeight(detail) }
        return header + (searching ? searchBar : 0) + content + footer
    }
}

/// One limit in a seat's details: what is left, when it resets and how old the reading is.
public struct SeatDetailLine: Codable, Sendable, Identifiable {
    public let id: String
    /// "5-hour", "Weekly", "Fable · Weekly" or "Tools · Mcp Monthly".
    public let title: String
    /// "84% left", "~84% left" or "unknown".
    public let value: String
    public let fraction: Double?
    /// "↻ 2h 10m", "reset passed" or "reset unknown".
    public let reset: String
    /// "3m ago" or "just now".
    public let age: String
    public let modelLimit: Bool
    public let spoken: String
}

/// Everything a seat's details show in place.
public struct SeatDetail: Codable, Sendable {
    public let key: String
    public let title: String
    /// "Account seat2 · person@example.invalid".
    public let identity: String
    public let lines: [SeatDetailLine]
    public let balances: [String]
    /// Refresh errors, sign-in needs and limits used up, one sentence each.
    public let issues: [String]
    public let spend: CostGlance?
    public let pinned: Bool
}

extension Presenter {
    /// The details of one account: 5-hour, weekly and model limits first, then any other window.
    public static func seatDetail(_ account: AccountPanel) -> SeatDetail {
        let ledger = account.ledger
        let first = [ledger.session, ledger.weekly].compactMap { $0 }
        let firstIDs = Set(first.map(\.id))
        let rest = (account.primary + account.extras).filter { !firstIDs.contains($0.id) }
        let models = rest.filter { $0.generation && $0.qualifier != nil }
        let others = rest.filter { !($0.generation && $0.qualifier != nil) }
        let lines = (first + models + others).map { row -> SeatDetailLine in
            let name: String
            switch row.lane {
            case .session: name = "5-hour"
            case .weekly: name = "Weekly"
            case .other: name = row.title
            }
            let title = row.qualifier.map { String($0.prefix(1)).uppercased() + $0.dropFirst() + " · " + name } ?? name
            let value = row.state == .unavailable ? "unknown" : row.valueText + " left"
            let reset: String
            if let resetIn = row.resetIn { reset = "↻ " + resetIn }
            else if row.resetText.hasPrefix("Reset passed") { reset = "reset passed" }
            else { reset = "reset unknown" }
            let age = row.sampleText.hasPrefix("Sampled ") ? String(row.sampleText.dropFirst("Sampled ".count)) : row.sampleText
            return SeatDetailLine(id: row.id, title: title, value: value, fraction: row.remainingFraction, reset: reset,
                                  age: age, modelLimit: row.generation && row.qualifier != nil && ledger.note == nil,
                                  spoken: row.accessibilityText)
        }
        var issues: [String] = []
        if let notice = account.notice {
            switch notice {
            case "Sign in needed": issues.append("Sign in needed: run the provider's sign-in, then refresh.")
            case "Refresh unavailable": issues.append("The last refresh failed. Showing the last reading.")
            case "Older reading": issues.append("This reading is too old to count. Refresh to check again.")
            case "Partial reading": issues.append("Some limits have no fresh reading.")
            case "Disabled": issues.append("This account is disabled.")
            case "Estimated": issues.append("These readings are estimates.")
            case "Account changed": issues.append("Account changed: the signed-in account is not this seat's. Sign in again.")
            default: issues.append(notice + ".")
            }
        }
        if account.directCheckFailed {
            issues.append("Last direct check failed. These numbers are from the last fresh reading.")
        }
        let identity = (["Account " + account.slot] + [account.email].compactMap { $0 }.filter { !$0.isEmpty })
            .joined(separator: " · ")
        return SeatDetail(key: account.key, title: account.label, identity: identity, lines: lines,
                          balances: account.balances.map { $0.title + ": " + $0.valueText },
                          issues: issues, spend: account.glance.cost, pinned: account.pinned)
    }
}

/// The gauge look's menu bar item: one mini bar per pinned Claude seat, then the best seat's number.
public enum StatusGauge {
    public static let barWidth = 3.0
    public static let barGap = 1.5
    public static let barHeight = 13.0
    /// Space between the bars and the number.
    public static let textGap = 4.0
    /// The existing menu bar text budget.
    public static let budget = 96.0

    public static func width(bars: Int, numberWidth: Double) -> Double {
        let barsWidth = bars > 0 ? Double(bars) * barWidth + Double(bars - 1) * barGap + textGap : 0
        return barsWidth + numberWidth
    }
    /// The gauge item fits when its bars and number stay inside the budget and there is at least one bar;
    /// otherwise the item falls back to the icon.
    public static func fits(bars: Int, numberWidth: Double, budget: Double = budget) -> Bool {
        bars > 0 && width(bars: bars, numberWidth: numberWidth) <= budget
    }
    /// The pinned Claude seats, in panel order.
    public static func bars(_ panel: PanelPresentation, data: DirectionPresentation) -> [SeatGlyph] {
        let pinned = Set(panel.providers.first { $0.key == "claude" }?.accounts.filter(\.pinned).map(\.key) ?? [])
        return data.seats.filter { $0.provider == "claude" && pinned.contains($0.id) }
    }
}
