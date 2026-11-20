import Foundation
import Testing
@testable import UsagePresentation

@Test func directionGlyphsKeepTheLedgerTrustRules() throws {
    let data = try snapshot([
        seat("alpha", fiveHour: 80, weekly: 66),
        seat("bravo", fiveHour: 50, weekly: 0),
        seat("charlie", fiveHour: 90, weekly: 90, sample: -1_200),
        account("solo", provider: "codex", windows: [window("primary", used: 40, remaining: 60)]),
    ], coverage: [["provider": "opencodego", "state": "unavailable", "reason": "source_error"]])
    let directions = Presenter.directions(Presenter.panel(data, now: instant))
    let claude = try #require(directions.sections.first { $0.id == "claude" })
    let seats = Dictionary(uniqueKeysWithValues: claude.seats.map { ($0.id, $0) })
    #expect(directions.best?.id == "claude:alpha")
    #expect(seats["claude:alpha"]?.best == true)
    #expect(seats["claude:alpha"]?.roomText == "66")
    #expect(seats["claude:alpha"]?.tone == .good)
    // Used up: zero with its own tone and symbol, and no 5-hour reset promise.
    #expect(seats["claude:bravo"]?.tone == .out)
    #expect(seats["claude:bravo"]?.sessionResetSeconds == nil)
    // Older than the trust window: no number, an age chip, never a coloured room.
    let stale = try #require(seats["claude:charlie"])
    #expect(stale.room == nil)
    #expect(stale.tone == .stale)
    #expect(stale.roomText == "—")
    #expect(stale.chip?.hasSuffix(" old") == true)
    // Single accounts and providers without a reading share one group, named by provider.
    let other = try #require(directions.sections.first { $0.id == "other" })
    #expect(other.seats.map(\.name) == ["Codex", "Go"])
    #expect(other.seats.last?.tone == .unknown)
    // Colour never carries state alone.
    #expect(Set([SeatTone.good, .low, .critical, .out, .stale, .unknown, .balance].map(\.symbol)).count == 7)
}

@Test func styleFlagSelectsTheVisualLooks() {
    #expect(PanelStyle(preference: nil) == .gauge)
    #expect(PanelStyle(preference: "unknown") == .gauge)
    #expect(PanelStyle(preference: "a") == .ledger)
    #expect(PanelStyle(preference: "rings").isDirection)
    #expect(PanelStyle(preference: "gauge").isDirection)
    #expect(PanelStyle(preference: "strip").isDirection)
    #expect(!PanelStyle(preference: "cards").isDirection)
}
