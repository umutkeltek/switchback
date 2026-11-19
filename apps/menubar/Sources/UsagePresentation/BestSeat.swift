import Foundation

/// The Claude account with the most room for one model whose weekly limit is tighter than the account's.
public struct ModelSeat: Codable, Sendable, Equatable {
    public let model: String
    public let key: String?
    public let slot: String?
    public let roomPercent: Double?
    /// For example "Fable: seat4 · 70% left".
    public let text: String
}

/// The Claude account with the most room right now. This is an observation, never admission authority.
public struct SeatAdvice: Codable, Sendable, Equatable {
    public let key: String?
    public let slot: String?
    public let roomPercent: Double?
    public let text: String
    public let detail: String
    /// Model-scoped limits whose best seat or room differs from the any-model answer.
    public let models: [ModelSeat]
    public init(key: String?, slot: String?, roomPercent: Double?, text: String, detail: String, models: [ModelSeat] = []) {
        self.key = key; self.slot = slot; self.roomPercent = roomPercent
        self.text = text; self.detail = detail; self.models = models
    }
    public var tooltipLine: String {
        (["Claude · " + text] + models.map { "Claude · " + $0.text }).joined(separator: "\n")
    }
}

extension Presenter {
    /// An account's room is the smaller remaining percentage of its 5-hour and weekly windows.
    /// Only fresh, trusted, measured readings count; ties go to the lower slot name.
    /// A model with its own weekly limit (for example Fable) also gets the best seat for that model:
    /// there an account's room is also capped by that model's remaining percentage.
    /// Returns nil when the snapshot has no Claude account at all.
    public static func bestSeat(_ snapshot: UsageSnapshot, now: Date = Date(), failure: String? = nil) -> SeatAdvice? {
        let rows = snapshot.accounts.filter { canonical($0.provider) == "claude" }
        guard !rows.isEmpty else { return nil }
        let fresh = snapshotIsFresh(snapshot, failure: failure, now: now)
        typealias Reading = (account: UsageAccount, room: Double, fiveHour: Double, weekly: Double)
        func best(_ readings: [Reading]) -> Reading? {
            readings.reduce(nil) { current, reading in
                guard let current else { return reading }
                if reading.room != current.room { return reading.room > current.room ? reading : current }
                return reading.account.slot < current.account.slot ? reading : current
            }
        }
        let readings: [Reading] = rows.compactMap { account in
            room(account, snapshotFresh: fresh, now: now).map { (account, $0.room, $0.fiveHour, $0.weekly) }
        }
        guard let winner = best(readings), let left = percent(winner.room) else {
            return SeatAdvice(key: nil, slot: nil, roomPercent: nil,
                              text: "Most room now: no fresh Claude reading",
                              detail: "No Claude account has fresh 5-hour and weekly readings. Refresh to check again.")
        }
        var models: [ModelSeat] = []
        for model in scopedModels(rows) {
            let capped: [Reading] = readings.compactMap { reading in
                switch modelRoom(reading.account, model: model, snapshotFresh: fresh, now: now) {
                case .none: return reading
                case .some(nil): return nil
                case .some(let value?): return (reading.account, min(reading.room, value), reading.fiveHour, reading.weekly)
                }
            }
            guard let pick = best(capped), let pickLeft = percent(pick.room) else {
                models.append(ModelSeat(model: model, key: nil, slot: nil, roomPercent: nil,
                                        text: model + ": no fresh reading"))
                continue
            }
            if pick.account.key == winner.account.key && pick.room == winner.room { continue }
            models.append(ModelSeat(model: model, key: pick.account.key, slot: clean(pick.account.slot),
                                    roomPercent: pick.room, text: model + ": " + clean(pick.account.slot) + " · " + pickLeft + " left"))
        }
        let slot = clean(winner.account.slot)
        return SeatAdvice(key: winner.account.key, slot: slot, roomPercent: winner.room,
                          text: "Most room now: \(slot) · \(left) left",
                          detail: "Lower of 5-hour (\(percent(winner.fiveHour) ?? "?") left) and weekly (\(percent(winner.weekly) ?? "?") left). " +
                                  "Stale, errored, disabled and sign-in-needed accounts are skipped.",
                          models: models)
    }

    /// Both 5-hour and weekly aggregate windows must be present and trusted.
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

    /// Model names with their own generation limit on any Claude account, in first-seen order.
    static func scopedModels(_ rows: [UsageAccount]) -> [String] {
        var seen: [String] = []
        for window in rows.flatMap(\.windows) where isGeneration(window) {
            let model = clean(window.model ?? "").trimmingCharacters(in: .whitespacesAndNewlines)
            if !model.isEmpty && !seen.contains(model) { seen.append(model) }
        }
        return seen
    }

    /// nil: the account has no limit of its own for this model. `.some(nil)`: it has one that cannot be trusted.
    static func modelRoom(_ account: UsageAccount, model: String, snapshotFresh: Bool, now: Date) -> Double?? {
        let windows = account.windows.filter {
            isGeneration($0) && clean($0.model ?? "").trimmingCharacters(in: .whitespacesAndNewlines) == model
        }
        guard !windows.isEmpty else { return .none }
        var values: [Double] = []
        for window in windows {
            guard !window.isEstimated, valid(window, account: account, snapshotFresh: snapshotFresh, now: now),
                  let left = window.remainingPercent ?? window.usedPercent.map({ 100 - $0 }) else { return .some(nil) }
            values.append(left)
        }
        return .some(values.min())
    }
}
