import Foundation
import Testing
@testable import UsagePresentation

// One test (or more) per re-review finding. Each fails on the rules before its fix.

@MainActor private final class Events {
    var list: [String] = []
}

// MARK: 1. The saved reading is drawn before the slow refresh ends

@MainActor @Test func rereviewEachRefreshRequestIsDrawnAsItEnds() async {
    let slot = RefreshSlot()
    let events = Events()
    let (ended, ending) = AsyncStream<Void>.makeStream()
    let started = slot.start([RefreshRequest.saved, .scheduled],
                             run: { request in events.list.append("read \(request)") },
                             afterEach: { events.list.append("draw") },
                             done: { events.list.append("done"); ending.finish() })
    #expect(started)
    for await _ in ended {}
    // The saved read is drawn before the scheduled refresh starts, not only when the whole run ends.
    #expect(events.list == ["read saved", "draw", "read scheduled", "draw", "done"])
    #expect(!slot.busy)
}

@MainActor @Test func rereviewARunningRefreshStartsNothingElse() async {
    let slot = RefreshSlot()
    let (ended, ending) = AsyncStream<Void>.makeStream()
    slot.start([RefreshRequest.saved], run: { _ in }, afterEach: {}, done: { ending.finish() })
    let second = slot.start([RefreshRequest.forced], run: { _ in }, afterEach: {})
    #expect(!second)
    for await _ in ended {}
}

// MARK: 2. The footer judges the readings, not the command

@Test func rereviewReadingsAgeFollowsTheNewestClaudeReading() throws {
    // Claude is 25 minutes old; another provider was read just now.
    let data = try snapshot([seat("seat1", fiveHour: 40, weekly: 70, sample: -1_500),
                             account("c", provider: "codex", windows: [window(used: 10, remaining: 90)])])
    let panel = Presenter.panel(data, now: instant)
    #expect(panel.readingsAgeSeconds == 1_500)
    let footer = Presenter.footer(checked: "Checked just now", readingsAgeSeconds: panel.readingsAgeSeconds, busy: false)
    #expect(footer.text == "Readings 25m old")
    #expect(footer.old)
    #expect(Presenter.footer(checked: "Checked just now", readingsAgeSeconds: 1_500, busy: true).text
            == "Readings 25m old · refreshing")
    #expect(Presenter.failureFooter(readingsAgeSeconds: panel.readingsAgeSeconds, hasReadings: true)
            == "Couldn’t update · showing readings from 25m ago")
}

@Test func rereviewReadingsAgeUsesAnyProviderWithoutAClaudeRow() throws {
    let data = try snapshot([account("c", provider: "codex", windows: [window(used: 10, remaining: 90)], sample: -120),
                             account("z", provider: "zai", windows: [window(used: 10, remaining: 90)], sample: -1_200)])
    #expect(Presenter.panel(data, now: instant).readingsAgeSeconds == 120)
}

@Test func rereviewFooterKeepsCheckedWithinTheTrustWindow() {
    let fresh = Presenter.footer(checked: "Checked just now", readingsAgeSeconds: 900, busy: false)
    #expect(fresh.text == "Checked just now")
    #expect(!fresh.old)
    #expect(Presenter.footer(checked: "Checked 2m ago", readingsAgeSeconds: 901, busy: false).text == "Readings 15m old")
    #expect(Presenter.footer(checked: nil, readingsAgeSeconds: nil, busy: true).text == "Refreshing…")
}

// MARK: 3. No fresh Claude reading keeps the card's place

@Test func rereviewNoFreshReadingKeepsTheUseNowCard() throws {
    let data = try snapshot([seat("seat1", fiveHour: 15, weekly: 70, sample: -1_500),
                             seat("seat2", fiveHour: 40, weekly: 70, sample: -1_560)])
    let directions = Presenter.directions(Presenter.panel(data, now: instant))
    #expect(directions.best == nil)
    #expect(directions.noRoom == nil)
    let card = try #require(directions.noFresh)
    #expect(card.title == "No fresh reading")
    #expect(card.detail == "Newest 25m old · click to refresh")
    #expect(card.spoken == "No fresh Claude reading. Newest reading 25 minutes old.")
    // The card takes the same height as "Use now", so the layout does not jump.
    let withCard = GaugeLayout.height(directions, searching: false, detail: nil, showBest: true)
    var without = directions
    without.noFresh = nil
    #expect(withCard - GaugeLayout.height(without, searching: false, detail: nil, showBest: true)
            == GaugeLayout.hero + GaugeLayout.sectionGap)
}

@Test func rereviewNoFreshCardPointsAtTheSeatsWhenReadingsAreRecent() throws {
    // Read a minute ago, but no seat counts: one needs sign-in, one has an error.
    let data = try snapshot([seat("seat1", fiveHour: 40, weekly: 70, sample: -60, login: true),
                             seat("seat2", fiveHour: 40, weekly: 70, sample: -60, error: "identity_mismatch")])
    let card = try #require(Presenter.directions(Presenter.panel(data, now: instant)).noFresh)
    #expect(card.detail == "See the seats below · click to refresh")
    #expect(card.spoken == "No fresh Claude reading. See the seats below.")
}

@Test func rereviewNoFreshCardOnlyWithoutABestSeatOrNoRoomNotice() throws {
    let fresh = try snapshot([seat("seat1", fiveHour: 40, weekly: 70)])
    #expect(Presenter.directions(Presenter.panel(fresh, now: instant)).noFresh == nil)
    let out = try snapshot([seat("seat1", fiveHour: 0, weekly: 70)])
    let outDirections = Presenter.directions(Presenter.panel(out, now: instant))
    #expect(outDirections.noRoom != nil)
    #expect(outDirections.noFresh == nil)
    let codexOnly = try snapshot([account("c", provider: "codex", windows: [window(used: 10, remaining: 90)], sample: -1_500)])
    #expect(Presenter.directions(Presenter.panel(codexOnly, now: instant)).noFresh == nil)
}

// MARK: 4. Sign-in seats say how to act

@Test func rereviewSignInCommandUsesTheSeatsRealProviderAndSlot() {
    #expect(Presenter.signInCommand(provider: "claude", slot: "seat2") == "sb account login --provider claude --slot seat2")
    #expect(Presenter.signInCommand(provider: "codex", slot: "work-1") == "sb account login --provider codex --slot work-1")
    #expect(Presenter.signInCommand(provider: "claude", slot: "two words") == "sb account login --provider claude --slot 'two words'")
    #expect(Presenter.signInCommand(provider: "claude", slot: "it's") == #"sb account login --provider claude --slot 'it'\''s'"#)
    #expect(Presenter.signInCommand(provider: "zai", slot: "seat2") == nil)
}

@Test func rereviewSignInSeatHasItsOwnSymbolAndCommand() throws {
    let data = try snapshot([seat("seat2", fiveHour: 40, weekly: 70, login: true),
                             seat("seat3", fiveHour: 40, weekly: 70, error: "identity_mismatch"),
                             seat("seat4", fiveHour: 40, weekly: 70, sample: -1_500)])
    let panel = Presenter.panel(data, now: instant)
    let seats = Presenter.directions(panel).seats
    let signIn = try #require(seats.first { $0.id == "claude:seat2" })
    #expect(signIn.chip == "sign in")
    #expect(signIn.symbol == "person.crop.circle.badge.exclamationmark")
    #expect(signIn.symbol != SeatTone.unknown.symbol)
    let changed = try #require(seats.first { $0.id == "claude:seat3" })
    #expect(changed.chip == "account changed")
    #expect(changed.symbol == SeatGlyph.signInSymbol)
    // An old reading is not a sign-in problem.
    #expect(seats.first { $0.id == "claude:seat4" }?.needsSignIn == false)

    let accounts = panel.providers.flatMap(\.accounts)
    let detail = Presenter.seatDetail(try #require(accounts.first { $0.key == "claude:seat2" }))
    #expect(detail.signInCommand == "sb account login --provider claude --slot seat2")
    #expect(Presenter.seatDetail(try #require(accounts.first { $0.key == "claude:seat3" })).signInCommand
            == "sb account login --provider claude --slot seat3")
    #expect(Presenter.seatDetail(try #require(accounts.first { $0.key == "claude:seat4" })).signInCommand == nil)
    #expect(GaugeLayout.detailHeight(detail) == GaugeLayout.detailHeight(Presenter.seatDetail(
        try #require(accounts.first { $0.key == "claude:seat4" }))) + GaugeLayout.commandRow
            + 18 * Double(detail.issues.count - 1))
}

// MARK: 6. The model legend only when a thin bar has fill

@Test func rereviewModelLegendHiddenWhenNoThinBarHasFill() throws {
    let empty = try snapshot([gaugeSeat("seat1", fiveHour: 60, weekly: 70, fable: 0),
                              gaugeSeat("seat2", fiveHour: 50, weekly: 60, fable: 0)])
    let claude = try #require(Presenter.directions(Presenter.panel(empty, now: instant)).sections.first { $0.id == "claude" })
    #expect(claude.legend == nil)
    #expect(!claude.hasModelBar)
    let some = try snapshot([gaugeSeat("seat1", fiveHour: 60, weekly: 70, fable: 0),
                             gaugeSeat("seat2", fiveHour: 50, weekly: 60, fable: 30)])
    let shown = try #require(Presenter.directions(Presenter.panel(some, now: instant)).sections.first { $0.id == "claude" })
    #expect(shown.legend == "Room for Fable")
    // 9. VoiceOver says the legend in sentence case.
    #expect(shown.legendSpoken?.hasPrefix("Room for Fable: ") == true)
}

// MARK: 7. "No seat has room" says the time once

@Test func rereviewNoRoomSubtitleNamesTheSeatOnce() throws {
    let data = try snapshot([gaugeSeat("seat1", fiveHour: 0, weekly: 70, fiveHourReset: 6 * 3_600 + 59 * 60 + 30),
                             gaugeSeat("seat2", fiveHour: 0, weekly: 70, fiveHourReset: 8 * 3_600)])
    let noRoom = try #require(Presenter.directions(Presenter.panel(data, now: instant)).noRoom)
    #expect(noRoom.detail == "Seat 1 is first back")
    #expect(noRoom.backIn == "6h 59m")
    #expect(!noRoom.detail.contains(noRoom.backIn ?? "-"))
}

// MARK: 8. A muted old tile and its details agree

@Test func rereviewOldTileDetailRowsShowTheLastNumber() throws {
    let data = try snapshot([seat("seat1", fiveHour: 15, weekly: 70, sample: -1_560)])
    let panel = Presenter.panel(data, now: instant)
    let glyph = try #require(Presenter.directions(panel).seats.first)
    #expect(glyph.tone == .stale)
    #expect(glyph.roomText == "15")
    let detail = Presenter.seatDetail(try #require(panel.providers.first?.accounts.first))
    let five = try #require(detail.lines.first)
    #expect(five.value == "15% left")
    #expect(five.age == "26m ago (too old)")
    #expect(five.value + " · " + five.age == "15% left · 26m ago (too old)")
    #expect(five.fraction == nil, "an old reading never draws a measured bar")
    #expect(five.tooOld)
    #expect(detail.lines.allSatisfy { $0.value != "unknown" })
}

@Test func rereviewSignInSeatDetailRowsStayUnknown() throws {
    let data = try snapshot([seat("seat2", fiveHour: 40, weekly: 70, login: true)])
    let detail = Presenter.seatDetail(try #require(Presenter.panel(data, now: instant).providers.first?.accounts.first))
    #expect(detail.lines.allSatisfy { $0.value == "unknown" })
}

// MARK: 9. Polish

@Test func rereviewOldTileHasOneClockMark() throws {
    let data = try snapshot([seat("seat1", fiveHour: 15, weekly: 70, sample: -1_560)])
    let glyph = try #require(Presenter.directions(Presenter.panel(data, now: instant)).seats.first)
    // The clock beside the name is the tone's symbol; the bar carries no second one.
    #expect(glyph.symbol == SeatTone.stale.symbol)
    #expect(!glyph.clock)
}

@Test func rereviewUseNowNamesALongSeatOnce() throws {
    let long = "Engineering shared workspace seat"
    var first = gaugeSeat("seat1", fiveHour: 80, weekly: 90, fable: 70)
    first["label"] = long
    let data = try snapshot([first, gaugeSeat("seat2", fiveHour: 60, weekly: 90, fable: 60)])
    let directions = Presenter.directions(Presenter.panel(data, now: instant))
    #expect(directions.best?.name == long)
    let pick = try #require(directions.modelBest.first)
    #expect(pick.seat?.id == directions.best?.id)
    #expect(pick.sameAsBest, "the card would name the same long seat twice")
    #expect(directions.best?.detail.contains(long) == true, "the full name is in the tooltip")
}

@Test func rereviewSmallPanelFitsItsContent() throws {
    let data = try snapshot([seat("seat1", fiveHour: 40, weekly: 70)])
    let directions = Presenter.directions(Presenter.panel(data, now: instant))
    let content = GaugeLayout.height(directions, searching: false, detail: nil, showBest: true)
    let fitted = GaugeLayout.fitted(content, maximum: 600)
    #expect(fitted == content, "a one-seat panel is padded to \(fitted) for \(content) of content")
    #expect(fitted < 300)
    // Never shorter than the header, one section and the footer.
    #expect(GaugeLayout.minimumPanel == GaugeLayout.header + GaugeLayout.padding * 2 + GaugeLayout.sectionTitle
            + GaugeLayout.rowHeight(modelBar: false) + GaugeLayout.footer)
    #expect(GaugeLayout.fitted(10, maximum: 600) == GaugeLayout.minimumPanel)
    #expect(GaugeLayout.fitted(900, maximum: 600) == 600)
}
