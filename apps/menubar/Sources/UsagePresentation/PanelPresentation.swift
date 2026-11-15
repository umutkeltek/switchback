import Foundation

public enum ReadingState: String, Codable, Sendable {
    case measured, estimated, unavailable
    var spoken: String {
        switch self {
        case .measured: "Current reading"
        case .estimated: "Estimated reading"
        case .unavailable: "Reading unavailable"
        }
    }
}
public struct QuotaRow: Codable, Sendable {
    public let id: String
    public let title: String
    public let qualifier: String?
    public let state: ReadingState
    public let isEstimate: Bool
    public let valueText: String
    public let remainingFraction: Double?
    public let resetText: String
    public let sampleText: String
    public let lastReading: String?
    public var accessibilityText: String {
        [title, qualifier, state.spoken,
         state == .unavailable ? lastReading ?? "Usage unknown" : valueText + " remaining",
         resetText, sampleText].compactMap { $0 }.joined(separator: ", ")
    }
}
public struct BalanceRow: Codable, Sendable {
    public let id: String
    public let title: String
    public let state: ReadingState
    public let isEstimate: Bool
    public let valueText: String
    public let sampleText: String
    public var accessibilityText: String {
        [title, valueText, state == .unavailable ? isEstimate ? "Last estimate" : "Last reading" : state.spoken,
         sampleText].joined(separator: ", ")
    }
}
public struct AccountPanel: Codable, Sendable {
    public let key: String
    public let label: String
    public let email: String?
    public let slot: String
    public let pinned: Bool
    public let summary: String
    public let notice: String?
    public let sampleText: String
    public let primary: [QuotaRow]
    public let extras: [QuotaRow]
    public let balances: [BalanceRow]
    public let details: [String]
}
public struct ProviderPanel: Codable, Sendable {
    public let key: String
    public let title: String
    public let emptyMessage: String?
    public let accounts: [AccountPanel]
    public init(key: String, title: String, emptyMessage: String?, accounts: [AccountPanel]) {
        self.key = key; self.title = title; self.emptyMessage = emptyMessage; self.accounts = accounts
    }
}
public struct PanelPresentation: Codable, Sendable {
    public let status: String
    public let accountCount: Int
    public let pinnedCount: Int
    public let providers: [ProviderPanel]
    public init(status: String = "Reading account limits…", accountCount: Int = 0,
                pinnedCount: Int = 0, providers: [ProviderPanel] = []) {
        self.status = status; self.accountCount = accountCount
        self.pinnedCount = pinnedCount; self.providers = providers
    }
}
extension Presenter {
    static func snapshotIsFresh(_ snapshot: UsageSnapshot, failure: String?, now: Date) -> Bool {
        failure == nil && UsageTime.fresh(snapshot.generatedAt, now: now)
    }
    static func isGeneration(_ window: UsageWindow) -> Bool {
        let scope = window.scope?.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()
        return scope == nil || scope == "" || scope == "generation"
    }
    static func windowTitle(_ window: UsageWindow) -> String {
        switch window.name {
        case "five_hour": return "5-hour window"
        case "seven_day": return "Weekly"
        case "primary", "secondary", "tertiary":
            if window.windowMinutes == 300 { return "5-hour window" }
            if window.windowMinutes == 10080 { return "Weekly" }
            if window.windowMinutes == 43200 { return "Monthly" }
            return window.name.capitalized
        default:
            return clean(window.name.replacingOccurrences(of: "_", with: " ").replacingOccurrences(of: "-", with: " ")).capitalized
        }
    }
    static func accountLabel(_ account: UsageAccount) -> String {
        let label = clean(account.label ?? "").trimmingCharacters(in: .whitespacesAndNewlines)
        if !label.isEmpty && label != account.slot { return label }
        if account.slot.hasPrefix("seat"), Int(account.slot.dropFirst(4)) != nil {
            return "Seat " + account.slot.dropFirst(4)
        }
        return clean(account.slot.replacingOccurrences(of: "_", with: " ")).capitalized
    }
    public static func panel(_ snapshot: UsageSnapshot, now: Date = Date(), pins: Set<String>? = nil,
                             refreshing: Bool = false, failure: String? = nil) -> PanelPresentation {
        let menu = make(snapshot, now: now, pins: pins, refreshing: refreshing, failure: failure)
        let fresh = snapshotIsFresh(snapshot, failure: failure, now: now)
        let selected = pins ?? defaults(snapshot)
        let providers = menu.groups.compactMap { group -> ProviderPanel? in
            let sourceRows = snapshot.accounts.filter { canonical($0.provider) == group.provider }
            let coverage = snapshot.coverage.first { canonical($0.provider) == group.provider }
            guard !sourceRows.isEmpty || coverage != nil else { return nil }
            let labelCounts = Dictionary(grouping: sourceRows, by: accountLabel).mapValues(\.count)
            let accounts = sourceRows.map { account -> AccountPanel in
                let key = account.key
                let summary = group.accounts.first { $0.key == key }?.summary ?? "?"
                var occurrences: [String: Int] = [:]
                let quotas = account.windows.map { window -> QuotaRow in
                    let base = [key, window.name, window.model ?? "", window.scope ?? ""].joined(separator: "|")
                    let occurrence = occurrences[base, default: 0]
                    occurrences[base] = occurrence + 1
                    let estimate = account.isEstimated || window.isEstimated
                    let good = valid(window, account: account, snapshotFresh: fresh, now: now)
                    let left = window.remainingPercent ?? window.usedPercent.map { 100 - $0 }
                    let measured = good && !estimate
                    let state: ReadingState = measured ? .measured : good && estimate ? .estimated : .unavailable
                    let value = good ? (estimate ? "~" : "") + (percent(left) ?? "—") : "—"
                    let qualifiers = [window.model, isGeneration(window) ? nil : window.scope].compactMap { $0 }.filter { !$0.isEmpty }
                    return QuotaRow(id: base + "|\(occurrence)", title: windowTitle(window),
                        qualifier: qualifiers.isEmpty ? nil : qualifiers.map(clean).joined(separator: " · "),
                        state: state, isEstimate: estimate, valueText: value,
                        remainingFraction: measured ? left.map { $0 / 100 } : nil,
                        resetText: reset(window, now: now), sampleText: age(window.sampledAt, now: now),
                        lastReading: !good ? percent(left).map { (estimate ? "Last estimate: " : "Last reading: ") + $0 + " remaining" } : nil)
                }
                let generation = account.windows.indices.filter { isGeneration(account.windows[$0]) }
                let aggregates = generation.filter { account.windows[$0].model == nil }
                let preferred = aggregates.filter {
                    ["five_hour", "seven_day", "primary", "secondary"].contains(account.windows[$0].name)
                }
                var ordered: [Int] = []
                for index in preferred + aggregates + generation where !ordered.contains(index) { ordered.append(index) }
                let primaryIndices = Array(ordered.prefix(2))
                let primarySet = Set(primaryIndices)
                let balances = account.balances.enumerated().compactMap { index, balance -> BalanceRow? in
                    guard let value = balance.balance, value.isFinite else { return nil }
                    let currency = clean(balance.currencyCode ?? "").trimmingCharacters(in: .whitespacesAndNewlines)
                    let estimate = account.isEstimated || balance.isEstimated
                    let good = !currency.isEmpty && !estimate && validAccount(account, snapshotFresh: fresh, now: now)
                    let state: ReadingState = good ? .measured : estimate && !currency.isEmpty && validAccount(account, snapshotFresh: fresh, now: now) ? .estimated : .unavailable
                    let valueText = currency.isEmpty ? "—" : currency + " " + String(format: "%.2f", value)
                    return BalanceRow(id: key + "|balance|\(index)", title: balance.kind == "prepaid" ? "Prepaid balance" : "Balance",
                                      state: state, isEstimate: estimate, valueText: valueText, sampleText: age(account.observedAt, now: now))
                }
                let notice: String?
                if account.enabled == false { notice = "Disabled" }
                else if account.reloginRequired == true { notice = "Sign in needed" }
                else if account.refreshError != nil { notice = "Refresh unavailable" }
                else if !validAccount(account, snapshotFresh: fresh, now: now) { notice = "Older reading" }
                else if account.isEstimated { notice = "Estimated" }
                else if summary == "0%" { notice = "At limit" }
                else if summary == "?" && !account.windows.isEmpty { notice = "Partial reading" }
                else { notice = nil }
                let label = accountLabel(account)
                return AccountPanel(key: key, label: labelCounts[label, default: 0] > 1 ? label + " · " + clean(account.slot) : label,
                    email: account.email.map(clean), slot: clean(account.slot), pinned: selected.contains(key),
                    summary: summary, notice: notice, sampleText: age(account.observedAt, now: now),
                    primary: primaryIndices.map { quotas[$0] },
                    extras: quotas.enumerated().filter { !primarySet.contains($0.offset) }.map(\.element),
                    balances: balances, details: group.accounts.first { $0.key == key }?.lines ?? [])
            }
            let message = accounts.isEmpty ? group.unavailable ?? "No usage reported yet" : nil
            return ProviderPanel(key: group.provider, title: group.provider == "zai" ? "Z.ai" : group.title,
                                 emptyMessage: message, accounts: accounts)
        }
        return PanelPresentation(status: menu.status, accountCount: providers.reduce(0) { $0 + $1.accounts.count },
                                 pinnedCount: providers.flatMap(\.accounts).filter(\.pinned).count, providers: providers)
    }
}

extension PanelPresentation {
    public func filtered(provider selected: String = "all", query: String = "", pinnedOnly: Bool = false) -> [ProviderPanel] {
        let term = query.trimmingCharacters(in: .whitespacesAndNewlines)
        return providers.compactMap { provider -> ProviderPanel? in
            let matches = selected == "all" || provider.key == selected ||
                (selected == "opencode" && ["opencodego", "opencodezen"].contains(provider.key))
            guard matches else { return nil }
            let providerMatches = provider.title.localizedCaseInsensitiveContains(term)
            let rows = provider.accounts.filter { account in
                (!pinnedOnly || account.pinned) && (term.isEmpty || providerMatches ||
                [account.label, account.email ?? "", account.slot].contains { $0.localizedCaseInsensitiveContains(term) })
            }
            if rows.isEmpty && (!provider.accounts.isEmpty || pinnedOnly || (!term.isEmpty && !providerMatches)) { return nil }
            return ProviderPanel(key: provider.key, title: provider.title, emptyMessage: provider.emptyMessage, accounts: rows)
        }
    }
}
