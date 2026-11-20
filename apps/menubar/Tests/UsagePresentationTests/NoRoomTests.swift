import Foundation
import Testing
@testable import UsagePresentation

@Test func useNowCardSaysNoSeatHasRoomAndNamesTheFirstBack() throws {
    let data = try snapshot([gaugeSeat("seat2", fiveHour: 0, weekly: 50, fiveHourReset: 3 * 3_600),
                             gaugeSeat("seat3", fiveHour: 100, weekly: 0, weeklyReset: 8 * 3_600)])
    let advice = try #require(Presenter.bestSeat(data, now: instant))
    #expect(advice.noSeatHasRoom)
    #expect(advice.backKey == "claude:seat2")
    #expect(advice.backInSeconds == 10_800.0)
    #expect(advice.models.isEmpty)
    let directions = Presenter.directions(Presenter.panel(data, now: instant))
    #expect(directions.best == nil)
    let card = try #require(directions.noRoom)
    #expect(card.title == "No seat has room")
    #expect(card.detail == "Seat 2 back in 3h 0m")
    #expect(card.backIn == "3h 0m")
    #expect(card.seat?.id == "claude:seat2")
    #expect(card.spoken == "No seat has room. Seat 2 is back in 3 hours")
    // The card keeps its place in the panel's height.
    let withCard = GaugeLayout.height(directions, searching: false, detail: nil, showBest: true)
    #expect(withCard == GaugeLayout.height(directions, searching: false, detail: nil, showBest: false) + GaugeLayout.hero + GaugeLayout.sectionGap)
}

@Test func useNowCardWithoutAnyResetTimeSaysTheReturnIsUnknown() throws {
    var out = window("seven_day", used: 100, remaining: 0)
    out["resets_at"] = NSNull()
    let data = try snapshot([account("seat1", windows: [window("five_hour", used: 10, remaining: 90), out])])
    let advice = try #require(Presenter.bestSeat(data, now: instant))
    #expect(advice.text == "Most room now: no seat has room · return time unknown")
    #expect(advice.backKey == nil)
    let card = try #require(Presenter.directions(Presenter.panel(data, now: instant)).noRoom)
    #expect(card.detail == "Return time unknown")
    #expect(card.seat == nil)
}

@Test func fableSideNeverRecommendsASeatWithNoFableRoom() throws {
    // Both seats have account-wide room, but neither has Fable room left.
    let data = try snapshot([gaugeSeat("seat1", fiveHour: 80, weekly: 70, fable: 0, weeklyReset: 86_400 + 60),
                             gaugeSeat("seat2", fiveHour: 60, weekly: 60, fable: 2)])
    let advice = try #require(Presenter.bestSeat(data, now: instant))
    #expect(advice.key == "claude:seat1")
    #expect(advice.models == [ModelSeat(model: "Fable", key: nil, slot: nil, roomPercent: nil,
                                        text: "Fable: no seat has room · seat1 back in 1d 0h", noSeatHasRoom: true,
                                        backKey: "claude:seat1", backSlot: "seat1", backInSeconds: 86_400)])
    let pick = try #require(Presenter.directions(Presenter.panel(data, now: instant)).modelBest.first)
    #expect(pick.seat == nil)
    #expect(pick.backText == "back 1d 0h")
    // With Fable room on one seat, that seat is the Fable pick as before.
    let some = try snapshot([gaugeSeat("seat1", fiveHour: 80, weekly: 70, fable: 0),
                             gaugeSeat("seat2", fiveHour: 60, weekly: 60, fable: 40)])
    let models = try #require(Presenter.bestSeat(some, now: instant)).models
    #expect(models.map(\.key) == ["claude:seat2"])
    #expect(models.first?.noSeatHasRoom == false)
}

@Test func failedDirectCheckShowsInDetailsNotInPlaceOfTheNumber() throws {
    let data = try snapshot([gaugeSeat("seat1", fiveHour: 40, weekly: 70).merging(["refresh_error": "native_usage_unavailable"]) { $1 },
                             gaugeSeat("seat2", fiveHour: 40, weekly: 70).merging(["refresh_error": "identity_mismatch"]) { $1 }])
    let panel = Presenter.panel(data, now: instant)
    let failed = try #require(panel.providers.first?.accounts.first { $0.key == "claude:seat1" })
    #expect(failed.directCheckFailed)
    #expect(failed.notice == nil)
    #expect(Presenter.seatDetail(failed).issues == ["Last direct check failed. These numbers are from the last fresh reading."])
    let glyph = try #require(Presenter.directions(panel).seats.first { $0.id == "claude:seat1" })
    #expect(glyph.roomText == "40")
    #expect(glyph.detail.contains("Last direct check failed"))
    let changed = try #require(panel.providers.first?.accounts.first { $0.key == "claude:seat2" })
    #expect(!changed.directCheckFailed)
    #expect(changed.notice == "Account changed")
    #expect(Presenter.seatDetail(changed).issues.first?.hasPrefix("Account changed") == true)
}

@Test func footerKeepsTheAgeOfShownDataWhileRefreshing() {
    #expect(Presenter.footerStatus(checked: "Checked 5m ago", busy: true) == "Checked 5m ago · refreshing")
    #expect(Presenter.footerStatus(checked: "Checked 5m ago", busy: false) == "Checked 5m ago")
    #expect(Presenter.footerStatus(checked: nil, busy: true) == "Refreshing…")
    #expect(Presenter.footerStatus(checked: nil, busy: false) == "Waiting for a reading")
}
