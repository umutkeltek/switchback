import Foundation
import Testing
@testable import UsagePresentation

// Seats with good readings must show them. These tests use only the presentation API that
// existed before the fix, so each one can run against the old rules and fail there.

@Test func aFailedDirectCheckDoesNotHideFreshWindows() throws {
    let data = try snapshot([seat("seat1", fiveHour: 40, weekly: 70, error: "native_usage_unavailable"),
                             seat("seat2", fiveHour: 30, weekly: 50)])
    #expect(Presenter.bestSeat(data, now: instant)?.key == "claude:seat1")
    let panel = Presenter.panel(data, now: instant)
    let directions = Presenter.directions(panel)
    let glyph = try #require(directions.seats.first { $0.id == "claude:seat1" })
    #expect(glyph.roomText == "40")
    #expect(glyph.tone == .good)
    #expect(glyph.chip == "↻ 1h 0m")
    #expect(directions.best?.id == "claude:seat1")
    let row = try #require(panel.providers.first?.accounts.first { $0.key == "claude:seat1" })
    #expect(row.summary == "40%")
    #expect(row.primary.allSatisfy { $0.state == .measured })
    #expect(Presenter.make(data, now: instant).groups[0].accounts.first { $0.key == "claude:seat1" }?.summary == "40%")
}

@Test func signInAndIdentityProblemsStillHideTheNumber() throws {
    var changed = seat("seat3", fiveHour: 90, weekly: 90)
    changed["identity_state"] = "changed"
    var matched = seat("seat4", fiveHour: 20, weekly: 30)
    matched["identity_state"] = "matched"
    let data = try snapshot([seat("seat1", fiveHour: 90, weekly: 90, error: "authentication_required"),
                             seat("seat2", fiveHour: 90, weekly: 90, error: "identity_mismatch"), changed, matched])
    let seats = Presenter.directions(Presenter.panel(data, now: instant)).seats
    for slot in ["seat1", "seat2", "seat3"] {
        let glyph = try #require(seats.first { $0.id == "claude:" + slot })
        #expect(glyph.roomText == "—", "\(slot)")
        #expect(glyph.room == nil, "\(slot)")
    }
    #expect(seats.first { $0.id == "claude:seat4" }?.roomText == "20")
    #expect(Presenter.bestSeat(data, now: instant)?.key == "claude:seat4")
}

@Test func idleFiveHourWindowWithoutResetGivesTheWeeklyRoom() throws {
    // Claude reports a 5-hour window that has not started as 0% used with no reset time.
    var idle = window("five_hour", used: 0, remaining: 100)
    idle["resets_at"] = NSNull()
    var week = window("seven_day", used: 30, remaining: 70, reset: 86_400)
    week["window_minutes"] = 10_080
    let data = try snapshot([account("seat1", windows: [idle, week])])
    let advice = try #require(Presenter.bestSeat(data, now: instant))
    #expect(advice.key == "claude:seat1")
    #expect(advice.roomPercent == 70)
    let glyph = try #require(Presenter.directions(Presenter.panel(data, now: instant)).seats.first)
    #expect(glyph.roomText == "70")
    #expect(!glyph.partial)
    #expect(glyph.chip == "↻ 1d 0h")
}

@Test func bestSeatNeverRecommendsASeatWithNoRoom() throws {
    // seat3 is used up for the week; seat1's only problem is a failed direct check.
    let usedUp = account("seat3", windows: [window("five_hour", used: 0, remaining: 100, reset: 3_600),
                                             window("seven_day", used: 100, remaining: 0, reset: 8 * 3_600)])
    let data = try snapshot([usedUp, seat("seat1", fiveHour: 40, weekly: 70, error: "native_usage_unavailable")])
    #expect(Presenter.bestSeat(data, now: instant)?.key == "claude:seat1")
    // The only seat with a reading is used up: it is not recommended.
    let alone = try snapshot([usedUp, seat("seat1", fiveHour: 90, weekly: 90, login: true)])
    let advice = try #require(Presenter.bestSeat(alone, now: instant))
    #expect(advice.key == nil)
    #expect(advice.roomPercent == nil)
    #expect(advice.text == "Most room now: no seat has room · seat3 back in 8h 0m")
    #expect(Presenter.directions(Presenter.panel(alone, now: instant)).best == nil)
}

@Test func everySeatAtItsLimitSaysNoneHasRoomAndTheSoonestReturn() throws {
    let data = try snapshot([
        account("seat2", windows: [window("five_hour", used: 100, remaining: 0, reset: 3 * 3_600),
                                   window("seven_day", used: 50, remaining: 50, reset: 86_400)]),
        account("seat3", windows: [window("five_hour", used: 0, remaining: 100, reset: 3_600),
                                   window("seven_day", used: 100, remaining: 0, reset: 8 * 3_600)]),
        // Both windows out: back only when the later one resets.
        account("seat4", windows: [window("five_hour", used: 97, remaining: 3, reset: 2 * 3_600),
                                   window("seven_day", used: 100, remaining: 0, reset: 5 * 3_600)]),
    ])
    let advice = try #require(Presenter.bestSeat(data, now: instant))
    #expect(advice.key == nil)
    #expect(advice.text == "Most room now: no seat has room · seat2 back in 3h 0m")
    #expect(advice.detail.contains("seat2 is back first, in 3h 0m"))
    #expect(Presenter.make(data, now: instant).groups[0].headline == advice.text)
    #expect(Presenter.directions(Presenter.panel(data, now: instant)).best == nil)
}

@Test func aSeatWithoutAUsableReadingSaysNoReading() throws {
    let data = try snapshot([seat("seat1", fiveHour: 80, weekly: 80, sample: -600, error: "authentication_required"),
                             seat("seat2", fiveHour: 80, weekly: 80, sample: -600, login: true),
                             seat("seat3", fiveHour: 50, weekly: 60)])
    let seats = Presenter.directions(Presenter.panel(data, now: instant)).seats
    for slot in ["seat1", "seat2"] {
        let glyph = try #require(seats.first { $0.id == "claude:" + slot })
        #expect(glyph.chip == "no reading", "\(slot)")
        #expect(glyph.roomText == "—", "\(slot)")
        // The age of the last reading moves to the tooltip.
        #expect(glyph.detail.contains("10m old"), "\(slot)")
    }
}
