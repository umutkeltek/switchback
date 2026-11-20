import Foundation

/// The Claude account with the most room for one model whose weekly limit is tighter than the account's.
public struct ModelSeat: Codable, Sendable, Equatable {
    public let model: String
    public let key: String?
    public let slot: String?
    public let roomPercent: Double?
    /// For example "Fable: seat4 · 70% left".
    public let text: String
    /// Every seat with a fresh reading is at this model's limit.
    public let noSeatHasRoom: Bool
    /// When no seat has room for this model: the seat back first and how soon; nil when unknown.
    public let backKey: String?
    public let backSlot: String?
    public let backInSeconds: Double?
    public init(model: String, key: String?, slot: String?, roomPercent: Double?, text: String,
                noSeatHasRoom: Bool = false, backKey: String? = nil, backSlot: String? = nil, backInSeconds: Double? = nil) {
        self.model = model; self.key = key; self.slot = slot; self.roomPercent = roomPercent; self.text = text
        self.noSeatHasRoom = noSeatHasRoom; self.backKey = backKey; self.backSlot = backSlot; self.backInSeconds = backInSeconds
    }
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
    /// Every seat with a fresh reading is at its limit. `key` is then nil and no seat is recommended.
    public let noSeatHasRoom: Bool
    /// When no seat has room: the seat back first and how soon; nil when no reset time is known.
    public let backKey: String?
    public let backSlot: String?
    public let backInSeconds: Double?
    public init(key: String?, slot: String?, roomPercent: Double?, text: String, detail: String, models: [ModelSeat] = [],
                noSeatHasRoom: Bool = false, backKey: String? = nil, backSlot: String? = nil, backInSeconds: Double? = nil) {
        self.key = key; self.slot = slot; self.roomPercent = roomPercent
        self.text = text; self.detail = detail; self.models = models
        self.noSeatHasRoom = noSeatHasRoom; self.backKey = backKey; self.backSlot = backSlot; self.backInSeconds = backInSeconds
    }
    public var tooltipLine: String {
        (["Claude · " + text] + models.map { "Claude · " + $0.text }).joined(separator: "\n")
    }
}

extension Presenter {
    /// A seat with no more than this much left has no room. It is the rule that marks a segment used up.
    static var noRoomPercent: Double { 100 - outThreshold }

    /// An account's room is the smaller remaining percentage of its 5-hour and weekly windows.
    /// Only fresh, trusted, measured readings count; ties go to the lower slot name.
    /// A seat with no room is never recommended: when every seat with a reading is at its limit,
    /// the advice says so and names the seat that is back first.
    /// A model with its own weekly limit (for example Fable) also gets the best seat for that model:
    /// there an account's room is also capped by that model's remaining percentage.
    /// Returns nil when the snapshot has no Claude account at all.
    public static func bestSeat(_ snapshot: UsageSnapshot, now: Date = Date(), failure: String? = nil) -> SeatAdvice? {
        let rows = snapshot.accounts.filter { canonical($0.provider) == "claude" }
        guard !rows.isEmpty else { return nil }
        let fresh = snapshotIsFresh(snapshot, failure: failure, now: now)
        typealias Reading = (account: UsageAccount, room: Double, fiveHour: Double, weekly: Double, back: Date?)
        func best(_ readings: [Reading]) -> Reading? {
            readings.filter { $0.room > noRoomPercent }.reduce(nil) { current, reading in
                guard let current else { return reading }
                if reading.room != current.room { return reading.room > current.room ? reading : current }
                return reading.account.slot < current.account.slot ? reading : current
            }
        }
        /// The seat with no room that is back first, and in how many seconds.
        func soonest(_ readings: [Reading]) -> (account: UsageAccount, seconds: Double)? {
            let back = readings.compactMap { reading in reading.back.map { (reading.account, $0) } }
            guard let first = back.min(by: { $0.1 != $1.1 ? $0.1 < $1.1 : $0.0.slot < $1.0.slot }) else { return nil }
            return (first.0, max(0, first.1.timeIntervalSince(now)))
        }
        func backText(_ back: (account: UsageAccount, seconds: Double)?) -> String {
            back.map { clean($0.account.slot) + " back in " + duration($0.seconds) } ?? "return time unknown"
        }
        let readings: [Reading] = rows.compactMap { account in
            room(account, snapshotFresh: fresh, now: now).map {
                (account, $0.room, $0.fiveHour, $0.weekly, backDate(account, model: nil, snapshotFresh: fresh, now: now))
            }
        }
        guard !readings.isEmpty else {
            return SeatAdvice(key: nil, slot: nil, roomPercent: nil,
                              text: "Most room now: no fresh Claude reading",
                              detail: "No Claude account has fresh 5-hour and weekly readings. Refresh to check again.")
        }
        guard let winner = best(readings), let left = percent(winner.room) else {
            let back = soonest(readings)
            let first = back.map { clean($0.account.slot) + " is back first, in " + duration($0.seconds) + "." }
                ?? "No reset time is known."
            return SeatAdvice(key: nil, slot: nil, roomPercent: nil,
                              text: "Most room now: no seat has room · " + backText(back),
                              detail: "Every Claude account with fresh 5-hour and weekly readings is at its limit. " + first,
                              noSeatHasRoom: true, backKey: back?.account.key, backSlot: back.map { clean($0.account.slot) },
                              backInSeconds: back?.seconds)
        }
        var models: [ModelSeat] = []
        for model in scopedModels(rows) {
            let capped: [Reading] = readings.compactMap { reading in
                switch modelRoom(reading.account, model: model, snapshotFresh: fresh, now: now) {
                case .none: return reading
                case .some(nil): return nil
                case .some(let value?):
                    let room = min(reading.room, value)
                    // Out for this model: back when every window that is out has reset.
                    let back = room > noRoomPercent ? nil : backDate(reading.account, model: model, snapshotFresh: fresh, now: now)
                    return (reading.account, room, reading.fiveHour, reading.weekly, back)
                }
            }
            guard !capped.isEmpty else {
                models.append(ModelSeat(model: model, key: nil, slot: nil, roomPercent: nil,
                                        text: model + ": no fresh reading"))
                continue
            }
            guard let pick = best(capped), let pickLeft = percent(pick.room) else {
                let back = soonest(capped)
                models.append(ModelSeat(model: model, key: nil, slot: nil, roomPercent: nil,
                                        text: model + ": no seat has room · " + backText(back),
                                        noSeatHasRoom: true, backKey: back?.account.key, backSlot: back.map { clean($0.account.slot) },
                                        backInSeconds: back?.seconds))
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
                                  "Stale, sign-in-needed, disabled and used-up accounts are skipped.",
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

    /// When a used-up account has room again: the latest reset among its trusted 5-hour and weekly windows
    /// (and, for a model, that model's own limit) that are at their limit. nil when the account is not out
    /// or a window that is out has no future reset time.
    static func backDate(_ account: UsageAccount, model: String?, snapshotFresh: Bool, now: Date) -> Date? {
        let windows = account.windows.filter { window in
            guard isGeneration(window) else { return false }
            let scoped = clean(window.model ?? "").trimmingCharacters(in: .whitespacesAndNewlines)
            if scoped.isEmpty { return ["5-hour window", "Weekly"].contains(windowTitle(window)) }
            return scoped == model
        }
        let out = windows.filter { window in
            guard valid(window, account: account, snapshotFresh: snapshotFresh, now: now),
                  let left = window.remainingPercent ?? window.usedPercent.map({ 100 - $0 }) else { return false }
            return left <= noRoomPercent
        }
        guard !out.isEmpty else { return nil }
        let resets = out.map { UsageTime.parse($0.resetsAt) }
        guard resets.allSatisfy({ $0.map { $0 > now } ?? false }) else { return nil }
        return resets.compactMap { $0 }.max()
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
