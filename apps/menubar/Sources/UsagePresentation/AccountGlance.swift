import Foundation

/// Which kind of window a quota row is, so compact layouts can line windows up in columns.
public enum QuotaLane: String, Codable, Sendable {
    case session, weekly, other
}

/// A straight-line projection: the average use since the window began, carried forward.
public struct RunOutForecast: Codable, Sendable, Equatable {
    public let lane: QuotaLane
    public let windowTitle: String
    public let seconds: Double
    public let text: String
    public let detail: String
}

/// Cost as the snapshot reported it. Never computed here.
public struct CostGlance: Codable, Sendable, Equatable {
    public let todayText: String
    public let weekText: String
    public let todayTokens: String?
    public let weekTokens: String?
    public let estimated: Bool
    public let detail: String
}

/// One-glance summary of an account for the compact panel styles.
public struct AccountGlance: Codable, Sendable, Equatable {
    public enum Status: String, Codable, Sendable {
        /// Every primary generation window is a trusted, measured reading.
        case ready
        /// A trusted, measured window is at 95% used or more.
        case out
        /// Some primary windows are trusted and some are not.
        case partial
        /// No primary window can be trusted (stale, errored, estimated, sign-in needed, disabled).
        case unavailable
        /// The account has no quota windows (for example a prepaid balance).
        case empty
    }
    public let status: Status
    /// The smaller remaining percentage of the primary windows; only when status is `.ready`.
    public let roomPercent: Double?
    public let roomText: String?
    public let tighterLane: QuotaLane?
    public let tighterTitle: String?
    /// Time until the tighter window resets, for example "2h 10m".
    public let resetText: String?
    /// For an account that is out: "Back in 40m", or "Reset time unknown".
    public let backInText: String?
    public let forecast: RunOutForecast?
    public let cost: CostGlance?

    public static let empty = AccountGlance(status: .empty, roomPercent: nil, roomText: nil, tighterLane: nil,
                                            tighterTitle: nil, resetText: nil, backInText: nil, forecast: nil, cost: nil)
}

extension Presenter {
    /// Used percentage at or above which a window counts as out.
    public static let outThreshold = 95.0

    static func lane(_ window: UsageWindow) -> QuotaLane {
        if window.name == "five_hour" { return .session }
        if window.name == "seven_day" { return .weekly }
        if window.windowMinutes == 300 { return .session }
        if window.windowMinutes == 10080 { return .weekly }
        return .other
    }
    static func laneTitle(_ lane: QuotaLane, fallback: String) -> String {
        switch lane {
        case .session: "5-hour"
        case .weekly: "Weekly"
        case .other: fallback
        }
    }
    static func resetIn(_ window: UsageWindow, now: Date) -> String? {
        guard let reset = UsageTime.parse(window.resetsAt), reset > now else { return nil }
        return duration(reset.timeIntervalSince(now))
    }
    /// Window length in seconds. Known Claude names win over a reported length.
    static func windowLength(_ window: UsageWindow) -> Double? {
        if window.name == "five_hour" { return 300 * 60 }
        if window.name == "seven_day" { return 10080 * 60 }
        guard let minutes = window.windowMinutes, minutes.isFinite, minutes > 0 else { return nil }
        return minutes * 60
    }

    /// Summarises the primary windows of one account. Only trusted, measured readings count:
    /// the same `valid` predicate as the bars, and never an estimate.
    public static func glance(_ account: UsageAccount, primary: [UsageWindow], snapshotFresh: Bool,
                              now: Date) -> AccountGlance {
        let cost = costGlance(account.cost, now: now)
        let generation = primary.filter(isGeneration)
        guard !generation.isEmpty else {
            return AccountGlance(status: .empty, roomPercent: nil, roomText: nil, tighterLane: nil, tighterTitle: nil,
                                 resetText: nil, backInText: nil, forecast: nil, cost: cost)
        }
        struct Reading { let window: UsageWindow; let used: Double; let left: Double; let reset: Date? }
        var trusted: [Reading] = []
        for window in generation {
            guard !account.isEstimated, !window.isEstimated,
                  valid(window, account: account, snapshotFresh: snapshotFresh, now: now),
                  let left = window.remainingPercent ?? window.usedPercent.map({ 100 - $0 }) else { continue }
            trusted.append(Reading(window: window, used: 100 - left, left: left, reset: UsageTime.parse(window.resetsAt)))
        }
        func title(_ window: UsageWindow) -> String { laneTitle(lane(window), fallback: windowTitle(window)) }

        let out = trusted.filter { $0.used >= outThreshold }
        if !out.isEmpty {
            // Every out window has to reset before the account has room again.
            let resets = out.map(\.reset)
            let backIn: String
            if resets.contains(where: { $0 == nil }) {
                backIn = "Reset time unknown"
            } else {
                let latest = resets.compactMap { $0 }.max() ?? now
                backIn = "Back in " + duration(latest.timeIntervalSince(now))
            }
            let binding = out.max { ($0.reset ?? .distantFuture) < ($1.reset ?? .distantFuture) } ?? out[0]
            return AccountGlance(status: .out, roomPercent: binding.left, roomText: percent(binding.left),
                                 tighterLane: lane(binding.window), tighterTitle: title(binding.window),
                                 resetText: nil, backInText: backIn, forecast: nil, cost: cost)
        }
        guard trusted.count == generation.count else {
            return AccountGlance(status: trusted.isEmpty ? .unavailable : .partial, roomPercent: nil, roomText: nil,
                                 tighterLane: nil, tighterTitle: nil, resetText: nil, backInText: nil,
                                 forecast: trusted.isEmpty ? nil : forecast(trusted.map(\.window), now: now), cost: cost)
        }
        // Tighter window: least room left; on a tie the later reset binds longer.
        guard let tighter = trusted.min(by: { lhs, rhs in
            if lhs.left != rhs.left { return lhs.left < rhs.left }
            return (lhs.reset ?? .distantPast) > (rhs.reset ?? .distantPast)
        }) else { return .empty }
        let resetText = tighter.reset.map { duration($0.timeIntervalSince(now)) }
        return AccountGlance(status: .ready, roomPercent: tighter.left, roomText: percent(tighter.left),
                             tighterLane: lane(tighter.window), tighterTitle: title(tighter.window),
                             resetText: resetText, backInText: nil,
                             forecast: forecast(trusted.map(\.window), now: now), cost: cost)
    }

    /// Minimum share of a window that must have passed before a projection is shown.
    public static let forecastMinimumElapsed = 0.10
    /// A projected run-out further away than this is not shown: too far ahead to act on, and mostly noise.
    public static let forecastHorizon = 24.0 * 3600

    /// Earliest window that, at its average rate since the window began, runs out before it resets
    /// and within `forecastHorizon`.
    /// Needs a known window length, a future reset inside that length, 10% of the window passed,
    /// and some use. The snapshot holds one sample per window, so a recent-rate forecast is not possible.
    static func forecast(_ windows: [UsageWindow], now: Date) -> RunOutForecast? {
        var best: RunOutForecast?
        for window in windows {
            guard let length = windowLength(window), let reset = UsageTime.parse(window.resetsAt),
                  let used = window.usedPercent ?? window.remainingPercent.map({ 100 - $0 }),
                  used > 0, used < outThreshold else { continue }
            let timeLeft = reset.timeIntervalSince(now)
            guard timeLeft > 0, timeLeft <= length else { continue }
            let elapsed = length - timeLeft
            guard elapsed >= length * forecastMinimumElapsed else { continue }
            let runOut = (100 - used) / (used / elapsed)
            guard runOut < timeLeft, runOut < forecastHorizon else { continue }
            if let current = best, current.seconds <= runOut { continue }
            let name = laneTitle(lane(window), fallback: windowTitle(window))
            let eta = "~" + duration(max(60, runOut))
            best = RunOutForecast(lane: lane(window), windowTitle: name, seconds: runOut,
                                  text: name + " runs out in " + eta + " at this pace",
                                  detail: "Average use since this window began, carried forward. It resets in " +
                                          duration(timeLeft) + ". Bursts of use make this rough.")
        }
        return best
    }

    /// Cost is shown only as reported, only while its sample is fresh and from today,
    /// and never when the two periods contradict each other.
    static func costGlance(_ cost: UsageCost?, now: Date, calendar: Calendar = .current) -> CostGlance? {
        guard let cost else { return nil }
        let currency = clean(cost.currencyCode ?? "").trimmingCharacters(in: .whitespacesAndNewlines)
        guard !currency.isEmpty, currency.count <= 8,
              UsageTime.fresh(cost.sampledAt, now: now), let sampled = UsageTime.parse(cost.sampledAt),
              calendar.isDate(sampled, inSameDayAs: now) else { return nil }
        func amount(_ value: Double?) -> Double? {
            guard let value, value.isFinite, value >= 0 else { return nil }
            return value
        }
        let today = amount(cost.today?.amount)
        let week = amount(cost.last7Days?.amount)
        guard today != nil || week != nil else { return nil }
        if let today, let week, week + 0.005 < today { return nil }
        func money(_ value: Double?) -> String {
            guard let value else { return "—" }
            return (currency == "USD" ? "$" : currency + " ") + String(format: "%.2f", value)
        }
        func tokens(_ value: Double?) -> String? {
            guard let value, value.isFinite, value >= 0 else { return nil }
            if value >= 1_000_000 { return String(format: "%.1fM", value / 1_000_000) }
            if value >= 1_000 { return String(format: "%.0fK", value / 1_000) }
            return String(format: "%.0f", value)
        }
        let estimated = cost.basis.map { $0.lowercased().contains("estimate") } ?? true
        return CostGlance(todayText: money(today), weekText: money(week),
                          todayTokens: tokens(cost.today?.tokens), weekTokens: tokens(cost.last7Days?.tokens),
                          estimated: estimated, detail: (estimated ? "Estimated from local logs, not a bill. " : "") + age(cost.sampledAt, now: now))
    }
}
