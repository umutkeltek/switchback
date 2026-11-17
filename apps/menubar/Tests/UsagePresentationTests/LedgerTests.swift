import Foundation
import Testing
@testable import UsagePresentation

private func scoped(_ name: String, used: Double, minutes: Double?, reset: TimeInterval,
                    model: String? = nil, scope: String? = nil, state: String = "fresh") -> [String: Any] {
    var row = window(name, used: used, remaining: 100 - used, reset: reset, state: state)
    row["window_minutes"] = minutes ?? NSNull()
    if let model { row["model"] = model }
    if let scope { row["scope"] = scope }
    return row
}
private func ledgerRow(_ windows: [[String: Any]], provider: String = "claude") throws -> AccountPanel {
    try #require(Presenter.panel(try snapshot([account("1", provider: provider, windows: windows)]), now: instant)
        .providers.first?.accounts.first)
}

@Test func shortNamesDropMarkersAndRepeatedProviders() {
    #expect(Presenter.shortName(label: "configured", slot: "zai-coding--configured", provider: "zai", title: "Z.ai") == "Coding")
    #expect(Presenter.shortName(label: "configured", slot: "minimax-coding--configured", provider: "minimax", title: "MiniMax") == "Coding")
    #expect(Presenter.shortName(label: nil, slot: "codex-personal", provider: "codex", title: "Codex") == "Personal")
    #expect(Presenter.shortName(label: "configured", slot: "deepseek--configured", provider: "deepseek", title: "deepseek") == "DeepSeek")
    #expect(Presenter.shortName(label: "configured", slot: "neuralwatt-coding--configured", provider: "neuralwatt", title: "neuralwatt") == "Coding")
    // A real label wins; a word that repeats the provider is dropped, a lone provider word is kept.
    #expect(Presenter.shortName(label: "Work Codex", slot: "codex-personal", provider: "codex", title: "Codex") == "Work")
    #expect(Presenter.shortName(label: "Codex", slot: "codex-personal", provider: "codex", title: "Codex") == "Codex")
    #expect(Presenter.shortName(label: "Team Alpha", slot: "zai-coding--configured", provider: "zai", title: "Z.ai") == "Team Alpha")
    #expect(Presenter.shortName(label: "seat2", slot: "seat2", provider: "claude", title: "Claude") == "Seat 2")
    #expect(Presenter.shortName(label: "Max", slot: "max", provider: "claude", title: "Claude") == "Max")
    #expect(Presenter.shortName(label: nil, slot: "max", provider: "claude", title: "Claude") == "Max")
    #expect(Presenter.shortName(label: nil, slot: "zai_work", provider: "z.ai", title: "Z.ai") == "Work")
}
@Test func panelCarriesShortNamesAndKeepsTheFullSlot() throws {
    func named(_ slot: String, _ provider: String, label: String?) -> [String: Any] {
        var row = account(slot, provider: provider, windows: [window()])
        row["label"] = label ?? NSNull()
        return row
    }
    let data = try snapshot([named("zai-coding--configured", "zai", label: "configured"),
                             named("minimax-coding--configured", "minimax", label: "configured"),
                             named("codex-personal", "codex", label: "Work Codex"),
                             named("deepseek--configured", "deepseek", label: "configured"),
                             named("neuralwatt-coding--configured", "neuralwatt", label: "configured")])
    let rows = Presenter.panel(data, now: instant).providers.flatMap(\.accounts)
    let names = Dictionary(uniqueKeysWithValues: rows.map { ($0.slot, $0.shortName) })
    #expect(names["zai-coding--configured"] == "Coding")
    #expect(names["minimax-coding--configured"] == "Coding")
    #expect(names["codex-personal"] == "Work")
    #expect(names["deepseek--configured"] == "DeepSeek")
    #expect(names["neuralwatt-coding--configured"] == "Coding")
    #expect(rows.first { $0.slot == "codex-personal" }?.label == "Work Codex")
    // Two accounts of one provider that would share a short name show their slots instead.
    let twins = Presenter.panel(try snapshot([named("zai-coding--configured", "zai", label: "configured"),
                                              named("coding", "zai", label: nil)]), now: instant).providers[0].accounts
    #expect(twins.map(\.shortName) == ["zai-coding--configured", "coding"])
}
@Test func ledgerPutsOnlyFiveHourAndWeeklyWindowsInTheColumns() throws {
    let claude = try ledgerRow([window("five_hour", used: 62, remaining: 38), window("seven_day", used: 27, remaining: 73)]).ledger
    #expect(claude.session?.valueText == "38%")
    #expect(claude.weekly?.valueText == "73%")
    #expect(claude.session?.accessibilityText.contains("38% remaining") == true)
    #expect(claude.note == nil)
    #expect(claude.lines.isEmpty)
    let minimax = try ledgerRow([scoped("general/interval", used: 3, minutes: 300, reset: 7_200, model: "general", scope: "generation"),
                                 scoped("general/weekly", used: 21, minutes: 10080, reset: 432_000, model: "general", scope: "generation"),
                                 scoped("video/interval", used: 0, minutes: 1440, reset: 32_400, model: "video", scope: "generation")],
                                provider: "minimax").ledger
    #expect(minimax.session?.valueText == "97%")
    #expect(minimax.weekly?.valueText == "79%")
    #expect(minimax.note == "General model")
    #expect(minimax.lines.isEmpty)
    let zai = try ledgerRow([scoped("mcp_monthly", used: 0, minutes: nil, reset: 1_728_000, scope: "tools"),
                             scoped("tokens_300m", used: 1, minutes: 300, reset: 10_800, scope: "generation")], provider: "zai").ledger
    #expect(zai.session?.valueText == "99%")
    #expect(zai.weekly == nil)
    #expect(zai.lines == ["Tools · Mcp Monthly: 100% left · ↻ 20d 0h"])
    let monthly = try ledgerRow([scoped("monthly", used: 10, minutes: 43200, reset: 86_400)], provider: "opencodego").ledger
    #expect(monthly.session == nil)
    #expect(monthly.weekly == nil)
    #expect(monthly.lines == ["Monthly: 90% left · ↻ 1d 0h"])
}
@Test func ledgerLinesStayShortAndSayWhenUnknown() throws {
    let tools = (1...4).map { scoped("tool_\($0)", used: 50, minutes: nil, reset: 3_600, scope: "tools") }
    let row = try ledgerRow([window("five_hour")] + tools).ledger
    #expect(row.lines.count == 3)
    #expect(row.lines.last == "+2 more in details")
    let stale = try ledgerRow([window("five_hour"), scoped("mcp", used: 10, minutes: nil, reset: 3_600, scope: "tools", state: "stale")]).ledger
    #expect(stale.lines == ["Tools · Mcp: unknown · ↻ 1h 0m"])
}
