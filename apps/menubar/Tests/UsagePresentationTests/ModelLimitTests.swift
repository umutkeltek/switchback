import Foundation
import Testing
@testable import UsagePresentation

/// A Claude seat as the collector publishes it, with a model's own weekly limit beside the two windows.
private func modelSeat(_ slot: String, fiveHour: Double, weekly: Double, fable: Double?,
                       fableSample: TimeInterval = 0, sample: TimeInterval = 0) -> [String: Any] {
    var windows = [window("five_hour", used: 100 - fiveHour, remaining: fiveHour, sample: sample),
                   window("seven_day", used: 100 - weekly, remaining: weekly, sample: sample, reset: 86_400)]
    if let fable {
        var limit = window("seven_day_fable", used: 100 - fable, remaining: fable, sample: fableSample, reset: 86_400)
        limit["window_minutes"] = 10080
        limit["model"] = "Fable"
        limit["limit_kind"] = "weekly_scoped"
        windows.append(limit)
    }
    return account(slot, windows: windows, sample: sample)
}

@Test func mostRoomCountsTheModelLimitForThatModelOnly() throws {
    let data = try snapshot([modelSeat("seat1", fiveHour: 90, weekly: 70, fable: 27),
                             modelSeat("seat2", fiveHour: 65, weekly: 66, fable: 64),
                             modelSeat("seat3", fiveHour: 10, weekly: 95, fable: 95)])
    let advice = try #require(Presenter.bestSeat(data, now: instant))
    // Any model: seat1 has 70% left. For Fable it has only 27%, so seat2 (64%) is the better Fable seat.
    #expect(advice.key == "claude:seat1")
    #expect(advice.text == "Most room now: seat1 · 70% left")
    #expect(advice.models == [ModelSeat(model: "Fable", key: "claude:seat2", slot: "seat2", roomPercent: 64,
                                        text: "Fable: seat2 · 64% left")])
    #expect(advice.tooltipLine == "Claude · Most room now: seat1 · 70% left\nClaude · Fable: seat2 · 64% left")
    let claude = try #require(Presenter.panel(data, now: instant).providers.first { $0.key == "claude" })
    #expect(claude.advice?.models.count == 1)
}

@Test func modelRoomIsOmittedWhenItAgreesAndHonestWhenItCannotBeTrusted() throws {
    let agree = try snapshot([modelSeat("seat1", fiveHour: 80, weekly: 70, fable: 90),
                              modelSeat("seat2", fiveHour: 50, weekly: 50, fable: 40)])
    #expect(try #require(Presenter.bestSeat(agree, now: instant)).models.isEmpty)
    // A stale Fable reading never counts, even when the 5-hour and weekly windows are fresh.
    let stale = try snapshot([modelSeat("seat1", fiveHour: 80, weekly: 70, fable: 60, fableSample: -1_000)])
    let advice = try #require(Presenter.bestSeat(stale, now: instant))
    #expect(advice.key == "claude:seat1")
    #expect(advice.models == [ModelSeat(model: "Fable", key: nil, slot: nil, roomPercent: nil, text: "Fable: no fresh reading")])
    // An account with no limit of its own for the model keeps its any-model room for it.
    let mixed = try snapshot([modelSeat("seat1", fiveHour: 80, weekly: 70, fable: 10),
                              modelSeat("seat2", fiveHour: 60, weekly: 60, fable: nil)])
    #expect(try #require(Presenter.bestSeat(mixed, now: instant)).models.map(\.text) == ["Fable: seat2 · 60% left"])
}

@Test func ledgerShowsTheModelLimitAndNamesWhichLimitIsUsedUp() throws {
    let data = try snapshot([modelSeat("seat1", fiveHour: 80, weekly: 11, fable: 0)])
    let row = try #require(Presenter.panel(data, now: instant).providers.first?.accounts.first)
    #expect(row.ledger.session?.valueText == "80%")
    #expect(row.ledger.weekly?.valueText == "11%")
    #expect(row.ledger.note == nil)
    #expect(row.ledger.lines == ["Fable · Weekly: 0% left · ↻ 1d 0h"])
    #expect(row.notice == "Fable at limit")
    let fable = try #require(row.extras.first { $0.qualifier == "Fable" })
    #expect(fable.title == "Weekly")
    #expect(fable.lane == .weekly)
    #expect(fable.accessibilityText.contains("Fable"))
    // A used-up account-wide window keeps the plain notice.
    let out = try snapshot([modelSeat("seat2", fiveHour: 0, weekly: 50, fable: 0)])
    #expect(Presenter.panel(out, now: instant).providers.first?.accounts.first?.notice == "At limit")
}

@Test func readingAgeShowsCompactlyOnceItIsFiveMinutesOld() throws {
    #expect(Presenter.compactAge(stamp(-240), now: instant) == nil)
    #expect(Presenter.compactAge(stamp(-720), now: instant) == "12m")
    #expect(Presenter.compactAge(nil, now: instant) == nil)
    let data = try snapshot([modelSeat("seat1", fiveHour: 80, weekly: 70, fable: 60, fableSample: -720, sample: -720)])
    let row = try #require(Presenter.panel(data, now: instant).providers.first?.accounts.first)
    #expect(row.ageText == "12m")
    #expect(row.glance.status == .ready)
    // Past the 15-minute trust window the reading is not current, whatever its age text says.
    let old = try snapshot([modelSeat("seat1", fiveHour: 80, weekly: 70, fable: 60, fableSample: -960, sample: -960)])
    let stale = try #require(Presenter.panel(old, now: instant).providers.first?.accounts.first)
    #expect(stale.glance.status == .unavailable)
    #expect(stale.ledger.session?.state == .unavailable)
}

@Test func providerWithoutAUsageSourceIsNotUnknownCapacity() throws {
    let coverage: [[String: Any]] = [["provider": "minimax", "state": "unknown", "reason": "no_usage_collector"],
                                     ["provider": "fixturegateway", "state": "unknown", "reason": "no_usage_collector"],
                                     ["provider": "zai", "state": "unknown", "reason": "no_saved_usage"]]
    let data = try snapshot([account("seat1")], coverage: coverage)
    let menu = Presenter.make(data, now: instant, pins: ["claude:seat1"])
    #expect(!menu.compactTitle.contains("MM"))
    #expect(menu.compactTitle.contains("ZAI ?"))
    let keys = Presenter.panel(data, now: instant).providers.map(\.key)
    #expect(!keys.contains("minimax"))
    #expect(!keys.contains("fixturegateway"))
    #expect(keys.contains("zai"))
    #expect(Presenter.issue("no_usage_collector") == "No usage source")
}
