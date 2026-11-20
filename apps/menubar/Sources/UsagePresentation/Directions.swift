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

    public var roomText: String {
        switch tone {
        case .balance: chip ?? "—"
        case .out: "0"
        default: room.map { String(format: "%.0f", ($0 * 100).rounded()) } ?? "—"
        }
    }
}

public struct DirectionSection: Codable, Sendable, Identifiable {
    public let id: String
    public let title: String
    public let seats: [SeatGlyph]
    /// The one-line legend for the thin model bar, for example "Fable limit". nil when no seat has a model limit.
    public let legend: String?
    public init(id: String, title: String, seats: [SeatGlyph]) {
        self.id = id; self.title = title; self.seats = seats
        var models: [String] = []
        for seat in seats where seat.modelLeft != nil {
            if let model = seat.modelName, !model.isEmpty, !models.contains(model) { models.append(model) }
        }
        legend = models.isEmpty ? nil : models.joined(separator: " · ") + " limit"
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
    public struct ModelPick: Codable, Sendable, Identifiable {
        public let model: String
        public let seat: SeatGlyph?
        public let roomText: String
        public var id: String { model }
    }
    /// Every seat, in drawing order: keyboard navigation follows it.
    public var seats: [SeatGlyph] { sections.flatMap(\.seats) }
}

extension Presenter {
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
        var picks: [DirectionPresentation.ModelPick] = []
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
            if let key = bestKey { bestSeat = bestSeat ?? seats.first { $0.id == key } }
            for model in provider.advice?.models ?? [] {
                picks.append(.init(model: model.model, seat: seats.first { $0.id == model.key },
                                   roomText: model.roomPercent.map { String(format: "%.0f", $0) } ?? "—"))
            }
            // Providers with several accounts keep their own group; single accounts and providers
            // without a reading share one "Other plans" group, named by provider.
            if seats.count > 1 {
                sections.append(DirectionSection(id: provider.key, title: title, seats: seats))
            } else if let seat = seats.first {
                others.append(seat)
            } else if let message = provider.emptyMessage {
                others.append(SeatGlyph(id: provider.key, provider: provider.key, name: shortTitle, title: title, room: nil,
                                        session: nil, weekly: nil, roomLane: nil, partial: false, modelName: nil, modelLeft: nil,
                                        tone: .unknown, chip: "no reading", age: nil, resetSeconds: nil,
                                        sessionResetSeconds: nil, balanceEmpty: false, best: false,
                                        spoken: title + ": no reading, " + message, detail: title + "\n" + message))
            }
        }
        if !others.isEmpty {
            // A filter that leaves one plan names it by its provider.
            let title = sections.isEmpty && others.count == 1 ? others[0].title : "Other plans"
            sections.append(DirectionSection(id: "other", title: title, seats: others))
        }
        return DirectionPresentation(best: bestSeat, modelBest: picks, sections: sections)
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
        if let balance {
            tone = balance.state == .measured ? .balance : .stale
            chip = compactBalance(balance)
            balanceEmpty = balance.amount <= 0
        } else {
            switch glance.status {
            case .out:
                tone = .out; room = 0; roomLane = glance.tighterLane
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
            case .unavailable:
                tone = account.notice == "Older reading" ? .stale : .unknown
                // Short enough for a segment: "25m old", or "old reading" when the sample time is unknown.
                chip = account.ageText.map { $0 + " old" } ?? (tone == .stale ? "old reading" : nil)
            case .empty:
                tone = .unknown
                chip = "no reading"
            }
        }
        let session = ledger.session?.remainingFraction
        let weekly = ledger.weekly?.remainingFraction
        let modelLeft = modelRow?.remainingFraction
        let modelName = modelRow?.qualifier.map { String($0.prefix(1)).uppercased() + $0.dropFirst() }

        // VoiceOver: "Seat 2: 38 percent left, weekly; Fable 73 percent used; resets in 4 days 19 hours".
        var said: [String] = []
        let lane = laneWord(roomLane)
        switch tone {
        case .balance:
            said.append("prepaid balance " + (chip ?? "unknown"))
        case .out:
            said.append("used up" + (lane.map { ", " + $0 } ?? ""))
        case .stale, .unknown:
            said.append(tone == .stale ? "old reading" : "no reading")
        default:
            var first = String(format: "%.0f percent left", (room ?? 0) * 100)
            if partial {
                let missing = account.primary.filter { $0.generation && $0.remainingFraction == nil }
                    .map { (laneWord($0.lane) ?? $0.title.lowercased()) + " unknown" }.joined(separator: ", ")
                first += (lane.map { ", " + $0 + " only" } ?? "") + (missing.isEmpty ? "" : ", " + missing)
            } else if let lane {
                first += ", " + lane
            }
            said.append(first)
        }
        if let modelName, let modelLeft {
            let used = Int(((1 - modelLeft) * 100).rounded())
            said.append(used >= 100 ? modelName + " limit used up" : modelName + " \(used) percent used")
        }
        if let resetSeconds {
            said.append((tone == .out ? "back in " : "resets in ") + spokenDuration(resetSeconds))
        } else if tone == .out, glance.backInText == "Reset time unknown" {
            said.append("reset time unknown")
        }
        if let age { said.append("reading " + age) }
        if tone == .stale, let chip { said.append(chip) }
        if let notice = account.notice, notice != "Older reading", notice != "Partial reading", !notice.hasSuffix("at limit") {
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
        if partial { lines.append("Partial: showing only the windows with a fresh reading") }
        if let age { lines.append("Reading " + age) }
        if let notice = account.notice { lines.append(notice) }
        return SeatGlyph(id: account.key, provider: provider, name: name, title: title ?? account.label, room: room,
                         session: session, weekly: weekly, roomLane: roomLane, partial: partial,
                         modelName: modelName, modelLeft: modelLeft, tone: tone, chip: chip, age: age,
                         resetSeconds: resetSeconds,
                         // A 5-hour reset gives room back only to an account that is not out for longer.
                         sessionResetSeconds: glance.status == .ready ? seconds(ledger.session?.resetIn) : nil,
                         balanceEmpty: balanceEmpty, best: best, spoken: spoken, detail: lines.joined(separator: "\n"))
    }
}
