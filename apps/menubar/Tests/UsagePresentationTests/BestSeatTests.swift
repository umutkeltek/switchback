import Foundation
import Testing
@testable import UsagePresentation

func seat(_ slot: String, fiveHour: Double, weekly: Double, sample: TimeInterval = 0,
          login: Bool = false, error: String? = nil) -> [String: Any] {
    account(slot, windows: [window("five_hour", used: 100 - fiveHour, remaining: fiveHour, sample: sample),
                            window("seven_day", used: 100 - weekly, remaining: weekly, sample: sample, reset: 86_400)],
            sample: sample, error: error, login: login)
}

@Test func bestSeatPicksLargestSmallerWindowAcrossClaudeAccounts() throws {
    let data = try snapshot([seat("max", fiveHour: 50, weekly: 90), seat("seat1", fiveHour: 80, weekly: 60),
                             seat("seat2", fiveHour: 71, weekly: 76), seat("seat3", fiveHour: 100, weekly: 32),
                             account("work", provider: "codex", windows: [window(used: 1, remaining: 99)])])
    let advice = try #require(Presenter.bestSeat(data, now: instant))
    #expect(advice.key == "claude:seat2")
    #expect(advice.roomPercent == 71)
    #expect(advice.text == "Most room now: seat2 · 71% left")
    #expect(advice.detail.contains("5-hour (71% left)"))
    #expect(advice.detail.contains("weekly (76% left)"))
    #expect(advice.tooltipLine == "Claude · Most room now: seat2 · 71% left")
    let menu = Presenter.make(data, now: instant)
    #expect(menu.groups.first { $0.provider == "claude" }?.headline == advice.text)
    #expect(menu.groups.first { $0.provider == "codex" }?.headline == nil)
    let text = Presenter.text(menu).components(separatedBy: "\n")
    let claude = try #require(text.firstIndex(of: "Claude"))
    #expect(text[claude + 1] == advice.text)
    let panel = Presenter.panel(data, now: instant)
    #expect(panel.providers.first { $0.key == "claude" }?.advice == advice)
    #expect(panel.filtered(provider: "claude", pinnedOnly: false).first?.advice == advice)
}
@Test func bestSeatTieGoesToLowerSlotName() throws {
    let data = try snapshot([seat("seat3", fiveHour: 70, weekly: 90), seat("seat1", fiveHour: 90, weekly: 70),
                             seat("seat2", fiveHour: 40, weekly: 95)])
    #expect(Presenter.bestSeat(data, now: instant)?.key == "claude:seat1")
    let withMax = try snapshot([seat("seat1", fiveHour: 70, weekly: 70), seat("max", fiveHour: 70, weekly: 80)])
    #expect(Presenter.bestSeat(withMax, now: instant)?.key == "claude:max")
}
@Test func bestSeatIgnoresStaleReadings() throws {
    var staleState = seat("seat1", fiveHour: 99, weekly: 99)
    staleState["state"] = "stale"
    var staleWindow = seat("seat3", fiveHour: 99, weekly: 99)
    staleWindow["windows"] = [window("five_hour", used: 1, remaining: 99, state: "stale"),
                              window("seven_day", used: 1, remaining: 99)]
    let data = try snapshot([seat("max", fiveHour: 99, weekly: 99, sample: -901), staleState, staleWindow,
                             seat("seat2", fiveHour: 40, weekly: 60), seat("seat4", fiveHour: 99, weekly: 99, error: "authentication_required")])
    #expect(Presenter.bestSeat(data, now: instant)?.key == "claude:seat2")
    var disabled = seat("seat4", fiveHour: 99, weekly: 99)
    disabled["enabled"] = false
    var estimated = seat("seat3", fiveHour: 99, weekly: 99)
    estimated["estimated"] = true
    #expect(Presenter.bestSeat(try snapshot([seat("seat2", fiveHour: 40, weekly: 60), disabled, estimated]), now: instant)?.key == "claude:seat2")
}
@Test func bestSeatIgnoresLoginRequiredAccounts() throws {
    let data = try snapshot([seat("seat1", fiveHour: 100, weekly: 100, login: true), seat("seat2", fiveHour: 30, weekly: 50)])
    let advice = try #require(Presenter.bestSeat(data, now: instant))
    #expect(advice.key == "claude:seat2")
    #expect(advice.text == "Most room now: seat2 · 30% left")
}
@Test func bestSeatSaysSoWhenNothingQualifies() throws {
    // A 5-hour window that is present with numbers but untrusted takes the seat out.
    let staleFiveHour = account("max", windows: [window("five_hour", used: 10, remaining: 90, sample: -901),
                                                 window("seven_day", used: 12, remaining: 88)])
    let data = try snapshot([staleFiveHour, seat("seat1", fiveHour: 90, weekly: 90, sample: -901),
                             seat("seat2", fiveHour: 90, weekly: 90, login: true)])
    let advice = try #require(Presenter.bestSeat(data, now: instant))
    #expect(advice.key == nil)
    #expect(advice.roomPercent == nil)
    #expect(advice.text == "Most room now: no fresh Claude reading")
    #expect(Presenter.make(data, now: instant).groups.first { $0.provider == "claude" }?.headline == advice.text)
    // A failed client refresh hides nothing: the readings are judged by their own sample times.
    let failed = Presenter.bestSeat(try snapshot([seat("seat2", fiveHour: 90, weekly: 90)]), now: instant, failure: "Could not read")
    #expect(failed?.key == "claude:seat2")
    let noClaude = try snapshot([account("work", provider: "codex")])
    #expect(Presenter.bestSeat(noClaude, now: instant) == nil)
    #expect(Presenter.make(noClaude, now: instant).groups.allSatisfy { $0.headline == nil })
}
