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
                  let left = effectiveLeft(window, account: account, snapshotFresh: snapshotFresh, now: now) else { continue }
            // A 5-hour window that has started again has no known reset time yet.
            let reset = UsageTime.parse(window.resetsAt).flatMap { $0 > now ? $0 : nil }
            trusted.append(Reading(window: window, used: 100 - left, left: left, reset: reset))
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

}
