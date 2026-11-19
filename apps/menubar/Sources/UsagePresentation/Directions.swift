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
    public let name: String
    /// Remaining fraction that binds now (smaller of the primary windows); nil when untrusted.
    public let room: Double?
    public let session: Double?
    public let weekly: Double?
    /// A model's own weekly limit on an account-wide row, for example Fable.
    public let modelName: String?
    public let modelLeft: Double?
    public let tone: SeatTone
    /// "2h", "back 1d 9h", "25m old" or a balance such as "$18.40".
    public let chip: String?
    /// Seconds until the tighter window resets (or the account is back); nil when unknown.
    public let resetSeconds: Double?
    /// Seconds until the 5-hour window resets: when short-term room comes back.
    public let sessionResetSeconds: Double?
    public let best: Bool
    public let spoken: String
    /// The same seat under another name, for example its provider's in the shared "Other plans" group.
    public func named(_ name: String) -> SeatGlyph {
        SeatGlyph(id: id, provider: provider, name: name, room: room, session: session, weekly: weekly,
                  modelName: modelName, modelLeft: modelLeft, tone: tone, chip: chip, resetSeconds: resetSeconds,
                  sessionResetSeconds: sessionResetSeconds, best: best, spoken: name + ", " + spoken)
    }
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
}

/// Everything the three explorations draw. Money and run-out sentences stay in details.
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

    public static func directions(_ panel: PanelPresentation) -> DirectionPresentation {
        var sections: [DirectionSection] = []
        var others: [SeatGlyph] = []
        var bestSeat: SeatGlyph?
        var picks: [DirectionPresentation.ModelPick] = []
        for provider in panel.providers {
            let bestKey = provider.advice?.key
            let seats = provider.accounts.map { account -> SeatGlyph in
                let glance = account.glance
                let ledger = account.ledger
                let modelRow = ledger.note == nil
                    ? account.extras.first { $0.generation && $0.qualifier != nil && $0.lane == .weekly } : nil
                let balance = account.primary.isEmpty ? account.balances.first : nil
                var tone: SeatTone
                var room: Double?
                var chip: String?
                var resetSeconds: Double?
                if let balance {
                    tone = balance.state == .measured ? .balance : .stale
                    chip = balance.valueText.replacingOccurrences(of: "USD ", with: "$")
                } else {
                    switch glance.status {
                    case .out:
                        tone = .out; room = 0
                        chip = glance.backInText?.replacingOccurrences(of: "Back in ", with: "back ")
                        resetSeconds = seconds(glance.backInText?.replacingOccurrences(of: "Back in ", with: ""))
                    case .ready:
                        room = glance.roomPercent.map { $0 / 100 }
                        tone = Presenter.tone(room: room)
                        chip = account.ageText.map { $0 + " old" } ?? glance.resetText
                        resetSeconds = seconds(glance.resetText)
                    case .unavailable, .partial:
                        tone = account.notice == "Older reading" ? .stale : .unknown
                        chip = account.ageText.map { $0 + " old" }
                            ?? (tone == .stale ? account.sampleText.replacingOccurrences(of: "Sampled ", with: "") : nil)
                    case .empty:
                        tone = .unknown
                    }
                }
                let spoken = [account.shortName, "account " + account.slot, tone.spoken,
                              room.map { String(format: "%.0f%% left", $0 * 100) },
                              modelRow.map { ($0.qualifier ?? "") + " " + $0.valueText + " left" },
                              chip].compactMap { $0 }.joined(separator: ", ")
                return SeatGlyph(id: account.key, provider: provider.key, name: account.shortName, room: room,
                                 session: ledger.session?.remainingFraction, weekly: ledger.weekly?.remainingFraction,
                                 modelName: modelRow?.qualifier, modelLeft: modelRow?.remainingFraction,
                                 tone: tone, chip: chip, resetSeconds: resetSeconds,
                                 // A 5-hour reset gives room back only to an account that is not out for longer.
                                 sessionResetSeconds: glance.status == .ready ? seconds(ledger.session?.resetIn) : nil,
                                 best: bestKey != nil && account.key == bestKey, spoken: spoken)
            }
            if let key = bestKey { bestSeat = bestSeat ?? seats.first { $0.id == key } }
            for model in provider.advice?.models ?? [] {
                picks.append(.init(model: model.model, seat: seats.first { $0.id == model.key },
                                   roomText: model.roomPercent.map { String(format: "%.0f", $0) } ?? "—"))
            }
            let title = provider.key == "zai" ? "Z.ai" : Presenter.displayTitle(key: provider.key, title: provider.title)
            let shortTitle = ["opencodego": "Go", "opencodezen": "Zen"][provider.key] ?? title
            // Providers with several accounts keep their own group; single accounts and providers
            // without a reading share one "Other plans" group, named by provider.
            if seats.count > 1 {
                sections.append(DirectionSection(id: provider.key, title: title, seats: seats))
            } else if let seat = seats.first {
                others.append(seat.named(shortTitle))
            } else if provider.emptyMessage != nil {
                others.append(SeatGlyph(id: provider.key, provider: provider.key, name: shortTitle, room: nil, session: nil,
                                        weekly: nil, modelName: nil, modelLeft: nil, tone: .unknown, chip: "no reading",
                                        resetSeconds: nil, sessionResetSeconds: nil, best: false,
                                        spoken: title + ", " + (provider.emptyMessage ?? "No reading")))
            }
        }
        if !others.isEmpty { sections.append(DirectionSection(id: "other", title: "Other plans", seats: others)) }
        return DirectionPresentation(best: bestSeat, modelBest: picks, sections: sections)
    }
}
