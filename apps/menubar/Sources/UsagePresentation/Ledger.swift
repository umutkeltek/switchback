import Foundation

/// How one account fills the ledger: the 5-hour and weekly columns, plus honest lines for
/// windows that are neither (tool quotas, monthly or daily windows).
public struct LedgerColumns: Sendable {
    public let session: QuotaRow?
    public let weekly: QuotaRow?
    /// Model the column windows belong to, for example "General model"; nil for account-wide windows.
    public let note: String?
    /// One label per window outside the two columns, for example "Tools · Mcp Monthly: 100% left · ↻ 20d 0h".
    public let lines: [String]
}

extension Presenter {
    /// Most extra lines a row shows; the rest stay in the expanded row.
    public static let ledgerLineLimit = 2

    /// Readable provider names for providers that arrive as raw lowercase keys.
    public static func displayTitle(key: String, title: String) -> String {
        switch key {
        case "deepseek": return "DeepSeek"
        case "openrouter": return "OpenRouter"
        case "neuralwatt": return "Neuralwatt"
        case "qwencloud": return "Qwen Cloud"
        default:
            guard title == key, let first = title.first else { return title }
            return String(first).uppercased() + title.dropFirst()
        }
    }

    /// A short name for the ledger's name column. The full slot stays in tooltips, VoiceOver and details.
    /// Uses the label when the account has a real one, dropping a leading or trailing word that repeats the
    /// provider. A label that only repeats the slot's `--marker` (for example `configured`) is not a real label.
    /// Without a label: drop a trailing `--configured`, then a provider prefix, then make it readable.
    public static func shortName(label: String?, slot: String, provider: String, title: String) -> String {
        let provider = canonical(provider)
        let names = Set([provider, squash(title), squash(provider)].filter { !$0.isEmpty })
        func dropProviderWords(_ words: [String]) -> [String] {
            var words = words
            if words.count > 1, let first = words.first, names.contains(squash(first)) { words.removeFirst() }
            if words.count > 1, let last = words.last, names.contains(squash(last)) { words.removeLast() }
            return words
        }
        let slot = clean(slot).trimmingCharacters(in: .whitespacesAndNewlines)
        let cleanLabel = clean(label ?? "").trimmingCharacters(in: .whitespacesAndNewlines)
        let marker = slot.range(of: "--", options: .backwards).map { String(slot[$0.upperBound...]) }
        let realLabel = !cleanLabel.isEmpty && cleanLabel.lowercased() != slot.lowercased() &&
            cleanLabel.lowercased() != marker?.lowercased()
        if realLabel {
            return dropProviderWords(cleanLabel.split(separator: " ").map(String.init)).joined(separator: " ")
        }
        if slot.hasPrefix("seat"), Int(slot.dropFirst(4)) != nil { return "Seat " + slot.dropFirst(4) }
        var base = slot
        if base.lowercased().hasSuffix("--configured") { base = String(base.dropLast("--configured".count)) }
        var words = base.split(whereSeparator: { $0 == "-" || $0 == "_" || $0 == " " }).map(String.init)
        if let first = words.first, names.contains(squash(first)) { words.removeFirst() }
        guard let head = words.first else { return displayTitle(key: provider, title: title) }
        words[0] = String(head.prefix(1)).uppercased() + head.dropFirst()
        return words.joined(separator: " ")
    }

    /// Lowercase letters and digits only, so "Z.ai", "zai" and "z-ai" compare equal.
    static func squash(_ text: String) -> String {
        String(text.lowercased().unicodeScalars.filter { CharacterSet.alphanumerics.contains($0) }.map(Character.init))
    }

    static func ledgerLine(_ row: QuotaRow) -> String {
        let name = row.qualifier.map { String($0.prefix(1)).uppercased() + $0.dropFirst() + " · " + row.title } ?? row.title
        let value = row.state == .unavailable ? "unknown" : row.valueText + " left"
        return name + ": " + value + (row.resetIn.map { " · ↻ " + $0 } ?? "")
    }
}

extension AccountPanel {
    /// The 5-hour column takes the first 5-hour window, the weekly column the first weekly window.
    /// Any other primary window, and any tool quota, gets its own labelled line instead of a column.
    public var ledger: LedgerColumns {
        let session = primary.first { $0.lane == .session }
        let weekly = primary.first { $0.lane == .weekly }
        let used = Set([session?.id, weekly?.id].compactMap { $0 })
        let outside = primary.filter { !used.contains($0.id) } + extras.filter { !$0.generation }
        var lines = outside.prefix(Presenter.ledgerLineLimit).map(Presenter.ledgerLine)
        if outside.count > Presenter.ledgerLineLimit {
            lines.append("+\(outside.count - Presenter.ledgerLineLimit) more in details")
        }
        let models = Array(Set([session, weekly].compactMap { $0?.qualifier })).sorted()
        let note = models.isEmpty ? nil : models.map { String($0.prefix(1)).uppercased() + $0.dropFirst() }
            .joined(separator: " · ") + (models.count == 1 ? " model" : " models")
        return LedgerColumns(session: session, weekly: weekly, note: note, lines: lines)
    }
}
