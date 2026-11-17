import Foundation

/// One window of a cost as reported, for example "$714" for today.
public struct CostFigure: Codable, Sendable, Equatable {
    public enum Window: String, Codable, Sendable {
        case today
        case last7Days = "last_7_days"
    }
    public let window: Window
    /// Formatted amount without the estimate mark; nil when the window was sent with an amount
    /// that cannot be shown (negative or not finite).
    public let amount: String?
    public let tokens: String?
    /// More than 2% of this window's tokens could not be charged to an account. Totals only.
    public let splitUnknown: Bool
    public var label: String { window == .today ? "today" : "in 7 days" }
    public var spokenLabel: String { window == .today ? "today" : "in the last 7 days" }
}

/// An account's cost as the snapshot reported it. Never computed here.
public struct CostGlance: Codable, Sendable, Equatable {
    /// Only the windows the snapshot sent, today first.
    public let figures: [CostFigure]
    public let estimated: Bool
    public let detail: String
    public var mark: String { estimated ? "~" : "" }
    /// For example "Spend ~$714 today" or "Spend ~$6.42 today · ~$71.95 in 7 days".
    public var text: String { Presenter.costLine(prefix: "Spend", figures: figures, mark: mark) }
    public var spoken: String { (estimated ? "Estimated spend " : "Spend ") + Presenter.costSpoken(figures, estimated: estimated) }
    /// Nil when the window was not sent; "—" when it was sent with an amount that cannot be shown.
    public var todayText: String? { figure(.today).map { $0.amount ?? "—" } }
    public var weekText: String? { figure(.last7Days).map { $0.amount ?? "—" } }
    public var todayTokens: String? { figure(.today)?.tokens }
    public var weekTokens: String? { figure(.last7Days)?.tokens }
    /// For example "Tokens 2.6M today · 28.4M in 7 days"; nil when no window carries tokens.
    public var tokensText: String? {
        let parts = figures.compactMap { figure in figure.tokens.map { $0 + " " + figure.label } }
        return parts.isEmpty ? nil : "Tokens " + parts.joined(separator: " · ")
    }
    func figure(_ window: CostFigure.Window) -> CostFigure? { figures.first { $0.window == window } }
}

/// A provider's cost total across all its accounts, from `cost_summary`.
public struct CostTotal: Codable, Sendable, Equatable {
    public let provider: String
    public let figures: [CostFigure]
    public let estimated: Bool
    /// For example "Est. spend ~$2,267 today · ~$12.0k in 7 days".
    public let text: String
    /// Tooltip: whose total it is, what it is not, the split note and the sample age.
    public let help: String
    public let accessibilityText: String
    /// For example "Split by account not known yet for 7 days."; nil when every window is split.
    public let splitNote: String?
    public var mark: String { estimated ? "~" : "" }
    public var label: String { estimated ? "Est. spend" : "Spend" }
}

extension Presenter {
    /// What an estimated cost is and is not.
    public static let costEstimateSentence = "At API list prices, from local logs. Not a bill."
    /// A window gets the split note when more than this share of its tokens is unattributed.
    public static let unattributedNoteShare = 0.02

    /// Money that always fits the row: under 100 two decimals ($6.42), under 10,000 whole units with a
    /// thousands separator ($714, $1,099), from 10,000 one decimal with k ($12.0k).
    /// Each range is chosen by the value after its own rounding, so $99.996 reads $100 and $9,999.50 reads $10.0k.
    public static func money(_ value: Double, currency: String) -> String {
        let symbol = currency == "USD" ? "$" : currency + " "
        if (value * 100).rounded() < 10_000 { return symbol + String(format: "%.2f", (value * 100).rounded() / 100) }
        let whole = value.rounded()
        if whole < 10_000 { return symbol + grouped(Int(whole)) }
        let thousands = (value / 100).rounded() / 10
        return symbol + (thousands < 1_000 ? String(format: "%.1f", thousands) : grouped(Int(thousands.rounded()))) + "k"
    }
    static func grouped(_ value: Int) -> String {
        let digits = Array(String(value))
        var out = ""
        for (index, digit) in digits.enumerated() {
            if index > 0 && (digits.count - index) % 3 == 0 { out.append(",") }
            out.append(digit)
        }
        return out
    }
    static func tokenText(_ value: Double?) -> String? {
        guard let value, value.isFinite, value >= 0 else { return nil }
        if value >= 1_000_000_000 { return String(format: "%.1fB", value / 1_000_000_000) }
        if value >= 1_000_000 { return String(format: "%.1fM", value / 1_000_000) }
        if value >= 1_000 { return String(format: "%.0fK", value / 1_000) }
        return String(format: "%.0f", value)
    }
    static func validAmount(_ value: Double?) -> Double? {
        guard let value, value.isFinite, value >= 0 else { return nil }
        return value
    }
    /// Shared trust rule for every cost: a known currency, a fresh sample and the same local day.
    static func costCurrency(_ code: String?, sampledAt: String?, now: Date, calendar: Calendar) -> String? {
        let currency = clean(code ?? "").trimmingCharacters(in: .whitespacesAndNewlines)
        guard !currency.isEmpty, currency.count <= 8, UsageTime.fresh(sampledAt, now: now),
              let sampled = UsageTime.parse(sampledAt), calendar.isDate(sampled, inSameDayAs: now) else { return nil }
        return currency
    }
    static func costLine(prefix: String, figures: [CostFigure], mark: String) -> String {
        prefix + " " + figures.map { ($0.amount.map { mark + $0 } ?? "—") + " " + $0.label }.joined(separator: " · ")
    }
    static func costSpoken(_ figures: [CostFigure], estimated: Bool) -> String {
        figures.map { figure in
            guard let amount = figure.amount else { return "unknown " + figure.spokenLabel }
            let words = amount.hasSuffix("k") ? String(amount.dropLast()) + " thousand" : amount
            return (estimated ? "about " : "") + words + " " + figure.spokenLabel
        }.joined(separator: ", ")
    }

    /// Cost is shown only as reported, only while its sample is fresh and from today, and never when the
    /// two windows contradict each other. A window the snapshot did not send is not drawn.
    static func costGlance(_ cost: UsageCost?, now: Date, calendar: Calendar = .current) -> CostGlance? {
        guard let cost, let currency = costCurrency(cost.currencyCode, sampledAt: cost.sampledAt, now: now, calendar: calendar)
        else { return nil }
        let sent: [(CostFigure.Window, UsageCostPeriod)] = [(.today, cost.today), (.last7Days, cost.last7Days)]
            .compactMap { window, period in period?.amount == nil ? nil : period.map { (window, $0) } }
        let values = Dictionary(uniqueKeysWithValues: sent.map { ($0.0, validAmount($0.1.amount)) })
        guard values.values.contains(where: { $0 != nil }) else { return nil }
        if let today = values[.today] ?? nil, let week = values[.last7Days] ?? nil, week + 0.005 < today { return nil }
        let figures = sent.map { window, period in
            CostFigure(window: window, amount: validAmount(period.amount).map { money($0, currency: currency) },
                       tokens: tokenText(period.tokens), splitUnknown: false)
        }
        let estimated = cost.basis.map { $0.lowercased().contains("estimate") } ?? true
        return CostGlance(figures: figures, estimated: estimated,
                          detail: (estimated ? costEstimateSentence + " " : "") + age(cost.sampledAt, now: now))
    }

    /// Share of a window that could not be charged to an account: by tokens, else by amount; nil when unknown.
    static func unattributedShare(_ total: UsageCostPeriod, _ part: UsageCostPeriod?) -> Double? {
        guard let part else { return nil }
        if let all = total.tokens, let loose = part.tokens, all.isFinite, loose.isFinite, all > 0, loose >= 0 { return loose / all }
        if let all = total.amount, let loose = part.amount, all.isFinite, loose.isFinite, all > 0, loose >= 0 { return loose / all }
        return nil
    }

    /// The provider total from `cost_summary`, under the same trust rules as an account's cost.
    /// Any sent window with an amount that cannot be shown, or two entries for one provider, hides the line.
    static func costTotal(_ entries: [UsageCostSummary], provider: String, title: String, now: Date,
                          calendar: Calendar = .current) -> CostTotal? {
        let matches = entries.filter { canonical($0.provider) == provider }
        guard matches.count == 1, let entry = matches.first,
              let currency = costCurrency(entry.currencyCode, sampledAt: entry.sampledAt, now: now, calendar: calendar)
        else { return nil }
        let windows: [(CostFigure.Window, UsageCostPeriod?, UsageCostPeriod?)] = [
            (.today, entry.today, entry.unattributed?.today), (.last7Days, entry.last7Days, entry.unattributed?.last7Days)]
        var figures: [CostFigure] = []
        var values: [CostFigure.Window: Double] = [:]
        for (window, period, loose) in windows {
            guard let period, period.amount != nil else { continue }
            guard let value = validAmount(period.amount) else { return nil }
            values[window] = value
            let share = unattributedShare(period, loose)
            figures.append(CostFigure(window: window, amount: money(value, currency: currency), tokens: tokenText(period.tokens),
                                      splitUnknown: share.map { $0 > unattributedNoteShare } ?? false))
        }
        guard !figures.isEmpty else { return nil }
        if let today = values[.today], let week = values[.last7Days], week + 0.005 < today { return nil }
        let estimated = entry.basis.map { $0.lowercased().contains("estimate") } ?? true
        let unsplit = figures.filter(\.splitUnknown).map { $0.window == .today ? "today" : "7 days" }
        let splitNote = unsplit.isEmpty ? nil : "Split by account not known yet for " + unsplit.joined(separator: " and ") + "."
        let mark = estimated ? "~" : ""
        let what = estimated ? costEstimateSentence : nil
        let help = [title + ", all accounts.", what, splitNote, age(entry.sampledAt, now: now) + "."]
            .compactMap { $0 }.joined(separator: " ")
        let spoken = (estimated ? "Estimated spend, all " : "Spend, all ") + title + " accounts: " +
            costSpoken(figures, estimated: estimated) + "."
        return CostTotal(provider: provider, figures: figures, estimated: estimated,
                         text: costLine(prefix: estimated ? "Est. spend" : "Spend", figures: figures, mark: mark),
                         help: help, accessibilityText: [spoken, what, splitNote].compactMap { $0 }.joined(separator: " "),
                         splitNote: splitNote)
    }
}
