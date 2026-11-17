import Foundation

/// The Claude account with the most room right now. This is observation, never admission authority.
public struct SeatAdvice: Codable, Sendable, Equatable {
    public let key: String?
    public let slot: String?
    public let roomPercent: Double?
    public let text: String
    public let detail: String
    public var tooltipLine: String { "Claude · " + text }
}

extension Presenter {
    /// An account's room is the smaller remaining percentage of its 5-hour and weekly windows.
    /// Only fresh, trusted, measured readings count; ties go to the lower slot name.
    /// Returns nil when the snapshot has no Claude account at all.
    public static func bestSeat(_ snapshot: UsageSnapshot, now: Date = Date(), failure: String? = nil) -> SeatAdvice? {
        let rows = snapshot.accounts.filter { canonical($0.provider) == "claude" }
        guard !rows.isEmpty else { return nil }
        let fresh = snapshotIsFresh(snapshot, failure: failure, now: now)
        var best: (account: UsageAccount, room: Double, fiveHour: Double, weekly: Double)?
        for account in rows {
            guard let reading = room(account, snapshotFresh: fresh, now: now) else { continue }
            if let current = best {
                if reading.room < current.room { continue }
                if reading.room == current.room && account.slot >= current.account.slot { continue }
            }
            best = (account, reading.room, reading.fiveHour, reading.weekly)
        }
        guard let best, let left = percent(best.room) else {
            return SeatAdvice(key: nil, slot: nil, roomPercent: nil,
                              text: "Most room now: no fresh Claude reading",
                              detail: "No Claude account has fresh 5-hour and weekly readings. Refresh to check again.")
        }
        let slot = clean(best.account.slot)
        return SeatAdvice(key: best.account.key, slot: slot, roomPercent: best.room,
                          text: "Most room now: \(slot) · \(left) left",
                          detail: "Lower of 5-hour (\(percent(best.fiveHour) ?? "?") left) and weekly (\(percent(best.weekly) ?? "?") left). " +
                                  "Stale, errored, disabled and sign-in-needed accounts are skipped.")
    }

    /// Both the 5-hour and the weekly aggregate windows must be present and trusted.
    static func room(_ account: UsageAccount, snapshotFresh: Bool, now: Date) -> (room: Double, fiveHour: Double, weekly: Double)? {
        guard !account.isEstimated else { return nil }
        let aggregate = account.windows.filter { isGeneration($0) && ($0.model ?? "").isEmpty }
        func remaining(_ title: String) -> Double? {
            let windows = aggregate.filter { windowTitle($0) == title }
            guard !windows.isEmpty else { return nil }
            var values: [Double] = []
            for window in windows {
                guard !window.isEstimated, valid(window, account: account, snapshotFresh: snapshotFresh, now: now),
                      let left = window.remainingPercent ?? window.usedPercent.map({ 100 - $0 }) else { return nil }
                values.append(left)
            }
            return values.min()
        }
        guard let fiveHour = remaining("5-hour window"), let weekly = remaining("Weekly") else { return nil }
        return (min(fiveHour, weekly), fiveHour, weekly)
    }
}
