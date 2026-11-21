import Foundation

/// State a compact visual carries by colour and by symbol together, never by colour alone.
public enum SeatTone: String, Codable, Sendable {
    case good, low, critical, out, stale, unknown, balance
    /// SF Symbol paired with the tone's colour.
    public var symbol: String {
        switch self {
        case .good: "checkmark.circle.fill"
        case .low: "exclamationmark.circle.fill"
        case .critical: "exclamationmark.triangle.fill"
        case .out: "xmark.circle.fill"
        case .stale: "clock"
        case .unknown: "questionmark.circle"
        case .balance: "creditcard"
        }
    }
    public var spoken: String {
        switch self {
        case .good: "Plenty left"
        case .low: "Running low"
        case .critical: "Almost out"
        case .out: "Used up"
        case .stale: "Old reading"
        case .unknown: "No reading"
        case .balance: "Prepaid balance"
        }
    }
}

/// One account reduced to what a glance needs: fractions left, a tone, one short chip.
/// Built only from the panel presentation, so every trust rule of the ledger still applies.
public struct SeatGlyph: Codable, Sendable, Identifiable {
    public let id: String
    public let provider: String
    /// Short name under the segment, for example "Seat 2" or "Neural".
    public let name: String
    /// Full name for tooltips and details, for example "Neuralwatt".
    public let title: String
    /// Remaining fraction that binds now: the tighter of the 5-hour and weekly windows. nil when untrusted.
    /// On a partial seat it is the tighter of the windows that are known.
    public let room: Double?
    public let session: Double?
    public let weekly: Double?
    /// The window `room` comes from.
    public let roomLane: QuotaLane?
    /// Some primary windows are trusted and some are not: `room` shows only what is known.
    public let partial: Bool
    /// A model's own weekly limit on an account-wide row, for example Fable.
    public let modelName: String?
    public let modelLeft: Double?
    public let tone: SeatTone
    /// The reset chip ("↻ 4d 19h"), when an out account is back ("back 1h 31m"), a stale age ("25m old")
    /// or a balance ("$18.40"). Never the age of a fresh reading: that is `age`.
    public let chip: String?
    /// The age of a trusted reading once it is at least 5 minutes old, for example "9m old"; nil when newer.
    public let age: String?
    /// Seconds until the tighter window resets (or the account is back); nil when unknown.
    public let resetSeconds: Double?
    /// Seconds until the 5-hour window resets: when short-term room comes back.
    public let sessionResetSeconds: Double?
    /// A prepaid balance of zero: drawn as an empty segment.
    public let balanceEmpty: Bool
    public let best: Bool
    /// One VoiceOver sentence, for example "Seat 2: 38 percent left, weekly; Fable 73 percent used; resets in 4 days".
    public let spoken: String
    /// The tooltip: both windows, a model's own limit, the full name and the slot.
    public let detail: String
    /// What was left at a reading too old to count, shown muted with its age; never room.
    public var lastRoom: Double? = nil
    /// The reading is at least `clockAfter` old: the segment carries a small clock mark.
    public var clock = false
    /// Why a partial seat is partial, for example "weekly only" when it has no 5-hour reading.
    public var partialNote: String? = nil

    /// The owner can fix this seat: it needs sign-in, or another account is signed in.
    public var needsSignIn: Bool { tone == .unknown && (chip == "sign in" || chip == "account changed") }
    /// The symbol beside the name. A seat that needs sign-in has its own, not the "?" of a seat with no reading.
    public var symbol: String {
        if needsSignIn { return Self.signInSymbol }
        return partial ? "circle.lefthalf.filled" : tone.symbol
    }
    public static let signInSymbol = "person.crop.circle.badge.exclamationmark"

    public var roomText: String {
        switch tone {
        case .balance: chip ?? "—"
        case .stale: lastRoom.map { String(format: "%.0f", ($0 * 100).rounded()) } ?? "—"
        // A used-up seat shows what is really left, for example 3, with its used-up symbol.
        default: room.map { String(format: "%.0f", ($0 * 100).rounded()) } ?? "—"
        }
    }
}

public struct DirectionSection: Codable, Sendable, Identifiable {
    public let id: String
    public let title: String
    public let seats: [SeatGlyph]
    /// The one-line legend for the thin model bar, for example "Room for Fable". nil when no seat has a model
    /// limit, or when no seat's thin bar has any fill: empty tracks under every seat say nothing.
    public let legend: String?
    /// The legend for VoiceOver, for example "Room for Fable: the thin bar under each seat, …".
    public let legendSpoken: String?
    public init(id: String, title: String, seats: [SeatGlyph]) {
        self.id = id; self.title = title; self.seats = seats
        var models: [String] = []
        for seat in seats where seat.modelLeft != nil {
            if let model = seat.modelName, !model.isEmpty, !models.contains(model) { models.append(model) }
        }
        let filled = seats.contains { ($0.modelLeft ?? 0) > 0 }
        let legend = models.isEmpty || !filled ? nil : "Room for " + models.joined(separator: " · ")
        self.legend = legend
        legendSpoken = legend.map {
            $0 + ": the thin bar under each seat, the smaller of the model's own limit and the seat's room."
        }
    }
    /// True when at least one seat draws the thin model bar.
    public var hasModelBar: Bool { legend != nil }
}

/// Everything the visual looks draw. Money and run-out sentences stay in details.
public struct DirectionPresentation: Codable, Sendable {
    public let best: SeatGlyph?
    /// For example ("Fable", seat glyph) when a model's best seat differs.
    public let modelBest: [ModelPick]
    public let sections: [DirectionSection]
    /// Set when every Claude seat with a fresh reading is at its limit: the card says so instead of naming a seat.
    public var noRoom: NoRoom? = nil
    /// Single plans with nothing to show (no windows, no balance, no error the owner can act on):
    /// one quiet line under the grid instead of empty tiles.
    public var noReading: NoReadingLine? = nil
    /// "No reading: MiniMax, Go, Zen, OpenRouter", with the full names and reasons in `help`.
    public struct NoReadingLine: Codable, Sendable {
        public let ids: [String]
        public let text: String
        public let help: String
        public let spoken: String
    }
    public struct ModelPick: Codable, Sendable, Identifiable {
        public let model: String
        public let seat: SeatGlyph?
        public let roomText: String
        /// No seat has room for this model: "back 2d 1h", or "no room" when no reset time is known.
        public var backText: String? = nil
        /// The seat back first and how soon, for example "Seat 2" and "1d 5h"; nil when unknown.
        public var backSeat: String? = nil
        public var backIn: String? = nil
        /// The model's best seat is the seat to use now: the card names it once, on the left.
        public var sameAsBest = false
        public var id: String { model }
    }
    /// "No seat has room", with the seat that is back first, for example "Seat 3 back in 8h 29m".
    public struct NoRoom: Codable, Sendable {
        public let title: String
        public let detail: String
        /// The seat back first; nil when no seat has a known reset time.
        public let seat: SeatGlyph?
        /// For example "8h 29m".
        public let backIn: String?
        /// The full sentence for the tooltip.
        public let help: String
        public let spoken: String
    }
    /// Set when there is a Claude section but no seat to use now and no "No seat has room": no Claude seat has
    /// a fresh reading. The card keeps its place and size and offers a refresh.
    public var noFresh: NoFresh? = nil
    /// "No fresh reading", "Newest 25m old · click to refresh".
    public struct NoFresh: Codable, Sendable {
        public let title: String
        public let detail: String
        public let help: String
        public let spoken: String
    }
    /// Every seat, in drawing order: keyboard navigation follows it.
    public var seats: [SeatGlyph] { sections.flatMap(\.seats) }
}

extension Presenter {
    /// Age after which a segment carries the clock mark. Normal refreshes run every few minutes.
    public static let clockAfter: TimeInterval = 1_200

    public static func tone(room: Double?) -> SeatTone {
        guard let room else { return .unknown }
        if room <= 0.05 { return .out }
        if room <= 0.10 { return .critical }
        if room <= 0.30 { return .low }
        return .good
    }

    static func seconds(_ text: String?) -> Double? {
        guard let text, !text.isEmpty else { return nil }
        var total = 0.0
        for part in text.split(separator: " ") {
            guard let unit = part.last, let value = Double(part.dropLast()) else { return nil }
            switch unit {
            case "d": total += value * 86_400
            case "h": total += value * 3_600
            case "m": total += value * 60
            default: return nil
            }
        }
        return total
    }

    /// "4 days 19 hours", "1 hour 31 minutes" or "40 minutes", for VoiceOver.
    public static func spokenDuration(_ seconds: Double) -> String {
        let minutes = max(0, Int(seconds / 60))
        func unit(_ value: Int, _ name: String) -> String { "\(value) \(name)" + (value == 1 ? "" : "s") }
        if minutes >= 1440 {
            let hours = (minutes % 1440) / 60
            return unit(minutes / 1440, "day") + (hours > 0 ? " " + unit(hours, "hour") : "")
        }
        if minutes >= 60 { return unit(minutes / 60, "hour") + (minutes % 60 > 0 ? " " + unit(minutes % 60, "minute") : "") }
        return unit(minutes, "minute")
    }

    /// A provider name short enough for a gauge segment; the full name stays in the tooltip.
    public static func gaugeName(key: String, title: String) -> String {
        switch key {
        case "zai": "Z.ai"
        case "minimax": "MiniMax"
        case "deepseek": "DeepSeek"
        case "neuralwatt": "Neural"
        case "openrouter": "OpenR."
        case "opencodego": "Go"
        case "opencodezen": "Zen"
        case "qwencloud": "Qwen"
        default: displayTitle(key: key, title: title)
        }
    }

    /// A balance short enough for a gauge segment: "$18.36", "$1,099", "$12.0k". The full value stays in details.
    public static func compactBalance(_ row: BalanceRow) -> String {
        guard let currency = row.currency else { return "—" }
        // A one-character symbol where it is unambiguous; other currencies keep their code.
        let symbols = ["USD": "$", "EUR": "€", "GBP": "£"]
        let text = symbols[currency].map { money(row.amount, currency: "USD").replacingOccurrences(of: "$", with: $0) }
            ?? money(row.amount, currency: currency)
        return (row.isEstimate ? "~" : "") + text
    }

    static func laneWord(_ lane: QuotaLane?) -> String? {
        switch lane {
        case .session: "5-hour"
        case .weekly: "weekly"
        default: nil
        }
    }

    public static func directions(_ panel: PanelPresentation) -> DirectionPresentation {
        var sections: [DirectionSection] = []
        var others: [SeatGlyph] = []
        var bestSeat: SeatGlyph?
        var noRoom: DirectionPresentation.NoRoom?
        var noFresh: DirectionPresentation.NoFresh?
        var picks: [DirectionPresentation.ModelPick] = []
        var quiet: [(id: String, name: String, title: String, reason: String)] = []
        for provider in panel.providers {
            let bestKey = provider.advice?.key
            let title = displayTitle(key: provider.key, title: provider.title)
            let shortTitle = gaugeName(key: provider.key, title: title)
            // A provider with one account is named by the provider in the shared "Other plans" group.
            let single = provider.accounts.count == 1
            let seats = provider.accounts.map {
                seatGlyph($0, provider: provider.key, best: bestKey != nil && $0.key == bestKey,
                          name: single ? shortTitle : nil, title: single ? title : nil)
            }
            if let key = bestKey, bestSeat == nil, var seat = seats.first(where: { $0.id == key }) {
                // The best seat with no 5-hour reading is marked on the card too.
                if provider.advice?.weeklyOnly == true { seat.partialNote = "weekly only" }
                bestSeat = seat
            }
            if let advice = provider.advice, advice.noSeatHasRoom, noRoom == nil {
                let back = seats.first { $0.id == advice.backKey }
                let backIn = advice.backInSeconds.map(duration)
                let name = back?.name ?? advice.backSlot
                // The big "Back in 6h 59m" on the right carries the time; the subtitle names the seat once.
                let detail = name.flatMap { name in backIn.map { _ in name + " is first back" } } ?? "Return time unknown"
                noRoom = .init(title: "No seat has room", detail: detail, seat: back, backIn: backIn, help: advice.detail,
                               spoken: "No seat has room. " + (name.flatMap { name in
                                   advice.backInSeconds.map { name + " is back in " + spokenDuration($0) } } ?? "Return time unknown"))
            }
            if provider.key == "claude", !provider.accounts.isEmpty, bestKey == nil,
               provider.advice?.noSeatHasRoom != true, noFresh == nil {
                // No seat to use now and no "No seat has room": say so in the card's place, and offer a refresh.
                // Old readings say how old the newest is. Recent ones that still do not count (sign-in needed, an
                // error, marked stale) point at the seats, which say why.
                let newest = provider.accounts.compactMap(\.ageSeconds).min().map { max(0, $0) }
                let old = newest.map { $0 > readingsOldAfter } ?? false
                let detail = newest == nil ? "No reading yet" : old ? "Newest " + duration(newest ?? 0) + " old" : "See the seats below"
                let said = newest == nil ? "No reading yet." : old ? "Newest reading " + spokenDuration(newest ?? 0) + " old."
                    : "See the seats below."
                noFresh = .init(title: "No fresh reading", detail: detail + " · click to refresh",
                                help: "No Claude seat has a fresh reading. Click to refresh.",
                                spoken: "No fresh Claude reading. " + said)
            }
            for model in provider.advice?.models ?? [] {
                var pick = DirectionPresentation.ModelPick(model: model.model, seat: seats.first { $0.id == model.key },
                                                           roomText: model.roomPercent.map { String(format: "%.0f", $0) } ?? "—")
                pick.sameAsBest = bestKey != nil && model.key == bestKey
                if model.noSeatHasRoom {
                    let name = seats.first { $0.id == model.backKey }?.name ?? model.backSlot
                    pick.backText = model.backInSeconds.map { (name.map { $0 + " " } ?? "") + "back in " + duration($0) } ?? "no room"
                    pick.backSeat = model.backInSeconds == nil ? nil : name
                    pick.backIn = model.backInSeconds.map(duration)
                }
                picks.append(pick)
            }
            // Providers with several accounts keep their own group; single accounts and providers
            // without a reading share one "Other plans" group, named by provider.
            // A single plan with nothing to show is named on one quiet line instead of drawing an empty tile.
            let quietName = shortTitle.hasSuffix(".") ? title : shortTitle
            if seats.count > 1 {
                sections.append(DirectionSection(id: provider.key, title: title, seats: seats))
            } else if let seat = seats.first {
                if seat.room == nil && seat.lastRoom == nil && seat.tone == .unknown && seat.chip == "no reading" {
                    let account = provider.accounts[0]
                    quiet.append((seat.id, quietName, title, account.notice ?? "No usage reported"))
                } else {
                    others.append(seat)
                }
            } else if let message = provider.emptyMessage {
                quiet.append((provider.key, quietName, title, message))
            }
        }
        if !others.isEmpty {
            // A filter that leaves one plan names it by its provider.
            let title = sections.isEmpty && others.count == 1 ? others[0].title : "Other plans"
            sections.append(DirectionSection(id: "other", title: title, seats: others))
        }
        var result = DirectionPresentation(best: bestSeat, modelBest: picks, sections: sections, noRoom: noRoom)
        result.noFresh = noFresh
        if !quiet.isEmpty {
            let names = quiet.map(\.name).joined(separator: ", ")
            result.noReading = .init(ids: quiet.map(\.id), text: "No reading: " + names,
                                     help: quiet.map { $0.title + ": " + $0.reason }.joined(separator: "\n"),
                                     spoken: "No reading for " + quiet.map(\.title).joined(separator: ", "))
        }
        return result
    }

    static func seatGlyph(_ account: AccountPanel, provider: String, best: Bool,
                          name: String? = nil, title: String? = nil) -> SeatGlyph {
        let name = name ?? account.shortName
        let glance = account.glance
        let ledger = account.ledger
        let modelRow = ledger.note == nil
            ? account.extras.first { $0.generation && $0.qualifier != nil && $0.lane == .weekly } : nil
        let balance = account.primary.isEmpty ? account.balances.first : nil
        var tone: SeatTone
        var room: Double?
        var roomLane: QuotaLane?
        var partial = false
        var chip: String?
        var age: String?
        var resetSeconds: Double?
        var backIn: String?
        var balanceEmpty = false
        var lastRoom: Double?
        var partialNote: String?
        if let balance {
            tone = balance.state == .measured ? .balance : .stale
            chip = compactBalance(balance)
            balanceEmpty = balance.amount <= 0
        } else {
            switch glance.status {
            case .out:
                // What is really left, for example 3; the tone and symbol say it is used up.
                tone = .out; room = glance.roomPercent.map { max(0, $0) / 100 } ?? 0; roomLane = glance.tighterLane
                backIn = glance.backInText?.replacingOccurrences(of: "Back in ", with: "")
                chip = glance.backInText?.replacingOccurrences(of: "Back in ", with: "back ")
                resetSeconds = seconds(backIn)
                age = account.ageText.map { $0 + " old" }
            case .ready:
                room = glance.roomPercent.map { $0 / 100 }
                roomLane = glance.tighterLane
                tone = Presenter.tone(room: room)
                chip = glance.resetText.map { "↻ " + $0 }
                resetSeconds = seconds(glance.resetText)
                age = account.ageText.map { $0 + " old" }
            case .partial:
                // Show what is known, marked as partial: the tighter of the trusted windows.
                let known = account.primary.filter { $0.generation && $0.remainingFraction != nil }
                if let tightest = known.min(by: { ($0.remainingFraction ?? 1) < ($1.remainingFraction ?? 1) }) {
                    room = tightest.remainingFraction
                    roomLane = tightest.lane
                    chip = tightest.resetIn.map { "↻ " + $0 }
                    resetSeconds = seconds(tightest.resetIn)
                }
                partial = room != nil
                tone = room == nil ? .unknown : Presenter.tone(room: room)
                age = account.ageText.map { $0 + " old" }
                // No 5-hour numbers at all (an idle seat): its room is its weekly room.
                let missing = account.primary.filter { $0.generation && $0.remainingFraction == nil }
                if partial, roomLane == .weekly, !missing.isEmpty, missing.allSatisfy({ $0.lane == .session && $0.lastReading == nil }) {
                    partialNote = "weekly only"
                }
            case .unavailable:
                // A reading too old to count shows its last number, muted, with its age under it ("40m ago").
                // It never counts as room. Sign-in and identity problems hide the number and say what to do.
                let last = account.primary.filter(\.generation).compactMap(\.lastFraction).min()
                if ["Older reading", "Refresh unavailable"].contains(account.notice), let last {
                    tone = .stale; lastRoom = last
                    chip = account.ageText.map { $0 + " ago" } ?? "old reading"
                } else {
                    tone = .unknown
                    chip = actionChip(account.notice) ?? "no reading"
                }
            case .empty:
                tone = .unknown
                chip = actionChip(account.notice) ?? "no reading"
            }
        }
        let session = ledger.session?.remainingFraction
        let weekly = ledger.weekly?.remainingFraction
        // The thin bar is the room usable for the model on this seat: the smaller of its own limit and the seat's room,
        // the same number the card's "For Fable" uses.
        let modelLeft = modelRow?.remainingFraction.map { limit in room.map { min(limit, $0) } ?? limit }
        let modelName = modelRow?.qualifier.map { String($0.prefix(1)).uppercased() + $0.dropFirst() }

        // VoiceOver: "Seat 2: 38 percent left, weekly; Fable 73 percent used; resets in 4 days 19 hours".
        var said: [String] = []
        let lane = laneWord(roomLane)
        switch tone {
        case .balance:
            said.append("prepaid balance " + (chip ?? "unknown"))
        case .out:
            said.append("used up" + (lane.map { ", " + $0 } ?? ""))
        case .stale:
            said.append("old reading" + (lastRoom.map { String(format: ", %.0f percent left then", $0 * 100) } ?? ""))
        case .unknown:
            switch chip {
            case "sign in": said.append("sign in needed")
            case "account changed": said.append("another account is signed in, sign in again")
            default: said.append("no reading")
            }
        default:
            var first = String(format: "%.0f percent left", (room ?? 0) * 100)
            if partial {
                let missing = account.primary.filter { $0.generation && $0.remainingFraction == nil }
                    .map { (laneWord($0.lane) ?? $0.title.lowercased()) + " unknown" }
                    .joined(separator: ", ")
                first += (lane.map { ", " + $0 + " only" } ?? "") + (missing.isEmpty ? "" : ", " + missing)
            } else if let lane {
                first += ", " + lane
            }
            said.append(first)
        }
        if let modelName, let modelLeft {
            let left = Int((modelLeft * 100).rounded())
            said.append(left == 0 ? "no room for " + modelName : "room for \(modelName) \(left) percent")
        }
        if let resetSeconds {
            said.append((tone == .out ? "back in " : "resets in ") + spokenDuration(resetSeconds))
        } else if tone == .out, glance.backInText == "Reset time unknown" {
            said.append("reset time unknown")
        }
        if let age { said.append("reading " + age) }
        if tone == .stale, let chip { said.append(chip) }
        let lastReading = tone == .unknown && balance == nil ? account.ageText.map { "last reading " + $0 + " old" } : nil
        if let lastReading { said.append(lastReading) }
        if let notice = account.notice, !["Older reading", "Partial reading", noReadingNotice].contains(notice),
           !notice.hasSuffix("at limit"), actionChip(notice) == nil || tone != .unknown {
            said.append(notice.lowercased())
        }
        if best { said.append("most room now") }
        let spoken = (title ?? account.shortName) + ": " + said.joined(separator: "; ")

        // Tooltip: both windows and a model's own limit, then identity.
        func line(_ name: String, _ row: QuotaRow?) -> String? {
            guard let row else { return nil }
            return name + " " + (row.state == .unavailable ? "unknown" : row.valueText + " left")
        }
        let windows = [line("5-hour", ledger.session), line("Weekly", ledger.weekly),
                       modelRow.flatMap { line((modelName ?? "Model") + " limit", $0) }].compactMap { $0 }
        var lines = [(title.map { $0 + " · " } ?? "") + account.label + " · account " + account.slot]
        if let balance { lines.append(balance.title + " " + balance.valueText) }
        if !windows.isEmpty { lines.append(windows.joined(separator: " · ")) }
        if partial {
            lines.append(partialNote == "weekly only" ? "Weekly only: no 5-hour reading yet"
                         : "Partial: showing only the windows with a fresh reading")
        }
        if let lastRoom {
            lines.append(String(format: "Last reading: %.0f%% left, too old to count", lastRoom * 100))
        }
        if let age { lines.append("Reading " + age) }
        if let lastReading { lines.append(String(lastReading.prefix(1)).uppercased() + lastReading.dropFirst()) }
        if let notice = account.notice { lines.append(notice) }
        if account.directCheckFailed { lines.append("Last direct check failed") }
        var glyph = SeatGlyph(id: account.key, provider: provider, name: name, title: title ?? account.label, room: room,
                              session: session, weekly: weekly, roomLane: roomLane, partial: partial,
                              modelName: modelName, modelLeft: modelLeft, tone: tone, chip: chip, age: age,
                              resetSeconds: resetSeconds,
                              // A 5-hour reset gives room back only to an account that is not out for longer.
                              sessionResetSeconds: glance.status == .ready ? seconds(ledger.session?.resetIn) : nil,
                              balanceEmpty: balanceEmpty, best: best, spoken: spoken, detail: lines.joined(separator: "\n"))
        glyph.lastRoom = lastRoom
        glyph.partialNote = partialNote
        // One clock mark only: a muted old tile already has the clock beside its name.
        glyph.clock = balance == nil && tone != .stale && (account.ageSeconds ?? 0) >= clockAfter
            && (room != nil || lastRoom != nil)
        return glyph
    }

    /// The tile chip for a seat the owner can fix: "sign in", or "account changed" when another account is signed in.
    static func actionChip(_ notice: String?) -> String? {
        switch notice {
        case "Sign in needed": "sign in"
        case "Account changed": "account changed"
        default: nil
        }
    }
}
